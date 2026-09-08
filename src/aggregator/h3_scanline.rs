use h3o::{CellIndex, LatLng, Resolution};

/// H3 average edge length in meters for resolutions 0..=15
pub const H3_EDGE_LENGTH_M: [f64; 16] = [
    1_107_712.0, 418_676.0, 158_244.0, 59_810.0, 22_606.0, 8_544.0, 3_229.0, 1_220.0,
    461.3, 174.4, 65.9, 24.9, 9.41, 3.56, 1.35, 0.509,
];

/// Helper to determine if a pixel size in meters is safe for 1-ring Voronoi neighbor caching
#[inline(always)]
pub fn can_use_neighbor_cache(px_diag_meters: f64, res: Resolution) -> bool {
    let r_idx = (res as usize).min(15);
    px_diag_meters < H3_EDGE_LENGTH_M[r_idx]
}

/// Cache of 1-ring neighbor cell centroids for ultra-fast boundary sub-pixel Voronoi resolution
#[derive(Clone, Copy, Debug)]
pub struct H3NeighborDiskCache {
    pub cell: u64,
    pub cos_lat_sq: f64,
    pub count: usize,
    pub centers: [(f64, f64, u64); 7],
}

impl Default for H3NeighborDiskCache {
    fn default() -> Self {
        Self {
            cell: 0,
            cos_lat_sq: 1.0,
            count: 0,
            centers: [(0.0, 0.0, 0); 7],
        }
    }
}

impl H3NeighborDiskCache {
    #[inline(always)]
    pub fn update(&mut self, cell_u64: u64, cos_lat_sq: f64) {
        if self.cell == cell_u64 && (self.cos_lat_sq - cos_lat_sq).abs() < 1e-6 {
            return;
        }
        self.cell = cell_u64;
        self.cos_lat_sq = cos_lat_sq;
        if let Ok(cell) = CellIndex::try_from(cell_u64) {
            let disk: Vec<CellIndex> = cell.grid_disk(1);
            self.count = disk.len().min(7);
            for (idx, &c) in disk.iter().take(7).enumerate() {
                let ctr = LatLng::from(c);
                self.centers[idx] = (ctr.lat(), ctr.lng(), c.into());
            }
        } else {
            self.count = 0;
        }
    }

    #[inline(always)]
    pub fn resolve_point(&self, p_lat: f64, p_lon: f64) -> u64 {
        if self.count == 0 {
            return self.cell;
        }
        let mut min_d2 = f64::INFINITY;
        let mut best_c = self.cell;
        for i in 0..self.count {
            let (c_lat, c_lon, c_u64) = self.centers[i];
            let d_lat = p_lat - c_lat;
            let d_lon = p_lon - c_lon;
            let d2 = d_lat * d_lat + d_lon * d_lon * self.cos_lat_sq;
            if d2 < min_d2 {
                min_d2 = d2;
                best_c = c_u64;
            }
        }
        best_c
    }

    #[inline(always)]
    pub fn is_in_run_cell(&self, p_lat: f64, p_lon: f64) -> bool {
        self.resolve_point(p_lat, p_lon) == self.cell
    }
}

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
        if let Ok(ll) = LatLng::new(lat, lon) {
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
        let remaining_guess = self.prev_hex_width.saturating_sub(self.current_hex_span).max(1);
        let mut guess_c = (c + remaining_guess).min(row_width);
        let mut guess_lon = lon_curr + ((guess_c - c) as f64) * d_lon_step;
        let mut last_cell_at_right: Option<u64> = None;
        
        while guess_c < row_width {
            if let Ok(ll) = LatLng::new(lat_row, guess_lon) {
                let guess_cell: u64 = ll.to_cell(res).into();
                if guess_cell == run_cell {
                    let step = (guess_c - c).max(1);
                    let new_guess_c = (guess_c + step).min(row_width);
                    if new_guess_c == guess_c { break; }
                    guess_c = new_guess_c;
                    guess_lon = lon_curr + ((guess_c - c) as f64) * d_lon_step;
                } else {
                    last_cell_at_right = Some(guess_cell);
                    break;
                }
            } else {
                break;
            }
        }

        let mut left = c + 1;
        let mut right = guess_c;
        while left < right {
            let mid = left + (right - left) / 2;
            let mid_lon = (lon_curr + d_lon_step) + ((mid - (c + 1)) as f64) * d_lon_step;
            if let Ok(ll) = LatLng::new(lat_row, mid_lon) {
                let cell: u64 = ll.to_cell(res).into();
                if cell == run_cell {
                    left = mid + 1;
                } else {
                    right = mid;
                    last_cell_at_right = Some(cell);
                }
            } else {
                right = mid;
                last_cell_at_right = None;
            }
        }
        (left, if left < row_width { last_cell_at_right } else { None })
    }

    /// Determine the end of the current H3 cell span for projected coordinates using exponential probe + binary search
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
        let remaining_guess = self.prev_hex_width.saturating_sub(self.current_hex_span).max(1);
        let mut guess_c = (c + remaining_guess).min(row_width);
        let mut guess_x = x_start + (guess_c as f64) * dx_step;
        let mut last_cell_at_right: Option<u64> = None;

        while guess_c < row_width {
            if let Some(guess_cell) = coord_to_cell(guess_x, y_row) {
                if guess_cell == run_cell {
                    let step = (guess_c - c).max(1);
                    let new_guess_c = (guess_c + step).min(row_width);
                    if new_guess_c == guess_c {
                        break;
                    }
                    guess_c = new_guess_c;
                    guess_x = x_start + (guess_c as f64) * dx_step;
                } else {
                    last_cell_at_right = Some(guess_cell);
                    break;
                }
            } else {
                break;
            }
        }

        let mut left = c + 1;
        let mut right = guess_c;
        while left < right {
            let mid = left + (right - left) / 2;
            let mid_x = x_start + (mid as f64) * dx_step;
            if let Some(cell) = coord_to_cell(mid_x, y_row) {
                if cell == run_cell {
                    left = mid + 1;
                } else {
                    right = mid;
                    last_cell_at_right = Some(cell);
                }
            } else {
                right = mid;
                last_cell_at_right = None;
            }
        }
        (left, if left < row_width { last_cell_at_right } else { None })
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

