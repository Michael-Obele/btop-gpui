//! Dialogs: the confirm-kill prompt, the process detail sheet and the help
//! overlay.
//!
//! # Open/close is application state
//!
//! There is no imperative `open()`. The app holds an enum (`AppView::dialog`)
//! and a dialog renders when its variant is active; closing is setting the
//! variant back to `None`. Nothing can be left open by a forgotten callback,
//! because there is no callback to forget.

use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, App, IntoElement};

use crate::format::{self, SizeScale};
use crate::model::ProcSnapshot;

/// A modal shell: a centred card over a dimmed backdrop.
pub fn modal(cx: &App, content: impl IntoElement) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .absolute()
        .inset_0()
        .items_center()
        .justify_center()
        .bg(theme.background)
        .p_4()
        .child(
            v_flex()
                .w(px(520.))
                .p_4()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .child(content),
        )
}

/// "Send SIGKILL to 1234?"
pub fn confirm_kill(pid: i32, signal: &str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .gap_3()
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(format!("Send {signal} to process {pid}?")),
        )
        .child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.foreground)
                        .child("Esc to cancel"),
                )
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(theme.danger)
                        .text_xs()
                        .text_color(theme.background)
                        .child(signal.to_string()),
                ),
        )
}

/// A read-only key/value row for the detail sheet.
fn row(label: &str, value: String, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .gap_2()
        .text_xs()
        .child(
            div()
                .w(px(96.))
                .text_color(theme.muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .text_color(theme.foreground)
                .child(value),
        )
}

/// The per-process detail sheet.
///
/// `/proc/<pid>/io` is read **here and only here**, never for the list. btop's
/// own comment is that parsing `smaps` across the list raises total CPU usage
/// by about 20x; the same reasoning applies here. It is usually EPERM for
/// another user's process, which shows as a dash rather than as zero.
pub fn process_detail(p: &ProcSnapshot, show_io: bool, cx: &App) -> impl IntoElement {
    let scale = SizeScale::Binary;
    let theme = cx.theme();

    let (read, write) = if show_io {
        crate::collect::proc::read_proc_io(p.pid)
    } else {
        (None, None)
    };

    v_flex()
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_weight(gpui_kit::FontWeight(500.0))
                .text_color(theme.foreground)
                .child(format!("{} ({})", p.name, p.pid)),
        )
        .child(row(
            "state",
            crate::collect::proc::state_label(p.state).to_string(),
            cx,
        ))
        .child(row("user", p.user.clone(), cx))
        .child(row("parent", p.ppid.to_string(), cx))
        .child(row("threads", p.threads.to_string(), cx))
        .child(row("nice", p.nice.to_string(), cx))
        .child(row("memory", format::bytes(p.mem_bytes, scale), cx))
        .child(row("cpu now", format::percent(p.cpu_percent, 1), cx))
        .child(row("cpu avg", format::percent(p.cpu_cumulative, 1), cx))
        .child(row(
            "read",
            read.map(|v| format::bytes(v, scale))
                .unwrap_or_else(|| "—".to_string()),
            cx,
        ))
        .child(row(
            "written",
            write.map(|v| format::bytes(v, scale))
                .unwrap_or_else(|| "—".to_string()),
            cx,
        ))
        .child(row("command", p.cmdline.clone(), cx))
}

/// The help overlay, listing every binding.
pub fn help(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    modal(
        cx,
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui_kit::FontWeight(500.0))
                    .text_color(theme.foreground)
                    .child("Keyboard"),
            )
            .children(crate::ui::actions::key_bindings().into_iter().map(
                |(key, what)| {
                    h_flex()
                        .gap_3()
                        .text_xs()
                        .child(div().w(px(80.)).text_color(theme.accent).child(key))
                        .child(div().text_color(theme.foreground).child(what))
                },
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Esc closes this"),
            ),
    )
}

/// A simple message or error.
pub fn message(text: &str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    modal(
        cx,
        div()
            .text_sm()
            .text_color(theme.foreground)
            .child(text.to_string()),
    )
}