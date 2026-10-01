//! The panels. Each one is a pure function of an `Option<&Snapshot>` and the
//! `History` — no `/proc` reads, no sorting, no allocation of collections.
//!
//! Every panel with no data renders a dash rather than a zero, because "idle"
//! and "not read yet" look identical otherwise and only one of them is true.

use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Div, IntoElement, div, px};

use crate::format::{self, SizeScale};
use crate::history::History;
use crate::model::{CPU_FIELD_NAMES, ProcSnapshot, Snapshot};
use crate::ui::chart;
use crate::ui::chrome::{meter, or_dash, panel_body};

/// Renders `—` when there is no snapshot yet.
fn no_data(cx: &App) -> Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child("—")
}

pub fn cpu_panel(
    snapshot: Option<&Snapshot>,
    history: &History,
    width: usize,
    cx: &App,
) -> impl IntoElement {
    let Some(s) = snapshot else {
        return no_data(cx);
    };
    let theme = cx.theme();
    let total = chart::current_label(&history.cpu_total, |v| format::percent(v, 0));

    panel_body()
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_lg()
                        .font_weight(gpui_kit::FontWeight(500.0))
                        .text_color(theme.foreground)
                        .child(format!("CPU {total}")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(s.cpu.model_name.clone()),
                ),
        )
        // Height goes on the PARENT: the chart element always requests
        // `Size::full()` and takes no size of its own.
        .child(div().h(px(110.)).child(chart::percent_chart(
            "cpu-total",
            &history.cpu_total,
            theme.accent,
            width,
        )))
        .child(
            div()
                .h(px(70.))
                .child(chart::cpu_fields_chart(history, &field_colors(cx), width)),
        )
        .child(meter(format!("total {total}"), s.cpu.total_percent, cx))
        .child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("load {:.2}", s.cpu.load_avg[0]))
                .child(format!("up {}", format::duration(s.cpu.uptime_seconds)))
                .child(format!("cores {}", s.cpu.core_count)),
        )
}

/// The colour per CPU field, in `CPU_FIELD_NAMES` order, taken from the theme
/// so a theme change restyles the graphs with no code edit.
fn field_colors(cx: &App) -> Vec<gpui_kit::Hsla> {
    let t = cx.theme();
    vec![
        t.chart_1,
        t.chart_2,
        t.chart_3,
        t.chart_4,
        t.chart_5,
        t.chart_bullish,
        t.chart_bearish,
        t.info,
    ]
}

pub fn mem_panel(
    snapshot: Option<&Snapshot>,
    history: &History,
    width: usize,
    scale: SizeScale,
    cx: &App,
) -> impl IntoElement {
    let Some(s) = snapshot else {
        return no_data(cx);
    };
    let theme = cx.theme();
    panel_body()
        .child(
            h_flex()
                .justify_between()
                .child(div().text_sm().text_color(theme.foreground).child(format!(
                    "Mem {}/{}",
                    format::bytes(s.mem.used_bytes, scale),
                    format::bytes(s.mem.total_bytes, scale)
                )))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format::percent(s.mem.used_percent, 0)),
                ),
        )
        .child(div().h(px(80.)).child(chart::percent_chart(
            "mem-used",
            &history.mem_used,
            theme.chart_2,
            width,
        )))
        .child(meter(
            format!("used {}", format::bytes(s.mem.used_bytes, scale)),
            s.mem.used_percent,
            cx,
        ))
        .child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "avail {}",
                    format::bytes(s.mem.available_bytes, scale)
                ))
                .child(format!(
                    "cached {}",
                    format::bytes(s.mem.cached_bytes, scale)
                )),
        )
        .child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "swap {}/{}",
                    format::bytes(s.mem.swap_used_bytes, scale),
                    format::bytes(s.mem.swap_total_bytes, scale)
                )),
        )
}

pub fn net_panel(
    snapshot: Option<&Snapshot>,
    history: &History,
    interface: Option<&str>,
    width: usize,
    scale: SizeScale,
    cx: &App,
) -> impl IntoElement {
    let Some(s) = snapshot else {
        return no_data(cx);
    };
    let theme = cx.theme();
    let chosen = s
        .nets
        .iter()
        .find(|n| Some(n.name.as_str()) == interface)
        .or_else(|| s.nets.first());

    let Some(net) = chosen else {
        return panel_body().child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("no interfaces"),
        );
    };

    let mut body = panel_body()
        .child(
            h_flex()
                .justify_between()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(net.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(if net.connected {
                            theme.success
                        } else {
                            theme.muted_foreground
                        })
                        .child(if net.connected { "up" } else { "down" }),
                ),
        )
        .child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "▼ {}",
                    format::rate(net.download_bytes_per_sec, scale)
                ))
                .child(format!(
                    "▲ {}",
                    format::rate(net.upload_bytes_per_sec, scale)
                ))
                .child(format!(
                    "total {}/{}",
                    format::bytes(net.total_download_bytes, scale),
                    format::bytes(net.total_upload_bytes, scale)
                )),
        );

    if let (Some(d), Some(u)) = (
        history.net_down.get(&net.name),
        history.net_up.get(&net.name),
    ) {
        body = body.child(div().h(px(80.)).child(chart::dual_chart(
            "net-rw",
            d,
            u,
            theme.chart_1,
            theme.chart_2,
            width,
        )));
    }
    body
}

pub fn disk_panel(
    snapshot: Option<&Snapshot>,
    history: &History,
    scale: SizeScale,
    cx: &App,
) -> impl IntoElement {
    let Some(s) = snapshot else {
        return no_data(cx);
    };
    let theme = cx.theme();

    if s.disks.is_empty() {
        return panel_body().child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("no disks"),
        );
    }

    let mut body = panel_body();
    for disk in &s.disks {
        body = body.child(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .justify_between()
                        .text_xs()
                        .child(
                            div()
                                .text_color(theme.foreground)
                                .child(format!("{} {}", disk.name, disk.mount_point)),
                        )
                        .child(div().text_color(theme.muted_foreground).child(format!(
                            "{}/{}",
                            format::bytes(disk.total_bytes.saturating_sub(disk.free_bytes), scale),
                            format::bytes(disk.total_bytes, scale)
                        ))),
                )
                .child(meter(
                    format!(
                        "r {}  w {}",
                        format::rate(disk.read_bytes_per_sec, scale),
                        format::rate(disk.write_bytes_per_sec, scale)
                    ),
                    disk.used_percent,
                    cx,
                )),
        );
    }

    // Throughput for the first disk that has both rings.
    if let Some(disk) = s.disks.first()
        && let (Some(r), Some(w)) = (
            history.disk_read.get(&disk.name),
            history.disk_write.get(&disk.name),
        )
    {
        body = body.child(div().h(px(70.)).child(chart::dual_chart(
            "disk-rw",
            r,
            w,
            theme.chart_3,
            theme.chart_4,
            60,
        )));
    }
    body
}

pub fn battery_panel(snapshot: Option<&Snapshot>, cx: &App) -> impl IntoElement {
    let Some(s) = snapshot else {
        return no_data(cx);
    };
    // A desktop has no battery, and the panel is not rendered at all.
    let Some(b) = &s.battery else {
        return panel_body().child(div());
    };
    let theme = cx.theme();
    panel_body()
        .child(
            h_flex()
                .justify_between()
                .text_sm()
                .child(div().text_color(theme.foreground).child(b.name.clone()))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format::percent(b.percent, 0)),
                ),
        )
        .child(meter(b.status.label().to_string(), b.percent, cx))
        .child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format::time_remaining(b.time_remaining_seconds))
                .child(format::watts(b.power_watts)),
        )
}

/// The process list. Sorting and filtering already happened in `pull()`; this
/// renders that order verbatim, which is why `render()` allocates nothing.
pub fn proc_panel(
    rows: &[ProcSnapshot],
    scale: SizeScale,
    selected: Option<i32>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    if rows.is_empty() {
        return panel_body().child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("no processes match"),
        );
    }
    let mut body = panel_body().gap_0();
    for p in rows {
        let is_selected = Some(p.pid) == selected;
        body = body.child(
            h_flex()
                .px_1()
                .gap_2()
                .items_center()
                .rounded_sm()
                .when(is_selected, |el| el.bg(theme.accent.opacity(0.18)))
                .child(
                    div()
                        .w(px(56.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(p.pid.to_string()),
                )
                .child(
                    div()
                        .w(px(96.))
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(format!("{}{}", p.tree_prefix, p.name)),
                )
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(p.user.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(format::bytes(p.mem_bytes, scale)),
                )
                .child(
                    div()
                        .w(px(56.))
                        .text_right()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(or_dash(Some(format::percent(p.cpu_percent, 1)))),
                ),
        );
    }
    body
}

/// The legend row for one CPU field, in the same order as `field_colors`.
pub fn field_legend(index: usize, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .items_center()
        .gap_1()
        .child(div().size_2().rounded_full().bg(field_colors(cx)[index]))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(CPU_FIELD_NAMES[index.min(CPU_FIELD_NAMES.len() - 1)].to_string()),
        )
}
