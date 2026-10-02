//! Scanline accumulation and lookahead indexing.
//!
//! Provides `H3ScanlineLookahead` for determining cell spans along raster scanlines.

use h3o::{LatLng, Resolution};

pub struct H3ScanlineLookahead {
    prev_hex_width: usize,
    current_hex_span: usize,
}

impl Default for H3ScanlineLookahead {
    fn default() -> Self {
        Self {
            prev_hex_width: 32,
            current_hex_span: 0,
        }
    }
}

impl H3ScanlineLookahead {
    #[inline(always)]
    pub fn with_initial_width(width: usize) -> Self {
        Self {
            prev_hex_width: width.max(1),
            current_hex_span: 0,
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
    pub fn set_certified_span_skip(&mut self, _skip: bool) {}

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
        for next in c + 1..row_width {
            let lon = lon_curr + (next - c) as f64 * d_lon_step;
            let cell = self.get_or_compute_cell(lat_row, lon, res);
            if cell != Some(run_cell) {
                return (next, cell);
            }
        }
        (row_width, None)
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
        if c >= row_width {
            return (row_width, None);
        }
        for next in c + 1..row_width {
            let x = x_start + next as f64 * dx_step;
            let cell = coord_to_cell(x, y_row);
            if cell != Some(run_cell) {
                return (next, cell);
            }
        }
        (row_width, None)
    }

    /// Find the sub-pixel core interval `[core_start, core_end)` within `[c, span_end)`
    /// where all sample points in the sampling pattern are guaranteed to lie within `run_cell`.
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
            (core_start, core_end)
        } else {
            (span_end, span_end)
        }
    }
}
