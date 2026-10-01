//! GPU — the v2 seam. v1 never populates it (docs/03-data-layer.md §7).
//!
//! The structs live in `model.rs`, not here: the UI and the eventual v2
//! collector both consume them, and a second definition would let the two
//! drift. §7's snippet predates `model.rs` gaining the `index` field.
//!
//! v2's verified backends, in the fixed integration order NVML -> RSMI ->
//! amdgpu sysfs -> i915 perf events, with internal units of mW, bytes and MHz.

use crate::model::GpuSnapshot;

/// Always empty in v1. Kept as a real function so the call site does not change
/// when v2 lands, and so `GpuSnapshot` stays reachable from the data layer.
pub fn collect_gpus() -> Vec<GpuSnapshot> {
    Vec::new()
}
