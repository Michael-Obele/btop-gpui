//! Network: per-interface counters, addresses, and the auto-scaled graph
//! ceiling. Follows docs/03-data-layer.md §4.
//!
//! ## The graph ceiling lives here, not in the UI
//!
//! btop's net graph is only readable because the ceiling tracks traffic
//! instead of being pinned at some fixed maximum. That is stateful hysteresis
//! over the last few samples ([`GraphScaler`]), so it has to run on the
//! collector. The UI reads [`NetSnapshot::graph_max_bps`] and does nothing.
//!
//! ## Why `getifaddrs` is called through libc and not nix
//!
//! `Cargo.toml` enables `nix`'s `fs, process, resource, signal, user` — not
//! `net`, which is the feature that gates `nix::ifaddrs`, and `Cargo.lock`
//! shows nothing else in the graph depends on `nix`, so no feature unification
//! provides it. This is the same `getifaddrs(3)` call `nix` wraps, with the
//! free owned by [`IfAddrs::drop`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr;
use std::time::Instant;

use crate::collect::sysfs;
use crate::config::Config;
use crate::logger;
use crate::model::NetSnapshot;

const NET_DIR: &str = "/sys/class/net";

/// Samples the auto-scaler averages over. btop's "last 5".
pub const GRAPH_SAMPLES: usize = 5;
/// Consecutive over-ceiling samples before the ceiling is recomputed. btop's 5.
pub const GRAPH_OVER_RUNS: u32 = 5;
/// Never scale below 10 KiB/s, or an idle link renders as noise.
pub const GRAPH_FLOOR_BPS: f64 = 10.0 * 1024.0;
/// Headroom applied when traffic grew gradually...
pub const GRAPH_GROWTH_GRADUAL: f64 = 1.3;
/// ...and when it dropped suddenly and we are scaling back down.
pub const GRAPH_GROWTH_SUDDEN: f64 = 3.0;
/// A delta larger than this is a counter reset, not traffic. 2^60 bytes is
/// 16 exabytes; at 100 Gbit/s that is ~400 years.
pub const MAX_PLAUSIBLE_DELTA: u64 = 1u64 << 60;

/// btop's auto-scale rule, as a pure state machine so it can be tested without
/// a network interface.
///
/// * A sample above the ceiling increments a strike counter; `GRAPH_OVER_RUNS`
///   consecutive strikes mean traffic grew, so recompute with a *small* 1.3x
///   headroom (adding more would make the graph permanently flat).
/// * A sample below a tenth of the ceiling means the ceiling is now far too
///   high, so recompute immediately with a *large* 3.0x headroom, since we are
///   scaling down and being generous costs nothing.
pub struct GraphScaler {
    max: f64,
    samples: VecDeque<f64>,
    over_runs: u32,
}

impl GraphScaler {
    pub fn new(initial_bps: f64) -> Self {
        Self {
            max: initial_bps.max(GRAPH_FLOOR_BPS),
            samples: VecDeque::with_capacity(GRAPH_SAMPLES),
            over_runs: 0,
        }
    }

    pub fn max_bps(&self) -> f64 {
        self.max
    }

    fn average(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    /// Feed one sample; returns the ceiling to draw this tick against.
    ///
    /// A non-finite sample is **discarded**, not clamped to zero. Treating NaN
    /// as a real zero reading would collapse the graph's ceiling every time a
    /// counter briefly went bad, which looks far worse than holding the old
    /// axis for one tick.
    pub fn push(&mut self, sample: f64) -> f64 {
        if !sample.is_finite() {
            return self.max;
        }
        let sample = sample.max(0.0);
        if self.samples.len() == GRAPH_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);

        if sample > self.max {
            self.over_runs += 1;
            if self.over_runs >= GRAPH_OVER_RUNS {
                self.max = (self.average() * GRAPH_GROWTH_GRADUAL).max(GRAPH_FLOOR_BPS);
                self.over_runs = 0;
            }
        } else if sample < self.max / 10.0 {
            // A sudden collapse. Rescale now, with generous headroom.
            self.over_runs = 0;
            self.max = (self.average() * GRAPH_GROWTH_SUDDEN).max(GRAPH_FLOOR_BPS);
        } else {
            self.over_runs = 0;
        }
        self.max
    }
}

/// Interface list from `getifaddrs(3)`.
///
/// The returned pointer owns a linked list that must be released exactly once;
/// the [`Drop`] impl is what makes that hard to get wrong.
struct IfAddrs(*mut libc::ifaddrs);

impl Drop for IfAddrs {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: non-null, came from `getifaddrs`, and `Drop` runs once.
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
}

fn get_ifaddrs() -> Option<IfAddrs> {
    let mut head: *mut libc::ifaddrs = ptr::null_mut();
    // SAFETY: `getifaddrs` writes a valid head pointer through the out-param on
    // success (0) and leaves it untouched on failure. Ownership moves into
    // `IfAddrs`, which frees it in `Drop`.
    let rc = unsafe { libc::getifaddrs(&mut head) };
    if rc != 0 || head.is_null() {
        None
    } else {
        Some(IfAddrs(head))
    }
}

/// The address on a `sockaddr`, or `None` for every non-INET family.
///
/// Filtering by family is how `AF_PACKET` is ignored, as the spec requires:
/// those entries exist for `tcpdump` and carry a link-layer address that would
/// otherwise be rendered as an IP.
fn sockaddr_to_ip(sa: &libc::sockaddr) -> Option<IpAddr> {
    match sa.sa_family as i32 {
        f if f == libc::AF_INET => {
            // SAFETY: sa_family == AF_INET, so the storage really is a
            // `sockaddr_in`; it is at least as large and correctly aligned.
            let sin = unsafe { &*(ptr::from_ref(sa).cast::<libc::sockaddr_in>()) };
            // `s_addr` is network byte order; `from_be` normalises to host.
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                sin.sin_addr.s_addr,
            ))))
        }
        f if f == libc::AF_INET6 => {
            // SAFETY: as above, for AF_INET6.
            let sin6 = unsafe { &*(ptr::from_ref(sa).cast::<libc::sockaddr_in6>()) };
            Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
struct IfaceInfo {
    ipv4: Option<String>,
    ipv6: Option<String>,
    connected: bool,
}

/// Every interface, with its addresses and link state.
fn read_interfaces() -> BTreeMap<String, IfaceInfo> {
    let mut out: BTreeMap<String, IfaceInfo> = BTreeMap::new();

    match get_ifaddrs() {
        Some(list) => {
            let mut cursor = list.0;
            while !cursor.is_null() {
                // SAFETY: `cursor` is a node of the live list owned by `list`,
                // which outlives this loop.
                let node = unsafe { &*cursor };
                let name = unsafe { CStr::from_ptr(node.ifa_name) }
                    .to_str()
                    .unwrap_or_default()
                    .to_string();
                let running = node.ifa_flags & (libc::IFF_RUNNING as u32) != 0;
                let ip = if node.ifa_addr.is_null() {
                    None
                } else {
                    // SAFETY: non-null, owned by the ifaddrs list.
                    sockaddr_to_ip(unsafe { &*node.ifa_addr })
                };

                if !name.is_empty() {
                    let entry = out.entry(name).or_default();
                    entry.connected |= running;
                    match ip {
                        Some(IpAddr::V4(v4)) if entry.ipv4.is_none() => {
                            entry.ipv4 = Some(v4.to_string())
                        }
                        Some(IpAddr::V6(v6)) if entry.ipv6.is_none() => {
                            entry.ipv6 = Some(v6.to_string())
                        }
                        _ => {}
                    }
                }
                cursor = node.ifa_next;
            }
        }
        None => {
            logger::once(
                "getifaddrs",
                "getifaddrs failed; falling back to sysfs only",
            );
        }
    }

    // `getifaddrs` yields one entry per *address*, so an interface with no
    // address (a downed NIC) never appears. sysfs is authoritative for the
    // name list, so the union is what the user expects to see.
    for path in sysfs::list_dir(NET_DIR) {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let entry = out.entry(name.to_string()).or_default();
        if entry.connected {
            continue;
        }
        // `/sys/class/net/<if>/operstate` is the sysfs view of the same link
        // state; "up" is the only value that means carrying traffic.
        if sysfs::read_str(path.join("operstate")).as_deref() == Some("up") {
            entry.connected = true;
        }
    }

    out
}

/// Remove `lo` unless it is the only interface there is.
pub fn without_loopback(mut names: Vec<String>) -> Vec<String> {
    if names.len() <= 1 {
        return names;
    }
    names.retain(|n| n != "lo");
    names
}

/// Cumulative rx/tx for one interface, read once per tick.
#[derive(Debug, Clone, Copy, Default)]
struct Counters {
    rx: u64,
    tx: u64,
}

/// Bytes/sec from a counter delta, rejecting resets and wraps.
///
/// `u64` byte counters genuinely wrap on very long uptimes and in some VMs, so
/// the subtraction is wrapping. A result above [`MAX_PLAUSIBLE_DELTA`] is not
/// traffic, it is a counter that went backwards — report 0 rather than a spike
/// that would wreck the graph's scale.
pub fn counter_rate(cur: u64, old: u64, dt: f64) -> f64 {
    let delta = cur.wrapping_sub(old);
    if delta > MAX_PLAUSIBLE_DELTA {
        return 0.0;
    }
    (delta as f64 / dt).max(0.0)
}

/// Pick the interface to graph when `net_iface` is `Auto`.
///
/// `connected` is the primary key: an unplugged dock or VPN interface can hold
/// a lifetime of bytes while carrying nothing now. Ties break on total
/// down+up, then on name, so the choice is stable across ticks rather than
/// flickering.
pub fn select_auto(snaps: &[NetSnapshot]) -> Option<String> {
    snaps
        .iter()
        .max_by(|a, b| {
            a.connected
                .cmp(&b.connected)
                .then_with(|| {
                    a.total_download_bytes
                        .saturating_add(a.total_upload_bytes)
                        .cmp(&b.total_download_bytes.saturating_add(b.total_upload_bytes))
                })
                .then_with(|| a.name.cmp(&b.name))
        })
        .map(|s| s.name.clone())
}

pub struct NetCollector {
    /// `net_iface`, or `"Auto"`.
    iface: String,
    net_auto: bool,
    /// Mirror the upload scale onto the download scale.
    net_sync: bool,
    /// `net_download` in Mibibits -> bytes/sec, used when `net_auto` is off.
    fixed_download_bps: f64,
    fixed_upload_bps: f64,
    scaler: GraphScaler,
    old: HashMap<String, Counters>,
    old_ts: Option<Instant>,
}

/// Mibibits/s to bytes/s.
fn mibits_to_bps(mibits: i64) -> f64 {
    let v = mibits.clamp(0, 1 << 20) as f64;
    v * 1024.0 * 1024.0 / 8.0
}

impl NetCollector {
    pub fn new(cfg: &Config) -> Self {
        Self {
            iface: cfg.str("net_iface"),
            net_auto: cfg.bool_or("net_auto", true),
            net_sync: cfg.bool("net_sync"),
            fixed_download_bps: mibits_to_bps(cfg.int("net_download")),
            fixed_upload_bps: mibits_to_bps(cfg.int("net_upload")),
            // Seed at the configured ceiling so the first tick has a sane scale.
            scaler: GraphScaler::new(mibits_to_bps(cfg.int("net_download"))),
            old: HashMap::new(),
            old_ts: None,
        }
    }

    /// The interface currently being graphed, or `None` on a machine with none.
    pub fn selected_iface(&self) -> Option<&str> {
        if self.iface.is_empty() || self.iface.eq_ignore_ascii_case("auto") {
            None
        } else {
            Some(self.iface.as_str())
        }
    }

    pub fn collect(&mut self, now: Instant) -> Vec<NetSnapshot> {
        let interfaces = read_interfaces();

        // `lo` reports IFF_RUNNING and would otherwise be a permanent row of
        // loopback traffic, so drop it unless there is literally nothing else.
        let names = without_loopback(interfaces.keys().cloned().collect());
        if names.is_empty() {
            return Vec::new();
        }

        let dt = self
            .old_ts
            .and_then(|t| now.checked_duration_since(t))
            .unwrap_or_default()
            .as_secs_f64()
            .max(0.001);

        let mut out: Vec<NetSnapshot> = Vec::with_capacity(names.len());
        let mut current: HashMap<String, Counters> = HashMap::with_capacity(names.len());

        for name in names {
            let info = interfaces.get(&name);
            // Each statistics file is read exactly once this tick.
            let counters = Counters {
                rx: sysfs::read_u64(format!("{NET_DIR}/{name}/statistics/rx_bytes")).unwrap_or(0),
                tx: sysfs::read_u64(format!("{NET_DIR}/{name}/statistics/tx_bytes")).unwrap_or(0),
            };

            let download = counter_rate(counters.rx, self.old.get(&name).map_or(0, |c| c.rx), dt);
            let upload = counter_rate(counters.tx, self.old.get(&name).map_or(0, |c| c.tx), dt);

            let ipv4 = info.and_then(|i| i.ipv4.clone());
            let ipv6 = info.and_then(|i| i.ipv6.clone());
            // Spec: only fall back to the MAC when there is no address at all.
            let mac = if ipv4.is_none() && ipv6.is_none() {
                sysfs::read_str(format!("{NET_DIR}/{name}/address"))
            } else {
                None
            };

            out.push(NetSnapshot {
                name: name.clone(),
                connected: info.is_some_and(|i| i.connected),
                ip: ipv4.or(ipv6),
                mac,
                download_bytes_per_sec: download,
                upload_bytes_per_sec: upload,
                total_download_bytes: counters.rx,
                total_upload_bytes: counters.tx,
                graph_max_bps: 0.0, // filled in below, once the scale is settled.
            });
            current.insert(name, counters);
        }

        self.old = current;
        self.old_ts = Some(now);

        // With `net_sync` the single ceiling has to fit both directions, since
        // the model carries one `graph_max_bps` for the whole graph. Without
        // it, the ceiling tracks download only, matching btop's primary axis.
        let chosen = self
            .selected_iface()
            .map(str::to_string)
            .or_else(|| select_auto(&out));
        let ceiling = self.apply_scale(&out, chosen.as_deref());

        for snap in &mut out {
            snap.graph_max_bps = ceiling;
        }
        out
    }

    /// The one place the graph ceiling is decided.
    fn apply_scale(&mut self, out: &[NetSnapshot], chosen: Option<&str>) -> f64 {
        if !self.net_auto {
            // Fixed ceilings: the larger of the two so neither axis clips.
            return self.fixed_download_bps.max(self.fixed_upload_bps);
        }
        let Some(chosen) = chosen.and_then(|n| out.iter().find(|s| s.name == n)) else {
            return self.scaler.max_bps();
        };
        let sample = if self.net_sync {
            chosen
                .download_bytes_per_sec
                .max(chosen.upload_bytes_per_sec)
        } else {
            chosen.download_bytes_per_sec
        };
        // The scaler carries hysteresis, so it must be mutated in place. It is
        // taken out and put straight back rather than living behind a
        // `RefCell`, which would make the hot path pay a borrow check.
        let mut scaler = std::mem::replace(&mut self.scaler, GraphScaler::new(GRAPH_FLOOR_BPS));
        let max = scaler.push(sample);
        self.scaler = scaler;
        max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- graph_max auto-scale ----

    #[test]
    fn scaler_holds_the_ceiling_for_a_quiet_link() {
        let mut s = GraphScaler::new(1_000_000.0);
        // Comfortably inside the ceiling: no rescale at all.
        for _ in 0..20 {
            assert_eq!(s.push(500_000.0), 1_000_000.0);
        }
    }

    #[test]
    fn scaler_grows_only_after_five_over_runs() {
        let mut s = GraphScaler::new(1_000_000.0);
        // Four strikes: the ceiling must not move yet.
        for _ in 0..(GRAPH_OVER_RUNS - 1) {
            assert_eq!(s.push(2_000_000.0), 1_000_000.0);
        }
        // The fifth triggers a recompute. The 5-sample window is now all
        // 2 MB/s, so the ceiling becomes avg * 1.3 = 2.6 MB/s.
        let grown = s.push(2_000_000.0);
        let expected = 2_000_000.0 * GRAPH_GROWTH_GRADUAL;
        assert!(
            (grown - expected).abs() < 1.0,
            "expected {expected}, got {grown}"
        );
        assert!(grown > 1_000_000.0, "the ceiling must have grown");
    }

    #[test]
    fn a_strike_run_is_reset_by_one_good_sample() {
        let mut s = GraphScaler::new(1_000_000.0);
        for _ in 0..(GRAPH_OVER_RUNS - 1) {
            s.push(2_000_000.0);
        }
        // One in-band sample clears the counter, so the next burst starts over
        // and does not reach the five-strike threshold. It must be *inside* the
        // current ceiling, or it would trigger the sudden-drop branch instead.
        s.push(500_000.0);
        let ceiling_before = s.max_bps();
        assert_eq!(
            s.push(2_000_000.0),
            ceiling_before,
            "the run restarted, so this strike only counts as 1 of 5"
        );
    }

    #[test]
    fn scaler_drops_immediately_on_a_sudden_collapse() {
        let mut s = GraphScaler::new(100_000_000.0);
        // Below a tenth of the ceiling: rescale on this very sample, with the
        // generous 3.0x headroom rather than 1.3x. The sample must clear the
        // floor, or the floor would win instead.
        let after = s.push(100_000.0);
        let expected = (100_000.0 * GRAPH_GROWTH_SUDDEN).max(GRAPH_FLOOR_BPS);
        assert!(after < 100_000_000.0, "ceiling must come down, got {after}");
        assert!(
            (after - expected).abs() < 1.0,
            "got {after}, want {expected}"
        );
    }

    #[test]
    fn scaler_never_drops_below_the_floor() {
        let mut s = GraphScaler::new(1_000_000_000.0);
        // Silence would compute 0; the floor keeps the graph readable.
        for _ in 0..10 {
            s.push(0.0);
        }
        assert_eq!(s.max_bps(), GRAPH_FLOOR_BPS);
    }

    #[test]
    fn scaler_ignores_nonsense_samples() {
        let mut s = GraphScaler::new(1_000_000.0);
        let before = s.max_bps();
        // Non-finite samples are discarded entirely, so the ceiling holds.
        assert_eq!(s.push(f64::NAN), before);
        assert_eq!(s.push(f64::INFINITY), before);
        assert_eq!(s.max_bps(), before);
        // A negative sample is nonsense too, but it clamps to zero, which is a
        // legitimate reading of "no traffic" and may legitimately drop the axis.
        assert!(s.push(-1.0).is_finite());
        assert!(s.max_bps() >= GRAPH_FLOOR_BPS);
    }

    #[test]
    fn scaler_averages_only_the_last_five_samples() {
        let mut s = GraphScaler::new(1_000_000.0);
        // 5 huge samples fill the window and trip the growth branch.
        for _ in 0..GRAPH_SAMPLES {
            s.push(1_000_000_000.0);
        }
        let grown = s.max_bps();
        assert!(grown > 1_000_000.0);
        // Now age them out with zeros. The ceiling must come down, and the
        // recomputed value uses the *current* window, not the old samples.
        for _ in 0..GRAPH_SAMPLES {
            s.push(0.0);
        }
        assert_eq!(s.max_bps(), GRAPH_FLOOR_BPS, "silence floors the ceiling");
        assert_eq!(s.samples.len(), GRAPH_SAMPLES);
    }

    // ---- wrap rejection ----

    #[test]
    fn counter_delta_rejects_a_wrap() {
        // A counter that went backwards yields a huge wrapped delta, which must
        // be rejected rather than divided by dt.
        // 5 - (u64::MAX - 2) wraps to 8, which is *below* the threshold, so the
        // real signature of a reset is the huge number: from a large old value.
        assert_eq!(counter_rate(5, 1_000_000, 1.0), 0.0);
        // A genuine wrap at the top of the range produces a small delta and is
        // real traffic, so it is kept. `wrapping_sub` is modular, so the delta
        // from u64::MAX-1000 to 2001 is (2001 + 1000 + 1) = 3002.
        assert_eq!(counter_rate(2_001, u64::MAX - 1_000, 1.0), 3_002.0);
        // Exactly at the threshold is still accepted.
        assert_eq!(
            counter_rate(MAX_PLAUSIBLE_DELTA, 0, 1.0),
            MAX_PLAUSIBLE_DELTA as f64
        );
    }

    #[test]
    fn counter_rate_is_plain_bytes_over_time() {
        assert_eq!(counter_rate(2_000, 1_000, 2.0), 500.0);
        assert_eq!(counter_rate(0, 0, 1.0), 0.0);
        // A pathological dt must not produce infinity.
        assert!(counter_rate(u64::MAX / 2, 0, 0.000_001).is_finite());
    }

    // ---- interface selection ----

    fn snap(name: &str, connected: bool, down: u64, up: u64) -> NetSnapshot {
        NetSnapshot {
            name: name.to_string(),
            connected,
            total_download_bytes: down,
            total_upload_bytes: up,
            ..Default::default()
        }
    }

    #[test]
    fn auto_prefers_connected_over_volume() {
        let snaps = vec![
            snap("eth0", false, 9_000_000, 9_000_000),
            snap("wlan0", true, 1_000, 1_000),
        ];
        assert_eq!(select_auto(&snaps).as_deref(), Some("wlan0"));
    }

    #[test]
    fn auto_breaks_ties_on_total_then_name() {
        let snaps = vec![snap("b", true, 5_000, 5_000), snap("a", true, 5_000, 5_000)];
        // Deterministic, so the graph does not swap interfaces between ticks.
        assert_eq!(select_auto(&snaps).as_deref(), Some("b"));
    }

    #[test]
    fn loopback_is_dropped_unless_it_is_all_there_is() {
        assert_eq!(
            without_loopback(vec!["lo".into(), "eth0".into()]),
            vec!["eth0"]
        );
        assert_eq!(without_loopback(vec!["lo".into()]), vec!["lo"]);
        assert_eq!(without_loopback(vec![]), Vec::<String>::new());
    }

    #[test]
    fn mibit_conversion() {
        assert_eq!(mibits_to_bps(100), 100.0 * 1024.0 * 1024.0 / 8.0);
        assert_eq!(mibits_to_bps(0), 0.0);
        // A garbage config value cannot produce a negative or absurd ceiling.
        assert_eq!(mibits_to_bps(-5), 0.0);
        assert!(mibits_to_bps(i64::MAX).is_finite());
    }
}
