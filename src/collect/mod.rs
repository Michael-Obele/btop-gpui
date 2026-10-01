//! The collector: one background thread that owns every read of `/proc` and
//! `/sys`, and hands finished [`Snapshot`]s to the UI thread.
//!
//! # Why a thread at all
//!
//! `statvfs()` on a stale NFS, CIFS or sshfs mount can block for **minutes**.
//! The upstream `system_monitor` example collects on the UI thread, which is
//! fine for `sysinfo`'s cheap calls but would freeze the window the moment a
//! network mount went bad. Everything here therefore runs off the UI thread, and
//! `render()` only ever reads an already-finished snapshot.
//!
//! # The hand-off
//!
//! The thread writes into an `Arc<Mutex<Option<Snapshot>>>` and bumps a
//! monotonic counter. The UI polls that counter on a timer and takes the value.
//! Waking an `Entity` from a foreign thread would need `AsyncApp::update` on a
//! captured `WeakEntity`; at a 2 s cadence the wasted timer wakeups are
//! irrelevant, so the better-understood poll wins. A counter rather than a bool
//! because the UI can miss an edge if two ticks land between two pulls.
//!
//! # `catch_unwind`
//!
//! Each tick is wrapped so one bad read on one tick cannot kill the process.
//! This is why `Cargo.toml` deliberately does **not** set `panic = "abort"`.

pub mod battery;
pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod mem;
pub mod net;
pub mod proc;
pub mod sysfs;
pub mod temp;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::logger;
use crate::model::Snapshot;

/// The shared cell the collector writes and the UI reads.
#[derive(Default)]
pub struct Shared {
    latest: Mutex<Option<Arc<Snapshot>>>,
    /// Incremented once per completed tick.
    counter: AtomicU64,
    /// Bumped when the caller wants a new config adopted.
    config_epoch: AtomicU64,
    config: Mutex<Config>,
}

impl Shared {
    /// Take the newest snapshot, but only if the collector produced one since
    /// the last call. `None` means nothing new arrived, so the UI can skip the
    /// re-render entirely.
    pub fn take_if_dirty(&self, last_seen: &mut u64) -> Option<Arc<Snapshot>> {
        let n = self.counter.load(Ordering::Acquire);
        if n == *last_seen {
            return None;
        }
        *last_seen = n;
        self.latest.lock().ok()?.take()
    }

    /// A copy of the current config. The UI reads it to render; the collector
    /// uses it to apply a change.
    pub fn config(&self) -> Config {
        self.config.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Ask the collector to adopt a new config at the start of its next tick.
    pub fn push_config(&self, cfg: Config) {
        if let Ok(mut guard) = self.config.lock() {
            *guard = cfg;
        }
        self.config_epoch.fetch_add(1, Ordering::Release);
    }
}

/// Owns every per-collector state. One instance lives for the life of the
/// thread; recreating it would discard the previous samples and make every rate
/// read as zero.
pub struct Collector {
    cpu: cpu::CpuCollector,
    mem: mem::MemCollector,
    disk: disk::DiskCollector,
    net: net::NetCollector,
    proc: proc::ProcCollector,
    battery: battery::BatteryCollector,
    /// Bumped when the UI pushes a config, so a change lands exactly once
    /// instead of every tick.
    applied_epoch: u64,
    shared: Arc<Shared>,
}

impl Collector {
    pub fn new(cfg: &Config, shared: Arc<Shared>) -> Self {
        let cpu = cpu::CpuCollector::new(cfg);
        let proc = proc::ProcCollector::new(cfg, cpu.clk_tck(), cpu.page_size());
        Self {
            cpu,
            mem: mem::MemCollector::new(cfg),
            disk: disk::DiskCollector::new(cfg),
            net: net::NetCollector::new(cfg),
            proc,
            battery: battery::BatteryCollector::new(cfg),
            applied_epoch: 0,
            shared,
        }
    }

    /// Re-read the config if the UI pushed a new one.
    fn maybe_reconfigure(&mut self) {
        let epoch = self.shared.config_epoch.load(Ordering::Acquire);
        if epoch == self.applied_epoch {
            return;
        }
        self.applied_epoch = epoch;
        let cfg = self.shared.config();
        // These are the collectors whose state is purely a policy choice, so
        // swapping them is safe. The CPU collector is deliberately *not*
        // replaced: its stored deltas define the meaning of every rate, and
        // resetting them mid-run would make the graph jump to zero.
        self.mem = mem::MemCollector::new(&cfg);
        self.net = net::NetCollector::new(&cfg);
        self.battery = battery::BatteryCollector::new(&cfg);
        logger::info("config applied to collectors");
    }

    /// One complete reading of the machine.
    pub fn tick(&mut self, now: Instant) -> Snapshot {
        self.maybe_reconfigure();

        // Order matters: the CPU collector must run before the process
        // collector, which needs `times_total` for its CPU% formula.
        let cpu = self.cpu.collect(now);
        let mem = self.mem.collect(now);
        let disks = self.disk.collect(now);
        let nets = self.net.collect(now);
        let procs = self.proc.collect(now, &cpu);
        let battery = self.battery.collect(now);

        Snapshot {
            at: now,
            cpu,
            mem,
            disks,
            nets,
            procs,
            battery,
        }
    }
}

/// Start the collector thread and return the shared cell.
///
/// The thread lives until the process exits; there is no shutdown path because
/// there is nothing to flush and the OS reclaims everything.
pub fn spawn(cfg: Config) -> Arc<Shared> {
    let shared = Arc::new(Shared {
        config: Mutex::new(cfg.clone()),
        ..Default::default()
    });

    let thread_shared = Arc::clone(&shared);
    let spawned = thread::Builder::new()
        .name("btop-collector".into())
        .spawn(move || {
            let mut collector = Collector::new(&cfg, Arc::clone(&thread_shared));

            loop {
                let start = Instant::now();

                // A panic inside one collector must not take down the app.
                // Each collector's state is independent, so the next tick just
                // starts from a slightly stale baseline.
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let snapshot = collector.tick(start);
                    if let Ok(mut guard) = thread_shared.latest.lock() {
                        *guard = Some(Arc::new(snapshot));
                    }
                    // Release, so the counter store cannot be reordered
                    // before the snapshot store it publishes.
                    thread_shared.counter.fetch_add(1, Ordering::Release);
                }));

                if result.is_err() {
                    logger::error("collector tick panicked; continuing");
                }

                // Pick up a changed `update_ms` without a restart. Re-read each
                // tick rather than tracking an epoch, so a config written by
                // the options dialog lands within one tick.
                let interval = thread_shared.config().update_interval();

                // Sleep for the remainder of the tick so the cadence does not
                // drift by however long collection took.
                let sleep = interval.saturating_sub(start.elapsed());
                thread::sleep(sleep.max(Duration::from_millis(1)));
            }
        });

    if spawned.is_err() {
        // The thread refused to spawn. The UI still runs; it simply never sees
        // a new snapshot, and the log says why.
        logger::error("could not spawn the collector thread");
    }
    shared
}
