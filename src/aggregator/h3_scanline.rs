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
            Some(ll.to_cell(res).into())
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
        // Certify every center. A parallel may leave and re-enter a spherical
        // cell at any resolution; endpoint probes cannot establish an interval.
        for k in c + 1..row_width {
            let lon = lon_curr + (k - c) as f64 * d_lon_step;
            let cell = LatLng::new(lat_row, lon)
                .ok()
                .map(|ll| ll.to_cell(res).into());
            if cell != Some(run_cell) {
                return (k, cell);
            }
        }
        (row_width, None)
    }

    /// Determine the first differing projected sample by checking every intermediate center.
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
        for k in c + 1..row_width {
            let cell = coord_to_cell(x_start + k as f64 * dx_step, y_row);
            if cell != Some(run_cell) {
                return (k, cell);
            }
        }
        (row_width, None)
    }

    /// Compatibility API: returns an empty core until actual samples can be certified.
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
        let _ = (c, dx_bounds, dy_bounds, &mut is_point_in_cell);
        // Four corners do not certify a curved spherical footprint. Callers
        // must evaluate their actual sample pattern until a stronger API exists.
        (span_end, span_end)
    }
}
