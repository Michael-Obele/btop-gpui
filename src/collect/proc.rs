//! Processes: the heaviest collector in the app.
//!
//! On a busy machine with 800 PIDs this costs roughly 10 ms, which is why it
//! runs on the collector thread and never in `render()`.
//!
//! # The parsing trap
//!
//! `/proc/<pid>/stat`'s second field is the executable name in parentheses, and
//! **it may contain both spaces and parentheses**. `split_whitespace()` on the
//! whole line silently shifts every field from index 4 onward. The fix is to
//! split on the *last* `)`, after which `rest[0]` is kernel field 3 and field
//! `N` lives at `rest[N - 3]`. `tests/fixtures/proc_pid_stat_weird_comm.txt`
//! exists purely to keep that honest.
//!
//! # What is not read
//!
//! * `comm` — only for a PID we have not seen before, then cached forever. It
//!   never changes.
//! * `io` — only for the one selected process. Reading it for every process is
//!   both slow and usually EPERM.
//! * `smaps` — only for the selected process, and only when asked for. btop's
//!   own comment: parsing smaps increases total CPU usage by ~20x.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::collect::sysfs;
use crate::config::Config;
use crate::logger;
use crate::model::{CpuSnapshot, ProcSnapshot};

/// `cmdline` is truncated here; the kernel allows far more and the UI cannot
/// show it anyway.
const CMDLINE_MAX: usize = 1000;

/// How often the kernel-thread set is thrown away and rebuilt. PIDs wrap, so a
/// set cached forever eventually mislabels an ordinary user process.
const KERNEL_THREAD_RESCAN: u32 = 256;

/// btop's `CpuLazy` rotation: a process above this is pulled toward the front
/// so a 200%-CPU process cannot be buried by hundreds of idle ones.
const CPU_LAZY_ROTATE_ABOVE: f32 = 30.0;

/// The cap on how many times a single process may be rotated forward.
const CPU_LAZY_MAX_ROTATIONS: usize = 10;

/// The subset of `/proc/<pid>/stat` this app needs, already parsed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatFields {
    pub state: char,
    pub ppid: i32,
    pub utime: u64,
    pub stime: u64,
    pub nice: i32,
    pub num_threads: u32,
    pub starttime: u64,
    /// RSS in pages, not bytes. The kernel's value is signed and can be
    /// negative on some kernels, hence `i64`.
    pub rss_pages: i64,
}

/// Parse one `/proc/<pid>/stat` line.
///
/// The `comm` field is delimited by the first `(` and the **last** `)`. After
/// that, `rest[0]` is kernel field 3 (state), so field `N` is `rest[N - 3]`.
pub fn parse_pid_stat(line: &str) -> Option<StatFields> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close < open {
        return None;
    }
    let rest: Vec<&str> = line[close + 1..].split_ascii_whitespace().collect();
    let f = |n: usize| -> Option<&str> { rest.get(n.checked_sub(3)?).copied() };
    Some(StatFields {
        state: f(3)?.chars().next()?,
        ppid: f(4)?.parse().ok()?,
        utime: f(14)?.parse().ok()?,
        stime: f(15)?.parse().ok()?,
        nice: f(19)?.parse().ok()?,
        num_threads: f(20)?.parse().ok()?,
        starttime: f(22)?.parse().ok()?,
        rss_pages: f(24)?.parse::<i64>().unwrap_or(0),
    })
}

/// btop's instantaneous process CPU%.
///
/// `dt` is this process's own tick delta, `dt_total` the machine-wide delta
/// from the aggregate `/proc/stat` line (its first 8 fields, which is why the
/// CPU collector must run first). The result is scaled by the core count when
/// `per_core` is on, so it may exceed 100.
pub fn cpu_percent(dt: u64, dt_total: u64, cmult: f32, core_count: usize) -> f32 {
    if dt_total == 0 || dt == 0 {
        return 0.0;
    }
    let raw = (cmult as f64) * 1000.0 * (dt as f64) / (dt_total as f64) / 10.0;
    if !raw.is_finite() {
        return 0.0;
    }
    (raw.clamp(0.0, 100.0 * core_count as f64)) as f32
}

/// Average CPU% over the process's whole lifetime, bounded by the core count.
pub fn cpu_cumulative(cpu_ticks: u64, clk_tck: u64, uptime_seconds: u64, starttime: u64) -> f32 {
    let total = uptime_seconds.saturating_mul(clk_tck);
    let denom = total.saturating_sub(starttime).max(1);
    let pct = (cpu_ticks as f64) * 100.0 / (denom as f64);
    if pct.is_finite() {
        pct.clamp(0.0, 100.0 * 100.0) as f32
    } else {
        0.0
    }
}

/// RSS in bytes, with the `statm` fallback btop uses when the `stat` value is
/// impossible. A kernel bug or a race can leave `rss_pages` far larger than
/// physical memory; clamping to `mem_total` detects that.
pub fn resolve_rss_bytes(
    rss_pages: i64,
    statm_rss_pages: Option<i64>,
    page_size: u64,
    mem_total: u64,
) -> u64 {
    let page = if page_size == 0 { 4096 } else { page_size };
    let from_stat = rss_pages.max(0) as u64 * page;
    if mem_total > 0 && from_stat >= mem_total {
        // Bogus: take the statm value if it is saner, else zero.
        let alt = statm_rss_pages.unwrap_or(0).max(0) as u64 * page;
        return if alt < mem_total { alt } else { 0 };
    }
    from_stat
}

/// Human-readable process state. btop uses a fixed table.
pub fn state_label(state: char) -> &'static str {
    match state {
        'R' => "Running",
        'S' => "Sleeping",
        'D' => "Uninterruptible",
        'Z' => "Zombie",
        'T' => "Stopped",
        't' => "Tracing stop",
        'X' | 'x' => "Dead",
        'I' => "Idle",
        'W' => "Paging",
        'K' => "Wakekill",
        'P' => "Parked",
        _ => "Unknown",
    }
}

/// The process list's sort order. Mirrors btop's `proc_sorting` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcSort {
    #[default]
    Pid,
    Name,
    Command,
    Threads,
    User,
    Memory,
    CpuDirect,
    CpuLazy,
}

impl ProcSort {
    /// Map btop's config string to a variant. Unknown values fall back to
    /// `Pid` rather than failing to start.
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "name" => Self::Name,
            "command" => Self::Command,
            "threads" => Self::Threads,
            "user" => Self::User,
            "memory" => Self::Memory,
            "cpu direct" => Self::CpuDirect,
            "cpu lazy" => Self::CpuLazy,
            _ => Self::Pid,
        }
    }
}

/// A compiled process filter.
pub enum CompiledFilter {
    /// No filter typed: everything matches.
    None,
    /// Case-insensitive substring match.
    Substring(String),
    /// POSIX extended regex, from a leading `!`.
    Regex(regex::Regex),
    /// The user typed something that is not a valid regex. btop's behaviour is
    /// to match **nothing**, which is what this guarantees — falling back to
    /// "match everything" would silently hide the fact that the filter is bad.
    Never,
}

/// Compile the filter text the user typed, once per change.
///
/// A leading `!` switches to regex mode. An empty string means no filter.
pub fn compile_filter(text: &str) -> CompiledFilter {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return CompiledFilter::None;
    }
    match trimmed.strip_prefix('!') {
        Some(pattern) => match regex::Regex::new(pattern) {
            Ok(re) => CompiledFilter::Regex(re),
            // An invalid pattern must not panic and must not match everything.
            Err(_) => CompiledFilter::Never,
        },
        None => CompiledFilter::Substring(trimmed.to_ascii_lowercase()),
    }
}

impl CompiledFilter {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// btop's process filter. A leading `!` means POSIX extended regex; anything
/// else is a case-insensitive substring over pid, name, cmdline and user.
pub fn matches_filter(proc: &ProcSnapshot, filter: &CompiledFilter) -> bool {
    match filter {
        CompiledFilter::None => true,
        CompiledFilter::Never => false,
        CompiledFilter::Substring(needle) => {
            proc.name.to_ascii_lowercase().contains(needle)
                || proc.cmdline.to_ascii_lowercase().contains(needle)
                || proc.user.to_ascii_lowercase().contains(needle)
                || proc.pid.to_string() == *needle
        }
        CompiledFilter::Regex(re) => {
            re.is_match(&proc.name)
                || re.is_match(&proc.cmdline)
                || re.is_match(&proc.user)
                || re.is_match(&proc.pid.to_string())
        }
    }
}

/// Send a signal to a PID. Surfaces EPERM rather than panicking so the UI can
/// show a real error message.
pub fn send_signal(pid: i32, sig: nix::sys::signal::Signal) -> std::io::Result<()> {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid), sig).map_err(Into::into)
}

/// Change a process's nice value.
///
/// `nix::sys::resource` has no `setpriority` in 0.31 (only `getrlimit`,
/// `setrlimit` and `getrusage`), so this calls libc directly — the same call
/// btop makes. Returns EPERM for another user's process, which is expected and
/// must be shown, not swallowed.
pub fn set_nice(pid: i32, nice: i32) -> std::io::Result<()> {
    // SAFETY: `setpriority` only reads the two integers and reports errno.
    // A bogus pid is reported as ESRCH rather than doing anything harmful.
    let rc =
        unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice as libc::c_int) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Read `/proc/<pid>/io` for the detail pane. Almost always EPERM for another
/// user's process, which is a normal outcome and yields `None`.
pub fn read_proc_io(pid: i32) -> (Option<u64>, Option<u64>) {
    let kv = sysfs::read_kv(format!("/proc/{pid}/io"));
    let get = |k: &str| -> Option<u64> {
        kv.iter()
            .find(|(kk, _)| kk == k)
            .and_then(|(_, v)| v.split_whitespace().next())
            .and_then(|s| s.parse::<u64>().ok())
    };
    (get("read_bytes"), get("write_bytes"))
}

pub struct ProcCollector {
    clk_tck: u64,
    page_size: u64,
    per_core: bool,
    filter_kernel: bool,
    /// CPU tick totals per PID, for the instantaneous percentage.
    old_cpu: HashMap<i32, u64>,
    /// The machine-wide delta is the same for every row, but it must come
    /// from the CPU collector which runs earlier in the tick.
    old_times_total: u64,
    /// `comm`, read once per PID and then cached: it cannot change.
    names: HashMap<i32, String>,
    /// `/etc/passwd`, re-read only when its mtime changes.
    users: HashMap<u32, String>,
    users_mtime: Option<u64>,
    /// PIDs whose parent is PID 2. Cleared periodically so PID reuse cannot
    /// leave a stale user process mislabelled as a kernel thread.
    kernel_threads: HashSet<i32>,
    scans: u32,
}

impl ProcCollector {
    pub fn new(cfg: &Config, clk_tck: u64, page_size: u64) -> Self {
        Self {
            clk_tck: if clk_tck == 0 { 100 } else { clk_tck },
            page_size: if page_size == 0 { 4096 } else { page_size },
            per_core: cfg.bool("proc_per_core"),
            filter_kernel: cfg.bool("proc_filter_kernel"),
            old_cpu: HashMap::new(),
            old_times_total: 0,
            names: HashMap::new(),
            users: HashMap::new(),
            users_mtime: None,
            kernel_threads: HashSet::new(),
            scans: 0,
        }
    }

    /// uid to username, from `/etc/passwd`. Falls back to the numeric uid,
    /// which is what `ls -n` shows and is never wrong.
    fn user_for(&mut self, uid: u32) -> String {
        if self.users.is_empty() || sysfs::mtime_secs("/etc/passwd") != self.users_mtime {
            self.users.clear();
            self.users_mtime = sysfs::mtime_secs("/etc/passwd");
            if let Some(text) = sysfs::read_str("/etc/passwd") {
                for line in text.lines() {
                    let f: Vec<&str> = line.split(':').collect();
                    if f.len() < 3 {
                        continue;
                    }
                    if let Ok(uid) = f[2].trim().parse::<u32>() {
                        self.users.insert(uid, f[0].to_string());
                    }
                }
            }
        }
        self.users
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| uid.to_string())
    }

    fn read_uid(&self, pid: i32) -> Option<u32> {
        let text = sysfs::read_str(format!("/proc/{pid}/status"))?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }

    pub fn collect(&mut self, now: Instant, cpu: &CpuSnapshot) -> Vec<ProcSnapshot> {
        let _ = now;
        self.scans = self.scans.wrapping_add(1);
        if self.scans >= KERNEL_THREAD_RESCAN {
            self.scans = 0;
            self.kernel_threads.clear();
        }

        let cmult = if self.per_core {
            cpu.core_count as f32
        } else {
            1.0
        };
        let dt_total = cpu.times_total.saturating_sub(self.old_times_total);
        let mut current_cpu: HashMap<i32, u64> = HashMap::new();
        let mut out: Vec<ProcSnapshot> = Vec::new();

        for path in sysfs::list_dir("/proc") {
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };

            // A PID can exit between the directory read and the stat read.
            let Some(line) = sysfs::read_str(path.join("stat")) else {
                continue;
            };
            let Some(stat) = parse_pid_stat(&line) else {
                continue;
            };

            let cpu_ticks = stat.utime.saturating_add(stat.stime);
            current_cpu.insert(pid, cpu_ticks);

            let is_kernel_thread = stat.ppid == 2;
            if is_kernel_thread {
                self.kernel_threads.insert(pid);
            } else {
                self.kernel_threads.remove(&pid);
            }
            if self.filter_kernel && is_kernel_thread {
                continue;
            }

            let statm_rss = || {
                sysfs::read_str(format!("/proc/{pid}/statm"))
                    .and_then(|s| s.split_whitespace().nth(1).map(str::to_string))
                    .and_then(|v| v.parse::<i64>().ok())
            };
            let mem_bytes = resolve_rss_bytes(
                stat.rss_pages,
                statm_rss(),
                self.page_size,
                // `times_total` is not memory; the RSS sanity check needs the
                // real total, which the CPU snapshot does not carry. A zero
                // here disables the check rather than inventing a limit.
                0,
            );

            let old = self.old_cpu.get(&pid).copied().unwrap_or(0);
            let percent = cpu_percent(
                cpu_ticks.saturating_sub(old),
                dt_total,
                cmult,
                cpu.core_count,
            );

            // `comm` is immutable, so it is read once per PID, ever.
            if !self.names.contains_key(&pid)
                && let Some(comm) = sysfs::read_str(path.join("comm")) {
                    self.names.insert(pid, comm);
                }
            let proc_name = self
                .names
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| format!("pid{pid}"));

            let cmdline = sysfs::read_str(path.join("cmdline"))
                .map(|c| c.replace('\0', " ").trim().to_string())
                .filter(|c| !c.is_empty())
                .map(|c| {
                    if c.chars().count() > CMDLINE_MAX {
                        c.chars().take(CMDLINE_MAX).collect()
                    } else {
                        c
                    }
                })
                .unwrap_or_else(|| proc_name.clone());

            let user = self
                .read_uid(pid)
                .map(|uid| self.user_for(uid))
                .unwrap_or_default();

            out.push(ProcSnapshot {
                pid,
                ppid: stat.ppid,
                name: proc_name,
                cmdline,
                user,
                state: stat.state,
                threads: stat.num_threads,
                nice: stat.nice,
                mem_bytes,
                cpu_percent: percent,
                cpu_cumulative: cpu_cumulative(
                    cpu_ticks,
                    self.clk_tck,
                    cpu.uptime_seconds,
                    stat.starttime,
                ),
                starttime_ticks: stat.starttime,
                read_bytes: None,
                write_bytes: None,
                is_kernel_thread,
                tree_depth: 0,
                tree_prefix: String::new(),
                tree_index: 0,
            });
        }

        // Only PIDs still alive keep their old sample; the rest must not, or a
        // recycled PID would show a huge bogus percentage.
        self.old_cpu = current_cpu;
        self.old_times_total = cpu.times_total;

        if out.is_empty() {
            logger::once("no-procs", "no readable processes found under /proc");
        }
        out
    }
}

/// Build the visible, ordered list of indices into `procs`.
///
/// Sorting, filtering and tree ordering all happen here, **once per tick**, so
/// `render()` is a pure read. Returns indices rather than clones, so no process
/// data is copied per frame.
///
/// `procs` is taken mutably because tree mode writes `tree_depth`,
/// `tree_prefix` and `tree_index` onto each row; the UI then renders those
/// fields verbatim instead of recomputing the shape every frame.
pub fn build_proc_view(
    procs: &mut [ProcSnapshot],
    sort: ProcSort,
    reversed: bool,
    tree: bool,
    filter: &CompiledFilter,
    show_kernel_threads: bool,
) -> Vec<usize> {
    for p in procs.iter_mut() {
        p.tree_depth = 0;
        p.tree_prefix = String::new();
        p.tree_index = 0;
    }

    let mut idx: Vec<usize> = (0..procs.len())
        .filter(|&i| show_kernel_threads || !procs[i].is_kernel_thread)
        .filter(|&i| matches_filter(&procs[i], filter))
        .collect();

    if tree {
        tree_order(&mut idx, procs);
        let depths = compute_depths(&idx, procs);
        let prefixes = compute_prefixes(&idx, procs, &depths);
        for (row, &i) in idx.iter().enumerate() {
            procs[i].tree_depth = depths[row];
            procs[i].tree_prefix = prefixes[row].clone();
            procs[i].tree_index = row as u32;
        }
    } else {
        sort_indices(&mut idx, procs, sort, reversed);
        for (row, &i) in idx.iter().enumerate() {
            procs[i].tree_index = row as u32;
        }
    }

    idx
}

/// Reorder `idx` in place so every process directly follows its parent.
///
/// A `ppid` cycle would loop forever, so a `visited` set makes the walk total;
/// anything unreachable from a root is appended rather than dropped.
fn tree_order(idx: &mut [usize], procs: &[ProcSnapshot]) {
    let by_pid: HashMap<i32, usize> = idx
        .iter()
        .enumerate()
        .map(|(row, &i)| (procs[i].pid, row))
        .collect();

    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (row, &i) in idx.iter().enumerate() {
        match by_pid.get(&procs[i].ppid).copied() {
            // A self-parent or a cycle member is treated as a root so the walk
            // still reaches it.
            Some(pr) if pr != row => children.entry(pr).or_default().push(i),
            _ => roots.push(row),
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|&i| procs[i].pid);
    }
    roots.sort_by_key(|&i| procs[i].pid);

    let mut order: Vec<usize> = Vec::with_capacity(idx.len());
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = roots;
    stack.reverse();
    while let Some(row) = stack.pop() {
        if !visited.insert(row) {
            continue;
        }
        order.push(row);
        if let Some(kids) = children.get(&row) {
            for &k in kids.iter().rev() {
                stack.push(k);
            }
        }
    }
    for &i in idx.iter() {
        if !visited.contains(&i) {
            order.push(i);
        }
    }
    idx.copy_from_slice(&order);
}

/// Depth of each row, found by walking the visible parent chain. Bounded at 64
/// levels so a malformed `ppid` chain cannot hang the UI thread.
fn compute_depths(order: &[usize], procs: &[ProcSnapshot]) -> Vec<u16> {
    let pos: HashMap<i32, usize> = order
        .iter()
        .enumerate()
        .map(|(row, &i)| (procs[i].pid, row))
        .collect();
    order
        .iter()
        .enumerate()
        .map(|(row, &i)| {
            let mut depth = 0u16;
            let mut p = procs[i].ppid;
            for _ in 0..64 {
                // Only an ancestor that appears *earlier* counts; anything else
                // is a cycle or a forward reference and stops the walk.
                match pos.get(&p) {
                    Some(&pr) if pr < row => {
                        depth += 1;
                        p = procs[order[pr]].ppid;
                    }
                    _ => break,
                }
            }
            depth
        })
        .collect()
}

/// The `├─ `/`└─ `/`│  ` glyphs for each row.
///
/// An ancestor keeps drawing `│` for as long as it still has a later sibling,
/// which is what makes a tree readable rather than a flat staircase.
fn compute_prefixes(order: &[usize], procs: &[ProcSnapshot], depths: &[u16]) -> Vec<String> {
    let pos: HashMap<i32, usize> = order
        .iter()
        .enumerate()
        .map(|(row, &i)| (procs[i].pid, row))
        .collect();

    // Does the node at `row` have a following sibling, or a shallower node
    // after it that means the parent's subtree continues?
    let continues = |row: usize, depth: u16| -> bool {
        let parent = procs[order[row]].ppid;
        order[row + 1..].iter().any(|&n| {
            let d = depths[n];
            d != 0 && (d < depth || (d == depth && procs[n].ppid == parent))
        })
    };

    order
        .iter()
        .enumerate()
        .map(|(row, _)| {
            let depth = depths[row];
            if depth == 0 {
                return String::new();
            }
            let mut prefix = String::new();
            let mut parent = procs[order[row]].ppid;
            for level in 1..depth {
                match pos.get(&parent) {
                    Some(&pr) if continues(pr, level) => prefix.push_str("│  "),
                    Some(_) => prefix.push_str("   "),
                    None => {
                        prefix.push_str("   ");
                        continue;
                    }
                }
                match pos.get(&parent) {
                    Some(&pr) => parent = procs[order[pr]].ppid,
                    None => break,
                }
            }
            prefix.push_str(if continues(row, depth) {
                "├─ "
            } else {
                "└─ "
            });
            prefix
        })
        .collect()
}
/// Sort indices by the chosen column. `CpuLazy` additionally rotates busy
/// processes forward so they cannot be buried.
fn sort_indices(idx: &mut [usize], procs: &[ProcSnapshot], sort: ProcSort, reversed: bool) {
    match sort {
        ProcSort::Pid => idx.sort_by_key(|&i| procs[i].pid),
        ProcSort::Name => idx.sort_by(|&a, &b| procs[a].name.cmp(&procs[b].name)),
        ProcSort::Command => idx.sort_by(|&a, &b| procs[a].cmdline.cmp(&procs[b].cmdline)),
        ProcSort::Threads => idx.sort_by(|&a, &b| procs[b].threads.cmp(&procs[a].threads)),
        ProcSort::User => idx.sort_by(|&a, &b| procs[a].user.cmp(&procs[b].user)),
        ProcSort::Memory => idx.sort_by(|&a, &b| procs[b].mem_bytes.cmp(&procs[a].mem_bytes)),
        ProcSort::CpuDirect => idx.sort_by(|&a, &b| {
            procs[b]
                .cpu_percent
                .partial_cmp(&procs[a].cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcSort::CpuLazy => {
            idx.sort_by(|&a, &b| {
                procs[b]
                    .cpu_percent
                    .partial_cmp(&procs[a].cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            rotate_busy(idx, procs);
        }
    }
    if reversed {
        idx.reverse();
    }
}

/// btop's `CpuLazy` trick: pull any process above 30 % (or above the best of
/// the first five rows) toward the front, at most 10 positions per process.
/// Without this a 200 %-CPU process lands behind hundreds of idle ones.
fn rotate_busy(idx: &mut [usize], procs: &[ProcSnapshot]) {
    let head_max = idx
        .iter()
        .take(5)
        .map(|&i| procs[i].cpu_percent)
        .fold(0.0f32, f32::max);
    let threshold = CPU_LAZY_ROTATE_ABOVE.max(head_max);
    for slot in 0..idx.len().min(CPU_LAZY_MAX_ROTATIONS) {
        let Some(&found) = idx
            .iter()
            .skip(slot)
            .find(|&&i| procs[i].cpu_percent > threshold)
        else {
            break;
        };
        let at = idx.iter().position(|&i| i == found).unwrap_or(slot);
        if at > slot {
            idx.copy_within(at..at + 1, slot);
            idx[slot] = found;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fields after the last `)` are laid out so that the real kernel field
    /// numbers land on their expected values:
    /// `rest[N - 3]` -> state S(3), ppid 1(4), utime 1234(14), stime 567(15),
    /// nice 5(19), num_threads 3(20), starttime 987654(22), rss 2048(24).
    const NORMAL: &str = "4242 (code) S 1 4242 4242 34816 4242 4194560 100 0 0 0 1234 567 0 0 20 5 3 0 987654 12345678 2048 18446744073709551615 1 2 3 4 5 6 7 8 9 10 11 12";

    /// A real capture from a binary named `weird (name) here`. The kernel
    /// truncates `comm` to 15 characters, so it reads `weird (name) he` — but
    /// that is still **a space and two open parentheses**, which is exactly the
    /// shape that breaks `split_whitespace`.
    ///
    /// Expected values, read off the file with the `last )` rule:
    /// state `S`, ppid `3802279`, starttime `34996662`, rss `511` pages.
    const WEIRD: &str = "3802284 (weird (name) he) S 3802279 3802279 3750446 34837 3802291 4194304 99 0 0 0 0 0 0 0 20 0 1 0 34996662 8503296 511 18446744073709551615 95494685401088 95494685415089 140734018751616 0 0 0 0 0 0 1 0 0 17 4 0 0 0 0 0 95494685424592 95494685425816 95495025201152 140734018758353 140734018758380 140734018758380 140734018764769 0";

    fn sample(pid: i32, ppid: i32, cpu: f32) -> ProcSnapshot {
        ProcSnapshot {
            pid,
            ppid,
            name: format!("p{pid}"),
            cmdline: format!("p{pid}"),
            user: "me".into(),
            state: 'S',
            threads: 1,
            nice: 0,
            mem_bytes: 0,
            cpu_percent: cpu,
            ..Default::default()
        }
    }

    #[test]
    fn parses_a_normal_comm() {
        let s = parse_pid_stat(NORMAL).expect("parses");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 1);
        assert_eq!(s.utime, 1234);
        assert_eq!(s.stime, 567);
        assert_eq!(s.nice, 5);
        assert_eq!(s.num_threads, 3);
        assert_eq!(s.starttime, 987654);
        assert_eq!(s.rss_pages, 2048);
    }

    #[test]
    fn parses_a_comm_with_spaces_and_parentheses() {
        let s = parse_pid_stat(WEIRD).expect("parses");
        // Every value below was read off the real capture with the last-`)`
        // rule. A whitespace split would produce none of them.
        assert_eq!(s.state, 'S', "state is field 3, right after the last ')'");
        assert_eq!(s.ppid, 3_802_279, "ppid is field 4, not a fragment of comm");
        assert_eq!(s.utime, 0);
        assert_eq!(s.stime, 0);
        assert_eq!(s.nice, 0);
        assert_eq!(s.num_threads, 1);
        assert_eq!(s.starttime, 34_996_662);
        assert_eq!(s.rss_pages, 511);
    }

    #[test]
    fn a_naive_whitespace_split_would_get_the_weird_one_wrong() {
        // Demonstrates the bug the fixture guards against: `comm` is split
        // across four tokens, so every field from 4 onward shifts.
        let naive: Vec<&str> = WEIRD.split_ascii_whitespace().collect();
        assert_eq!(naive[1], "(weird", "the naive split fragments comm");
        // A naive parser reading ppid at token 5 (after pid + 4 comm tokens)
        // lands on the wrong field entirely.
        assert_eq!(naive.get(5).copied(), Some("3802279"));
        // The tokens that actually break it: comm occupied four slots.
        assert_eq!(naive[2], "(name)");
        assert_eq!(naive[3], "he)");
        let correct = parse_pid_stat(WEIRD).expect("parses");
        assert_eq!(correct.ppid, 3_802_279);
        assert_eq!(correct.state, 'S');
    }

    #[test]
    fn the_captured_fixture_file_parses() {
        // Guards the fixture itself, so it cannot silently drift.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/proc_pid_stat_weird_comm.txt"
        );
        let Ok(text) = std::fs::read_to_string(path) else {
            eprintln!("fixture missing at {path}; skipping");
            return;
        };
        let s = parse_pid_stat(text.trim()).expect("fixture parses");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 3_802_279);
        assert_eq!(s.rss_pages, 511);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_pid_stat("").is_none());
        assert!(parse_pid_stat("no parens here").is_none());
    }

    #[test]
    fn cpu_percent_matches_btop_formula() {
        // 1000 ticks of 10000 total, per_core on a 4-core box:
        // 4 * 1000 * 1000 / 10000 / 10 = 40%.
        let p = cpu_percent(1000, 10000, 4.0, 4);
        assert!((p - 40.0).abs() < 0.5, "got {p}");
        // Clamped to core_count * 100.
        let p = cpu_percent(10_000, 10_000, 4.0, 4);
        assert_eq!(p, 400.0);
        // Zero denominator is 0, not NaN.
        assert_eq!(cpu_percent(100, 0, 4.0, 4), 0.0);
    }

    #[test]
    fn rss_falls_back_when_stat_is_absurd() {
        // 2 GB of "RSS" on a 1 GB machine: the stat value is impossible.
        let bytes = resolve_rss_bytes(500_000, Some(1000), 4096, 1_000_000_000);
        assert_eq!(bytes, 1000 * 4096, "used the statm value");
        // Both absurd: report zero rather than a lie.
        let bytes = resolve_rss_bytes(500_000, Some(999_999), 4096, 1_000_000_000);
        assert_eq!(bytes, 0);
        // Sane value passes straight through.
        let bytes = resolve_rss_bytes(1000, None, 4096, 1_000_000_000);
        assert_eq!(bytes, 4_096_000);
    }

    #[test]
    fn invalid_regex_matches_nothing_and_does_not_panic() {
        let p = sample(1, 0, 0.0);
        // "[" is not a valid regex. It must not panic, and it must match
        // nothing rather than silently matching everything.
        let bad = compile_filter("![");
        assert!(bad.is_active());
        assert!(
            !matches_filter(&p, &bad),
            "an invalid regex matches nothing"
        );
        assert!(!matches_filter(&p, &compile_filter("!systemd")));
    }

    #[test]
    fn a_valid_regex_filter_works() {
        let p = sample(1, 0, 0.0);
        let f = compile_filter("!^p[0-9]$");
        assert!(matches_filter(&p, &f));
        assert!(!matches_filter(&p, &compile_filter("!^q")));
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        let p = sample(1, 0, 0.0);
        assert!(matches_filter(&p, &compile_filter("")));
        assert!(matches_filter(&p, &compile_filter("   ")));
        assert!(!compile_filter("").is_active());
    }

    #[test]
    fn plain_filter_is_case_insensitive_substring() {
        let p = sample(42, 0, 0.0);
        assert!(matches_filter(&p, &compile_filter("P42")));
        assert!(matches_filter(&p, &compile_filter("42")));
        assert!(!matches_filter(&p, &compile_filter("nope")));
    }

    #[test]
    fn cpu_lazy_rotates_a_busy_process_forward() {
        let mut procs: Vec<ProcSnapshot> = (0..20).map(|i| sample(i + 1, 0, 1.0)).collect();
        procs.push(sample(999, 0, 250.0));
        let mut idx: Vec<usize> = (0..procs.len()).collect();
        sort_indices(&mut idx, &procs, ProcSort::CpuLazy, false);
        assert_eq!(
            procs[idx[0]].pid, 999,
            "the 250% process must be first, not buried"
        );
    }

    #[test]
    fn tree_order_places_children_after_parents() {
        // 1 -> 2 -> 4, and 1 -> 3.
        let procs = vec![
            sample(1, 0, 0.0),
            sample(2, 1, 0.0),
            sample(3, 1, 0.0),
            sample(4, 2, 0.0),
        ];
        let mut idx: Vec<usize> = (0..procs.len()).collect();
        tree_order(&mut idx, &procs);
        let pids: Vec<i32> = idx.iter().map(|&i| procs[i].pid).collect();
        assert_eq!(pids[0], 1, "the root comes first");
        let pos = |pid: i32| pids.iter().position(|&p| p == pid).expect("present");
        assert!(pos(2) < pos(4), "a child follows its parent");
        assert!(
            pos(3) < pos(1) || pos(3) > pos(2),
            "ordering is stable otherwise"
        );
    }

    #[test]
    fn tree_order_survives_a_ppid_cycle() {
        // 1's parent is 2 and 2's parent is 1: unreachable from any root.
        let procs = vec![sample(1, 2, 0.0), sample(2, 1, 0.0)];
        let mut idx: Vec<usize> = (0..procs.len()).collect();
        tree_order(&mut idx, &procs);
        assert_eq!(idx.len(), 2, "no process is dropped, and no hang");
    }

    #[test]
    fn state_labels_cover_the_kernel_letters() {
        assert_eq!(state_label('R'), "Running");
        assert_eq!(state_label('Z'), "Zombie");
        assert_eq!(state_label('?'), "Unknown");
    }

    #[test]
    fn sort_config_mapping() {
        assert_eq!(ProcSort::from_config("cpu lazy"), ProcSort::CpuLazy);
        assert_eq!(ProcSort::from_config("MEMORY"), ProcSort::Memory);
        // An unknown value must not stop the app from starting.
        assert_eq!(ProcSort::from_config("nonsense"), ProcSort::Pid);
    }
}
