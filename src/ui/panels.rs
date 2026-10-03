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
use crate::config::Config;
use crate::format::{self, SizeScale, TempScale};
use crate::history::History;
use crate::model::{CPU_FIELD_NAMES, ProcSnapshot, Snapshot};
use crate::ui::chart;
use crate::ui::chrome::{meter, panel_body};
use crate::ui::theme;

/// The config-driven display options, read **once per tick** in `pull()` and
/// passed to each panel.
///
/// Every one of these used to be a key in `DESCRIPTIONS` that nothing read, so
/// the config file advertised switches with no effect. Gathering them into one
/// struct means a panel cannot accidentally read the config during `render()`,
/// and adding a display option is a change in exactly two places.
#[derive(Debug, Clone, Copy)]
pub struct PanelOpts {
    pub show_uptime: bool,
    pub show_swap: bool,
    pub show_cpu_watts: bool,
    pub show_io_stat: bool,
    pub mem_graphs: bool,
    pub net_auto: bool,
    /// Fixed network graph ceilings in **bytes/sec**, used when `net_auto` is
    /// off. The config stores Mibibits/s, so it converts on the way in.
    pub net_download_max: f32,
    pub net_upload_max: f32,
    pub proc_colors: bool,
    pub proc_mem_bytes: bool,
    pub cpu_meter_style: CpuMeterStyle,
    pub temp_scale: TempScale,
}

impl PanelOpts {
    /// Read every display option out of the config.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            show_uptime: cfg.bool("show_uptime"),
            show_swap: cfg.bool("show_swap"),
            show_cpu_watts: cfg.bool("show_cpu_watts"),
            show_io_stat: cfg.bool("show_io_stat"),
            mem_graphs: cfg.bool("mem_graphs"),
            net_auto: cfg.bool("net_auto"),
            net_download_max: mibits_to_bytes_per_sec(cfg.int("net_download")),
            net_upload_max: mibits_to_bytes_per_sec(cfg.int("net_upload")),
            proc_colors: cfg.bool("proc_colors"),
            proc_mem_bytes: cfg.bool("proc_mem_bytes"),
            cpu_meter_style: CpuMeterStyle::from_config(&cfg.str("cpu_meter_style")),
            temp_scale: TempScale::from_config(&cfg.str("temp_scale")),
        }
    }
}

impl Default for PanelOpts {
    /// Every display option on, which is what the config defaults describe.
    fn default() -> Self {
        Self::from_config(&Config::defaults())
    }
}

/// Mibibits/s, as btop stores `net_download` / `net_upload`, to the bytes/sec
/// the history rings actually hold.
///
/// A ceiling of zero would flatten the graph to nothing, so it is floored at 1.
fn mibits_to_bytes_per_sec(mibits: i64) -> f32 {
    (mibits.max(1) as f32) * 1024.0 * 1024.0 / 8.0
}

/// How per-core load is drawn: a bar, or a filled chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuMeterStyle {
    Bar,
    Chip,
}

impl CpuMeterStyle {
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "chip" => Self::Chip,
            _ => Self::Bar,
        }
    }
}

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
    tick_secs: f32,
    opts: &PanelOpts,
    cx: &App,
) -> Div {
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
        //
        // `flex_1` + `min_h` rather than a fixed height, so that when
        // `items_stretch` on the row hands this panel more room than its
        // contents asked for, the room goes into the graph instead of becoming a
        // band of blank pixels under the per-core grid.
        .child(div().flex_1().min_h(px(110.)).child(chart::percent_chart(
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
                // `show_uptime` was declared and never read; uptime was drawn
                // unconditionally.
                .when(opts.show_uptime, |el| {
                    el.child(format!("up {}", format::duration(s.cpu.uptime_seconds)))
                })
                .child(format!("cores {}", s.cpu.core_count))
                // Watts are collected by the CPU collector (and need
                // cap_perfmon), but were never displayed at all.
                .when(opts.show_cpu_watts, |el| {
                    el.child(format::watts(s.cpu.watts))
                }),
        )
        .child(per_core_grid(s, cx, opts))
}

/// One compact cell per logical CPU, laid out in a wrapping row — btop's
/// per-core breakdown.
///
/// Each cell carries the three things a core actually has to say: how busy it
/// is, how fast it is running, and how hot it is. The speed and temperature are
/// `Option` because both are absent in a container or a VM, and a missing sensor
/// has to read as a dash rather than as zero.
fn per_core_grid(snapshot: &Snapshot, cx: &App, opts: &PanelOpts) -> impl IntoElement {
    let theme = cx.theme();
    // Copied out as `Copy` values so the closure below captures no borrow of
    // `cx` — a captured borrow is what makes `.children(iter.map(..))` fail to
    // compile when the caller also holds `cx` mutably.
    let (muted, foreground) = (theme.muted_foreground, theme.foreground);
    let fill = theme::stroke(cx, 0);
    // Copied out so the closure captures plain `Copy` values rather than a
    // borrow of `opts`, which would not outlive the iterator.
    let (style, temp_scale) = (opts.cpu_meter_style, opts.temp_scale);

    h_flex()
        .flex_wrap()
        .gap_x_3()
        .gap_y_2()
        .children(
            snapshot
                .cpu
                .cores
                .iter()
                .enumerate()
                .map(move |(ix, core)| {
                    // Temperatures were formatted inline as `{temp:.0}°C`, so
                    // `temp_scale` had no effect even though `format` had a
                    // scale-aware function all along.
                    let temp = format::temperature(core.temp_c, temp_scale);
                    let detail = match (core.mhz, core.temp_c) {
                        (Some(mhz), Some(_)) => format!("{mhz} MHz · {temp}"),
                        (Some(mhz), None) => format!("{mhz} MHz"),
                        (None, Some(_)) => temp,
                        (None, None) => "—".to_string(),
                    };

                    // `Chip` draws the load as a filled block instead of a
                    // Progress bar, which is denser when there are 16+ cores.
                    let load = match style {
                        CpuMeterStyle::Chip => div()
                            .h(px(6.))
                            .w_full()
                            .rounded_full()
                            .bg(fill.opacity(0.25))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui_kit::relative(core.percent.clamp(0.0, 100.0) / 100.0))
                                    .rounded_full()
                                    .bg(fill),
                            )
                            .into_any_element(),
                        CpuMeterStyle::Bar => {
                            // The id must be unique per core: gpui-kit derives
                            // the a11y node id from it and two bars sharing one
                            // panic at window-build time.
                            gpui_kit::component::progress::Progress::new(format!("core-{ix}"))
                                .value(core.percent.clamp(0.0, 100.0))
                                .color(fill)
                                .into_any_element()
                        }
                    };

                    v_flex()
                        .flex_none()
                        .w(px(120.))
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .text_xs()
                                .child(div().text_color(muted).child(format!("c{ix}")))
                                .child(
                                    div()
                                        .text_color(foreground)
                                        .child(format!("{:.0}%", core.percent)),
                                ),
                        )
                        .child(load)
                        .child(div().text_xs().text_color(muted).child(detail))
                }),
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
    opts: &PanelOpts,
    cx: &App,
) -> Div {
    let Some(s) = snapshot else {
        return no_data(cx);
    };
    let theme = cx.theme();
    let mut body = panel_body()
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
        );

    // `mem_graphs = False` drops the chart but keeps the panel's other
    // readouts, so the box shrinks instead of disappearing.
    if opts.mem_graphs {
        body = body.child(div().flex_1().min_h(px(80.)).child(chart::percent_chart(
            "mem-used",
            &history.mem_used,
            theme::stroke(cx, 1),
            "used",
            tick_secs,
        )));
    }

    // Swap is frequently zero and frequently unwanted; it was previously drawn
    // unconditionally, so `show_swap = False` did nothing.
    if opts.show_swap {
        body = body.child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!(
                    "swap {}/{}",
                    format::bytes(s.mem.swap_used_bytes, scale),
                    format::bytes(s.mem.swap_total_bytes, scale)
                )),
        );
    }

    body
}

pub fn net_panel(
    snapshot: Option<&Snapshot>,
    history: &History,
    interface: Option<&str>,
    scale: SizeScale,
    tick_secs: f32,
    opts: &PanelOpts,
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
        body = body.child(div().flex_1().min_h(px(80.)).child(chart::dual_chart(
            "net-rw",
            d,
            u,
            theme::stroke(cx, 0),
            theme::stroke(cx, 2),
            chart::SeriesSpec {
                names: ("download", "upload"),
                unit: " B/s",
                tick_secs,
                auto_scale: opts.net_auto,
                // Both series share one axis, so the ceiling is whichever of
                // the two configured limits is higher.
                fixed_max: Some(opts.net_download_max.max(opts.net_upload_max)),
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
    opts: &PanelOpts,
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
        let mut row = v_flex().gap_1().child(
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
        );

        // The capacity bar is always drawn; the r/w rates and the throughput
        // chart below are what `show_io_stat = False` removes.
        if opts.show_io_stat {
            row = row.child(meter(
                format!(
                    "r {}  w {}",
                    format::rate(disk.read_bytes_per_sec, scale),
                    format::rate(disk.write_bytes_per_sec, scale)
                ),
                disk.used_percent,
                cx,
            ));
        } else {
            // `show_io_stat = False`: the bar still has to have a label,
            // because `meter` derives its element id from that label and two
            // bars sharing an id panic at window-build time.
            row = row.child(meter(
                format!("{} used", disk.mount_point),
                disk.used_percent,
                cx,
            ));
        }

        body = body.child(row);
    }

    // Throughput for the first disk that has both rings.
    if let Some(disk) = s.disks.first()
        && let (Some(r), Some(w)) = (
            history.disk_read.get(&disk.name),
            history.disk_write.get(&disk.name),
        )
    {
        body = body.child(div().flex_1().min_h(px(70.)).child(chart::dual_chart(
            "disk-rw",
            r,
            w,
            theme::stroke(cx, 1),
            theme::stroke(cx, 3),
            chart::SeriesSpec {
                names: ("read", "write"),
                unit: " B/s",
                tick_secs,
                // The disk graph has no auto-scale switch of its own.
                auto_scale: true,
                fixed_max: None,
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
    // Centred: the box is stretched to the process list's height because the row
    // is `items_stretch`, and a battery is three lines at most. Centring makes
    // it read as a tile that owns its box rather than a box with a hole in it.
    panel_body()
        .justify_center()
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

/// The process panel's controls: which column, which direction, and the filter.
///
/// Grouped rather than passed one by one — the panel already takes rows, a scale,
/// a selection, a scroll handle and a context, and clippy's seven-argument limit
/// is a fair line.
pub struct ProcControls<'a> {
    pub sort: ProcSort,
    /// `false` is descending, which is verified behaviour rather than a guess.
    pub reversed: bool,
    pub filter: &'a str,
    /// True while the filter box has the keyboard.
    pub editing: bool,
}

/// The process list: a filter line, a clickable sort bar, then a clipped,
/// scrollable list of interactive rows.
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
    controls: &ProcControls<'_>,
    scroll: &ScrollHandle,
    opts: &PanelOpts,
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
            rows_view = rows_view.child(proc_row(p, scale, selected, opts, cx));
        }
        rows_view.into_any_element()
    };

    panel_body()
        .child(filter_line(controls, cx))
        .child(proc_sort_bar(controls, cx))
        .child(
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
/// The filter box, shown only while a filter is set or being typed.
///
/// Hidden when empty so it costs no vertical space in the common case, and
/// visible while editing so the caret has somewhere to live — a filter you
/// cannot see is a filter you cannot trust.
fn filter_line(controls: &ProcControls<'_>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let (muted, foreground, border) = (theme.muted_foreground, theme.foreground, theme.border);
    let shown = controls.editing || !controls.filter.is_empty();
    let caret = if controls.editing { "▌" } else { "" };

    h_flex()
        .w_full()
        .when(shown, |el| el.flex_none())
        .when(!shown, |el| el.hidden())
        .items_center()
        .gap_2()
        .pb_1()
        .text_xs()
        .child(div().flex_none().text_color(muted).child("filter:"))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(foreground)
                // The caret is drawn after the text because this is a keyboard
                // mode rather than a real text field, so there is nowhere else
                // to show that the keyboard has been captured.
                .child(if controls.filter.is_empty() {
                    "type to filter · Enter keeps · Esc undoes".to_string()
                } else {
                    format!("{}{caret}", controls.filter)
                }),
        )
        .when(!controls.filter.is_empty() && !controls.editing, |el| {
            el.child(div().flex_none().text_color(border).child("Del to clear"))
        })
}

fn proc_sort_bar(controls: &ProcControls<'_>, cx: &mut Context<AppView>) -> impl IntoElement {
    let theme = cx.theme();
    let (sort, reversed) = (controls.sort, controls.reversed);
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
    // Read before the listener below borrows `cx` mutably.
    let radius = theme.radius;

    div()
        .id(ElementId::Name(SharedString::from(format!("sort-{label}"))))
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded(radius)
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
    opts: &PanelOpts,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let is_selected = Some(p.pid) == selected;
    let pid = p.pid;
    // Copied out of the theme up front: `cx` is borrowed mutably below for the
    // click listeners, so the theme borrow has to be over by then.
    let (muted, foreground, accent) = (theme.muted_foreground, theme.foreground, theme.accent);
    let (hover_bg, selected_bg) = (accent.opacity(0.10), accent.opacity(0.18));

    // `proc_colors = False` drops the CPU and memory columns to plain text.
    // These are `Copy` so the cell below captures no borrow of `cx`.
    let hot = theme::stroke(cx, 0);
    let warm = theme::stroke(cx, 1);
    let colorize = opts.proc_colors;
    let load_color = if p.cpu_percent >= 80.0 {
        hot
    } else if p.cpu_percent >= 30.0 {
        warm
    } else {
        foreground
    };
    // Half the theme radius: a full-radius pill on a dense table row reads as a
    // button rather than as the row it is.
    let radius = theme.radius / 2.;

    h_flex()
        .id(ElementId::Name(SharedString::from(format!(
            "proc-row-{pid}"
        ))))
        .px_1()
        .gap_2()
        .items_center()
        .rounded(radius)
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
                .text_color(if colorize { load_color } else { muted })
                .child(format::bytes(p.mem_bytes, scale)),
        )
        .child(
            div()
                .flex_none()
                .w(px(56.))
                .text_right()
                .text_xs()
                .text_color(if colorize { load_color } else { muted })
                .child(format::percent(p.cpu_percent, 1)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn defaults_have_every_display_option_on() {
        // The panel branches on these, so a default of `false` would silently
        // hide a row rather than error.
        let opts = PanelOpts::from_config(&Config::defaults());
        assert!(opts.show_uptime);
        assert!(opts.show_swap);
        assert!(opts.show_cpu_watts);
        assert!(opts.show_io_stat);
        assert!(opts.mem_graphs);
        assert!(opts.net_auto);
        assert!(opts.proc_colors);
        assert_eq!(opts.cpu_meter_style, CpuMeterStyle::Bar);
        assert_eq!(opts.temp_scale, TempScale::Celsius);
    }

    #[test]
    fn each_switch_is_readable_from_the_config() {
        // Every one of these used to be declared and never read.
        let cfg = Config::parse(
            "show_uptime = False\n\
             show_swap = False\n\
             show_cpu_watts = False\n\
             show_io_stat = False\n\
             mem_graphs = False\n\
             net_auto = False\n\
             proc_colors = False\n",
        );
        let opts = PanelOpts::from_config(&cfg);
        assert!(!opts.show_uptime);
        assert!(!opts.show_swap);
        assert!(!opts.show_cpu_watts);
        assert!(!opts.show_io_stat);
        assert!(!opts.mem_graphs);
        assert!(!opts.net_auto);
        assert!(!opts.proc_colors);
    }

    #[test]
    fn the_meter_style_parses_both_spellings_and_falls_back_to_bar() {
        assert_eq!(CpuMeterStyle::from_config("chip"), CpuMeterStyle::Chip);
        assert_eq!(CpuMeterStyle::from_config("  CHIP "), CpuMeterStyle::Chip);
        assert_eq!(CpuMeterStyle::from_config("bar"), CpuMeterStyle::Bar);
        // An unrecognised value must not panic and must not hide the bars.
        assert_eq!(CpuMeterStyle::from_config("wibble"), CpuMeterStyle::Bar);
    }

    #[test]
    fn bitrate_ceilings_convert_and_never_collapse_to_zero() {
        // Mibibits/s in, bytes/s out.
        assert_eq!(mibits_to_bytes_per_sec(1), 1024.0 * 1024.0 / 8.0);
        assert_eq!(mibits_to_bytes_per_sec(100), 100.0 * 1024.0 * 1024.0 / 8.0);
        // A zero or negative ceiling would flatten the graph to a line, so it
        // is floored rather than honoured.
        assert!(mibits_to_bytes_per_sec(0) > 0.0);
        assert!(mibits_to_bytes_per_sec(-5) > 0.0);
    }
}
