//! Memory: `/proc/meminfo`, plus the ZFS ARC when `zfs_arc_cached` is on.
//!
//! The maths is btop's verbatim (docs/03-data-layer.md §2) and the maths is
//! trivial: `/proc/meminfo` is absolute, not a rate, so there is no previous
//! sample to diff against. That is why [`MemCollector::collect`] takes `now`
//! and ignores it — the parameter exists so every collector in
//! `Collector::tick` has the same signature.
//!
//! ## Why the kB shift is a named function
//!
//! `/proc/meminfo` is in kibibytes. The model boundary is bytes. Getting the
//! shift wrong is a 1024x error that still *looks* plausible on screen, so it
//! lives in one place with a test.
//!
//! ## Why `arcstats` needs its own parser
//!
//! `sysfs::read_kv` splits on `:`, which ZFS `arcstats` records do not contain
//! — they are `<name> <type> <value>`. `sysfs.rs` documents this and defers to
//! this module. See [`arcstats_value`].

use std::time::Instant;

use crate::collect::sysfs;
use crate::config::Config;
use crate::logger;
use crate::model::MemSnapshot;

/// `/proc/meminfo` values are in kibibytes; the model boundary is bytes.
const KB_SHIFT: u32 = 10;

pub struct MemCollector {
    zfs_arc_cached: bool,
}

/// kibibytes to bytes. Saturating so a corrupt `/proc` cannot wrap a `u64`
/// into a plausible-looking small number.
pub fn kb_to_bytes(kb: u64) -> u64 {
    kb.saturating_mul(1 << KB_SHIFT)
}

/// One value out of a parsed `/proc/meminfo`, converted to bytes.
///
/// The value column is `"16384 kB"`, so the first whitespace-separated token is
/// the number and the unit is dropped rather than parsed — the kernel has never
/// emitted anything but kB here and checking would only add a failure mode.
pub fn meminfo_bytes(kv: &[(String, String)], key: &str) -> Option<u64> {
    let (_, raw) = kv.iter().find(|(k, _)| k == key)?;
    let kb: u64 = raw.split_whitespace().next()?.parse().ok()?;
    Some(kb_to_bytes(kb))
}

/// `MemAvailable` is the right number to subtract from `MemTotal`, but it only
/// exists on Linux 3.14 and later. btop falls back to `MemFree + Cached`.
///
/// The result is clamped to `total`: a kernel that reports a stale or larger
/// `MemAvailable` would otherwise produce a negative `used`, and the model has
/// no way to represent "impossible".
pub fn resolve_available(total: u64, free: u64, cached: u64, available: Option<u64>) -> u64 {
    let avail = available.unwrap_or_else(|| free.saturating_add(cached));
    avail.min(total)
}

/// One value out of `/proc/spl/kstat/zfs/arcstats`.
///
/// Records are `<name> <type> <value>`; the type column is skipped. btop reads
/// `size` and `c_min`, both of which are type-4 (mu_bytes) counters.
pub fn arcstats_value(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next()? == key {
            // Skip the type column.
            if fields.next().is_some() {
                return fields.last()?.parse().ok();
            }
        }
    }
    None
}

/// Percentage that is 0 rather than `NaN` when the denominator is 0.
fn percent(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        return 0.0;
    }
    let pct = (part as f64) * 100.0 / (whole as f64);
    if pct.is_finite() {
        pct.clamp(0.0, 100.0) as f32
    } else {
        0.0
    }
}

impl MemCollector {
    pub fn new(cfg: &Config) -> Self {
        Self {
            zfs_arc_cached: cfg.bool("zfs_arc_cached"),
        }
    }

    /// `now` is unused: `/proc/meminfo` is absolute, so there is no rate to
    /// compute. Kept for signature parity with the other collectors.
    pub fn collect(&mut self, _now: Instant) -> MemSnapshot {
        let kv = sysfs::read_kv("/proc/meminfo");
        if kv.is_empty() {
            // /proc vanishing is fatal for the whole app; startup already
            // checked, so this is a degraded tick, not a crash.
            logger::once("no-proc-meminfo", "/proc/meminfo unreadable");
            return MemSnapshot::default();
        }

        // btop returns rather than guessing when MemTotal is missing, because
        // every other field is a fraction of it.
        let Some(total_bytes) = meminfo_bytes(&kv, "MemTotal") else {
            logger::once("no-memtotal", "/proc/meminfo has no MemTotal");
            return MemSnapshot::default();
        };

        let free_bytes = meminfo_bytes(&kv, "MemFree").unwrap_or(0);
        let cached_bytes = meminfo_bytes(&kv, "Cached").unwrap_or(0);
        let available_bytes = resolve_available(
            total_bytes,
            free_bytes,
            cached_bytes,
            meminfo_bytes(&kv, "MemAvailable"),
        );

        let swap_total_bytes = meminfo_bytes(&kv, "SwapTotal").unwrap_or(0);
        let swap_free_bytes = meminfo_bytes(&kv, "SwapFree").unwrap_or(0);
        // zram needs no special case: it is already in SwapTotal/SwapFree.

        let mut cached_bytes = cached_bytes;
        let mut available_bytes = available_bytes;
        if self.zfs_arc_cached {
            match sysfs::read_str("/proc/spl/kstat/zfs/arcstats") {
                Some(text) => {
                    let c_min = arcstats_value(&text, "c_min");
                    let size = arcstats_value(&text, "size");
                    if let (Some(c_min), Some(size)) = (c_min, size) {
                        cached_bytes = cached_bytes.saturating_add(c_min).saturating_add(size);
                        available_bytes =
                            available_bytes.saturating_add(size.saturating_sub(c_min));
                    }
                }
                None => {
                    // Logged once, not once per tick.
                    logger::once("no-zfs-arcstats", "ZFS arcstats unreadable");
                }
            }
        }

        let used_bytes = total_bytes.saturating_sub(available_bytes);
        let swap_used_bytes = swap_total_bytes.saturating_sub(swap_free_bytes);

        MemSnapshot {
            total_bytes,
            used_bytes,
            available_bytes,
            cached_bytes,
            free_bytes,
            swap_total_bytes,
            swap_used_bytes,
            swap_free_bytes,
            used_percent: percent(used_bytes, total_bytes),
            swap_percent: percent(swap_used_bytes, swap_total_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn kb_to_bytes_shifts_by_ten() {
        assert_eq!(kb_to_bytes(0), 0);
        assert_eq!(kb_to_bytes(1), 1024);
        assert_eq!(kb_to_bytes(1024), 1_048_576);
        assert_eq!(kb_to_bytes(16_384), 16_777_216);
        // Saturating, never wrapping to something plausible-looking.
        assert_eq!(kb_to_bytes(u64::MAX), u64::MAX);
    }

    #[test]
    fn meminfo_values_are_parsed_and_shifted() {
        let t = kv(&[
            ("MemTotal", "16384 kB"),
            ("MemFree", "2048 kB"),
            ("MemAvailable", "8192 kB"),
            ("SwapTotal", "0 kB"),
        ]);
        assert_eq!(meminfo_bytes(&t, "MemTotal"), Some(16_777_216));
        assert_eq!(meminfo_bytes(&t, "MemFree"), Some(2_097_152));
        assert_eq!(meminfo_bytes(&t, "MemAvailable"), Some(8_388_608));
        // Absent keys are None, not zero.
        assert_eq!(meminfo_bytes(&t, "MemAvailableX"), None);
        assert_eq!(meminfo_bytes(&t, "Nope"), None);
    }

    #[test]
    fn meminfo_ignores_a_missing_or_unparseable_unit() {
        // "1024" with no unit still parses; that is the historical kernel format.
        let t = kv(&[("MemTotal", "1024")]);
        assert_eq!(meminfo_bytes(&t, "MemTotal"), Some(1_048_576));
        // Garbage yields None so the caller can decide, rather than 0.
        let bad = kv(&[("MemTotal", "lots kB")]);
        assert_eq!(meminfo_bytes(&bad, "MemTotal"), None);
    }

    #[test]
    fn missing_mem_available_falls_back_to_free_plus_cached() {
        let total = kb_to_bytes(16_384);
        let free = kb_to_bytes(2_048);
        let cached = kb_to_bytes(4_096);

        // Pre-3.14 kernel: no MemAvailable at all.
        assert_eq!(
            resolve_available(total, free, cached, None),
            free + cached,
            "fallback is MemFree + Cached"
        );
        // A real MemAvailable wins over the fallback.
        let avail = kb_to_bytes(8_192);
        assert_eq!(resolve_available(total, free, cached, Some(avail)), avail);
    }

    #[test]
    fn available_is_clamped_to_total() {
        let total = kb_to_bytes(1_024);
        // A kernel reporting more available than total must not make `used`
        // wrap around to near-u64::MAX.
        let absurd = kb_to_bytes(9_999);
        assert_eq!(resolve_available(total, 0, 0, Some(absurd)), total);
        let used = total - resolve_available(total, 0, 0, Some(absurd));
        assert_eq!(used, 0);
    }

    #[test]
    fn arcstats_parses_name_type_value() {
        // The real file has no colons, which is why read_kv cannot read it.
        let text = "13 1 0x01 1082954 4270892247\n\
                    c_min 4 12345\n\
                    size 4 987654321\n\
                    hits 4 42\n";
        assert_eq!(arcstats_value(text, "c_min"), Some(12_345));
        assert_eq!(arcstats_value(text, "size"), Some(987_654_321));
        assert_eq!(arcstats_value(text, "hits"), Some(42));
        assert_eq!(arcstats_value(text, "c_max"), None);
    }

    #[test]
    fn arcstats_ignores_malformed_records() {
        // Fewer than three columns has no value to take.
        assert_eq!(arcstats_value("size 4\n", "size"), None);
        assert_eq!(arcstats_value("", "size"), None);
        // A non-numeric value column is None, not 0.
        assert_eq!(arcstats_value("size 4 unknown\n", "size"), None);
    }

    #[test]
    fn percent_never_returns_nan() {
        assert_eq!(percent(0, 0), 0.0);
        assert_eq!(percent(1, 2), 50.0);
        // Over-full input is clamped rather than shown as 150%.
        assert_eq!(percent(3, 2), 100.0);
    }
}
