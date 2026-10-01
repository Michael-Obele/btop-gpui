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
use gpui_kit::{IntoElement, div};

use crate::history::Ring;

/// Rendered when there is not yet enough data to draw an honest graph. btop
/// shows a dash rather than a fabricated zero, and so do we.
pub const NOT_ENOUGH_DATA: &str = "—";

/// The minimum samples before a graph means anything: one sample has no
/// extent, and two cannot show a trend.
pub const MIN_SAMPLES: usize = 2;

/// One plotted sample: the value plus a label for the x axis.
///
/// `AreaChart` requires `X: Into<SharedString>`, and **only string types**
/// implement that (`&str`, `String`, `Arc<str>`) — a numeric x does not
/// compile. So the index is carried as a label rather than as a number.
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
    values: Vec<f32>,
}

fn samples_from(ring: &Ring<f32>) -> Vec<Sample> {
    ring.iter()
        .enumerate()
        .map(|(i, v)| Sample {
            label: i.to_string(),
            value: *v,
        })
        .collect()
}

/// A filled area graph of one series on a 0-100 axis.
pub fn percent_chart(
    id: &'static str,
    ring: &Ring<f32>,
    color: gpui_kit::Hsla,
    width: usize,
) -> gpui_kit::AnyElement {
    let samples = samples_from(ring);
    if samples.len() < MIN_SAMPLES {
        return div().h_full().w_full().into_any_element();
    }
    // X is the sample index: `AreaChart` requires `X: Into<SharedString>`, so a
    // float x is not even representable.
    AreaChart::new(samples)
        .id(id)
        .x(|s: &Sample| s.label.clone())
        .y(|s: &Sample| s.value)
        .stroke(color)
        // The official docs use `.fill(color.opacity(0.4))` for an area chart;
        // a solid fill hides the grid lines underneath it.
        .fill(color.opacity(0.4))
        .grid(true)
        .y_domain(0.0f32, 100.0f32)
        // Lays the x axis out for `width` slots rather than for however many
        // samples are buffered, so the axis does not breathe as the ring fills.
        .point_count(width.max(MIN_SAMPLES))
        .interactive(false)
        .into_any_element()
}

/// Two series on one axis — network down and up, disk read and write.
pub fn dual_chart(
    id: &'static str,
    a: &Ring<f32>,
    b: &Ring<f32>,
    color_a: gpui_kit::Hsla,
    color_b: gpui_kit::Hsla,
    width: usize,
) -> impl IntoElement {
    let first = samples_from(a);
    let second = samples_from(b);
    if first.len() < MIN_SAMPLES || second.len() < MIN_SAMPLES {
        return div().h_full().w_full().into_any_element();
    }
    // Both series share one domain, so the limit comes from the larger of the
    // two rings rather than from either alone.
    let peak = first
        .iter()
        .chain(second.iter())
        .map(|s| s.value)
        .fold(0.0f32, f32::max);
    AreaChart::new(first)
        .id(id)
        .x(|s: &Sample| s.label.clone())
        .y(|s: &Sample| s.value)
        .stroke(color_a)
        .fill(color_a.opacity(0.4))
        .y(|s: &Sample| s.value)
        .stroke(color_b)
        .fill(color_b.opacity(0.4))
        .grid(true)
        .y_domain(0.0f32, peak.max(1.0))
        .point_count(width.max(MIN_SAMPLES))
        .interactive(false)
        .into_any_element()
}

/// The CPU field breakdown: one filled area per field on a shared 0-100 axis.
///
/// A multi-series chart is a single `AreaChart` with `.y()` chained once per
/// series — not one chart per field, which would give each its own axis.
pub fn cpu_fields_chart(
    history: &crate::history::History,
    colors: &[gpui_kit::Hsla],
    width: usize,
) -> gpui_kit::AnyElement {
    // Every field has its own data set, and `AreaChart` runs each series'
    // accessor over the SAME `data` passed to `new`. So a multi-series chart
    // must be built from a single datum type that carries every field:
    // `FieldRow { label, values: Vec<f32> }`, with one `.y()` per index into
    // `values`. Passing the first field's data and adding a second `.y()` that
    // reads `s.value` would just redraw field 0.
    let mut fields = history
        .cpu_fields
        .iter()
        .map(samples_from)
        .filter(|f: &Vec<Sample>| f.len() >= MIN_SAMPLES)
        .collect::<Vec<Vec<Sample>>>();
    if fields.is_empty() {
        return div().h_full().w_full().into_any_element();
    }
    // Row-wise: sample i of every field becomes one datum.
    let rows: Vec<FieldRow> = (0..fields[0].len())
        .map(|i| FieldRow {
            label: i.to_string(),
            values: fields
                .iter()
                .map(|f| f.get(i).map_or(0.0, |s| s.value))
                .collect(),
        })
        .collect();
    fields.clear();

    let c0 = colors.first().copied().unwrap_or_default();
    let mut chart = AreaChart::new(rows)
        .id("cpu-fields")
        .x(|r: &FieldRow| r.label.clone())
        .y(|r: &FieldRow| r.values.first().copied().unwrap_or(0.0))
        .stroke(c0)
        .fill(c0.opacity(0.4))
        .grid(true)
        .y_domain(0.0f32, 100.0f32)
        .point_count(width.max(MIN_SAMPLES))
        .interactive(false);
    // One `.y()` per field beyond the first; index i+1 selects that field.
    for (i, _) in (1..fields_len_hint(history)).enumerate() {
        let c = colors.get(i + 1).copied().unwrap_or(c0);
        let index = i + 1;
        chart = chart
            .y(move |r: &FieldRow| r.values.get(index).copied().unwrap_or(0.0))
            .stroke(c)
            .fill(c.opacity(0.4));
    }
    chart.into_any_element()
}

/// How many CPU field rings currently hold data.
fn fields_len_hint(history: &crate::history::History) -> usize {
    history.cpu_fields.len()
}

/// A plain line for one trace.
pub fn line_chart(
    id: &'static str,
    ring: &Ring<f32>,
    color: gpui_kit::Hsla,
    width: usize,
) -> gpui_kit::AnyElement {
    let samples = samples_from(ring);
    if samples.len() < MIN_SAMPLES {
        return div().h_full().w_full().into_any_element();
    }
    LineChart::new(samples)
        .id(id)
        .x(|s: &Sample| s.label.clone())
        .y(|s: &Sample| s.value)
        .stroke(color)
        .grid(true)
        .point_count(width.max(MIN_SAMPLES))
        .interactive(false)
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
