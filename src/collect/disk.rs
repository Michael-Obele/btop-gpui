//! Disks: the mount list, `statvfs` free space, and `/sys/block/*/stat` I/O.
//!
//! Follows docs/03-data-layer.md §3. Three things here are load-bearing and
//! easy to get wrong, so they are spelled out rather than left to a reader:
//!
//! * **Sectors are always 512 bytes.** Not the device's logical block size, not
//!   `f_bsize`. A 4Kn disk still reports 512-byte sectors in
//!   `/sys/block/*/stat`, and multiplying by 4096 overstates throughput 8x.
//! * **`statvfs` on a stale NFS/CIFS/sshfs mount blocks for *minutes*.** It is
//!   therefore only ever called from this collector, which runs on the
//!   collector thread. Nothing in this file may ever be called from `render()`.
//! * **A failed mount is logged once, then ignored forever.** Retrying a dead
//!   NFS mount every 2 s is how a monitor becomes the thing that hangs the box.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use nix::sys::statvfs::statvfs;

use crate::collect::sysfs;
use crate::config::Config;
use crate::logger;
use crate::model::DiskSnapshot;

/// `/sys/block/*/stat` counts 512-byte sectors unconditionally.
pub const SECTOR_BYTES: u64 = 512;

/// `/proc/filesystems`, preferred over the mount table when `use_fstab` is set.
const PROC_FILESYSTEMS: &str = "/proc/filesystems";
const MOUNT_TABLE: &str = "/etc/mtab";
const PROC_MOUNTS: &str = "/proc/self/mounts";
const FSTAB: &str = "/etc/fstab";
const BLOCK_DIR: &str = "/sys/block";

/// btop adds these to the physical-filesystem set explicitly, because they are
/// absent from `/proc/filesystems` on many kernels.
const EXTRA_PHYSICAL: [&str; 3] = ["zfs", "wslfs", "drvfs"];

/// The cumulative counters we diff between ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockStat {
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Milliseconds the device spent doing I/O, cumulative since boot.
    pub io_ticks: u64,
}

/// One row of the mount table, after filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Raw source from the table, e.g. `/dev/sda1`.
    pub device: String,
    /// Unescaped mount point.
    pub mount: String,
    pub fstype: String,
    /// Resolved `/sys/block/<name>`, when one exists.
    pub block: Option<String>,
}

/// The filesystems that are real disks rather than kernel plumbing.
///
/// `/proc/filesystems` lines are `"nodev\tsquashfs"` or `"\text4"`. btop skips
/// anything marked `nodev` plus squashfs/nullfs, and adds ZFS-family names by
/// hand.
pub fn parse_physical_filesystems(text: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in text.lines() {
        if line.contains("nodev") || line.contains("squashfs") || line.contains("nullfs") {
            continue;
        }
        if let Some(name) = line.split_ascii_whitespace().last() {
            set.insert(name.to_string());
        }
    }
    for extra in EXTRA_PHYSICAL {
        set.insert(extra.to_string());
    }
    set
}

/// Parse a mount table into raw (still-escaped) triples.
///
/// Escaping means no field can contain a literal space, so splitting on
/// whitespace is safe; unescaping happens afterwards, per field.
pub fn parse_mount_table(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        let (Some(device), Some(mount), Some(fstype)) = (f.first(), f.get(1), f.get(2)) else {
            continue;
        };
        out.push((
            sysfs::unescape_mount(device),
            sysfs::unescape_mount(mount),
            fstype.to_ascii_lowercase(),
        ));
    }
    out
}

/// Is `mount` inside an already-accepted mount point?
///
/// Without this, a machine with `/` and `/var/lib/docker` on the same device
/// would render the same filesystem twice, and the disk box's totals would
/// double-count it. The longest mount point wins, which is what the kernel
/// itself would show you.
pub fn is_nested_mount(mount: &str, accepted: &[String]) -> bool {
    accepted
        .iter()
        .any(|kept| kept != mount && mount.starts_with(kept.as_str()))
}

/// `/sys/block/<name>/stat`.
///
/// Field order is Linux `struct disk_stats` as printed by `part_stat_show`.
/// Only 2, 6 and 9 matter; the rest are deliberately skipped rather than
/// parsed, so a kernel that appends fields later still works.
pub fn parse_block_stat(text: &str) -> Option<BlockStat> {
    let f: Vec<&str> = text.split_ascii_whitespace().collect();
    let sectors_read: u64 = f.get(2)?.parse().ok()?;
    let sectors_written: u64 = f.get(6)?.parse().ok()?;
    let io_ticks: u64 = f.get(9)?.parse().ok()?;
    Some(BlockStat {
        read_bytes: sectors_read.saturating_mul(SECTOR_BYTES),
        write_bytes: sectors_written.saturating_mul(SECTOR_BYTES),
        io_ticks,
    })
}

fn block_exists(name: &str) -> bool {
    !name.is_empty() && Path::new(BLOCK_DIR).join(name).is_dir()
}

/// `/dev/sda1` -> `sda`, because partitions live under `/sys/block/<disk>/`, not
/// `/sys/block/`. `/dev/dm-0` -> `dm-0` directly, so dm-crypt/LUKS needs no
/// special case.
///
/// A trailing run of digits is stripped one at a time -- `sda12` -> `sda1` ->
/// `sda` -- because some kernels expose partitions at the top level and others
/// do not. The run must be non-empty: a name with no trailing digits
/// (`/dev/mapper/vg-lv`) has nothing to strip and is rejected immediately,
/// which is also what makes this loop terminate.
///
/// A `/dev/mapper/<vg>-<lv>` source only resolves if the mount table already
/// reported the mapped device, which it does on every mainstream setup;
/// otherwise there is no portable way to guess the `dm-N` number.
pub fn resolve_block_name(source: &str) -> Option<String> {
    let name = source.strip_prefix("/dev/").unwrap_or(source);
    // A real block name is short; anything longer is a device-mapper or
    // network path that will never match, and bailing early bounds the work.
    if name.is_empty() || name.len() > 64 {
        return None;
    }
    if block_exists(name) {
        return Some(name.to_string());
    }
    let mut trimmed = name;
    while !trimmed.is_empty() {
        let digits = trimmed.len() - trimmed.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            // No trailing digits left to strip: nothing more to try.
            return None;
        }
        trimmed = &trimmed[..trimmed.len() - digits];
        if block_exists(trimmed) {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// btop's I/O activity percentage.
///
/// `io_ticks` is milliseconds of device-busy time and uptime is seconds, so the
/// ratio is already a fraction; the `/10.0` is btop's scaling into percent.
/// Note the denominator is *uptime since the previous tick*, not wall time, so
/// a suspended machine does not report a permanently busy disk.
pub fn io_percent(d_io_ticks: u64, d_uptime: u64) -> f32 {
    if d_uptime == 0 {
        return 0.0;
    }
    let pct = ((d_io_ticks as f64) / (d_uptime as f64) / 10.0).round();
    if pct.is_finite() {
        pct.clamp(0.0, 100.0) as f32
    } else {
        0.0
    }
}

/// `total`, `free` bytes via `statvfs`.
///
/// **May block for minutes on a stale network mount.** Collector thread only.
fn collect_space(mount: &str, use_bfree: bool) -> Option<(u64, u64)> {
    let st = statvfs(mount).ok()?;
    let fragment = st.fragment_size() as u64;
    // A zero fragment size would make every total zero; treat it as a failure.
    if fragment == 0 {
        return None;
    }
    let total = (st.blocks() as u64).saturating_mul(fragment);
    let free = if use_bfree {
        st.blocks_free()
    } else {
        st.blocks_available()
    } as u64;
    Some((total, free.saturating_mul(fragment)))
}

fn used_percent(total: u64, free: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    let used = total.saturating_sub(free);
    let pct = (used as f64) * 100.0 / (total as f64);
    if pct.is_finite() {
        pct.clamp(0.0, 100.0) as f32
    } else {
        0.0
    }
}

/// `disks_filter`. A leading `!` excludes; a bare entry whitelists, and the
/// whitelist only takes effect if at least one non-`!` entry is present.
pub fn filter_allows(name: &str, filter: &[String]) -> bool {
    if filter.is_empty() {
        return true;
    }
    let mut has_include = false;
    let mut included = false;
    for entry in filter {
        let needle = entry.to_ascii_lowercase();
        match needle.strip_prefix('!') {
            Some(pattern) if name.to_ascii_lowercase().contains(pattern) => return false,
            Some(_) => {}
            None => {
                has_include = true;
                if name.to_ascii_lowercase().contains(&needle) {
                    included = true;
                }
            }
        }
    }
    !has_include || included
}

pub struct DiskCollector {
    only_physical: bool,
    use_fstab: bool,
    /// `true` = `f_bfree` (root's view), `false` = `f_bavail` (your view).
    disk_free_priv: bool,
    zfs_hide_datasets: bool,
    swap_disk: bool,
    filter: Vec<String>,

    /// Cached mount list, rebuilt when the table's mtime changes.
    mounts: Vec<Mount>,
    table_path: Option<PathBuf>,
    table_mtime: Option<u64>,
    physical: HashSet<String>,

    /// Mounts whose `statvfs` failed. Permanent, so a dead NFS mount is not
    /// re-probed (and re-blocking the collector thread) every single tick.
    ignore_list: HashSet<String>,

    old: HashMap<String, (BlockStat, Instant)>,
    old_uptime: u64,
    swap: Option<(u64, u64)>,
}

impl DiskCollector {
    pub fn new(cfg: &Config) -> Self {
        Self {
            only_physical: cfg.bool("only_physical"),
            use_fstab: cfg.bool("use_fstab"),
            disk_free_priv: cfg.bool("disk_free_priv"),
            zfs_hide_datasets: cfg.bool("zfs_hide_datasets"),
            swap_disk: cfg.bool("swap_disk"),
            filter: cfg.list("disks_filter"),
            mounts: Vec::new(),
            table_path: None,
            table_mtime: None,
            physical: HashSet::new(),
            ignore_list: HashSet::new(),
            old: HashMap::new(),
            old_uptime: 0,
            swap: None,
        }
    }

    /// Feed swap in from the memory collector for the synthetic `swap` row.
    /// `DiskCollector` must not read `/proc/meminfo` a second time per tick.
    pub fn set_swap(&mut self, total_bytes: u64, free_bytes: u64) {
        self.swap = Some((total_bytes, free_bytes));
    }

    /// ZFS is pool-level only in v1 (`zfs_hide_datasets` is parsed and stored
    /// but has no dataset rows to hide yet).
    pub fn zfs_hide_datasets(&self) -> bool {
        self.zfs_hide_datasets
    }

    pub fn collect(&mut self, now: Instant) -> Vec<DiskSnapshot> {
        self.refresh_mounts();
        // Read /proc/uptime exactly once per tick.
        let uptime = read_uptime();

        let mut out = Vec::with_capacity(self.mounts.len() + 1);
        let mut current: HashMap<String, (BlockStat, Instant)> = HashMap::new();

        for mount in &self.mounts {
            if self.ignore_list.contains(&mount.mount) {
                continue;
            }
            let Some((total, free)) = collect_space(&mount.mount, self.disk_free_priv) else {
                logger::once(
                    &format!("disk-statvfs:{}", mount.mount),
                    &format!(
                        "statvfs failed for {}, ignoring it from now on",
                        mount.mount
                    ),
                );
                self.ignore_list.insert(mount.mount.clone());
                continue;
            };

            let mut snap = DiskSnapshot {
                name: mount.block.clone().unwrap_or_else(|| mount.device.clone()),
                mount_point: mount.mount.clone(),
                filesystem: mount.fstype.clone(),
                total_bytes: total,
                free_bytes: free,
                used_percent: used_percent(total, free),
                synthetic: false,
                ..Default::default()
            };

            if let Some(block) = &mount.block {
                // One read of /sys/block/<dev>/stat, diffed against last tick.
                if let Some(stat) = read_block_stat(block) {
                    let (read_bps, write_bps, pct) = self.rate(block, &stat, now, uptime);
                    snap.read_bytes_per_sec = read_bps;
                    snap.write_bytes_per_sec = write_bps;
                    snap.io_percent = pct;
                    current.insert(block.clone(), (stat, now));
                }
            }

            out.push(snap);
        }

        self.old = current;
        self.old_uptime = uptime;

        if self.swap_disk
            && let Some((total, free)) = self.swap
        {
            let row = DiskSnapshot {
                name: "swap".to_string(),
                mount_point: "swap".to_string(),
                filesystem: "swap".to_string(),
                total_bytes: total,
                free_bytes: free,
                used_percent: used_percent(total, free),
                synthetic: true,
                ..Default::default()
            };
            // btop puts swap immediately after the root row.
            match out.iter().position(|d| d.mount_point == "/") {
                Some(idx) => out.insert(idx + 1, row),
                None => out.push(row),
            }
        }

        out
    }

    /// Rates for one device, in bytes/sec, plus btop's io_percent.
    fn rate(&self, block: &str, stat: &BlockStat, now: Instant, uptime: u64) -> (f64, f64, f32) {
        let Some((old, old_ts)) = self.old.get(block) else {
            // First tick: no baseline, so 0 rather than a fabricated spike.
            return (0.0, 0.0, 0.0);
        };
        // `checked_duration_since` because a clock that appears to go backwards
        // must yield 0, never a panic on the collector thread.
        let dt = now
            .checked_duration_since(*old_ts)
            .unwrap_or_default()
            .as_secs_f64()
            .max(0.001);
        let d_read = stat.read_bytes.saturating_sub(old.read_bytes);
        let d_write = stat.write_bytes.saturating_sub(old.write_bytes);
        let d_uptime = uptime.saturating_sub(self.old_uptime);
        let pct = io_percent(stat.io_ticks.saturating_sub(old.io_ticks), d_uptime);
        (d_read as f64 / dt, d_write as f64 / dt, pct)
    }

    /// Rebuild the mount list when the underlying table changes.
    fn refresh_mounts(&mut self) {
        let path = if self.use_fstab {
            PathBuf::from(FSTAB)
        } else if Path::new(MOUNT_TABLE).exists() {
            PathBuf::from(MOUNT_TABLE)
        } else {
            PathBuf::from(PROC_MOUNTS)
        };
        let mtime = sysfs::mtime_secs(&path);

        if self.mounts.is_empty()
            || self.table_path.as_deref() != Some(path.as_path())
            || mtime != self.table_mtime
        {
            if self.physical.is_empty() {
                if let Some(text) = sysfs::read_str(PROC_FILESYSTEMS) {
                    self.physical = parse_physical_filesystems(&text);
                } else {
                    // Without the whitelist every row would be filtered out.
                    logger::once("no-proc-filesystems", "/proc/filesystems unreadable");
                }
            }
            self.mounts = self.build_mounts(&path);
            self.table_path = Some(path);
            self.table_mtime = mtime;
            // A remount means the previous baselines are meaningless.
            self.old.clear();
        }
    }

    fn build_mounts(&self, path: &Path) -> Vec<Mount> {
        let Some(text) = sysfs::read_str(path) else {
            logger::once(
                &format!("disk-table:{}", path.display()),
                &format!("{} unreadable, no disks", path.display()),
            );
            return Vec::new();
        };

        let mut out: Vec<Mount> = Vec::new();
        let mut accepted: Vec<String> = Vec::new();
        let mut seen_mounts: HashSet<String> = HashSet::new();

        for (device, mount, fstype) in parse_mount_table(&text) {
            if self.use_fstab {
                // /etc/fstab has no filesystem column; the third field is
                // options. `none` and `swap` are not real mounts.
                if fstype == "none" || fstype == "swap" {
                    continue;
                }
            }
            if self.only_physical && !self.physical.contains(&fstype) {
                continue;
            }
            if !filter_allows(&device, &self.filter) && !filter_allows(&mount, &self.filter) {
                continue;
            }
            if mount.is_empty() || !seen_mounts.insert(mount.clone()) {
                continue;
            }
            if is_nested_mount(&mount, &accepted) {
                continue;
            }
            accepted.push(mount.clone());
            out.push(Mount {
                block: resolve_block_name(&device),
                device,
                mount,
                fstype,
            });
        }
        out
    }
}

/// `/proc/uptime` is `"<seconds> <idle>"`; the first field may be fractional.
fn read_uptime() -> u64 {
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

fn read_block_stat(block: &str) -> Option<BlockStat> {
    let path = Path::new(BLOCK_DIR).join(block).join("stat");
    parse_block_stat(&sysfs::read_str(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 512-byte sector maths ----

    #[test]
    fn sectors_are_always_512_bytes_even_on_4kn() {
        // reads_completed, reads_merged, sectors_read, ms, writes_completed,
        // writes_merged, sectors_written, ms, ios, ms_doing_io, weighted_ms
        let s = parse_block_stat("1 2 100 30 4 5 200 40 0 900 0").unwrap();
        assert_eq!(s.read_bytes, 100 * 512);
        assert_eq!(s.write_bytes, 200 * 512);
        assert_eq!(s.io_ticks, 900);
        // Not 4096, on any disk, ever.
        assert_ne!(s.read_bytes, 100 * 4096);
    }

    #[test]
    fn block_stat_tolerates_extra_and_missing_fields() {
        // A future kernel appending fields must not break the parse.
        let s = parse_block_stat("1 2 100 30 4 5 200 40 0 900 0 11 12 13").unwrap();
        assert_eq!(s.read_bytes, 51_200);
        // Truncated line: None, not a panic and not a zero.
        assert_eq!(parse_block_stat("1 2 100"), None);
        assert_eq!(parse_block_stat(""), None);
        // Non-numeric: None.
        assert_eq!(parse_block_stat("1 2 x 30 4 5 200 40 0 900 0"), None);
    }

    // ---- mount unescaping ----

    #[test]
    fn mount_points_are_unescaped() {
        // \040 is a space, and the field survives whitespace splitting because
        // the kernel never emits a literal space inside a field.
        let table = "/dev/sda1 /mnt/my\\040disk ext4 rw,relatime 0 0\n";
        let rows = parse_mount_table(table);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "/mnt/my disk");
        assert_eq!(rows[0].2, "ext4");
    }

    #[test]
    fn unescape_handles_every_octal_escape() {
        assert_eq!(sysfs::unescape_mount("/a\\040b"), "/a b");
        assert_eq!(sysfs::unescape_mount("/a\\011b"), "/a\tb");
        assert_eq!(sysfs::unescape_mount("/a\\134b"), "/a\\b");
        assert_eq!(sysfs::unescape_mount("/plain"), "/plain");
    }

    #[test]
    fn mount_table_parsing_skips_junk_lines() {
        let table = "\n\
                     proc /proc proc rw 0 0\n\
                     /dev/nvme0n1p2 /boot\\040dir ext4 rw 0 0\n\
                     only-one-field\n";
        let rows = parse_mount_table(table);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].0, "/dev/nvme0n1p2");
        assert_eq!(rows[1].1, "/boot dir");
    }

    // ---- mount filtering ----

    #[test]
    fn physical_filesystems_excludes_kernel_plumbing() {
        let text = "nodev\tsysfs\n\
                    nodev\tproc\n\
                    \text4\n\
                    \txfs\n\
                    nodev\ttmpfs\n\
                    \tsquashfs\n\
                    \tnullfs\n";
        let set = parse_physical_filesystems(text);
        assert!(set.contains("ext4"));
        assert!(set.contains("xfs"));
        assert!(!set.contains("proc"));
        assert!(!set.contains("sysfs"));
        assert!(!set.contains("tmpfs"));
        assert!(!set.contains("squashfs"));
        assert!(!set.contains("nullfs"));
        // btop adds the ZFS family by hand; it is not always in the file.
        for extra in EXTRA_PHYSICAL {
            assert!(set.contains(extra), "{extra} must be whitelisted");
        }
    }

    #[test]
    fn nested_mounts_are_dropped() {
        let accepted = vec!["/".to_string()];
        assert!(is_nested_mount("/var/lib/docker", &accepted));
        assert!(is_nested_mount("/boot/efi", &accepted));
        // The mount point itself is not nested in itself.
        assert!(!is_nested_mount("/", &accepted));
        // A sibling sharing a prefix string is NOT nested. `/data` really is
        // under `/`, so this checks a different accepted set.
        let accepted = vec!["/home".to_string()];
        assert!(!is_nested_mount("/data", &accepted));
        assert!(is_nested_mount("/home/user/xfs", &accepted));
    }

    // ---- io_percent ----

    #[test]
    fn io_percent_matches_btops_formula() {
        // 10 ms busy in 10 s of uptime: 10/10/10 = 0.1 -> rounds to 0.
        assert_eq!(io_percent(10, 10), 0.0);
        // 5000 ms busy in 10 s: 5000/10/10 = 50%.
        assert_eq!(io_percent(5000, 10), 50.0);
        // 1000/10/10 = 10%.
        assert_eq!(io_percent(1000, 10), 10.0);
    }

    #[test]
    fn io_percent_clamps_and_degrades() {
        // Absurd input clamps to 100 rather than overflowing the meter.
        assert_eq!(io_percent(10_000_000, 10), 100.0);
        // No elapsed uptime is 0, not a division by zero.
        assert_eq!(io_percent(100, 0), 0.0);
    }

    // ---- disks_filter ----

    #[test]
    fn filter_excludes_with_bang() {
        let f = vec!["!sdb".to_string()];
        assert!(!filter_allows("sdb", &f));
        assert!(filter_allows("sda", &f));
    }

    #[test]
    fn filter_whitelists_only_when_a_bare_entry_exists() {
        let inc = vec!["nvme".to_string()];
        assert!(filter_allows("nvme0n1", &inc));
        assert!(!filter_allows("sda", &inc));
        // Mixing both: the whitelist wins over "everything else".
        let both = vec!["nvme".to_string(), "!sdb".to_string()];
        assert!(filter_allows("nvme0n1", &both));
        assert!(!filter_allows("sdb", &both));
        assert!(!filter_allows("sda", &both));
        // Empty filter is a no-op.
        assert!(filter_allows("sda", &[]));
    }

    // ---- block name resolution ----

    #[test]
    fn block_resolution_never_panics_on_odd_input() {
        // Pure-logic guard: none of these exist on the test machine, so all
        // return None rather than panicking or reading garbage.
        assert_eq!(resolve_block_name(""), None);
        assert_eq!(resolve_block_name("/dev/"), None);
        assert_eq!(resolve_block_name("no-leading-slash-that-is-real"), None);
        assert_eq!(resolve_block_name("/dev/123"), None);
        assert_eq!(resolve_block_name("/dev/mapper/vg-lv"), None);
    }
}
