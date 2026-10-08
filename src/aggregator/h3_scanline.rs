//! Exact H3 indexing and sequential scanline span discovery.
use h3o::{LatLng, Resolution};

#[inline(always)]
pub fn cell_at(lat: f64, lon: f64, res: Resolution) -> Option<u64> {
    if !(-90.0..=90.0).contains(&lat) {
        return None;
    }
    LatLng::new(lat, lon)
        .ok()
        .map(|ll| crate::aggregator::multi_horizon::profile::index(ll, res).into())
}

/// Evaluate every candidate; return the first different cell for reuse by the walker.
#[inline(always)]
pub fn span_end(
    c: usize,
    row_end: usize,
    run_cell: u64,
    mut index_at: impl FnMut(usize) -> Option<u64>,
) -> (usize, Option<u64>) {
    for next in c + 1..row_end {
        let cell = index_at(next);
        if cell != Some(run_cell) {
            return (next, cell);
        }
    }
    (row_end, None)
}
