//! Chart helpers.
//!
//! # Why the data is built in `pull()`, not `render()`
//!
//! `AreaChart::new` takes an **iterator**, and the plot element requests
//! `Size::full()` — a chart always fills its parent. Both facts push the same
//! way: building a `Vec` in `render()` would allocate on every frame, which at
//! 2 s ticks and 60 fps is pure waste. So `AppView::pull` converts the rings
//! into owned series once per tick and `render()` only reads them.
//!
//! # There is no `Point` and no `Series`
//!
//! `AreaChart<T, X, Y>` is generic over *your* datum with accessor closures.
//! A "series" is `.y(closure)` called once per series, paired in call order
//! with `.stroke(..)` / `.fill(..)`. Those three are parallel vectors indexed
//! by series number — verified against the official chart docs, not guessed.

use gpui_kit::component::chart::{AreaChart, LineChart};
use gpui_kit::prelude::*;
use gpui_kit::{IntoElement, SharedString, div};

use crate::history::Ring;

/// Rendered when there is not yet enough data to draw an honest graph. btop
/// shows a dash rather than a fabricated zero, and so do we.
pub const NOT_ENOUGH_DATA: &str = "—";

/// The minimum samples before a graph means anything: one sample has no
/// extent, and two cannot show a trend.
pub const MIN_SAMPLES: usize = 2;

/// One plotted sample: the value plus its age, for the x axis.
///
/// `AreaChart` requires `X: Into<SharedString>`, and **only string types**
/// implement that (`&str`, `String`, `Arc<str>`) — a numeric x does not
/// compile. So the age is carried as a label.
#[derive(Clone)]
struct Sample {
    label: String,
    value: f32,
}

/// One plotted sample per CPU field, so a multi-series chart can be built
/// from a single data set.
#[derive(Clone)]
struct FieldRow {
    label: String,
    /// Indexed by the **original** `CPU_FIELD_NAMES` position, so the colour a
    /// field gets does not change when another field is left out of the plot.
    values: Vec<f32>,
}

/// How many ticks back the x axis labels show, at most.
///
/// Without this the chart prints one label under *every* sample it is given —
/// up to 240 of them — which is what smeared unreadable numbers across the
/// bottom of each panel.
const X_TICKS: usize = 4;

fn samples_from(ring: &Ring<f32>, tick_secs: f32) -> Vec<Sample> {
    let len = ring.len();
    ring.iter()
        .enumerate()
        // `len - 1 - i` samples back is how old this one is, so the newest
        // reads "now". The previous code used the raw index, which put "0" at
        // both ends of the axis and told the reader nothing.
        .map(|(i, v)| Sample {
            label: age_label((len - 1 - i) as f32 * tick_secs),
            value: *v,
        })
        .collect()
}

/// The x-axis label for a sample `age_secs` old.
///
/// Seconds rather than a tick count: on a rolling graph, "how far back is this
/// point" is the only thing the x axis means, and the tick interval is a config
/// value the reader should not have to know to read the chart.
fn age_label(age_secs: f32) -> String {
    let secs = age_secs.round();
    if secs <= 0.0 {
        "now".to_string()
    } else if secs < 60.0 {
        format!("-{secs:.0}s")
    } else {
        format!("-{:.0}m", (secs / 60.0).round())
    }
}

/// A compact byte-rate label for an axis tick: `1.2M`, `181k`, `0`.
///
/// The raw value is bytes per second, and an axis tick reading `180878.5` is
/// unreadable — the magnitude is the point of the number, not its precision.
fn compact_rate(value: f64) -> String {
    const K: f64 = 1_000.0;
    const M: f64 = 1_000_000.0;
    if value >= M {
        format!("{:.1}M", value / M)
    } else if value >= K {
        format!("{:.0}k", value / K)
    } else {
        format!("{value:.0}")
    }
}

/// A percentage label for an axis tick.
fn percent_tick(value: f64) -> String {
    format!("{value:.0}%")
}

/// The element shown when there is nothing honest to draw yet: an empty box
/// that still occupies its parent, so the panel does not jump when data lands.
fn empty() -> gpui_kit::AnyElement {
    div().h_full().w_full().into_any_element()
}

/// A filled area graph of one series on a 0-100 axis.
///
/// `point_count` is deliberately **not** set. Setting it to the ring capacity
/// (as this did) lays the x axis out for every slot the ring *could* hold, so
/// a graph that had collected three samples drew a one-pixel sliver against
/// the left edge for the first several minutes and looked broken. Omitting it
/// falls back to the sample count, so the trace always spans the panel. The
/// axis labels are relative ages, so nothing is lost by rescaling.
pub fn percent_chart(
    id: &'static str,
    ring: &Ring<f32>,
    color: gpui_kit::Hsla,
    name: &'static str,
    tick_secs: f32,
) -> gpui_kit::AnyElement {
    let samples = samples_from(ring, tick_secs);
    if samples.len() < MIN_SAMPLES {
        return empty();
    }
    AreaChart::new(samples)
        .id(id)
        .x(|s: &Sample| s.label.clone())
        .y(|s: &Sample| s.value)
        .stroke(color)
        // The docs use `.fill(color.opacity(0.4))` for an area chart; a solid
        // fill hides the grid lines underneath it.
        .fill(color.opacity(0.4))
        // The label for this series' row in the tooltip.
        .name(name)
        .grid(true)
        .grid_dashed(false)
        .y_axis(true)
        .y_tick_count(3)
        .x_tick_count(X_TICKS)
        .y_domain(0.0f32, 100.0f32)
        // `y_padding` keeps 10px of headroom above the highest value by
        // default, which is why a 0-100 domain labelled its top tick 112.2.
        .y_padding(0.0, 0.0)
        .y_tick_format(percent_tick)
        .tooltip_title(|s: &Sample| SharedString::from(s.label.clone()))
        .tooltip_value(|_s: &Sample, _ix: usize, value: f64| {
            SharedString::from(format!("{value:.1}%"))
        })
        .into_any_element()
}

/// How a two-series chart should be labelled.
///
/// Grouped rather than passed as three more parameters: `dual_chart` already
/// takes six, and clippy's limit of seven is a fair line.
#[derive(Clone, Copy)]
pub struct SeriesSpec {
    /// Names for the two series, in the order their `.y()` accessors are added.
    pub names: (&'static str, &'static str),
    /// Suffix for the tooltip value, e.g. `" B/s"`.
    pub unit: &'static str,
    /// Seconds one sample covers, so the x axis can read `-8s` instead of `-4`.
    pub tick_secs: f32,
    /// When `false`, `net_auto = False` is in force and the y domain comes from
    /// `fixed_max` instead of following the data.
    pub auto_scale: bool,
    /// The ceiling to use when `auto_scale` is `false`, in the same unit as the
    /// data. `None` falls back to auto-scaling rather than drawing a flat line.
    pub fixed_max: Option<f32>,
}

/// Two series on one axis — network down and up, disk read and write.
pub fn dual_chart(
    id: &'static str,
    a: &Ring<f32>,
    b: &Ring<f32>,
    color_a: gpui_kit::Hsla,
    color_b: gpui_kit::Hsla,
    spec: SeriesSpec,
) -> impl IntoElement {
    let first = samples_from(a, spec.tick_secs);
    let second = samples_from(b, spec.tick_secs);
    if first.len() < MIN_SAMPLES || second.len() < MIN_SAMPLES {
        return empty();
    }
    let unit = spec.unit;
    // Both series share one domain, so the limit comes from the larger of the
    // two rings rather than from either alone.
    let peak = first
        .iter()
        .chain(second.iter())
        .map(|s| s.value)
        .fold(0.0f32, f32::max);

    // `net_auto = False` pins the domain, so a quiet link reads as a flat line
    // near the bottom instead of being magnified to fill the panel. A fixed max
    // of zero would draw nothing, so that falls back to auto.
    let domain_max = match (spec.auto_scale, spec.fixed_max) {
        (false, Some(max)) if max > 0.0 => max,
        _ => peak.max(1.0),
    };
    AreaChart::new(first)
        .id(id)
        .x(|s: &Sample| s.label.clone())
        .y(|s: &Sample| s.value)
        .stroke(color_a)
        .fill(color_a.opacity(0.4))
        .name(spec.names.0)
        .y(|s: &Sample| s.value)
        .stroke(color_b)
        .fill(color_b.opacity(0.4))
        .name(spec.names.1)
        .grid(true)
        .grid_dashed(false)
        .y_axis(true)
        .y_tick_count(3)
        .x_tick_count(X_TICKS)
        .y_domain(0.0f32, domain_max)
        // The y scale here follows the data, so headroom would misreport the
        // peak; the top tick must be the actual maximum.
        .y_padding(0.0, 0.0)
        .y_tick_format(compact_rate)
        .tooltip_title(|s: &Sample| SharedString::from(s.label.clone()))
        .tooltip_value(move |_s: &Sample, _ix: usize, value: f64| {
            SharedString::from(format!("{value:.1}{unit}"))
        })
        .into_any_element()
}

/// The `CPU_FIELD_NAMES` entries that are worth plotting.
///
/// `idle` is excluded, and that single omission is the difference between a
/// readable chart and a grey wedge: idle is normally 90%+, so on a shared
/// 0-100 axis it flattens every other field onto the floor. The remaining
/// fields partition *busy* time, so they genuinely belong on one scale.
pub fn plotted_field_indices() -> Vec<usize> {
    crate::model::CPU_FIELD_NAMES
        .iter()
        .enumerate()
        .filter(|(_, name)| !name.eq_ignore_ascii_case("idle"))
        .map(|(i, _)| i)
        .collect()
}

/// The CPU field breakdown: one area per *busy* field on a shared 0-100 axis.
///
/// gpui-kit has no stacked-area chart, so these overlap rather than stack. The
/// fill is therefore kept faint and the stroke solid: with eight (now seven)
/// 40%-opacity fills on top of each other nothing was legible.
pub fn cpu_fields_chart(
    history: &crate::history::History,
    colors: &[gpui_kit::Hsla],
    tick_secs: f32,
) -> gpui_kit::AnyElement {
    // Every field has its own ring, and `AreaChart` runs each series' accessor
    // over the SAME `data` passed to `new`. So a multi-series chart must be
    // built from one datum type carrying every field: `FieldRow`, with one
    // `.y()` per index into `values`. Passing field 0's data and adding a
    // second `.y()` that reads `s.value` would just redraw field 0.
    let mut rings = history
        .cpu_fields
        .iter()
        .map(|r| samples_from(r, tick_secs))
        .filter(|f: &Vec<Sample>| f.len() >= MIN_SAMPLES)
        .collect::<Vec<Vec<Sample>>>();
    if rings.is_empty() {
        return empty();
    }
    // Row-wise: sample i of every field becomes one datum, keeping the ring's
    // own field order so `values[field_ix]` is always that field.
    let rows: Vec<FieldRow> = (0..rings[0].len())
        .map(|i| FieldRow {
            label: rings[0]
                .get(i)
                .map_or_else(String::new, |s| s.label.clone()),
            values: rings
                .iter()
                .map(|f| f.get(i).map_or(0.0, |s| s.value))
                .collect(),
        })
        .collect();
    rings.clear();

    let plotted = plotted_field_indices();
    let first = match plotted.first().copied() {
        Some(ix) => ix,
        None => return empty(),
    };
    let c0 = colors.get(first).copied().unwrap_or_default();

    let mut chart = AreaChart::new(rows)
        .id("cpu-fields")
        .x(|r: &FieldRow| r.label.clone())
        .y(move |r: &FieldRow| r.values.get(first).copied().unwrap_or(0.0))
        .stroke(c0)
        .fill(c0.opacity(0.10))
        // Names each series' row in the tooltip, so the reader is told *which*
        // field the number belongs to instead of having to match colours.
        .name(cpu_field_name(first))
        .grid(true)
        .grid_dashed(false)
        .y_axis(true)
        .y_tick_count(3)
        .x_tick_count(X_TICKS)
        .y_domain(0.0f32, 100.0f32)
        .y_padding(0.0, 0.0)
        .y_tick_format(percent_tick)
        .tooltip_title(|r: &FieldRow| SharedString::from(r.label.clone()))
        .tooltip_value(|_r: &FieldRow, _ix: usize, value: f64| {
            SharedString::from(format!("{value:.1}%"))
        });
    // One `.y()` per remaining field; each is the index of that field in
    // `values`, so the series and the colours stay aligned.
    for &ix in plotted.iter().skip(1) {
        let c = colors.get(ix).copied().unwrap_or(c0);
        chart = chart
            .y(move |r: &FieldRow| r.values.get(ix).copied().unwrap_or(0.0))
            .stroke(c)
            .fill(c.opacity(0.10))
            .name(cpu_field_name(ix));
    }
    chart.into_any_element()
}

/// The display name of a `CPU_FIELD_NAMES` entry.
///
/// Falls back to `"?"` rather than indexing: a chart label is not worth a panic
/// on the UI thread, which would take the window down.
fn cpu_field_name(ix: usize) -> &'static str {
    crate::model::CPU_FIELD_NAMES
        .get(ix)
        .copied()
        .unwrap_or("?")
}

/// A plain line for one trace.
pub fn line_chart(
    id: &'static str,
    ring: &Ring<f32>,
    color: gpui_kit::Hsla,
    name: &'static str,
    tick_secs: f32,
) -> gpui_kit::AnyElement {
    let samples = samples_from(ring, tick_secs);
    if samples.len() < MIN_SAMPLES {
        return empty();
    }
    LineChart::new(samples)
        .id(id)
        .x(|s: &Sample| s.label.clone())
        .y(|s: &Sample| s.value)
        .stroke(color)
        .name(name)
        .grid(true)
        .grid_dashed(false)
        .y_axis(true)
        .y_tick_count(3)
        .x_tick_count(X_TICKS)
        .y_padding(0.0, 0.0)
        .tooltip_title(|s: &Sample| SharedString::from(s.label.clone()))
        .tooltip_value(|_s: &Sample, value: f64| SharedString::from(format!("{value:.1}")))
        .into_any_element()
}

/// The label for a chart's current value, or the dash when there is nothing
/// honest to show yet.
pub fn current_label(ring: &Ring<f32>, format: impl Fn(f32) -> String) -> String {
    if ring.len() < MIN_SAMPLES {
        return NOT_ENOUGH_DATA.to_string();
    }
    ring.last()
        .map(|v| format(*v))
        .unwrap_or_else(|| NOT_ENOUGH_DATA.to_string())
}

/// A legend swatch, used by the CPU panel.
pub fn swatch(color: gpui_kit::Hsla) -> impl IntoElement {
    div().size_2().rounded_full().bg(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_with_fewer_than_two_samples_shows_a_dash() {
        let mut r: Ring<f32> = Ring::new(8);
        assert_eq!(current_label(&r, |v| format!("{v}")), NOT_ENOUGH_DATA);
        r.push(50.0);
        // One sample still shows a dash: a single point is not a trend.
        assert_eq!(current_label(&r, |v| format!("{v}")), NOT_ENOUGH_DATA);
        r.push(60.0);
        assert_eq!(current_label(&r, |v| format!("{v}")), "60");
    }
}
