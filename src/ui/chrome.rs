//! The window chrome: title bar, status bar, panel frames and presets.
//!
//! Everything here is presentational. It reads the snapshot and history that
//! `AppView` already owns and never touches `/proc` — a read in `render()`
//! would block the UI thread, which is the exact failure the collector thread
//! exists to prevent.

use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Div, IntoElement, WindowOptions, div, px};

use crate::model::Snapshot;

/// A bordered box with a title.
///
/// Returns a concrete `Div`, **not** `impl IntoElement`, so callers can keep
/// chaining `.child(..)` onto it — an opaque `impl IntoElement` hides the
/// `ParentElement` methods and the call site fails to compile.
pub fn panel_frame(title: &str, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .border_1()
        .rounded_md()
        .border_color(theme.border)
        .bg(theme.popover)
        .overflow_hidden()
        .child(
            h_flex()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui_kit::FontWeight(500.0))
                        .text_color(theme.foreground)
                        .child(title.to_string()),
                ),
        )
}

/// The body area of a panel frame, between the title and the content.
pub fn panel_body() -> Div {
    v_flex().flex_1().min_h_0().p_2().gap_2()
}

/// A panel frame with its body and a single content child, which is the shape
/// every panel in this app has. Saves five lines per panel at the call site.
pub fn panel(title: &str, body: impl IntoElement, cx: &App) -> Div {
    panel_frame(title, cx).child(panel_body().child(body))
}

/// Window options with the gpui-kit title bar wired in.
pub fn window_options() -> WindowOptions {
    WindowOptions {
        window_min_size: Some(gpui_kit::size(px(900.), px(600.))),
        ..TitleBar::window_options()
    }
}

/// The live counters along the bottom of the window.
pub fn status_bar(snapshot: Option<&Snapshot>) -> impl IntoElement {
    let text = match snapshot {
        Some(s) => {
            use crate::format;
            format!(
                "CPU {}   Mem {}   Net {}   Procs {}",
                format::percent(s.cpu.total_percent, 0),
                format::percent(s.mem.used_percent, 0),
                s.nets.len(),
                s.procs.len(),
            )
        }
        None => "starting…".to_string(),
    };
    StatusBar::new().left(text).right("btop-gpui")
}

/// One of btop's layout presets, as a list of box names in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset(pub &'static [&'static str]);

pub const PRESETS: [Preset; 4] = [
    Preset(&["cpu", "mem", "net", "proc"]),
    Preset(&["cpu", "cpu", "mem", "disk", "net", "proc"]),
    Preset(&["cpu", "mem", "disk", "proc"]),
    Preset(&["cpu", "net", "disk", "mem", "proc"]),
];

impl Preset {
    /// This preset's position, or 0 if it is somehow not in the list.
    fn index(&self) -> usize {
        PRESETS.iter().position(|p| p == self).unwrap_or(0)
    }

    /// `p` and `Shift-P` step through the list, wrapping at both ends.
    pub fn next(self) -> Preset {
        PRESETS[(self.index() + 1) % PRESETS.len()]
    }

    pub fn prev(self) -> Preset {
        let i = self.index();
        PRESETS[(i + PRESETS.len() - 1) % PRESETS.len()]
    }

    pub fn contains(&self, box_name: &str) -> bool {
        self.0.contains(&box_name)
    }
}

/// A thin progress bar with a label beside it.
///
/// `Progress` in gpui-kit is **not** a `ParentElement`, so the label has to be
/// a sibling rather than a child — `.child("50%")` on a `Progress` does not
/// compile.
///
/// The element id **must** be unique per instance: gpui-kit derives the a11y
/// node id from it, and two bars sharing one id panic at window-build time with
/// "Duplicate a11y node id". The label is used as the discriminator, which is
/// why it is passed in rather than being derived from the value.
pub fn meter(label: String, value: f32, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let id = format!("meter-{label}");
    h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            gpui_kit::component::progress::Progress::new(id)
                .value(value.clamp(0.0, 100.0))
                .flex_1(),
        )
}

/// Renders `—` when there is no snapshot yet, so "idle" is never confused with
/// "not read yet". The first CPU tick is exactly that case.
pub fn or_dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "—".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_cycle_in_both_directions() {
        let first = PRESETS[0];
        let last = PRESETS[PRESETS.len() - 1];
        // Wrapping at both ends.
        assert_eq!(last.next(), first);
        assert_eq!(first.prev(), last);
        // A step forward then back returns to where it started.
        assert_eq!(first.next().prev(), first);
        // A step back then forward likewise.
        assert_eq!(first.prev().next(), first);
        // Two steps forward is not the same as one step forward.
        assert_ne!(first.next().next(), first.next());
    }

    #[test]
    fn every_preset_is_distinct() {
        // `Preset::next` looks its index up by equality, so two presets with
        // identical contents would make cycling skip or repeat one.
        for (i, a) in PRESETS.iter().enumerate() {
            for (j, b) in PRESETS.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "presets {i} and {j} are identical");
                }
            }
        }
    }

    #[test]
    fn a_preset_reports_its_members() {
        assert!(PRESETS[0].contains("cpu"));
        assert!(PRESETS[0].contains("proc"));
        assert!(!PRESETS[0].contains("disk"));
    }

    #[test]
    fn dash_is_the_missing_value() {
        assert_eq!(or_dash(None), "—");
        assert_eq!(or_dash(Some("50%".to_string())), "50%");
    }
}
