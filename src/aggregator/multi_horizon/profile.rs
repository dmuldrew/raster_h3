//! Per-worker instrumentation. Enable `stream-profile` for counters and stage
//! timers; normal builds compile hot-path measurement out entirely.
use crate::h3::{CellIndex, LatLng, Resolution};
use serde::Serialize;
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
    static NATIVE_CACHE: Cell<Option<crate::h3::certificate::CachedIndexer>> = const { Cell::new(None) };
    static VERIFY_GEOMETRY: Cell<bool> = const { Cell::new(false) };
    static LOOKAHEAD: Cell<bool> = const { Cell::new(true) };
    static SPAN_SKIP: Cell<bool> = const { Cell::new(false) };
    #[cfg(feature="stream-profile")]
    static PROFILE: Cell<WorkerProfile> = Cell::new(WorkerProfile::default());
}
/// Restore counters even if a kernel panics or recursively calls another stream.
pub struct WorkerScope {
    old_native_cache: Option<crate::h3::certificate::CachedIndexer>,
    old_lookahead: bool,
    old_verify_geometry: bool,
    old_span_skip: bool,
    #[cfg(feature = "stream-profile")]
    old_profile: WorkerProfile,
}
impl WorkerScope {
    pub fn new(lookahead: bool) -> Self {
        Self::with_geometry_verification(lookahead, false)
    }
    pub fn with_geometry_verification(lookahead: bool, verify: bool) -> Self {
        Self::with_options(lookahead, verify, false)
    }
    pub fn with_options(lookahead: bool, verify: bool, cache_native: bool) -> Self {
        Self::with_all_options(lookahead, verify, cache_native, false)
    }
    pub fn with_all_options(
        lookahead: bool,
        verify: bool,
        cache_native: bool,
        span_skip: bool,
    ) -> Self {
        Self {
            old_native_cache: NATIVE_CACHE.with(|v| {
                v.replace(cache_native.then(crate::h3::certificate::CachedIndexer::default))
            }),
            old_verify_geometry: VERIFY_GEOMETRY.with(|v| v.replace(verify)),
            old_lookahead: LOOKAHEAD.with(|v| v.replace(lookahead)),
            old_span_skip: SPAN_SKIP.with(|v| v.replace(span_skip)),
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
        NATIVE_CACHE.with(|v| v.set(self.old_native_cache));
        LOOKAHEAD.with(|v| v.set(self.old_lookahead));
        VERIFY_GEOMETRY.with(|v| v.set(self.old_verify_geometry));
        SPAN_SKIP.with(|v| v.set(self.old_span_skip));
        #[cfg(feature = "stream-profile")]
        PROFILE.with(|v| v.set(self.old_profile));
    }
}
#[inline(always)]
pub fn index(ll: LatLng, res: Resolution) -> CellIndex {
    #[cfg(feature = "stream-profile")]
    let start = std::time::Instant::now();
    let (result, reused) = NATIVE_CACHE.with(|v| {
        if let Some(mut cache) = v.get() {
            let result = cache.index(ll, res);
            v.set(Some(cache));
            result
        } else {
            (ll.to_cell(res), false)
        }
    });
    let _ = reused;
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.h3_calls += 1;
        p.native_projection_calls += 1;
        p.native_cache_hits += u64::from(reused);
        p.h3_ns += start.elapsed().as_nanos() as u64;
        v.set(p);
    });
    result
}

/// A guarded request still projects its actual input. Keep these requests in
/// h3_calls so profiling cannot mistake avoided quantization for skipped work
/// in the geographic projection stage.
#[inline(always)]
pub fn index_guarded(
    ll: LatLng,
    res: Resolution,
    guarded: &mut crate::h3::certificate::GuardedCellIndexer,
) -> (CellIndex, bool) {
    #[cfg(feature = "stream-profile")]
    let start = std::time::Instant::now();
    let result = guarded.index(ll, res);
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

pub fn lookahead_enabled() -> bool {
    LOOKAHEAD.with(Cell::get)
}

pub fn span_skip_enabled() -> bool {
    SPAN_SKIP.with(Cell::get)
}

#[inline(always)]
pub fn span_skip_record(skipped: usize) {
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.span_skips += 1;
        p.skipped_pixels += skipped as u64;
        v.set(p);
    });
    #[cfg(not(feature = "stream-profile"))]
    let _ = skipped;
}

#[inline(always)]
pub fn prefix_test() {
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.prefix_tests += 1;
        v.set(p);
    });
}

pub fn geometry_verification_enabled() -> bool {
    VERIFY_GEOMETRY.with(Cell::get)
}

pub fn geometry_query(accepted: bool) {
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        p.geometry_queries += 1;
        p.geometry_accepts += u64::from(accepted);
        v.set(p);
    });
    #[cfg(not(feature = "stream-profile"))]
    let _ = accepted;
}
pub fn geometry_result(center: bool, count: usize, agrees: bool) {
    #[cfg(feature = "stream-profile")]
    PROFILE.with(|v| {
        let mut p = v.get();
        if center {
            p.geometry_proposed_center_pixels += count as u64;
        } else {
            p.geometry_proposed_core_samples += count as u64;
        }
        p.geometry_mismatches += u64::from(!agrees);
        v.set(p);
    });
    #[cfg(not(feature = "stream-profile"))]
    let _ = (center, count, agrees);
}
