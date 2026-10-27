//! Per-worker instrumentation. Enable `stream-profile` for counters and stage
//! timers; normal builds compile hot-path measurement out entirely.
use h3o::{CellIndex, LatLng, LatLngIndexer, Resolution};
use serde::Serialize;
#[cfg(feature = "stream-profile")]
use std::cell::Cell;

#[derive(Clone, Copy, Default, Debug, Serialize)]
pub struct WorkerProfile {
    pub h3_calls: u64,
    pub h3_ns: u64,
    pub transform_calls: u64,
    pub transform_ns: u64,
    pub copied_bytes: u64,
}
impl WorkerProfile {
    pub fn merge(&mut self, other: Self) {
        self.h3_calls += other.h3_calls;
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
        p.h3_ns += start.elapsed().as_nanos() as u64;
        v.set(p);
    });
    result
}
/// Index one point at every resolution, sharing h3o's projection work and
/// reusing the indexer's last cell where the point's grid cell is unchanged.
#[inline(always)]
pub fn index_all(
    indexer: &mut LatLngIndexer,
    ll: LatLng,
    resolutions: &[Resolution],
    cells: &mut [u64],
) {
    #[cfg(feature = "stream-profile")]
    let start = std::time::Instant::now();
    for (cell, index) in cells
        .iter_mut()
        .zip(indexer.to_cells(ll, resolutions.iter().copied()))
    {
        *cell = u64::from(index);
    }
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.h3_calls += resolutions.len() as u64;
        p.h3_ns += start.elapsed().as_nanos() as u64;
        v.set(p);
    });
}
#[inline(always)]
pub fn transform(
    crs: &crate::crs::transformer::CrsTransformer,
    x: f64,
    y: f64,
) -> crate::error::Result<(f64, f64)> {
    measure_transform(|| crs.transform_point(x, y))
}

/// Measure the shipping per-sample path, including row-cache use but not row setup.
#[inline(always)]
pub fn measure_transform<T>(transform: impl FnOnce() -> T) -> T {
    #[cfg(feature = "stream-profile")]
    let start = std::time::Instant::now();
    let result = transform();
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
    /// Wall time spent in parallel kernel waves.
    pub kernel_wall_ns: u64,
    /// Most window jobs observed running at the same instant.
    pub peak_concurrent_jobs: usize,
    pub merge_ns: u64,
    pub spill_ns: u64,
    pub jobs: u64,
    /// Kernel waves dispatched (each runs up to `worker_tasks` window jobs).
    pub waves: u64,
    /// Waves handed to the dedicated pool through a helper OS thread because
    /// the caller was a worker of another Rayon pool (one thread spawn each).
    pub bridged_waves: u64,
    /// Waves run serially on the calling thread: the caller was already a
    /// worker of the dedicated pool, or a Rayon worker with no dedicated pool.
    pub serial_waves: u64,
    pub worker_map_sets: u64,
    pub peak_worker_bytes: usize,
    pub decoded_bytes: u64,
    pub prefetch_wait_ns: u64,
    /// Highest decoded-buffer bytes held at once (see `decode_budget_bytes`).
    pub peak_decoded_bytes: usize,
    /// Highest estimated bytes of pending compaction groups.
    pub peak_compaction_bytes: usize,
    /// Sibling groups decomposed early to respect the compaction byte limit.
    pub forced_compaction_flushes: u64,
}
