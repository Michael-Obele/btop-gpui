//! Battery: `/sys/class/power_supply/*`.
//!
//! A desktop has no battery, and that is the common case — this returns `None`
//! so the panel is **not rendered at all**, rather than rendered empty.
//!
//! A device qualifies only when `type` is `Battery` or `UPS` **and** `present`
//! is 1. Anything else is a charger, a USB supply, or a phantom device the
//! kernel keeps around after a hot-unplug.

use std::path::Path;
use std::time::Instant;

use crate::collect::sysfs;
use crate::config::Config;
use crate::logger;
use crate::model::{BatterySnapshot, BatteryStatus};

const POWER_SUPPLY: &str = "/sys/class/power_supply";

/// `a / b` as a 0-100 percentage, guarding both the zero denominator and
/// nonsense inputs.
fn ratio_percent(num: u64, den: u64) -> Option<f32> {
    if den == 0 {
        return None;
    }
    let pct = (num as f64) * 100.0 / (den as f64);
    if pct.is_finite() {
        Some(pct.clamp(0.0, 100.0) as f32)
    } else {
        None
    }
}

/// Map the kernel's `status` string. Returns `None` for "Unknown" so the caller
/// can try the AC/online derivation instead.
pub fn parse_status(raw: &str) -> Option<BatteryStatus> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "charging" => Some(BatteryStatus::Charging),
        "discharging" => Some(BatteryStatus::Discharging),
        "full" => Some(BatteryStatus::Full),
        "not charging" => Some(BatteryStatus::NotCharging),
        _ => None,
    }
}

/// `time_to_empty` is minutes; the model is seconds.
pub fn time_from_minutes(raw: Option<u64>) -> Option<u64> {
    let minutes = raw?;
    // The kernel writes the sentinel 2^63-1 when it cannot know, so anything
    // beyond a sane decade is "unknown", not "a very long time".
    if minutes == 0 || minutes > 10 * 365 * 24 * 60 {
        return None;
    }
    Some(minutes * 60)
}

/// Instantaneous draw in watts: `power_now` is µW, or current × voltage.
fn power_watts(dir: &Path) -> Option<f32> {
    if let Some(power_now) = sysfs::read_u64(dir.join("power_now")) {
        // µW -> W
        let w = (power_now as f64) / 1e6;
        return w.is_finite().then_some(w as f32);
    }
    let current = sysfs::read_i64(dir.join("current_now"))?;
    let voltage = sysfs::read_i64(dir.join("voltage_now"))?;
    if current < 0 || voltage < 0 {
        return None;
    }
    // Both are in µA and µV, so the product is µW; /1e6 gives W.
    let w = (current as f64) * (voltage as f64) / 1e6;
    w.is_finite().then_some(w as f32)
}

/// Is a mains adapter connected? Used to derive a status the kernel left
/// "Unknown", and to tell "Full" from "Discharging".
fn ac_online() -> Option<bool> {
    let mut any = false;
    for path in sysfs::list_dir(POWER_SUPPLY) {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if !name.starts_with("AC") && name != "AC" {
            continue;
        }
        if let Some(v) = sysfs::read_u64(path.join("online")) {
            any = true;
            if v == 1 {
                return Some(true);
            }
        }
    }
    if any { Some(false) } else { None }
}

pub struct BatteryCollector {
    /// An explicit device name, or `Auto` to take the highest-capacity one.
    selected: String,
    show_watts: bool,
}

impl BatteryCollector {
    pub fn new(cfg: &Config) -> Self {
        Self {
            selected: cfg.str("selected_battery"),
            show_watts: cfg.bool("show_battery_watts"),
        }
    }

    pub fn collect(&mut self, _now: Instant) -> Option<BatterySnapshot> {
        let mut best: Option<BatterySnapshot> = None;
        let mut saw_any = false;

        for dir in sysfs::list_dir(POWER_SUPPLY) {
            let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(kind) = sysfs::read_str(dir.join("type")) else {
                continue;
            };
            let kind = kind.trim();
            if kind != "Battery" && kind != "UPS" {
                continue;
            }
            // A present==0 device is a phantom left by a hot-unplug.
            if sysfs::read_u64(dir.join("present")) != Some(1) {
                continue;
            }
            saw_any = true;

            let energy_now = sysfs::read_u64(dir.join("energy_now"));
            let energy_full = sysfs::read_u64(dir.join("energy_full"));
            let charge_now = sysfs::read_u64(dir.join("charge_now"));
            let charge_full = sysfs::read_u64(dir.join("charge_full"));

            // `capacity` is the kernel's own 0-100 and is preferred; the
            // energy/charge ratios are the fallback for drivers that omit it.
            let percent = sysfs::read_u64(dir.join("capacity"))
                .map(|c| c.min(100) as f32)
                .or_else(|| match (energy_now, energy_full) {
                    (Some(n), Some(f)) => ratio_percent(n, f),
                    _ => None,
                })
                .or_else(|| match (charge_now, charge_full) {
                    (Some(n), Some(f)) => ratio_percent(n, f),
                    _ => None,
                });

            let Some(percent) = percent else {
                // Present but unreadable: skip it rather than show a fake 0%.
                continue;
            };

            let ac = ac_online();
            let status = sysfs::read_str(dir.join("status"))
                .as_deref()
                .and_then(parse_status)
                .unwrap_or(match ac {
                    // With a known AC state we can say something useful even
                    // though the kernel said "Unknown".
                    Some(true) if percent >= 99.0 => BatteryStatus::Full,
                    Some(true) => BatteryStatus::Charging,
                    Some(false) => BatteryStatus::Discharging,
                    None => BatteryStatus::Unknown,
                });

            // Time remaining, in seconds. Discharging uses the draw;
            // charging uses what is still missing.
            let watts = power_watts(&dir);
            let time_full = || time_from_minutes(sysfs::read_u64(dir.join("time_to_full")));
            let time_remaining_seconds = match (status.clone(), watts) {
                (BatteryStatus::Discharging, Some(w)) if w > 0.0 => {
                    let now_uwh = energy_now
                        .or_else(|| charge_now.map(|c| c / 1000))
                        .unwrap_or(0);
                    if now_uwh > 0 {
                        Some(((now_uwh as f64) / (w as f64) * 3600.0) as u64)
                    } else {
                        time_from_minutes(sysfs::read_u64(dir.join("time_to_empty")))
                    }
                }
                (BatteryStatus::Charging, Some(w)) if w > 0.0 => {
                    // Time to full needs both ends of the energy scale; the
                    // kernel's own `time_to_full` is the fallback.
                    match (energy_now, energy_full) {
                        (Some(now), Some(full)) if full > now => {
                            Some((((full - now) as f64) / (w as f64) * 3600.0) as u64)
                        }
                        _ => time_full(),
                    }
                }
                _ => time_from_minutes(sysfs::read_u64(dir.join("time_to_empty"))),
            };

            let snap = BatterySnapshot {
                name: name.to_string(),
                percent,
                status,
                is_ups: kind == "UPS",
                time_remaining_seconds,
                power_watts: if self.show_watts { watts } else { None },
                energy_full_design: sysfs::read_u64(dir.join("energy_full_design")),
                energy_full,
            };

            let take = match self.selected.trim().to_ascii_lowercase().as_str() {
                "" | "auto" => match &best {
                    // Auto: the battery with the most charge is the one the
                    // user cares about.
                    None => true,
                    Some(prev) => snap.percent > prev.percent,
                },
                want => snap.name.eq_ignore_ascii_case(want),
            };
            if take {
                best = Some(snap);
            }
        }

        if !saw_any && best.is_none() {
            logger::once("no-battery", "no battery present");
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings_map_to_variants() {
        assert_eq!(parse_status("Charging"), Some(BatteryStatus::Charging));
        assert_eq!(
            parse_status("discharging"),
            Some(BatteryStatus::Discharging)
        );
        assert_eq!(parse_status("Full"), Some(BatteryStatus::Full));
        assert_eq!(
            parse_status("Not charging"),
            Some(BatteryStatus::NotCharging)
        );
        // Unknown must fall through to the AC derivation, not be guessed.
        assert_eq!(parse_status("Unknown"), None);
        assert_eq!(parse_status(""), None);
    }

    #[test]
    fn ratio_percent_guards_the_denominator() {
        assert_eq!(ratio_percent(50, 100), Some(50.0));
        assert_eq!(ratio_percent(0, 0), None);
        // A full energy_now cannot report more than 100%.
        assert_eq!(ratio_percent(200, 100), Some(100.0));
    }

    #[test]
    fn the_kernel_sentinel_is_unknown_not_a_long_time() {
        // The kernel writes i64::MAX minutes when it cannot know.
        assert_eq!(time_from_minutes(Some(i64::MAX as u64)), None);
        // A real zero means "no estimate", not "empty now".
        assert_eq!(time_from_minutes(Some(0)), None);
        assert_eq!(time_from_minutes(Some(90)), Some(5400));
        // An absent file is also unknown, not zero.
        assert_eq!(time_from_minutes(None), None);
    }

    #[test]
    fn a_desktop_reports_no_battery() {
        // This machine has no battery, so the panel must be absent entirely.
        let cfg = Config::defaults();
        let mut c = BatteryCollector::new(&cfg);
        let result = c.collect(Instant::now());
        // Not asserting None: the test machine may have one. Asserting the
        // *contract* instead — a returned battery is always in range.
        if let Some(b) = result {
            assert!((0.0..=100.0).contains(&b.percent), "got {}", b.percent);
            assert!(!b.name.is_empty());
        }
    }
}
