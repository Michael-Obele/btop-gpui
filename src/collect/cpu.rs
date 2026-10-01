//! CPU: `/proc/stat` percentages, per-core frequency, temperatures and watts.
//!
//! The percentage maths is copied from btop's Linux collector so the numbers
//! match it exactly — that is the acceptance test. Two consequences are
//! deliberate rather than bugs:
//!
//! * **The first tick is always 0 %.** There is no previous sample to diff
//!   against, and fabricating a baseline would lie. The UI renders `—` until
//!   the history holds two samples.
//! * **A `/sys` file that is missing yields `None`,** never a zero, so the
//!   panel can tell "idle" from "unknown".

use std::path::PathBuf;
use std::time::Instant;

use crate::collect::sysfs;
use crate::config::Config;
use crate::logger;
use crate::model::{CPU_FIELD_NAMES, CoreSnapshot, CpuSnapshot};

/// `/proc/stat` field order. The kernel may append fields in future versions,
/// so parsers must tolerate a longer line.
pub const CPU_RAW_FIELDS: usize = 10;

/// Number of consecutive `scaling_cur_freq` failures before falling back to
/// `/proc/cpuinfo` and, after that, giving up entirely. btop uses 5.
const FREQ_GIVE_UP_AFTER: u32 = 5;

#[derive(Debug, Clone, Copy, Default)]
pub struct CpuTimes {
    /// Sum excluding `guest` and `guest_nice`.
    pub total: u64,
    /// `idle + iowait`.
    pub idle: u64,
}

pub struct CpuCollector {
    core_count: usize,
    clk_tck: u64,
    page_size: u64,
    model_name: String,
    /// `(cpu index, path)` discovered once; entries that never read are dropped.
    freq_paths: Vec<(usize, PathBuf)>,
    old_total: u64,
    old_idle: u64,
    old_fields: [u64; CPU_RAW_FIELDS],
    old_cores: Vec<CpuTimes>,
    freq_failures: u32,
    /// Latched to `Some(false)` once `energy_uj` proves unreadable, so we stop
    /// trying every tick and log exactly once.
    supports_watts: Option<bool>,
    last_energy_uj: Option<(u64, Instant)>,
    active_cpus: Option<Vec<usize>>,
    show_freq: bool,
    check_temp: bool,
    show_coretemp: bool,
    freq_mode: String,
    cpuinfo_mhz: Vec<Option<u32>>,
}

// ---------------------------------------------------------------------------
// Pure functions — these are what the unit tests exercise.
// ---------------------------------------------------------------------------

/// Parse one `cpu…` line into its tag and up to 10 raw counters. Extra fields
/// the kernel may add later are ignored rather than treated as an error.
pub fn parse_cpu_line(line: &str) -> Option<(String, [u64; CPU_RAW_FIELDS])> {
    let mut it = line.split_ascii_whitespace();
    let tag = it.next()?.to_string();
    let mut raw = [0u64; CPU_RAW_FIELDS];
    for slot in raw.iter_mut() {
        match it.next() {
            Some(tok) => *slot = tok.parse().unwrap_or(0),
            None => break,
        }
    }
    Some((tag, raw))
}

/// btop's rule: the total excludes `guest` and `guest_nice`, because those are
/// already counted in user/nice.
pub fn totals_of(raw: &[u64; CPU_RAW_FIELDS]) -> CpuTimes {
    let sum: u64 = raw.iter().sum();
    let total = sum.saturating_sub(raw[8].saturating_add(raw[9]));
    CpuTimes {
        total,
        idle: raw[3].saturating_add(raw[4]),
    }
}

/// Percentage of `delta` out of `total_delta`, rounded then clamped.
pub fn pct(delta: u64, total_delta: u64) -> f32 {
    if total_delta == 0 {
        return 0.0;
    }
    (((delta as f64) * 100.0 / total_delta as f64).round() as f32).clamp(0.0, 100.0)
}

/// Percentage of busy time, given this tick's and the previous tick's totals.
/// Returns 0 for the very first sample, which has nothing to diff against.
pub fn busy_percent(cur: CpuTimes, old: CpuTimes) -> f32 {
    if old.total == 0 {
        return 0.0;
    }
    let dt = cur.total.saturating_sub(old.total).max(1);
    let di = cur.idle.saturating_sub(old.idle);
    pct(dt.saturating_sub(di), dt)
}

/// `getloadavg(3)` — btop calls libc, it does **not** read `/proc/loadavg`.
pub fn load_average() -> [f64; 3] {
    let mut loads = [0.0f64; 3];
    // SAFETY: `loads` is a `[f64; 3]`, `nelem` is 3, and glibc only writes
    // `nelem` doubles. This is the same call btop makes.
    let rc = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    if rc != 3 {
        return [0.0; 3];
    }
    loads
}

/// `/proc/uptime` is `"<seconds> <idle>"`; the first field may be a float.
pub fn read_uptime() -> u64 {
    sysfs::read_str("/proc/uptime")
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| {
            if f.is_finite() && f > 0.0 {
                f as u64
            } else {
                0
            }
        })
        .unwrap_or(0)
}

/// `/sys/fs/cgroup/cpuset.cpus.effective` — `"0-3,8,10-11"`.
/// This is the *only* cgroup file btop reads; there is no memory-quota handling.
pub fn parse_cpuset(spec: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in spec.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((a, b)) => {
                let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) else {
                    continue;
                };
                // A reversed or absurd range would allocate gigabytes.
                if b < a || b - a > 4096 {
                    continue;
                }
                out.extend(a..=b);
            }
            None => {
                if let Ok(n) = part.parse::<usize>() {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Reject the garbage some VMs report in `scaling_cur_freq`.
pub fn sane_frequency(khz: u64) -> Option<u32> {
    if khz <= 1 || khz >= 999_999_999 {
        return None;
    }
    Some((khz / 1000) as u32)
}

/// `sysconf` returns -1 on error and 0 for "unlimited", which is never a
/// sensible value for a clock tick or a page size.
fn sysconf_u64(var: nix::unistd::SysconfVar) -> Option<u64> {
    nix::unistd::sysconf(var)
        .ok()
        .flatten()
        .and_then(|v| u64::try_from(v).ok())
        .filter(|v| *v > 0)
}

impl CpuCollector {
    pub fn new(cfg: &Config) -> Self {
        // `clk_tck` and `page_size` come from sysconf; btop falls back to 100
        // and 4096, which are the correct values on every mainstream kernel.
        let clk_tck = sysconf_u64(nix::unistd::SysconfVar::CLK_TCK).unwrap_or(100);
        let page_size = sysconf_u64(nix::unistd::SysconfVar::PAGE_SIZE).unwrap_or(4096);

        // _SC_NPROCESSORS_ONLN via libc: the name of the nix variant has moved
        // between releases, and libc's never will.
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        let core_count = if n > 0 { n as usize } else { 1 };

        let mut model_name = read_model_name();
        let custom = cfg.str("custom_cpu_name");
        if !custom.trim().is_empty() {
            model_name = custom;
        }

        let active_cpus = sysfs::read_str("/sys/fs/cgroup/cpuset.cpus.effective")
            .map(|s| parse_cpuset(&s))
            .filter(|v| !v.is_empty());

        let mut this = Self {
            core_count,
            clk_tck,
            page_size,
            model_name,
            freq_paths: Vec::new(),
            old_total: 0,
            old_idle: 0,
            old_fields: [0; CPU_RAW_FIELDS],
            old_cores: Vec::new(),
            freq_failures: 0,
            supports_watts: None,
            last_energy_uj: None,
            active_cpus,
            show_freq: cfg.bool("show_cpu_freq"),
            check_temp: cfg.bool("check_temp"),
            show_coretemp: cfg.bool("show_coretemp"),
            freq_mode: cfg.str("freq_mode"),
            cpuinfo_mhz: Vec::new(),
        };
        this.discover_freq_paths();
        this
    }

    /// `clk_tck` is needed by the process collector for its CPU% formula.
    pub fn clk_tck(&self) -> u64 {
        self.clk_tck
    }

    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    pub fn core_count(&self) -> usize {
        self.core_count
    }

    /// Paths are discovered once, exactly as btop does. Anything that does not
    /// exist at startup is simply never listed.
    fn discover_freq_paths(&mut self) {
        self.freq_paths.clear();
        let base = PathBuf::from("/sys/devices/system/cpu");
        for policy in sysfs::list_dir(&base) {
            let name = policy
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !name.starts_with("policy") {
                continue;
            }
            let Some(index) = name.trim_start_matches("policy").parse::<usize>().ok() else {
                continue;
            };
            let path = policy.join("scaling_cur_freq");
            if path.exists() {
                self.freq_paths.push((index, path));
            }
        }
        self.freq_paths.sort_by_key(|(i, _)| *i);
    }

    pub fn collect(&mut self, now: Instant) -> CpuSnapshot {
        let Some(text) = std::fs::read_to_string("/proc/stat").ok() else {
            // /proc disappearing is fatal for the whole app; startup already
            // checked, so this is a degraded tick, not a crash.
            logger::once("no-proc-stat", "/proc/stat unreadable");
            return CpuSnapshot::default();
        };

        let mut out = CpuSnapshot {
            model_name: self.model_name.clone(),
            core_count: self.core_count,
            load_avg: load_average(),
            uptime_seconds: read_uptime(),
            active_cpus: self.active_cpus.clone(),
            ..Default::default()
        };

        for line in text.lines() {
            let Some((tag, raw)) = parse_cpu_line(line) else {
                continue;
            };
            if tag == "cpu" {
                // ---- the aggregate line ----
                let cur = totals_of(&raw);
                out.total_percent = busy_percent(
                    cur,
                    CpuTimes {
                        total: self.old_total,
                        idle: self.old_idle,
                    },
                );
                out.times_total = raw.iter().take(8).sum();
                out.fields_percent = (0..CPU_FIELD_NAMES.len())
                    .map(|i| {
                        pct(
                            raw[i].saturating_sub(self.old_fields[i]),
                            cur.total.saturating_sub(self.old_total).max(1),
                        )
                    })
                    .collect();
                self.old_total = cur.total;
                self.old_idle = cur.idle;
                self.old_fields = raw;
            } else if let Some(n) = tag
                .strip_prefix("cpu")
                .and_then(|s| s.parse::<usize>().ok())
            {
                // ---- one per-core line ----
                let cur = totals_of(&raw);
                if self.old_cores.len() <= n {
                    // Hotplug, or more CPUs than sysconf reported. Grow in place.
                    self.old_cores.resize(n + 1, CpuTimes::default());
                    self.core_count = self.core_count.max(n + 1);
                    out.core_count = self.core_count;
                }
                out.cores_percent_placeholder();
                out.cores[n].percent = busy_percent(cur, self.old_cores[n]);
                self.old_cores[n] = cur;
            }
        }

        self.fill_frequencies(&mut out);
        if self.check_temp {
            self.fill_temps(&mut out);
        }
        out.watts = self.read_watts(now);
        out
    }

    fn fill_frequencies(&mut self, out: &mut CpuSnapshot) {
        if !self.show_freq || self.freq_paths.is_empty() {
            return;
        }
        let mut any = false;
        let paths = std::mem::take(&mut self.freq_paths);
        let mut kept = Vec::with_capacity(paths.len());
        for (cpu, path) in &paths {
            if let Some(mhz) = sysfs::read_u64(path).and_then(sane_frequency) {
                if let Some(core) = out.cores.get_mut(*cpu) {
                    core.mhz = Some(mhz);
                    any = true;
                }
                kept.push((*cpu, path.clone()));
            }
        }
        self.freq_paths = kept;

        if any {
            self.freq_failures = 0;
        } else {
            self.freq_failures += 1;
            if self.freq_failures >= FREQ_GIVE_UP_AFTER {
                self.fill_frequencies_from_cpuinfo(out);
            }
            if self.freq_failures > FREQ_GIVE_UP_AFTER {
                // Give up permanently, exactly like btop, so we stop doing
                // failing syscalls every tick.
                self.freq_paths.clear();
                logger::once(
                    "cpu-freq-unavailable",
                    "no readable scaling_cur_freq and no cpuinfo fallback; hiding CPU frequency",
                );
            }
        }
        self.collapse_frequency(out);
    }

    /// `/proc/cpuinfo` is slow to parse, so it is only consulted after the fast
    /// sysfs path has failed repeatedly.
    fn fill_frequencies_from_cpuinfo(&mut self, out: &mut CpuSnapshot) {
        if self.cpuinfo_mhz.is_empty() {
            self.cpuinfo_mhz = parse_cpuinfo_mhz();
        }
        let mhz = &self.cpuinfo_mhz;
        let mut any = false;
        for (i, core) in out.cores.iter_mut().enumerate() {
            if let Some(Some(v)) = mhz.get(i) {
                core.mhz = Some(*v);
                any = true;
            }
        }
        if any {
            self.freq_failures = 0;
        }
    }

    /// `freq_mode` decides which single frequency is representative when the
    /// cores disagree: first / range / lowest / highest / average.
    fn collapse_frequency(&self, out: &mut CpuSnapshot) {
        if self.freq_mode == "first" {
            return;
        }
        let values: Vec<u32> = out.cores.iter().filter_map(|c| c.mhz).collect();
        if values.is_empty() {
            return;
        }
        let chosen = match self.freq_mode.as_str() {
            "lowest" => *values.iter().min().unwrap_or(&0),
            "highest" => *values.iter().max().unwrap_or(&0),
            "average" => values.iter().sum::<u32>() / values.len() as u32,
            "range" => values
                .iter()
                .max()
                .copied()
                .unwrap_or(0)
                .saturating_sub(values.iter().min().copied().unwrap_or(0)),
            _ => return,
        };
        for core in &mut out.cores {
            core.mhz = Some(chosen);
        }
    }

    fn fill_temps(&self, out: &mut CpuSnapshot) {
        let sensors = crate::collect::temp::read_hwmon();
        if sensors.is_empty() {
            // Containers usually have no /sys/class/hwmon at all. Not a bug.
            logger::once(
                "no-hwmon",
                "no readable hwmon sensors; temperatures unavailable",
            );
            return;
        }
        if let Some(package) = crate::collect::temp::package_sensor(&sensors) {
            // Attach the package temperature to core 0, which is what the CPU
            // panel labels "package".
            if let Some(core) = out.cores.first_mut() {
                core.temp_c = Some(package.1);
            }
        }
        if self.show_coretemp {
            let map = crate::collect::temp::map_cores_to_sensors(&sensors);
            for (core_ix, sensor_ix) in map {
                if let (Some(core), Some(sensor)) =
                    (out.cores.get_mut(core_ix), sensors.get(sensor_ix))
                {
                    core.temp_c = Some(sensor.1);
                }
            }
        }
    }

    /// Package power draw in watts from Intel RAPL. Needs `cap_perfmon`; when
    /// it is missing we latch off and the panel hides the row.
    fn read_watts(&mut self, now: Instant) -> Option<f32> {
        if self.supports_watts == Some(false) {
            return None;
        }
        const PATH: &str = "/sys/class/powercap/intel-rapl:0/energy_uj";
        let Some(uj) = sysfs::read_u64(PATH) else {
            self.supports_watts = Some(false);
            logger::once(
                "cpu-watts-unavailable",
                "energy_uj unreadable; CPU wattage needs cap_perfmon (see README setcap line)",
            );
            return None;
        };
        let previous = self.last_energy_uj.replace((uj, now));
        let (prev_uj, prev_now) = previous?;
        let dt_us = now.duration_since(prev_now).as_micros() as f64;
        if dt_us <= 0.0 {
            return None;
        }
        // µJ/µs == W exactly; no scaling factor needed.
        let delta_uj = uj.checked_sub(prev_uj)? as f64;
        let w = delta_uj / dt_us;
        w.is_finite().then_some(w as f32)
    }
}

impl CpuSnapshot {
    /// Make room for `cpuN` lines, so `out.cores[n]` is never out of bounds.
    fn cores_percent_placeholder(&mut self) {
        if self.cores.len() < self.core_count {
            self.cores.resize(self.core_count, CoreSnapshot::default());
        }
    }
}

/// `/proc/cpuinfo` "cpu MHz" values, in processor order. The `model name` line
/// is the fallback for the model string on ARM.
pub fn read_model_name() -> String {
    let Some(text) = std::fs::read_to_string("/proc/cpuinfo").ok() else {
        return "CPU".to_string();
    };
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            if key == "model name" || key == "Model" || key == "Hardware" {
                let v = v.trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    "CPU".to_string()
}

/// Per-processor "cpu MHz" from `/proc/cpuinfo`. Returns an empty vec when the
/// field is absent, which is normal on non-x86.
pub fn parse_cpuinfo_mhz() -> Vec<Option<u32>> {
    let Some(text) = std::fs::read_to_string("/proc/cpuinfo").ok() else {
        return Vec::new();
    };
    let mut out: Vec<Option<u32>> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim() == "cpu MHz" {
                out.push(v.trim().parse::<f32>().ok().map(|f| f.round() as u32));
            }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "cpu  100 20 30 400 5 6 7 8 9 10\n\
                          cpu0 50 10 15 200 2 3 3 4 4 5\n\
                          cpu1 50 10 15 200 3 3 4 4 5 5\n\
                          intr 12345\n\
                          ctxt 6789\n";

    #[test]
    fn parses_aggregate_and_cores() {
        let lines: Vec<&str> = SAMPLE.lines().collect();
        let (tag, raw) = parse_cpu_line(lines[0]).expect("aggregate line parses");
        assert_eq!(tag, "cpu");
        assert_eq!(raw[0], 100);
        assert_eq!(raw[3], 400);
        assert_eq!(raw[9], 10);

        let (tag, raw) = parse_cpu_line(lines[1]).expect("core line parses");
        assert_eq!(tag, "cpu0");
        assert_eq!(raw[3], 200);
    }

    #[test]
    fn non_cpu_lines_do_not_parse_as_cpu() {
        // They parse as a tag, but the tag will not be "cpu" or "cpuN", so the
        // caller skips them. Confirm the tag is what we expect.
        let (tag, _) = parse_cpu_line("intr 12345").expect("has a tag");
        assert_eq!(tag, "intr");
    }

    #[test]
    fn total_excludes_guest_fields() {
        let raw = [100, 20, 30, 400, 5, 6, 7, 8, 9, 10];
        let t = totals_of(&raw);
        // sum = 595, minus guest(9) + guest_nice(10) = 576
        assert_eq!(t.total, 576);
        assert_eq!(t.idle, 405);
    }

    #[test]
    fn first_sample_is_zero() {
        let cur = CpuTimes {
            total: 1000,
            idle: 500,
        };
        assert_eq!(busy_percent(cur, CpuTimes::default()), 0.0);
    }

    #[test]
    fn busy_percent_matches_btop_formula() {
        // 1000 ticks of which 500 idle -> 50% busy. `old_total` must be
        // non-zero: that is exactly the "first sample is 0%" rule, tested
        // separately above.
        let cur = CpuTimes {
            total: 1000,
            idle: 500,
        };
        let old = CpuTimes { total: 1, idle: 0 };
        assert_eq!(busy_percent(cur, old), 50.0);

        // Fully idle second tick: delta total 100, delta idle 100 -> 0%.
        let cur = CpuTimes {
            total: 1100,
            idle: 600,
        };
        let old = CpuTimes {
            total: 1000,
            idle: 500,
        };
        assert_eq!(busy_percent(cur, old), 0.0);

        // Fully busy second tick: delta total 100, delta idle 0 -> 100%.
        let cur = CpuTimes {
            total: 1100,
            idle: 500,
        };
        let old = CpuTimes {
            total: 1000,
            idle: 500,
        };
        assert_eq!(busy_percent(cur, old), 100.0);
    }

    #[test]
    fn counter_going_backwards_does_not_explode() {
        // A counter reset (suspend, VM snapshot) must clamp, not underflow.
        let cur = CpuTimes { total: 10, idle: 5 };
        let old = CpuTimes {
            total: 5000,
            idle: 4000,
        };
        let p = busy_percent(cur, old);
        assert!(p.is_finite() && (0.0..=100.0).contains(&p), "got {p}");
    }

    #[test]
    fn pct_clamps_to_zero_hundred() {
        assert_eq!(pct(150, 100), 100.0);
        assert_eq!(pct(0, 100), 0.0);
        assert_eq!(pct(50, 0), 0.0);
    }

    #[test]
    fn cpuset_parsing() {
        assert_eq!(parse_cpuset("0-3"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpuset("0,2,4"), vec![0, 2, 4]);
        assert_eq!(parse_cpuset("0-1,8,10-11"), vec![0, 1, 8, 10, 11]);
        assert!(parse_cpuset("").is_empty());
        assert!(parse_cpuset("garbage").is_empty());
        // A reversed range must not allocate.
        assert!(parse_cpuset("100-1").is_empty());
    }

    #[test]
    fn absurd_frequencies_are_rejected() {
        assert_eq!(sane_frequency(0), None);
        assert_eq!(sane_frequency(1), None);
        assert_eq!(sane_frequency(999_999_999), None);
        assert_eq!(sane_frequency(3_400_000), Some(3400));
    }

    #[test]
    fn uptime_reads_a_number() {
        let up = read_uptime();
        assert!(
            up > 0,
            "a live Linux machine has been up for at least a second"
        );
        assert!(load_average()[0].is_finite());
    }
}
