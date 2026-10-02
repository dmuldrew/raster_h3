use super::h3_edge_certificate::{EdgeModel, ParallelModel};
use crate::h3::certificate::GuardedCellIndexer;
use crate::h3::{CellIndex, LatLng, Resolution};

pub struct H3ScanlineLookahead {
    prev_hex_width: usize,
    current_hex_span: usize,
    certified_search: bool,
    certified_span_skip: bool,
    verify_geometry: bool,
    edge_model: Option<(u64, Option<Box<EdgeModel>>)>,
    guarded_indexer: Option<(u64, Option<GuardedCellIndexer>)>,
}

impl Default for H3ScanlineLookahead {
    fn default() -> Self {
        Self {
            prev_hex_width: 32,
            current_hex_span: 0,
            certified_search: crate::aggregator::multi_horizon::profile::lookahead_enabled(),
            certified_span_skip: crate::aggregator::multi_horizon::profile::span_skip_enabled(),
            verify_geometry:
                crate::aggregator::multi_horizon::profile::geometry_verification_enabled(),
            edge_model: None,
            guarded_indexer: None,
        }
    }
}

impl H3ScanlineLookahead {
    #[inline(always)]
    pub fn with_initial_width(width: usize) -> Self {
        Self {
            prev_hex_width: width.max(1),
            current_hex_span: 0,
            certified_search: crate::aggregator::multi_horizon::profile::lookahead_enabled(),
            certified_span_skip: crate::aggregator::multi_horizon::profile::span_skip_enabled(),
            verify_geometry:
                crate::aggregator::multi_horizon::profile::geometry_verification_enabled(),
            edge_model: None,
            guarded_indexer: None,
        }
    }

    #[inline(always)]
    pub fn for_resolution(res: Resolution) -> Self {
        let r_u8 = res as u8;
        let initial_width = match r_u8 {
            0..=6 => 64,
            7 => 35,
            8 => 14,
            9 => 5,
            _ => 2,
        };
        Self::with_initial_width(initial_width)
    }

    #[inline(always)]
    pub fn reset_row(&mut self) {
        self.current_hex_span = 0;
    }

    #[inline(always)]
    pub fn get_or_compute_cell(&mut self, lat: f64, lon: f64, res: Resolution) -> Option<u64> {
        if !(-90.0..=90.0).contains(&lat) {
            None
        } else if let Ok(ll) = LatLng::new(lat, lon) {
            Some(crate::aggregator::multi_horizon::profile::index(ll, res).into())
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn on_cell_changed(&mut self) {
        if self.current_hex_span > 0 {
            self.prev_hex_width = self.current_hex_span;
        }
        self.current_hex_span = 0;
    }

    #[inline(always)]
    pub fn advance_span(&mut self, num_stepped: usize) {
        self.current_hex_span += num_stepped;
    }

    #[inline(always)]
    pub fn set_certified_span_skip(&mut self, skip: bool) {
        self.certified_span_skip = skip;
    }

    #[inline(always)]
    pub fn find_span_end(
        &mut self,
        c: usize,
        row_width: usize,
        lon_curr: f64,
        lat_row: f64,
        d_lon_step: f64,
        res: Resolution,
        run_cell: u64,
    ) -> (usize, Option<u64>) {
        if c >= row_width {
            return (row_width, None);
        }
        if c + 1 >= row_width {
            return (row_width, None);
        }
        if self.certified_search {
            if self.verify_geometry {
                let proposal =
                    self.geometry_prefix(c, row_width, lon_curr, lat_row, d_lon_step, run_cell);
                let exact =
                    certified_span_end(c, row_width, self.prev_hex_width, run_cell, |next| {
                        self.get_or_compute_cell(
                            lat_row,
                            lon_curr + (next - c) as f64 * d_lon_step,
                            res,
                        )
                    });
                if proposal > c + 1 {
                    crate::aggregator::multi_horizon::profile::geometry_result(
                        true,
                        proposal - c - 1,
                        proposal <= exact.0,
                    );
                }
                return exact;
            }

            if self.certified_span_skip {
                let verifier = match self.guarded_indexer {
                    Some((cell, v)) if cell == run_cell => v,
                    _ => {
                        let v = CellIndex::try_from(run_cell)
                            .ok()
                            .and_then(GuardedCellIndexer::new);
                        self.guarded_indexer = Some((run_cell, v));
                        v
                    }
                };
                if let Some(verifier) = verifier {
                    return self.find_span_end_certified_skip(
                        c, row_width, lon_curr, lat_row, d_lon_step, res, run_cell, verifier,
                    );
                }
            }

            return certified_span_end(c, row_width, self.prev_hex_width, run_cell, |next| {
                self.get_or_compute_cell(lat_row, lon_curr + (next - c) as f64 * d_lon_step, res)
            });
        }
        self.find_span_end_exact_seq(c, row_width, lon_curr, lat_row, d_lon_step, res, run_cell)
    }

    #[inline(always)]
    fn find_span_end_exact_seq(
        &mut self,
        c: usize,
        row_width: usize,
        lon_curr: f64,
        lat_row: f64,
        d_lon_step: f64,
        res: Resolution,
        run_cell: u64,
    ) -> (usize, Option<u64>) {
        for next in c + 1..row_width {
            let cell =
                self.get_or_compute_cell(lat_row, lon_curr + (next - c) as f64 * d_lon_step, res);
            if cell != Some(run_cell) {
                return (next, cell);
            }
        }
        (row_width, None)
    }

    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn find_span_end_certified_skip(
        &mut self,
        c: usize,
        row_width: usize,
        lon_curr: f64,
        lat_row: f64,
        d_lon_step: f64,
        res: Resolution,
        run_cell: u64,
        verifier: GuardedCellIndexer,
    ) -> (usize, Option<u64>) {
        let mut guarded = verifier;
        let mut skipped = 0;
        let mut result = (row_width, None);
        // A span is accepted only after visiting EVERY actual pixel coordinate.
        // Endpoint interpolation cannot enclose unspecified native libm errors.
        for next in c + 1..row_width {
            let lon = lon_curr + (next - c) as f64 * d_lon_step;
            let cell = if !(-90.0..=90.0).contains(&lat_row) {
                None
            } else if let Ok(point) = LatLng::new(lat_row, lon) {
                let (cell, certified) = crate::aggregator::multi_horizon::profile::index_guarded(
                    point,
                    res,
                    &mut guarded,
                );
                skipped += usize::from(certified);
                Some(cell.into())
            } else {
                None
            };
            if cell != Some(run_cell) {
                result = (next, cell);
                break;
            }
        }
        self.guarded_indexer = Some((run_cell, Some(guarded)));
        if skipped > 0 {
            crate::aggregator::multi_horizon::profile::span_skip_record(skipped);
        }
        result
    }

    pub(crate) fn verifies_geometry(&self) -> bool {
        self.verify_geometry
    }

    fn parallel_model(&mut self, cell: u64, lat: f64, lon: f64) -> Option<ParallelModel> {
        if !self.verify_geometry {
            return None;
        }
        if self.edge_model.as_ref().map(|v| v.0) != Some(cell) {
            self.edge_model = Some((cell, EdgeModel::new(cell).map(Box::new)));
        }
        self.edge_model.as_ref()?.1.as_ref()?.parallel(lat, lon)
    }
    pub(crate) fn geometry_parallel(&self, lat: f64, start: f64, end: f64) -> bool {
        if !self.verify_geometry {
            return false;
        }
        let accepted = self
            .edge_model
            .as_ref()
            .and_then(|(_, model)| model.as_ref())
            .and_then(|model| model.parallel(lat, start))
            .is_some_and(|row| row.contains_to(end));
        crate::aggregator::multi_horizon::profile::geometry_query(accepted);
        accepted
    }
    fn geometry_prefix(
        &mut self,
        c: usize,
        end: usize,
        lon: f64,
        lat: f64,
        step: f64,
        cell: u64,
    ) -> usize {
        if !self.verify_geometry || !step.is_finite() {
            return c;
        }
        let Some(row) = self.parallel_model(cell, lat, lon) else {
            return c;
        };
        // The H3-assigned starting pixel is not automatically inside the polygon
        // model: verify it too. Every accepted proposal is subsequently checked
        // by the exact prefix certificate, including all intermediate pixels.
        let starts_inside = row.contains_to(lon);
        crate::aggregator::multi_horizon::profile::geometry_query(starts_inside);
        if !starts_inside {
            return c;
        }
        search_prefix(c, end, self.prev_hex_width, |e| {
            let accepted = row.contains_to(lon + (e - c - 1) as f64 * step);
            crate::aggregator::multi_horizon::profile::geometry_query(accepted);
            accepted
        })
    }

    /// Determine the end of the current H3 cell span for projected coordinates by checking every intermediate center
    #[inline(always)]
    pub fn find_span_end_projected<F>(
        &mut self,
        c: usize,
        row_width: usize,
        x_start: f64,
        y_row: f64,
        dx_step: f64,
        mut coord_to_cell: F,
        run_cell: u64,
    ) -> (usize, Option<u64>)
    where
        F: FnMut(f64, f64) -> Option<u64>,
    {
        for next in c + 1..row_width {
            let cell = coord_to_cell(x_start + next as f64 * dx_step, y_row);
            if cell != Some(run_cell) {
                return (next, cell);
            }
        }
        (row_width, None)
    }

    /// Find the sub-pixel core interval `[core_start, core_end)` within `[c, span_end)`
    /// where all sample points in the sampling pattern are guaranteed to lie within `run_cell`.
    ///
    /// The closure `is_point_in_cell(px, py)` tests whether sub-pixel coordinate `(px, py)`
    /// lies within `run_cell`.
    #[inline(always)]
    pub fn find_core_span<F>(
        &self,
        c: usize,
        span_end: usize,
        dx_bounds: (f64, f64),
        dy_bounds: (f64, f64),
        mut is_point_in_cell: F,
    ) -> (usize, usize)
    where
        F: FnMut(f64, f64) -> bool,
    {
        if span_end.saturating_sub(c) <= 2 {
            return (span_end, span_end);
        }

        // Four corners cannot certify a curved H3 boundary's interior.
        // This legacy rectangle API can only certify degenerate point samples.
        if dx_bounds.0 != dx_bounds.1 || dy_bounds.0 != dy_bounds.1 {
            return (span_end, span_end);
        }
        let (min_dx, max_dx) = dx_bounds;
        let (min_dy, max_dy) = dy_bounds;

        let mut is_pixel_core = |k: usize| -> bool {
            let k_f = k as f64;
            // Test 4 extremal corners of pixel k:
            // Top-Left, Top-Right, Bottom-Left, Bottom-Right
            is_point_in_cell(k_f + min_dx, min_dy)
                && is_point_in_cell(k_f + max_dx, min_dy)
                && is_point_in_cell(k_f + min_dx, max_dy)
                && is_point_in_cell(k_f + max_dx, max_dy)
        };

        let mut core_start = c + 1;
        let mut core_end = span_end - 1;

        while core_start < core_end {
            if is_pixel_core(core_start) {
                break;
            }
            core_start += 1;
        }

        while core_end > core_start {
            if is_pixel_core(core_end - 1) {
                break;
            }
            core_end -= 1;
        }

        if core_start < core_end {
            // Certify that every intermediate pixel in [core_start, core_end) is strictly core
            let mut all_certified = true;
            for k in core_start..core_end {
                if !is_pixel_core(k) {
                    all_certified = false;
                    break;
                }
            }
            if all_certified {
                (core_start, core_end)
            } else {
                (span_end, span_end)
            }
        } else {
            (span_end, span_end)
        }
    }
}

/// Search exclusive prefix ends, never endpoint membership. The caller has
/// already assigned pixel `start` to `cell`. The exact certificate below is
/// monotone even when individual pixel memberships leave and re-enter `cell`.
///
/// This is NOT a geometric shortcut: each newly covered center is indexed once.
/// The binary refinement reuses the certificate and performs no new H3 calls.
fn certified_span_end(
    start: usize,
    end: usize,
    initial_width: usize,
    cell: u64,
    mut index: impl FnMut(usize) -> Option<u64>,
) -> (usize, Option<u64>) {
    if start >= end {
        return (end, None);
    }
    let mut verified_end = start + 1;
    let mut first_failure = None;
    let mut certify = |prefix_end: usize| {
        crate::aggregator::multi_horizon::profile::prefix_test();
        if prefix_end <= verified_end {
            return true;
        }
        if first_failure.is_some() {
            return false;
        }
        while verified_end < prefix_end {
            let observed = index(verified_end);
            if observed != Some(cell) {
                first_failure = Some(observed);
                return false;
            }
            verified_end += 1;
        }
        true
    };
    let result = search_prefix(start, end, initial_width, &mut certify);
    (result, first_failure.flatten())
}

/// The oracle must certify EVERY pixel in [start, prefix_end). A geometric
/// oracle may be substituted only if it establishes that same contract.
fn search_prefix(
    start: usize,
    end: usize,
    initial_width: usize,
    mut certify: impl FnMut(usize) -> bool,
) -> usize {
    let mut good = start + 1;
    if good >= end {
        return end;
    }
    let mut probe = start.saturating_add(initial_width.max(2)).min(end);
    loop {
        if certify(probe) {
            good = probe;
            if good == end {
                return end;
            }
            probe = start
                .saturating_add((good - start).saturating_mul(2))
                .min(end);
        } else {
            let mut bad = probe;
            while bad - good > 1 {
                let mid = good + (bad - good) / 2;
                if certify(mid) {
                    good = mid;
                } else {
                    bad = mid;
                }
            }
            return good;
        }
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::*;

    #[test]
    fn every_membership_pattern_returns_first_failure_without_reindexing() {
        // Exhaustively include arbitrarily many leave/re-enter intervals.
        for mask in 0u32..1 << 11 {
            for initial in [0, 1, 2, 3, 7, 32, usize::MAX] {
                let mut calls = [0u8; 12];
                let cell_at = |col: usize| {
                    if mask & (1 << (col - 1)) == 0 {
                        Some(42)
                    } else {
                        Some(43)
                    }
                };
                let expected = (1..12).find(|&c| cell_at(c) != Some(42)).unwrap_or(12);
                let (end, next) = certified_span_end(0, 12, initial, 42, |col| {
                    calls[col] += 1;
                    cell_at(col)
                });
                assert_eq!(end, expected);
                assert_eq!(next, if expected == 12 { None } else { Some(43) });
                assert!(calls.iter().all(|&n| n <= 1));
                assert!(calls[1..expected].iter().all(|&n| n == 1));
                if expected < 12 {
                    assert_eq!(calls[expected], 1);
                }
            }
        }
    }

    #[test]
    fn invalid_coordinate_is_a_sticky_prefix_failure() {
        let (end, next) = certified_span_end(5, 40, 32, 7, |c| {
            assert!(c <= 9, "must stop checking at first invalid coordinate");
            if c == 9 {
                None
            } else {
                Some(7)
            }
        });
        assert_eq!((end, next), (9, None));
    }

    #[test]
    fn empty_singleton_and_large_indices_do_not_overflow() {
        assert_eq!(certified_span_end(0, 0, 32, 1, |_| panic!()), (0, None));
        assert_eq!(certified_span_end(4, 5, 32, 1, |_| panic!()), (5, None));
        assert_eq!(
            certified_span_end(usize::MAX - 9, usize::MAX, usize::MAX, 1, |_| Some(1)),
            (usize::MAX, None)
        );
    }
}
