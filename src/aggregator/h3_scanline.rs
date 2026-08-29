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
    ) -> usize {
        let remaining_guess = self.prev_hex_width.saturating_sub(self.current_hex_span).max(1);
        let mut guess_c = (c + remaining_guess).min(row_width);
        let mut guess_lon = lon_curr + ((guess_c - c) as f64) * d_lon_step;
        
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
                }
            } else {
                right = mid;
            }
        }
        left
    }
}
