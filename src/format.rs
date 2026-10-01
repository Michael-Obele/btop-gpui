//! Human-readable formatting. **Only ever called from `render()`** — the model
//! stores raw numbers, never strings.
//!
//! Nothing here panics: every input that could be nonsensical (NaN, infinity,
//! negative, absurdly large) is clamped before it reaches `format!`.

/// Which unit ladder to use. Mirrors btop's `base_10_sizes` / `base_10_bitrate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeScale {
    /// 1024-based: KiB, MiB, GiB (what btop calls non-`base_10_sizes`).
    #[default]
    Binary,
    /// 1000-based: KB, MB, GB.
    Decimal,
}

impl SizeScale {
    fn base(&self) -> f64 {
        match self {
            Self::Binary => 1024.0,
            Self::Decimal => 1000.0,
        }
    }

    fn units(&self) -> [&'static str; 6] {
        match self {
            Self::Binary => ["B", "KiB", "MiB", "GiB", "TiB", "PiB"],
            Self::Decimal => ["B", "KB", "MB", "GB", "TB", "PB"],
        }
    }
}

/// Temperature scale. Conversion happens here, never in a collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TempScale {
    #[default]
    Celsius,
    Fahrenheit,
    Kelvin,
}

impl TempScale {
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "fahrenheit" => Self::Fahrenheit,
            "kelvin" => Self::Kelvin,
            _ => Self::Celsius,
        }
    }

    fn convert(self, celsius: f32) -> f32 {
        match self {
            Self::Celsius => celsius,
            Self::Fahrenheit => celsius * 9.0 / 5.0 + 32.0,
            Self::Kelvin => celsius + 273.15,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
            Self::Kelvin => "K",
        }
    }
}

/// The em dash btop uses for "we do not know". Never show a fake zero.
pub const UNKNOWN: &str = "—";

fn sanitize(v: f64) -> f64 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

/// Split a byte count into (value, unit index). Exposed for tests.
fn ladder(value: f64, scale: SizeScale) -> (f64, usize) {
    let base = scale.base();
    let mut v = sanitize(value);
    let mut idx = 0usize;
    while v >= base && idx < scale.units().len() - 1 {
        v /= base;
        idx += 1;
    }
    (v, idx)
}

/// `1536` -> `"1.5 KiB"`. btop's default is binary; `base_10_sizes` switches it.
pub fn bytes(value: u64, scale: SizeScale) -> String {
    let (v, idx) = ladder(value as f64, scale);
    format_scaled(v, scale.units()[idx])
}

/// Bytes per second -> `"1.5 MiB/s"`.
pub fn rate(value: f64, scale: SizeScale) -> String {
    let (v, idx) = ladder(value, scale);
    format_scaled(v, &format!("{}/s", scale.units()[idx]))
}

fn format_scaled(v: f64, unit: &str) -> String {
    if v >= 100.0 {
        format!("{v:.0} {unit}")
    } else if v >= 10.0 {
        format!("{v:.1} {unit}")
    } else if v >= 1.0 {
        format!("{v:.2} {unit}")
    } else {
        format!("{v:.3} {unit}")
    }
}

/// MHz -> `"3.42 GHz"` / `"840 MHz"` / `"1.20 THZ"`.
pub fn frequency(mhz: u32) -> String {
    let m = mhz as f64;
    if m > 999_999.0 {
        format!("{:.2} THz", m / 1_000_000.0)
    } else if m > 999.0 {
        format!("{:.2} GHz", m / 1000.0)
    } else {
        format!("{m} MHz")
    }
}

/// Seconds -> `"3d 4h 20m"`, `"4h 20m"`, `"20m 11s"`, `"11s"`.
pub fn duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3600;
    let mins = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// A bare seconds count, e.g. for a battery timer: `"2h 15m"` or `—`.
pub fn time_remaining(seconds: Option<u64>) -> String {
    match seconds {
        Some(s) => duration(s),
        None => UNKNOWN.to_string(),
    }
}

pub fn percent(value: f32, decimals: usize) -> String {
    let v = if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    };
    format!("{v:.decimals$}%")
}

/// Temperature with its scale applied. `None` renders as `—`.
pub fn temperature(celsius: Option<f32>, scale: TempScale) -> String {
    match celsius {
        Some(c) if c.is_finite() => format!("{:.1}{}", scale.convert(c), scale.unit()),
        _ => UNKNOWN.to_string(),
    }
}

/// Watts with one decimal, e.g. `"12.4W"`.
pub fn watts(value: Option<f32>) -> String {
    match value {
        Some(w) if w.is_finite() && w >= 0.0 => format!("{w:.1}W"),
        _ => UNKNOWN.to_string(),
    }
}

/// Bytes with a fixed number of decimals, for the disk table's capacity column.
pub fn bytes_precise(value: u64, scale: SizeScale) -> String {
    let (v, idx) = ladder(value as f64, scale);
    format!("{v:.2} {}", scale.units()[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_bytes() {
        // Precision is adaptive, like btop: the smaller the number, the more
        // decimals, so a fraction of a byte stays readable.
        assert_eq!(bytes(0, SizeScale::Binary), "0.000 B");
        assert_eq!(bytes(1024, SizeScale::Binary), "1.00 KiB");
        assert_eq!(bytes(1536, SizeScale::Binary), "1.50 KiB");
        assert_eq!(bytes(1024 * 1024, SizeScale::Binary), "1.00 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024, SizeScale::Binary), "3.00 GiB");
        // >= 10 drops to one decimal, >= 100 drops to none.
        assert_eq!(bytes(15 * 1024, SizeScale::Binary), "15.0 KiB");
        assert_eq!(bytes(900 * 1024, SizeScale::Binary), "900 KiB");
    }

    #[test]
    fn decimal_bytes() {
        assert_eq!(bytes(1000, SizeScale::Decimal), "1.00 KB");
        assert_eq!(bytes(1_000_000, SizeScale::Decimal), "1.00 MB");
    }

    #[test]
    fn rate_appends_per_second() {
        assert_eq!(rate(1024.0, SizeScale::Binary), "1.00 KiB/s");
        assert_eq!(rate(0.0, SizeScale::Binary), "0.000 B/s");
    }

    #[test]
    fn rate_survives_nonsense() {
        assert_eq!(rate(f64::NAN, SizeScale::Binary), "0.000 B/s");
        assert_eq!(rate(f64::INFINITY, SizeScale::Binary), "0.000 B/s");
        assert_eq!(rate(-5.0, SizeScale::Binary), "0.000 B/s");
    }

    #[test]
    fn frequency_scales() {
        assert_eq!(frequency(842), "842 MHz");
        assert_eq!(frequency(3400), "3.40 GHz");
        assert_eq!(frequency(1_500_000), "1.50 THz");
    }

    #[test]
    fn duration_formats() {
        assert_eq!(duration(9), "9s");
        assert_eq!(duration(65), "1m 5s");
        assert_eq!(duration(3_725), "1h 2m");
        // 273000s = 3d 3h 50m exactly; 3*86400 + 3*3600 + 50*60 = 273000.
        assert_eq!(duration(273_000), "3d 3h 50m");
    }

    #[test]
    fn temperature_scales() {
        assert_eq!(temperature(Some(50.0), TempScale::Celsius), "50.0°C");
        assert_eq!(temperature(Some(50.0), TempScale::Fahrenheit), "122.0°F");
        // 0 °C is 273.15 K, which renders at one decimal as 273.1K.
        assert_eq!(temperature(Some(0.0), TempScale::Kelvin), "273.1K");
        assert_eq!(temperature(None, TempScale::Celsius), UNKNOWN);
    }

    #[test]
    fn percent_clamps() {
        assert_eq!(percent(12.345, 1), "12.3%");
        assert_eq!(percent(150.0, 0), "100%");
        assert_eq!(percent(-1.0, 0), "0%");
        assert_eq!(percent(f32::NAN, 0), "0%");
    }
}
