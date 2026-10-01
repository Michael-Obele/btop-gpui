//! Plain data. Every struct here is produced by a collector and consumed by the
//! UI. **No GPUI import may ever appear in this file** (or anywhere under
//! `src/collect/`), because that is what makes the data layer testable without
//! a window.
//!
//! # Units are fixed at this boundary
//!
//! | Quantity            | Unit                     | Field suffix        |
//! |---------------------|--------------------------|---------------------|
//! | memory, sizes       | bytes (`u64`)            | `_bytes`            |
//! | throughput          | bytes per second (`f64`) | `_bytes_per_sec`    |
//! | CPU frequency       | MHz (`u32`)              | `mhz`               |
//! | temperature         | °C (`f32`)               | `temp_c`            |
//! | power               | milliwatts (`f32`)       | `watts` is actually W — see `read_watts` |
//! | percentages         | 0.0–100.0 (`f32`)        | `_percent`          |
//! | time                | seconds (`u64`)          | `_seconds`          |
//! | CPU ticks           | raw jiffies (`u64`)      | `_ticks`            |
//!
//! Conversion to human strings happens in `format.rs` and **only** in `render()`.

use std::time::Instant;

/// One complete reading of the machine. Expensive to clone (it owns a `Vec` of
/// processes) — hand it around behind an `Arc`, never copy it per frame.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub at: Instant,
    pub cpu: CpuSnapshot,
    pub mem: MemSnapshot,
    pub disks: Vec<DiskSnapshot>,
    pub nets: Vec<NetSnapshot>,
    pub procs: Vec<ProcSnapshot>,
    pub battery: Option<BatterySnapshot>,
}

#[derive(Debug, Clone, Default)]
pub struct CpuSnapshot {
    pub model_name: String,
    /// Logical CPUs.
    pub core_count: usize,
    pub total_percent: f32,
    /// One entry per logical CPU, index == kernel `cpuN` number.
    pub cores: Vec<CoreSnapshot>,
    /// 1 / 5 / 15 minute load averages, from `getloadavg(3)`.
    pub load_avg: [f64; 3],
    pub uptime_seconds: u64,
    /// user, nice, system, idle, iowait, irq, softirq, steal — same length as
    /// [`CPU_FIELD_NAMES`].
    pub fields_percent: Vec<f32>,
    /// The cgroup cpuset mask, when the process is restricted to a subset.
    pub active_cpus: Option<Vec<usize>>,
    /// CPU package power draw in **watts**; `None` without `cap_perfmon`.
    pub watts: Option<f32>,
    /// Sum of the first 8 `/proc/stat` fields, needed by the process
    /// collector's CPU% formula. Kept here because the CPU collector runs
    /// first in `Collector::tick`.
    pub times_total: u64,
}

pub const CPU_FIELD_NAMES: [&str; 8] = [
    "user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal",
];

#[derive(Debug, Clone, Default)]
pub struct CoreSnapshot {
    pub percent: f32,
    pub mhz: Option<u32>,
    pub temp_c: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct MemSnapshot {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub cached_bytes: u64,
    pub free_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_free_bytes: u64,
    pub used_percent: f32,
    pub swap_percent: f32,
}

#[derive(Debug, Clone, Default)]
pub struct DiskSnapshot {
    /// "sda1", "dm-0", or the synthetic "swap" row.
    pub name: String,
    pub mount_point: String,
    pub filesystem: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub used_percent: f32,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
    pub io_percent: f32,
    /// True for the synthetic `swap` row, which has no device of its own.
    pub synthetic: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NetSnapshot {
    /// "wlan0"
    pub name: String,
    pub connected: bool,
    pub ip: Option<String>,
    pub mac: Option<String>,
    pub download_bytes_per_sec: f64,
    pub upload_bytes_per_sec: f64,
    pub total_download_bytes: u64,
    pub total_upload_bytes: u64,
    /// The auto-scaled graph ceiling in bytes/sec, decided by the collector so
    /// the UI never has to.
    pub graph_max_bps: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ProcSnapshot {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub cmdline: String,
    pub user: String,
    pub state: char,
    pub threads: u32,
    pub nice: i32,
    pub mem_bytes: u64,
    /// Instantaneous, needs two ticks to be meaningful.
    pub cpu_percent: f32,
    /// 0..core_count: average since the process started.
    pub cpu_cumulative: f32,
    pub starttime_ticks: u64,
    /// `None` when `/proc/<pid>/io` is unreadable (other users' processes).
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    pub is_kernel_thread: bool,
    /// Index into the flat, tree-ordered list. Filled by the tree builder.
    pub tree_depth: u16,
    /// "├─ ", "└─ ", or "" when tree mode is off.
    pub tree_prefix: String,
    /// Monotonic position assigned by the tree walk; the final ordering key.
    pub tree_index: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BatteryStatus {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Full,
    NotCharging,
}

impl BatteryStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Charging => "Charging",
            Self::Discharging => "Discharging",
            Self::Full => "Full",
            Self::NotCharging => "Not charging",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BatterySnapshot {
    pub name: String,
    pub percent: f32,
    pub status: BatteryStatus,
    /// `true` when `type` was `UPS` rather than `Battery`.
    pub is_ups: bool,
    /// Seconds until full (charging) or empty (discharging); `None` if unknown.
    pub time_remaining_seconds: Option<u64>,
    /// Instantaneous draw in watts.
    pub power_watts: Option<f32>,
    pub energy_full_design: Option<u64>,
    pub energy_full: Option<u64>,
}

// ---------------------------------------------------------------------------
// GPU — the v2 seam. v1 never populates it; see docs/03-data-layer.md §7.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GpuVendor {
    #[default]
    Unknown,
    Nvidia,
    Amd,
    Intel,
    Apple,
}

impl GpuVendor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Nvidia => "nvidia",
            Self::Amd => "amd",
            Self::Intel => "intel",
            Self::Apple => "apple",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GpuSnapshot {
    pub name: String,
    pub vendor: GpuVendor,
    pub index: usize,
    pub utilisation_percent: f32,
    pub vram_used_bytes: u64,
    pub vram_total_bytes: u64,
    pub power_mw: u32,
    pub temp_c: f32,
}
