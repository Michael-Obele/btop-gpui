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

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, TitleBar, h_flex, v_flex, window_border};
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Div, ElementId, FocusHandle, Render, ScrollHandle, SharedString, Window, div,
};

use crate::collect::proc::ProcSort;
use crate::collect::{self, Shared};
use crate::config::Config;
use crate::history::{History, clamp_columns};
use crate::model::{ProcSnapshot, Snapshot};
use crate::ui::chrome::{PRESETS, Preset, panel, status_bar};
use crate::ui::theme::{self, ThemeChoice};
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
    /// The right-click menu for a process row.
    ProcessMenu(i32),
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
    /// The theme the user asked for. Kept as the *choice* rather than the
    /// resolved mode, so `System` keeps following the desktop instead of
    /// freezing into whichever mode it happened to resolve to at startup.
    theme_choice: ThemeChoice,
    /// The interface the net panel pins, or `None` for Auto. Cached here
    /// because the config hands out owned strings and `selected_net` has to
    /// return a borrow.
    net_iface: Option<String>,
    /// Scroll position of the process list. Owned here because the list has to
    /// stay clipped and scrollable, not grow to its content height.
    proc_scroll: ScrollHandle,
    /// How long between collector ticks. Held as state rather than captured when
    /// the loop starts, so changing it in the options dialog takes effect on the
    /// next tick instead of needing a new loop.
    tick_interval: Duration,
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
            theme_choice: ThemeChoice::parse(&config.str("theme_mode")),
            net_iface: configured_iface(&config),
            proc_scroll: ScrollHandle::new(),
            tick_interval: Duration::from_secs(2),
            cols: 120,
            last_seen: 0,
            focus_handle: cx.focus_handle(),
            history: History::new(120),
            snapshot: None,
            proc_rows: Vec::new(),
            shared,
            config,
        };
        view.tick_interval = view.config.update_interval();
        view.start_tick_loop(cx);
        view
    }

    /// Start the per-tick timer loop. Detached: it runs for the life of the app,
    /// and there is exactly **one** of it.
    ///
    /// The interval is re-read from the view on every iteration rather than
    /// captured. The previous version passed it in as a parameter and
    /// `apply_config` started a *second* loop — `.detach()` meant the old one
    /// kept running, so every settings change added another loop, each one
    /// pulling the snapshot and notifying.
    fn start_tick_loop(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let interval = match this.update(cx, |this, _| this.tick_interval) {
                    Ok(interval) => interval,
                    Err(_) => break,
                };
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

    /// Apply a config that has already been mutated in memory.
    ///
    /// The collector picks up its half on the next tick, and the running tick
    /// loop re-reads `tick_interval` each iteration — so this needs no `Context`
    /// and, crucially, starts no second loop.
    fn apply_config(&mut self, cfg: Config) {
        self.proc_sort = collect::proc::ProcSort::from_config(&cfg.str("proc_sorting"));
        self.proc_tree = cfg.bool("proc_tree");
        self.filter = collect::proc::compile_filter(&cfg.str("proc_filter"));
        self.net_iface = configured_iface(&cfg);
        self.tick_interval = cfg.update_interval();
        self.config = cfg.clone();
        self.shared.push_config(cfg);
    }

    /// Which interface the net panel shows, honouring `net_iface`.
    ///
    /// The configured name is read from `self.net_iface`, not from the config:
    /// `Config::str` hands back an owned `String`, so the previous version
    /// `Box::leak`ed a fresh copy on every call — and `render()` calls this
    /// once per frame, so it leaked the interface name sixty times a second
    /// for the life of the process.
    fn selected_net(&self) -> Option<&str> {
        if let Some(name) = &self.net_iface {
            return Some(name.as_str());
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
        // Read the one token the root itself paints with, as a `Copy` value.
        // Holding the result of `cx.theme()` across the calls below would borrow
        // `cx` for the whole function, and the process panel needs it mutably.
        let window_bg = cx.theme().background;
        let snap = self.snapshot.as_deref();
        let scale = self.config.size_scale();
        // How much time one ring sample represents. The charts turn this into
        // "-8s" axis labels and tooltip titles, so the reader is told how far
        // back a point is instead of being handed a tick index.
        let tick_secs = self.config.update_interval().as_secs_f32();

        // The right-hand column: the narrow boxes, stacked. `v_flex` leaves the
        // cross axis alone, so these stretch to the column width.
        let mut side = v_flex().flex_none().w_1_3().min_w_0().gap_2();
        if self.shows("mem") {
            side = side.child(panel(
                "Memory",
                panels::mem_panel(snap, &self.history, scale, tick_secs, cx),
                cx,
            ));
        }
        if self.shows("net") {
            side = side.child(panel(
                "Network",
                panels::net_panel(
                    snap,
                    &self.history,
                    self.selected_net(),
                    scale,
                    tick_secs,
                    cx,
                ),
                cx,
            ));
        }
        if self.shows("disks") {
            side = side.child(panel(
                "Disks",
                panels::disk_panel(snap, &self.history, scale, tick_secs, cx),
                cx,
            ));
        }
        // The battery box exists only when there is a battery, which is the
        // normal case on a desktop.
        if snap.and_then(|s| s.battery.as_ref()).is_some() {
            side = side.child(panel("Battery", panels::battery_panel(snap, cx), cx));
        }

        // The top row: a wide CPU box beside the narrow stack.
        //
        // `items_stretch` is load-bearing. `h_flex()` is `.flex_row()
        // .items_center()`, so without it every panel is centred on the cross
        // axis at its own content height and the row floats in the middle of
        // the window — which is exactly how it used to look.
        let mut top = h_flex().items_stretch().flex_none().w_full().gap_2();
        if self.shows("cpu") {
            top = top.child(panel(
                "CPU",
                panels::cpu_panel(snap, &self.history, tick_secs, cx),
                cx,
            ));
        }
        top = top.child(side);

        // The process list takes the height that is left over — and only that
        // much, because its own contents are clipped and scrollable.
        let mut lower = v_flex().flex_1().min_h_0().w_full();
        if self.shows("proc") {
            lower = lower.child(panel(
                "Processes",
                panels::proc_panel(
                    &self.proc_rows,
                    scale,
                    self.selected_pid,
                    self.proc_sort,
                    self.proc_reversed,
                    &self.proc_scroll,
                    cx,
                ),
                cx,
            ));
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
            Dialog::Options => self.options_dialog(cx).into_any_element(),
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
            Dialog::ProcessMenu(pid) => {
                let heading = self
                    .proc_rows
                    .iter()
                    .find(|p| p.pid == *pid)
                    .map(|p| format!("{} · {}", p.pid, p.name))
                    .unwrap_or_else(|| format!("process {pid}"));
                dialogs::process_menu(*pid, &heading, cx).into_any_element()
            }
        };

        // `window_border()` draws the theme-aware window edge. A `TitleBar`
        // ELEMENT is what actually paints the header — setting only
        // `TitleBar::window_options()` reserves the 34px strip and lets the
        // compositor drag the window by it, but draws nothing there. That is
        // why the app had no header at all.
        window_border().child(
            v_flex()
                .size_full()
                .bg(window_bg)
                .child(self.title_bar(cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .p_2()
                        .gap_2()
                        .child(top)
                        .child(lower),
                )
                .child(overlay)
                .child(status_bar(snap))
                // Without a tracked focus handle nothing in this element tree is
                // focusable, and the key handlers below would never fire.
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(Self::on_key_down)),
        )
    }
}

impl AppView {
    /// The window's title bar: the app's icon and name, then the buttons.
    ///
    /// Every action here is also a key — see `on_key_down` and
    /// `actions::key_bindings`. A GUI needs both: the keys for muscle memory,
    /// and visible controls, because a mouse user cannot discover a key map.
    fn title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, foreground, accent) = (theme.muted_foreground, theme.foreground, theme.accent);
        TitleBar::new().child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .gap_3()
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            // `Activity` is a pulse line — it reads as "live
                            // monitor" rather than a generic screen outline.
                            // Size lives on the wrapper: the svg inherits both
                            // size and colour from the text style.
                            div()
                                .flex_none()
                                .size_4()
                                .text_color(accent)
                                .child(Icon::new(IconName::Activity)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui_kit::FontWeight(600.0))
                                .text_color(foreground)
                                .child("btop-gpui"),
                        )
                        .child(div().text_xs().text_color(muted).child(format!(
                            "layout {}/{}",
                            self.preset.number(),
                            PRESETS.len(),
                        ))),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(title_button(
                            "layout",
                            IconName::LayoutDashboard,
                            cx.listener(|this, _e, _w, cx| this.cycle_preset(cx)),
                        ))
                        .child(title_button(
                            self.theme_choice.label(),
                            theme::icon_for(self.theme_choice),
                            cx.listener(|this, _e, _w, cx| this.cycle_theme(cx)),
                        ))
                        .child(title_button(
                            "options",
                            IconName::Settings,
                            cx.listener(|this, _e, _w, cx| this.show_dialog(Dialog::Options, cx)),
                        ))
                        .child(title_button(
                            "help",
                            IconName::Info,
                            cx.listener(|this, _e, _w, cx| this.show_dialog(Dialog::Help, cx)),
                        ))
                        .child(title_button(
                            "quit",
                            IconName::LogOut,
                            cx.listener(|this, _e, _w, cx| this.quit(cx)),
                        )),
                ),
        )
    }
}

/// One title-bar button: an icon, a caption, and a click.
///
/// The caption carries more weight than it looks — an icon-only bar is a
/// guessing game for anyone who does not already know the app.
fn title_button(
    label: &'static str,
    icon: IconName,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    h_flex()
        .id(ElementId::Name(SharedString::from(format!(
            "title-{label}"
        ))))
        .flex_none()
        .items_center()
        .gap_1()
        .px_2()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .child(div().flex_none().size_4().child(Icon::new(icon)))
        .child(div().text_xs().child(label))
        .on_click(on_click)
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
        // Shift arrives as a modifier, not in `key`, so `t` and `Shift-T` are
        // both reported as "t" here.
        let shift = event.keystroke.modifiers.shift;

        // Keys that work whether or not a process is selected.
        //
        // The map follows btop's own wherever it can be checked against
        // `btop_input.cpp`, so muscle memory carries over. Every one of these is
        // also a clickable control: a GUI cannot make a keystroke the only route
        // to anything.
        match (key, shift) {
            ("escape", _) => {
                self.dialog = Dialog::None;
                cx.notify();
                return;
            }
            // btop uses `h`; `?` is the convention on this side. Both work.
            ("question-mark", _) | ("h", false) => {
                self.dialog = match self.dialog {
                    Dialog::Help => Dialog::None,
                    _ => Dialog::Help,
                };
                cx.notify();
                return;
            }
            // btop's `m` opens its main menu; ours is the options dialog.
            ("m", false) => {
                self.show_dialog(Dialog::Options, cx);
                return;
            }
            ("q", false) => {
                // `q` must not quit out from under an open dialog — the user
                // typing at a confirmation is not asking to close the app.
                if matches!(self.dialog, Dialog::None) {
                    self.quit(cx);
                }
                return;
            }
            ("p", false) => {
                self.cycle_preset(cx);
                return;
            }
            ("p", true) => {
                self.preset = self.preset.prev();
                cx.notify();
                return;
            }
            // btop uses `e` for the tree. `t` is SIGTERM, so the old
            // Shift-T binding was both wrong and in the way.
            ("e", false) => {
                self.toggle_tree(cx);
                return;
            }
            ("r", false) => {
                self.proc_reversed = !self.proc_reversed;
                cx.notify();
                return;
            }
            ("d", true) => {
                self.cycle_theme(cx);
                return;
            }
            _ => {}
        }

        // Everything below acts on a selected process.
        let Some(pid) = self.selected_pid else {
            return;
        };
        match (key, shift) {
            // Destructive, so these ask first — the same path the context menu
            // takes, so the guard cannot be bypassed by using the mouse.
            ("t", false) => self.request_signal(pid, "SIGTERM", cx),
            ("k", false) => self.request_signal(pid, "SIGKILL", cx),
            ("enter", _) => self.open_detail(pid, cx),
            ("+", _) | ("=", _) => self.apply_nice(pid, 5, cx),
            ("-", _) | ("_", _) => self.apply_nice(pid, -5, cx),
            _ => {}
        }
    }

    // ---- imperative API used by tests and the options dialog ----

    pub fn reload_config(&mut self, cfg: Config, cx: &mut Context<Self>) {
        self.apply_config(cfg);
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

    /// Set the sort column, or flip the direction if it is already active.
    ///
    /// One implementation behind three entry points: the `c` key, clicking a
    /// sort pill, and the options dialog. Behaviour that can be reached four
    /// ways must not be written four times.
    pub fn sort_by(&mut self, column: ProcSort, cx: &mut Context<Self>) {
        if self.proc_sort == column {
            self.proc_reversed = !self.proc_reversed;
        } else {
            self.proc_sort = column;
            // `proc_reversed == false` is *descending* — verified against the
            // running app, not assumed. So "biggest first" for the cost columns
            // means false, and the identifier columns want true.
            self.proc_reversed = !matches!(
                column,
                ProcSort::Memory | ProcSort::CpuDirect | ProcSort::CpuLazy
            );
            // The column is a config value and outlives the window; the
            // direction is view state and deliberately does not.
            self.config.set("proc_sorting", self.proc_sort.label());
            self.persist_or_log();
        }
        cx.notify();
    }

    /// Open the per-process detail sheet. Bound to `Enter` and to a double click.
    pub fn open_detail(&mut self, pid: i32, cx: &mut Context<Self>) {
        self.selected_pid = Some(pid);
        self.dialog = Dialog::Detail(pid);
        cx.notify();
    }

    /// Open the right-click menu for a process row.
    pub fn open_process_menu(&mut self, pid: i32, cx: &mut Context<Self>) {
        self.selected_pid = Some(pid);
        self.dialog = Dialog::ProcessMenu(pid);
        cx.notify();
    }

    /// Ask before signalling a process.
    ///
    /// Both `t`/`k` and the context menu land here, so the destructive path has
    /// a single implementation and a single guard. btop confirms too.
    pub fn request_signal(&mut self, pid: i32, signal: &'static str, cx: &mut Context<Self>) {
        self.dialog = Dialog::ConfirmKill(pid, signal);
        cx.notify();
    }

    /// Send the confirmed signal and report the outcome.
    pub fn apply_signal(&mut self, pid: i32, signal: &'static str, cx: &mut Context<Self>) {
        use crate::ui::actions::signal_result;
        let result = match signal {
            "SIGKILL" => signal_result(pid, nix::sys::signal::Signal::SIGKILL),
            _ => signal_result(pid, nix::sys::signal::Signal::SIGTERM),
        };
        // `EPERM` is the common failure and it is not an app error — the
        // process just belongs to someone else. Either way the user is told.
        self.dialog = Dialog::Message(match result {
            Ok(text) => text,
            Err(text) => text,
        });
        cx.notify();
    }

    /// Change a process' nice value, reporting the outcome.
    pub fn apply_nice(&mut self, pid: i32, delta: i32, cx: &mut Context<Self>) {
        use crate::ui::actions::nice_result;
        self.dialog = Dialog::Message(match nice_result(pid, delta) {
            Ok(text) => text,
            Err(text) => text,
        });
        cx.notify();
    }

    /// Write the config back to disk, logging once if it fails.
    ///
    /// A failed save is not worth interrupting the user for — the change has
    /// already been applied in memory.
    fn persist_or_log(&self) {
        if self.config.save(&crate::config::config_path()).is_err() {
            crate::logger::once("config-save-failed", "could not save the configuration");
        }
    }

    /// Apply a config that has already been mutated in memory.
    fn apply_and_persist(&mut self, cx: &mut Context<Self>) {
        self.persist_or_log();
        self.apply_config(self.config.clone());
        cx.notify();
    }

    /// Flip a boolean option, apply it live, and persist it.
    fn toggle_option(&mut self, key: &str, cx: &mut Context<Self>) {
        let next = !self.config.bool(key);
        self.config.set(key, if next { "True" } else { "False" });
        self.apply_and_persist(cx);
    }

    /// Step through btop's update intervals.
    ///
    /// Picks the first step *longer* than the current value rather than the
    /// next array slot, so a hand-edited `update_ms` still moves forward
    /// instead of snapping back to the start.
    fn cycle_update_interval(&mut self, cx: &mut Context<Self>) {
        const STEPS_MS: [u64; 5] = [500, 1_000, 2_000, 5_000, 10_000];
        let now = self.config.update_interval().as_millis() as u64;
        let next = STEPS_MS
            .iter()
            .copied()
            .find(|ms| *ms > now)
            .unwrap_or(STEPS_MS[0]);
        self.config.set("update_ms", &next.to_string());
        self.apply_and_persist(cx);
    }

    /// Advance the sort column, always moving to a different one.
    fn cycle_sort(&mut self, cx: &mut Context<Self>) {
        self.sort_by(self.proc_sort.next(), cx);
    }

    /// Flip the sort direction.
    fn reverse_sort(&mut self, cx: &mut Context<Self>) {
        self.proc_reversed = !self.proc_reversed;
        cx.notify();
    }

    /// btop's options menu, as a GUI.
    ///
    /// Every row applies live and is written back to the config, and every row
    /// is a click target — the same rule as everywhere else: a keystroke may be
    /// a shortcut for a control, never the only way to reach it.
    fn options_dialog(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, foreground) = (theme.muted_foreground, theme.foreground);
        let on_off = |on: bool| if on { "on" } else { "off" };
        let scale_label = if self.config.size_scale() == crate::format::SizeScale::Decimal {
            "decimal (GB)"
        } else {
            "binary (GiB)"
        };

        let rows = v_flex()
            .gap_1()
            .child(dialogs::option_section("General", muted))
            .child(dialogs::option_row(
                "theme",
                self.theme_choice.label(),
                foreground,
                cx.listener(|this, _e, _w, cx| this.cycle_theme(cx)),
            ))
            .child(dialogs::option_row(
                "update interval",
                &format!("{} ms", self.config.update_interval().as_millis()),
                foreground,
                cx.listener(|this, _e, _w, cx| this.cycle_update_interval(cx)),
            ))
            .child(dialogs::option_row(
                "size units",
                scale_label,
                foreground,
                cx.listener(|this, _e, _w, cx| {
                    let next = if this.config.bool("base_10_sizes") {
                        "False"
                    } else {
                        "True"
                    };
                    this.config.set("base_10_sizes", next);
                    this.apply_and_persist(cx);
                }),
            ))
            .child(dialogs::option_row(
                "battery box",
                on_off(self.config.bool("show_battery")),
                foreground,
                cx.listener(|this, _e, _w, cx| this.toggle_option("show_battery", cx)),
            ))
            .child(dialogs::option_row(
                "disk box",
                on_off(self.config.bool("show_disks")),
                foreground,
                cx.listener(|this, _e, _w, cx| this.toggle_option("show_disks", cx)),
            ))
            .child(dialogs::option_section("Processes", muted))
            .child(dialogs::option_row(
                "tree view",
                on_off(self.proc_tree),
                foreground,
                cx.listener(|this, _e, _w, cx| {
                    this.proc_tree = !this.proc_tree;
                    this.config
                        .set("proc_tree", if this.proc_tree { "True" } else { "False" });
                    this.apply_and_persist(cx);
                }),
            ))
            .child(dialogs::option_row(
                "per-core cpu",
                on_off(self.config.bool("proc_per_core")),
                foreground,
                cx.listener(|this, _e, _w, cx| this.toggle_option("proc_per_core", cx)),
            ))
            .child(dialogs::option_row(
                "sort column",
                self.proc_sort.label(),
                foreground,
                cx.listener(|this, _e, _w, cx| this.cycle_sort(cx)),
            ))
            .child(dialogs::option_row(
                "sort direction",
                if self.proc_reversed {
                    "ascending"
                } else {
                    "descending"
                },
                foreground,
                cx.listener(|this, _e, _w, cx| this.reverse_sort(cx)),
            ));

        dialogs::modal(
            cx,
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui_kit::FontWeight(500.0))
                        .text_color(foreground)
                        .child("Options"),
                )
                .child(rows)
                .child(
                    div()
                        .pt_1()
                        .text_xs()
                        .text_color(muted)
                        .child("Esc closes this · everything here is also a key"),
                ),
        )
    }

    /// Quit. Bound to `q` and to the title-bar button.
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        cx.quit();
    }

    pub fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        self.proc_tree = !self.proc_tree;
        cx.notify();
    }

    /// Advance the theme: System -> Dark -> Light -> System.
    ///
    /// The choice is resolved once and applied globally — `Theme::change`
    /// restyles every panel that reads `cx.theme()` — then written back to the
    /// config so it survives a restart. A failed save is logged once and is not
    /// worth interrupting the user for; the theme has already changed.
    pub fn cycle_theme(&mut self, cx: &mut Context<Self>) {
        self.theme_choice = self.theme_choice.next();
        theme::set(self.theme_choice.resolve(), cx);
        self.config.set("theme_mode", self.theme_choice.label());
        if self.config.save(&crate::config::config_path()).is_err() {
            crate::logger::once("theme-save-failed", "could not save the theme choice");
        }
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

/// `net_iface = Auto` (or an empty value) means "whichever interface the
/// collector ranked busiest"; anything else is a name to pin.
///
/// Split out of `AppView` so it can be tested without a window.
pub fn configured_iface(cfg: &Config) -> Option<String> {
    let value = cfg.str("net_iface");
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_and_blank_mean_no_pin() {
        for value in ["Auto", "auto", "  AUTO  ", ""] {
            let cfg = Config::parse(&format!("net_iface = {value}\n"));
            assert!(
                configured_iface(&cfg).is_none(),
                "{value:?} should not pin an interface"
            );
        }
    }

    #[test]
    fn a_named_interface_is_pinned_and_trimmed() {
        let cfg = Config::parse("net_iface = wlan0\n");
        assert_eq!(configured_iface(&cfg).as_deref(), Some("wlan0"));
        let cfg = Config::parse("net_iface =   enp0s3  \n");
        assert_eq!(configured_iface(&cfg).as_deref(), Some("enp0s3"));
    }
}
