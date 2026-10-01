//! The panels. Each one is a pure function of an `Option<&Snapshot>` and the
//! `History` — no `/proc` reads, no sorting, no allocation of collections.
//!
//! Every panel with no data renders a dash rather than a zero, because "idle"
//! and "not read yet" look identical otherwise and only one of them is true.

use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Div, ElementId, IntoElement, MouseButton, ScrollHandle, SharedString, div, px,
};

use crate::app::AppView;
use crate::collect::proc::ProcSort;
use crate::format::{self, SizeScale};
use crate::history::History;
use crate::model::{CPU_FIELD_NAMES, ProcSnapshot, Snapshot};
use crate::ui::chart;
use crate::ui::chrome::{meter, panel_body};
use crate::ui::theme;

/// Renders `—` when there is no snapshot yet.
fn no_data(cx: &App) -> Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child("—")
}

pub fn cpu_panel(snapshot: Option<&Snapshot>, history: &History, tick_secs: f32, cx: &App) -> Div {
    let Some(s) = snapshot else {
        return no_data(cx);
    };
    let theme = cx.theme();
    let total = chart::current_label(&history.cpu_total, |v| format::percent(v, 0));

    panel_body()
        .child(
            // `gap_2` and a shrinking model name are what stop these two texts
            // colliding. `justify_between` alone cannot spread them in a box
            // too narrow for both, so they simply butted together and read as
            // "CPU 37%11th Gen Intel(R) Core(TM)".
            h_flex()
                .justify_between()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .text_lg()
                        .font_weight(gpui_kit::FontWeight(500.0))
                        .text_color(theme.foreground)
                        .child(format!("CPU {total}")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_right()
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
            theme::stroke(cx, 0),
            "cpu",
            tick_secs,
        )))
        .child(div().h(px(70.)).child(chart::cpu_fields_chart(
            history,
            &field_colors(cx),
            tick_secs,
        )))
        .child(field_legend_row(cx))
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

/// A compact legend for the field chart, naming exactly the fields the chart
/// draws.
///
/// It is derived from `chart::plotted_field_indices`, not hand-written, so a
/// change to which fields are plotted cannot leave the legend naming a series
/// that is not on the graph.
fn field_legend_row(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let colors = field_colors(cx);
    let entries: Vec<(gpui_kit::Hsla, &'static str)> = chart::plotted_field_indices()
        .into_iter()
        .map(|ix| {
            (
                colors.get(ix).copied().unwrap_or_default(),
                CPU_FIELD_NAMES.get(ix).copied().unwrap_or("?"),
            )
        })
        .collect();
    h_flex()
        .flex_wrap()
        .gap_2()
        .children(entries.into_iter().map(move |(color, name)| {
            h_flex()
                .items_center()
                .gap_1()
                .child(div().size_2().rounded_full().bg(color))
                .child(div().text_xs().text_color(muted).child(name))
        }))
}

/// The colour per CPU field, in `CPU_FIELD_NAMES` order, taken from the theme
/// so a theme change restyles the graphs with no code edit.
///
/// The theme's five chart slots are all shades of **one** blue, which is not
/// enough to tell seven overlapping series apart, so it is topped up with the
/// two market colours and `info` — the only other distinct hues the theme
/// offers.
fn field_colors(cx: &App) -> Vec<gpui_kit::Hsla> {
    let t = cx.theme();
    let mut colors = theme::series_colors(cx);
    colors.push(t.chart_bullish);
    colors.push(t.chart_bearish);
    colors.push(t.info);
    colors
}

pub fn mem_panel(
    snapshot: Option<&Snapshot>,
    history: &History,
    scale: SizeScale,
    tick_secs: f32,
    cx: &App,
) -> Div {
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
            theme::stroke(cx, 1),
            "used",
            tick_secs,
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
    scale: SizeScale,
    tick_secs: f32,
    cx: &App,
) -> Div {
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
            theme::stroke(cx, 0),
            theme::stroke(cx, 2),
            chart::SeriesSpec {
                names: ("download", "upload"),
                unit: " B/s",
                tick_secs,
            },
        )));
    }
    body
}

pub fn disk_panel(
    snapshot: Option<&Snapshot>,
    history: &History,
    scale: SizeScale,
    tick_secs: f32,
    cx: &App,
) -> Div {
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
            theme::stroke(cx, 1),
            theme::stroke(cx, 3),
            chart::SeriesSpec {
                names: ("read", "write"),
                unit: " B/s",
                tick_secs,
            },
        )));
    }
    body
}

pub fn battery_panel(snapshot: Option<&Snapshot>, cx: &App) -> Div {
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

/// The process list: a clickable sort bar, then a clipped, scrollable list of
/// interactive rows.
///
/// Sorting and filtering already happened in `pull()`; this renders that order
/// verbatim. This is the one panel that takes a `&mut Context<AppView>` rather
/// than `&App`, because its rows are the app's primary mouse target. Every
/// handler below is a single call into an `AppView` method that the keyboard
/// also uses, so the pointer path — which cannot be exercised on this machine —
/// holds no logic of its own.
pub fn proc_panel(
    rows: &[ProcSnapshot],
    scale: SizeScale,
    selected: Option<i32>,
    sort: ProcSort,
    reversed: bool,
    scroll: &ScrollHandle,
    cx: &mut Context<AppView>,
) -> Div {
    let muted = cx.theme().muted_foreground;

    let list = if rows.is_empty() {
        div()
            .text_sm()
            .text_color(muted)
            .child("no processes match")
            .into_any_element()
    } else {
        // A `for` loop rather than `.map()`: a closure would capture `cx` and
        // could not hand the mutable borrow back out once per row.
        let mut rows_view = v_flex().w_full().gap_0();
        for p in rows {
            rows_view = rows_view.child(proc_row(p, scale, selected, cx));
        }
        rows_view.into_any_element()
    };

    panel_body().child(proc_sort_bar(sort, reversed, cx)).child(
        div()
            .id("proc-scroll")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .child(list),
    )
}

/// The sort bar above the process list.
///
/// Deliberately a labelled row of buttons rather than a table header: a header
/// has to line up exactly with five columns of a scrolling list to look right,
/// and an unaligned header reads as a bug. This also makes the sort reachable at
/// a glance, which a header that only responds to clicks does not.
fn proc_sort_bar(sort: ProcSort, reversed: bool, cx: &mut Context<AppView>) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .w_full()
        .flex_none()
        .items_center()
        .gap_2()
        .pb_1()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(
                    // Says out loud which way the list is ordered, so `r` and the
                    // arrow on the active pill both have something to refer to.
                    // `reversed == false` is descending: the header read
                    // "ascending" over a list sorted 100% → 1.5% until this was
                    // checked against the running app.
                    format!(
                        "sort: {} {}",
                        sort.label(),
                        if reversed { "ascending" } else { "descending" }
                    ),
                ),
        )
        .child(sort_button("pid", ProcSort::Pid, sort, reversed, cx))
        .child(sort_button("name", ProcSort::Name, sort, reversed, cx))
        .child(sort_button("user", ProcSort::User, sort, reversed, cx))
        .child(sort_button("memory", ProcSort::Memory, sort, reversed, cx))
        .child(sort_button("cpu", ProcSort::CpuDirect, sort, reversed, cx))
        // btop's `cpu lazy` averages over process lifetime instead of using the
        // instantaneous delta, which is why `top` and `btop` disagree. Both are
        // offered because the two answer different questions.
        .child(sort_button(
            "cpu lazy",
            ProcSort::CpuLazy,
            sort,
            reversed,
            cx,
        ))
}

/// One sort pill. Clicking the active column flips the direction — btop's `r`.
fn sort_button(
    label: &'static str,
    wanted: ProcSort,
    sort: ProcSort,
    reversed: bool,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let active = wanted == sort;
    // The active pill is **filled**, not just outlined.
    //
    // `accent` is the theme's hover/selected *surface* token and
    // `accent_foreground` is its matching text colour — the crate documents them
    // as a pair. Using `accent` itself as the text colour, as this first did,
    // gives near-black text on the dark theme and near-white on the light one:
    // unreadable in one mode whichever way the theme is set.
    let (fg, border) = if active {
        (theme.accent_foreground, theme.accent)
    } else {
        (theme.muted_foreground, theme.border)
    };
    let fill = theme.accent;

    div()
        .id(ElementId::Name(SharedString::from(format!("sort-{label}"))))
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_1()
        .border_color(border)
        .when(active, move |el| el.bg(fill))
        .cursor_pointer()
        .text_xs()
        .text_color(fg)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(
                move |this, _event: &gpui_kit::MouseDownEvent, _window, cx| {
                    this.sort_by(wanted, cx);
                },
            ),
        )
        .child(if active {
            // The arrow only exists on the active pill, so the bar reads as a
            // sort control rather than a row of glyphs.
            format!("{label} {}", if reversed { "▾" } else { "▴" })
        } else {
            label.to_string()
        })
}

/// One row of the process list.
///
/// Interactive, four ways: hover lifts it, a click selects, a double click opens
/// the detail sheet, a right click opens the process menu.
///
/// The element id comes from the **pid**, never the row's position. The list is
/// re-sorted every 2 seconds, so an index-keyed row would hand its hover and
/// selection state to whichever process moved into that slot.
fn proc_row(
    p: &ProcSnapshot,
    scale: SizeScale,
    selected: Option<i32>,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let is_selected = Some(p.pid) == selected;
    let pid = p.pid;
    // Copied out of the theme up front: `cx` is borrowed mutably below for the
    // click listeners, so the theme borrow has to be over by then.
    let (muted, foreground, accent) = (theme.muted_foreground, theme.foreground, theme.accent);
    let (hover_bg, selected_bg) = (accent.opacity(0.10), accent.opacity(0.18));

    h_flex()
        .id(ElementId::Name(SharedString::from(format!(
            "proc-row-{pid}"
        ))))
        .px_1()
        .gap_2()
        .items_center()
        .rounded_sm()
        .cursor_pointer()
        .when(is_selected, |el| el.bg(selected_bg))
        .hover(move |s| s.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &gpui_kit::MouseDownEvent, _window, cx| {
                // `click_count` lets one handler cover both gestures, so there
                // is no separate double-click registration to keep in sync.
                if event.click_count >= 2 {
                    this.open_detail(pid, cx);
                } else {
                    this.select(Some(pid), cx);
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(
                move |this, _event: &gpui_kit::MouseDownEvent, _window, cx| {
                    this.open_process_menu(pid, cx);
                },
            ),
        )
        .child(
            div()
                .flex_none()
                .w(px(56.))
                .text_xs()
                .text_color(muted)
                .child(p.pid.to_string()),
        )
        .child(
            div()
                .w(px(160.))
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(foreground)
                .child(format!("{}{}", p.tree_prefix, p.name)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(muted)
                .child(p.user.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(foreground)
                .child(format::bytes(p.mem_bytes, scale)),
        )
        .child(
            div()
                .flex_none()
                .w(px(56.))
                .text_right()
                .text_xs()
                .text_color(foreground)
                .child(format::percent(p.cpu_percent, 1)),
        )
}
