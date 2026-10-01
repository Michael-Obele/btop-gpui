//! Graph history: one fixed-capacity ring buffer per plotted metric.
//!
//! This is the only part of btop's graphing model we keep. btop's rule — the
//! deques hold `graph_width * 2` samples (two samples per rendered column) —
//! is preserved exactly, because it is what bounds memory on a maximised 4K
//! window while still giving the chart enough resolution.

use std::collections::{HashMap, VecDeque};

/// A fixed-capacity ring. Never allocates a zero-length buffer: callers can
/// legitimately pass a width they do not know yet.
#[derive(Debug, Clone)]
pub struct Ring<T> {
    data: VecDeque<T>,
    cap: usize,
}

impl<T> Ring<T> {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            data: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Resize, dropping the oldest samples if the new capacity is smaller.
    pub fn set_capacity(&mut self, cap: usize) {
        self.cap = cap.max(1);
        while self.data.len() > self.cap {
            self.data.pop_front();
        }
    }

    pub fn push(&mut self, v: T) {
        while self.data.len() >= self.cap {
            self.data.pop_front();
        }
        self.data.push_back(v);
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn last(&self) -> Option<&T> {
        self.data.back()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.data.iter()
    }

    /// Charts take an owned `Vec`; this is the single place that allocation
    /// happens, and it happens once per tick in `AppView::pull`, never in
    /// `render`.
    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.data.iter().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.data.clear();
    }
}

/// Clamp a graph width into a range that cannot blow up memory on a 4K display.
pub fn clamp_columns(cols: usize) -> usize {
    cols.clamp(64, 2048)
}

/// Everything the UI plots. Owned by `AppView`, pushed to once per tick.
#[derive(Debug, Clone)]
pub struct History {
    pub cpu_total: Ring<f32>,
    /// One ring per entry of `CPU_FIELD_NAMES`.
    pub cpu_fields: Vec<Ring<f32>>,
    /// One ring per logical CPU. Capped at `PER_CORE_CAP` (40) — btop's number.
    pub cpu_cores: Vec<Ring<f32>>,
    pub cpu_temps: Vec<Ring<f32>>,
    pub mem_used: Ring<f32>,
    pub swap_used: Ring<f32>,
    pub net_down: HashMap<String, Ring<f32>>,
    pub net_up: HashMap<String, Ring<f32>>,
    pub disk_read: HashMap<String, Ring<f32>>,
    pub disk_write: HashMap<String, Ring<f32>>,
}

/// Per-core rings keep 40 samples regardless of window width. With 256 logical
/// CPUs, `width * 2` per core would be a lot of memory for data nobody reads.
pub const PER_CORE_CAP: usize = 40;

impl Default for History {
    /// An empty history. `mem::take` in `pull()` needs this; the rings are
    /// replaced wholesale on the next tick anyway.
    fn default() -> Self {
        Self::new(120)
    }
}

impl History {
    pub fn new(cols: usize) -> Self {
        let cap = clamp_columns(cols) * 2;
        Self {
            cpu_total: Ring::new(cap),
            cpu_fields: Vec::new(),
            cpu_cores: Vec::new(),
            cpu_temps: Vec::new(),
            mem_used: Ring::new(cap),
            swap_used: Ring::new(cap),
            net_down: HashMap::new(),
            net_up: HashMap::new(),
            disk_read: HashMap::new(),
            disk_write: HashMap::new(),
        }
    }

    /// Called from the resize path once the graph width in columns is known.
    pub fn set_columns(&mut self, cols: usize) {
        let cap = clamp_columns(cols) * 2;
        self.cpu_total.set_capacity(cap);
        for r in &mut self.cpu_fields {
            r.set_capacity(cap);
        }
        for r in &mut self.cpu_cores {
            r.set_capacity(PER_CORE_CAP);
        }
        for r in &mut self.cpu_temps {
            r.set_capacity(PER_CORE_CAP);
        }
        self.mem_used.set_capacity(cap);
        self.swap_used.set_capacity(cap);
        for r in self.net_down.values_mut() {
            r.set_capacity(cap);
        }
        for r in self.net_up.values_mut() {
            r.set_capacity(cap);
        }
        for r in self.disk_read.values_mut() {
            r.set_capacity(cap);
        }
        for r in self.disk_write.values_mut() {
            r.set_capacity(cap);
        }
    }

    /// Grow the per-core rings to match the CPU count. Called each tick so a
    /// hotplugged CPU gets a ring without a restart.
    pub fn ensure_cores(&mut self, count: usize) {
        if self.cpu_cores.len() < count {
            self.cpu_cores
                .resize_with(count, || Ring::new(PER_CORE_CAP));
            self.cpu_temps
                .resize_with(count, || Ring::new(PER_CORE_CAP));
        }
    }

    pub fn ensure_fields(&mut self, count: usize, cols: usize) {
        let cap = clamp_columns(cols) * 2;
        if self.cpu_fields.len() < count {
            self.cpu_fields.resize_with(count, || Ring::new(cap));
        }
    }

    pub fn ring_for<'a>(
        map: &'a mut HashMap<String, Ring<f32>>,
        key: &str,
        cols: usize,
    ) -> &'a mut Ring<f32> {
        let cap = clamp_columns(cols) * 2;
        map.entry(key.to_string()).or_insert_with(|| Ring::new(cap))
    }

    /// Fold one snapshot into the graphs. Called **once per tick**, never from
    /// `render()`: this is where the allocation happens.
    ///
    /// The per-core and per-field rings are grown here rather than at startup
    /// so a CPU that appears later still gets a trace.
    pub fn push_snapshot(&mut self, snapshot: &crate::model::Snapshot, cols: usize) {
        let cap = clamp_columns(cols) * 2;

        self.cpu_total.push(snapshot.cpu.total_percent);
        self.ensure_fields(snapshot.cpu.fields_percent.len(), cols);
        for (ring, value) in self.cpu_fields.iter_mut().zip(&snapshot.cpu.fields_percent) {
            ring.push(*value);
        }

        self.ensure_cores(snapshot.cpu.cores.len());
        for (ring, core) in self.cpu_cores.iter_mut().zip(&snapshot.cpu.cores) {
            ring.push(core.percent);
        }
        for (ring, core) in self.cpu_temps.iter_mut().zip(&snapshot.cpu.cores) {
            // `None` is skipped rather than pushed as 0, so a missing sensor
            // does not read as an idling core.
            if let Some(c) = core.temp_c {
                ring.push(c);
            }
        }

        self.mem_used.push(snapshot.mem.used_percent);
        self.swap_used.push(snapshot.mem.swap_percent);

        for net in &snapshot.nets {
            Self::ring_for(&mut self.net_down, &net.name, cols)
                .push(net.download_bytes_per_sec as f32);
            Self::ring_for(&mut self.net_up, &net.name, cols)
                .push(net.upload_bytes_per_sec as f32);
        }
        for disk in &snapshot.disks {
            Self::ring_for(&mut self.disk_read, &disk.name, cols)
                .push(disk.read_bytes_per_sec as f32);
            Self::ring_for(&mut self.disk_write, &disk.name, cols)
                .push(disk.write_bytes_per_sec as f32);
        }

        // Keep the capacities honest if the window was resized.
        debug_assert!(self.cpu_total.capacity() >= cap || self.cpu_total.len() <= cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_capacity_drops_oldest() {
        let mut r: Ring<u32> = Ring::new(4);
        for i in 0..10 {
            r.push(i);
        }
        assert_eq!(r.len(), 4);
        assert_eq!(r.to_vec(), vec![6, 7, 8, 9]);
    }

    #[test]
    fn ring_never_has_zero_capacity() {
        let r: Ring<u8> = Ring::new(0);
        assert_eq!(r.capacity(), 1);
    }

    #[test]
    fn ring_shrink_drops_oldest() {
        let mut r: Ring<u32> = Ring::new(8);
        for i in 0..6 {
            r.push(i);
        }
        r.set_capacity(3);
        assert_eq!(r.to_vec(), vec![3, 4, 5]);
        r.set_capacity(10);
        assert_eq!(r.len(), 3, "growing must not resurrect dropped samples");
    }

    #[test]
    fn history_caps_per_core_at_forty() {
        let mut h = History::new(1000); // 1000 columns -> 2000 samples
        h.ensure_cores(2);
        for _ in 0..100 {
            h.cpu_cores[0].push(1.0);
        }
        assert_eq!(h.cpu_cores[0].len(), PER_CORE_CAP);
        assert_eq!(h.cpu_total.capacity(), 2000);
    }

    #[test]
    fn columns_are_clamped() {
        assert_eq!(clamp_columns(3), 64);
        assert_eq!(clamp_columns(5000), 2048);
        assert_eq!(clamp_columns(120), 120);
    }
}
