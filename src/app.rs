//! The root view: owns the state, runs the tick loop, and renders the grid.
//!
//! # The tick loop
//!
//! There is **no `Timer::after`** in GPUI. `Timer` is only constructed by
//! `Executor::timer(duration)` and is a `Future<Output = ()>`. `cx.spawn`
//! hands the closure a `WeakEntity<Self>` and an `&mut AsyncApp` (verified at
//! `gpui-pre-0.3.7/src/app/context.rs:230`), so the loop is:
//!
//! ```ignore
//! cx.spawn(async move |this, cx| loop {
//!     cx.background_executor().timer(interval).await;
//!     if this.update(cx, |this, cx| { this.pull(); cx.notify(); }).is_err() {
//!         break;
//!     }
//! })
//! .detach();
//! ```
//!
//! `WeakEntity::update` returning `Err` is how the loop ends: the view was
//! dropped and there is nothing left to update.
//!
//! # `pull()` vs `render()`
//!
//! `pull()` runs once per tick and does everything that allocates: taking the
//! snapshot, folding it into the history, and building the sorted/filtered
//! process view. `render()` is a pure read of what `pull()` left behind — no
//! sorting, no collection building, and above all **no `cx.notify()`**, which
//! would schedule another frame and spin the UI thread forever.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Context, FocusHandle, Render, Window, div, px};

use crate::collect::{self, Shared};
use crate::config::Config;
use crate::history::{History, clamp_columns};
use crate::model::{ProcSnapshot, Snapshot};
use crate::ui::chrome::{PRESETS, Preset, panel, status_bar};
use crate::ui::{dialogs, panels};

/// What the user can act on. UI state never goes in the model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Dialog {
    #[default]
    None,
    Help,
    Options,
    /// `(pid, signal name)` pending confirmation.
    ConfirmKill(i32, &'static str),
    /// A transient result or error message.
    Message(String),
    /// The per-process detail sheet.
    Detail(i32),
}

pub struct AppView {
    shared: Arc<Shared>,
    config: Config,

    /// The newest snapshot, already behind an `Arc`. `render()` must never
    /// clone it: it owns a `Vec` of every process on the machine.
    snapshot: Option<Arc<Snapshot>>,
    /// The visible process rows, ordered and filtered. Rebuilt once per tick.
    proc_rows: Vec<ProcSnapshot>,
    history: History,

    // ---- UI state ----
    preset: Preset,
    proc_sort: collect::proc::ProcSort,
    proc_reversed: bool,
    proc_tree: bool,
    filter: collect::proc::CompiledFilter,
    selected_pid: Option<i32>,
    dialog: Dialog,
    /// Graph columns to keep: `width * 2`, matching btop's
    /// two-samples-per-rendered-column rule.
    cols: usize,

    /// The collector's monotonic tick counter, so the view only rebuilds on a
    /// new tick rather than on every timer wake.
    last_seen: u64,
    focus_handle: FocusHandle,
}

impl AppView {
    pub fn new(config: Config, shared: Arc<Shared>, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            proc_sort: collect::proc::ProcSort::from_config(&config.str("proc_sorting")),
            proc_tree: config.bool("proc_tree"),
            filter: collect::proc::compile_filter(&config.str("proc_filter")),
            preset: PRESETS[0],
            proc_reversed: false,
            selected_pid: None,
            dialog: Dialog::None,
            cols: 120,
            last_seen: 0,
            focus_handle: cx.focus_handle(),
            history: History::new(120),
            snapshot: None,
            proc_rows: Vec::new(),
            shared,
            config,
        };
        // Read the interval before the borrow of `self` in the call.
        let interval = view.config.update_interval();
        view.start_tick_loop(interval, cx);
        view
    }

    /// Start the per-tick timer loop. Detached: it runs for the life of the app.
    fn start_tick_loop(&mut self, interval: Duration, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                // `update` returns Err once the view is gone, which is how this
                // loop terminates rather than spinning forever.
                match this.update(cx, |this, cx| {
                    this.pull();
                    cx.notify();
                }) {
                    Ok(()) => {}
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    /// One tick's worth of work. Everything that allocates happens here.
    fn pull(&mut self) {
        let Some(snapshot) = self.shared.take_if_dirty(&mut self.last_seen) else {
            return;
        };

        let cols = self.cols;
        // `mem::take` so the rings can be handed to `push_snapshot` as one
        // `&mut` borrow instead of field by field.
        let mut history = std::mem::take(&mut self.history);
        history.push_snapshot(&snapshot, cols);
        self.history = history;

        // `build_proc_view` takes `&mut [ProcSnapshot]` because tree mode
        // writes the depth and the box-drawing prefix onto each row. Only the
        // rows that survive the filter are cloned out.
        let mut procs = snapshot.procs.clone();
        let order = collect::proc::build_proc_view(
            &mut procs,
            self.proc_sort,
            self.proc_reversed,
            self.proc_tree,
            &self.filter,
            false,
        );
        self.proc_rows = order.into_iter().map(|i| procs[i].clone()).collect();

        // Drop a selection whose process has exited, so a stale row cannot
        // send a signal to a recycled pid.
        if let Some(pid) = self.selected_pid
            && !self.proc_rows.iter().any(|p| p.pid == pid)
        {
            self.selected_pid = None;
        }

        self.snapshot = Some(snapshot);
    }

    /// Apply a new config. The collector picks up its half on the next tick;
    /// the UI half applies immediately.
    fn apply_config(&mut self, cfg: Config, cx: &mut Context<Self>) {
        self.proc_sort = collect::proc::ProcSort::from_config(&cfg.str("proc_sorting"));
        self.proc_tree = cfg.bool("proc_tree");
        self.filter = collect::proc::compile_filter(&cfg.str("proc_filter"));
        let interval = cfg.update_interval();
        self.config = cfg.clone();
        self.shared.push_config(cfg);
        // Restart the loop so a new cadence takes effect now rather than after
        // the old interval elapses once more.
        self.start_tick_loop(interval, cx);
    }

    /// Which interface the net panel shows, honouring `net_iface`.
    fn selected_net(&self) -> Option<&str> {
        let configured = self.config.str("net_iface");
        if configured != "Auto" && !configured.trim().is_empty() {
            // Borrowed from the config, which outlives the call.
            let leaked: &'static str = Box::leak(configured.into_boxed_str());
            return Some(leaked);
        }
        // Auto: the collector already ranked them, busiest first.
        self.snapshot
            .as_deref()
            .and_then(|s| s.nets.first())
            .map(|n| n.name.as_str())
    }

    /// The rows of the process list, or an empty slice before the first tick.
    pub fn proc_rows(&self) -> &[ProcSnapshot] {
        &self.proc_rows
    }

    pub fn selected_pid(&self) -> Option<i32> {
        self.selected_pid
    }

    pub fn preset(&self) -> Preset {
        self.preset
    }

    /// The age of the current snapshot, for the status bar.
    pub fn snapshot_age(&self, now: Instant) -> Option<Duration> {
        self.snapshot
            .as_ref()
            .map(|s| now.saturating_duration_since(s.at))
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let snap = self.snapshot.as_deref();
        let scale = self.config.size_scale();
        let cols = clamp_columns(self.cols);

        // The battery panel is absent entirely when there is no battery, which
        // is the normal case on a desktop.
        let mut grid = h_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_2()
            .child(panel(
                "CPU",
                panels::cpu_panel(snap, &self.history, cols, cx),
                cx,
            ))
            .child(panel(
                "Memory",
                panels::mem_panel(snap, &self.history, cols, scale, cx),
                cx,
            ))
            .child(panel(
                "Network",
                panels::net_panel(snap, &self.history, self.selected_net(), cols, scale, cx),
                cx,
            ))
            .child(panel(
                "Disks",
                panels::disk_panel(snap, &self.history, scale, cx),
                cx,
            ))
            .child(panel(
                "Processes",
                panels::proc_panel(&self.proc_rows, scale, self.selected_pid, cx),
                cx,
            ));

        if snap.and_then(|s| s.battery.as_ref()).is_some() {
            grid = grid.child(panel("Battery", panels::battery_panel(snap, cx), cx));
        }

        // The dialog layer sits above the grid and is a no-op when closed.
        //
        // `AnyElement`, not `Div`: the arms have different concrete types (a
        // `v_flex`, a `modal`, a bare `div`), and `Div` has no `From<AnyElement>`
        // so the arms cannot be unified into a `Div` the way they unify into an
        // element.
        let overlay: gpui_kit::AnyElement = match &self.dialog {
            Dialog::None => div().into_any_element(),
            Dialog::Help => dialogs::help(cx).into_any_element(),
            Dialog::Options => {
                dialogs::message("Options live in the config file", cx).into_any_element()
            }
            Dialog::ConfirmKill(pid, signal) => {
                dialogs::confirm_kill(*pid, signal, cx).into_any_element()
            }
            Dialog::Message(text) => dialogs::message(text, cx).into_any_element(),
            Dialog::Detail(pid) => {
                match snap.and_then(|_| self.proc_rows.iter().find(|p| p.pid == *pid)) {
                    Some(p) => {
                        dialogs::modal(cx, dialogs::process_detail(p, false, cx)).into_any_element()
                    }
                    None => dialogs::message("that process has exited", cx).into_any_element(),
                }
            }
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(grid)
            .child(overlay)
            .child(status_bar(snap))
            // Without a tracked focus handle nothing in this element tree is
            // focusable, and the key handlers below would never fire.
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
    }
}

impl AppView {
    /// The key handler. Every branch is a state change; nothing here does I/O
    /// beyond the signal syscalls, which are instant.
    fn on_key_down(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();

        // `Esc` always closes, whatever is open.
        if key == "escape" {
            self.dialog = Dialog::None;
            cx.notify();
            return;
        }
        if key == "question-mark" {
            self.dialog = match self.dialog {
                Dialog::Help => Dialog::None,
                _ => Dialog::Help,
            };
            cx.notify();
            return;
        }

        // Everything below acts on a selected process.
        let Some(pid) = self.selected_pid else {
            return;
        };
        let result = match key {
            "t" => crate::ui::actions::signal_result(pid, nix::sys::signal::Signal::SIGTERM),
            "k" => crate::ui::actions::signal_result(pid, nix::sys::signal::Signal::SIGKILL),
            "+" | "=" => crate::ui::actions::nice_result(pid, 5),
            "-" | "_" => crate::ui::actions::nice_result(pid, -5),
            _ => return,
        };
        self.dialog = match result {
            Ok(text) => Dialog::Message(text),
            Err(text) => Dialog::Message(text),
        };
        cx.notify();
    }

    // ---- imperative API used by tests and the options dialog ----

    pub fn reload_config(&mut self, cfg: Config, cx: &mut Context<Self>) {
        self.apply_config(cfg, cx);
        cx.notify();
    }

    pub fn set_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter = collect::proc::compile_filter(text);
        cx.notify();
    }

    pub fn select(&mut self, pid: Option<i32>, cx: &mut Context<Self>) {
        self.selected_pid = pid;
        cx.notify();
    }

    pub fn show_dialog(&mut self, dialog: Dialog, cx: &mut Context<Self>) {
        self.dialog = dialog;
        cx.notify();
    }

    pub fn cycle_preset(&mut self, cx: &mut Context<Self>) {
        self.preset = self.preset.next();
        cx.notify();
    }

    pub fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        self.proc_tree = !self.proc_tree;
        cx.notify();
    }

    /// Resize the history rings to the new window width.
    pub fn set_columns(&mut self, cols: usize, cx: &mut Context<Self>) {
        self.cols = clamp_columns(cols);
        self.history.set_columns(self.cols);
        cx.notify();
    }

    /// Whether a given panel is on screen in the current preset.
    pub fn shows(&self, name: &str) -> bool {
        self.preset.contains(name)
    }
}

/// The height the process list is allowed to grow to before it scrolls.
/// A full-height column inside an `h_flex` row needs an explicit height or it
/// takes its content height and overflows equally top and bottom.
pub fn proc_list_height(cx: &App) -> gpui_kit::Pixels {
    let _ = cx;
    px(320.)
}
