//! Per-worker instrumentation. Enable `stream-profile` for counters and stage
//! timers; normal builds compile hot-path measurement out entirely.
use h3o::{CellIndex, LatLng, Resolution};
use serde::Serialize;
#[cfg(feature = "stream-profile")]
use std::cell::Cell;

#[derive(Clone, Copy, Default, Debug, Serialize)]
pub struct WorkerProfile {
    pub h3_calls: u64,
    /// Native geographic projections executed, including guarded requests.
    pub native_projection_calls: u64,
    /// Integer index constructions avoided; projections still execute.
    pub native_cache_hits: u64,
    pub prefix_tests: u64,
    /// Scanline searches with at least one projected-rectangle certificate hit.
    pub span_skips: u64,
    /// Native quantizations and integer index constructions avoided. Every one
    /// of these pixels still executes its native geographic projection.
    pub skipped_pixels: u64,
    pub geometry_queries: u64,
    pub geometry_accepts: u64,
    pub geometry_proposed_center_pixels: u64,
    pub geometry_proposed_core_samples: u64,
    pub geometry_mismatches: u64,
    pub h3_ns: u64,
    pub transform_calls: u64,
    pub transform_ns: u64,
    pub copied_bytes: u64,
}
impl WorkerProfile {
    pub fn merge(&mut self, other: Self) {
        self.h3_calls += other.h3_calls;
        self.native_projection_calls += other.native_projection_calls;
        self.native_cache_hits += other.native_cache_hits;
        self.prefix_tests += other.prefix_tests;
        self.span_skips += other.span_skips;
        self.skipped_pixels += other.skipped_pixels;
        self.geometry_queries += other.geometry_queries;
        self.geometry_accepts += other.geometry_accepts;
        self.geometry_proposed_center_pixels += other.geometry_proposed_center_pixels;
        self.geometry_proposed_core_samples += other.geometry_proposed_core_samples;
        self.geometry_mismatches += other.geometry_mismatches;
        self.h3_ns += other.h3_ns;
        self.transform_calls += other.transform_calls;
        self.transform_ns += other.transform_ns;
        self.copied_bytes += other.copied_bytes;
    }
}
thread_local! {
    #[cfg(feature="stream-profile")]
    static PROFILE: Cell<WorkerProfile> = Cell::new(WorkerProfile::default());
}
/// Restore counters even if a kernel panics or recursively calls another stream.
#[derive(Default)]
pub struct WorkerScope {
    #[cfg(feature = "stream-profile")]
    old_profile: WorkerProfile,
}
impl WorkerScope {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "stream-profile")]
            old_profile: PROFILE.with(|v| v.replace(WorkerProfile::default())),
        }
    }
    pub fn snapshot(&self) -> WorkerProfile {
        #[cfg(feature = "stream-profile")]
        {
            PROFILE.with(Cell::get)
        }
        #[cfg(not(feature = "stream-profile"))]
        {
            WorkerProfile::default()
        }
    }
}
impl Drop for WorkerScope {
    fn drop(&mut self) {
        #[cfg(feature = "stream-profile")]
        PROFILE.with(|v| v.set(self.old_profile));
    }
}
#[inline(always)]
pub fn index(ll: LatLng, res: Resolution) -> CellIndex {
    #[cfg(feature = "stream-profile")]
    let start = std::time::Instant::now();
    let result = ll.to_cell(res);
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.h3_calls += 1;
        p.native_projection_calls += 1;
        p.h3_ns += start.elapsed().as_nanos() as u64;
        v.set(p);
    });
    result
}
#[inline(always)]
pub fn transform(
    crs: &crate::crs::transformer::CrsTransformer,
    x: f64,
    y: f64,
) -> crate::error::Result<(f64, f64)> {
    #[cfg(feature = "stream-profile")]
    let start = std::time::Instant::now();
    let result = crs.transform_point(x, y);
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.transform_calls += 1;
        p.transform_ns += start.elapsed().as_nanos() as u64;
        v.set(p);
    });
    result
}
#[inline(always)]
pub fn copied(bytes: usize) {
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.copied_bytes += bytes as u64;
        v.set(p);
    });
    #[cfg(not(feature = "stream-profile"))]
    let _ = bytes;
}

#[derive(Default, Debug, Serialize)]
pub struct StreamProfile {
    pub worker: WorkerProfile,
    /// Sum of worker elapsed times; may exceed wall time with parallel workers.
    pub worker_ns: u64,
    pub merge_ns: u64,
    pub spill_ns: u64,
    pub jobs: u64,
    pub worker_map_sets: u64,
    pub peak_worker_bytes: usize,
    pub decoded_bytes: u64,
    pub prefetch_wait_ns: u64,
}
