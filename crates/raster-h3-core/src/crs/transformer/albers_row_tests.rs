use super::{AlbersConicFast, AlbersInverse, RowPathBounds};
use h3o::{CellIndex, LatLng, LatLngIndexer, Resolution, RowPath};

/// Deterministic PRNG in [0, 1).
fn rng(mut state: u64) -> impl FnMut() -> f64 {
    move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1_u64 << 53) as f64
    }
}

fn projections() -> Vec<(&'static str, AlbersConicFast)> {
    vec![
        ("EPSG:5070 CONUS", AlbersConicFast::epsg_5070()),
        (
            "Alaska (EPSG:3338 parameters)",
            AlbersConicFast::new(55.0, 65.0, 50.0, -154.0),
        ),
        (
            "Australia (EPSG:3577 parameters, n < 0)",
            AlbersConicFast::new(-18.0, -36.0, 0.0, 132.0),
        ),
        (
            "near-equatorial cone (small n)",
            AlbersConicFast::new(2.0, 8.0, 0.0, 20.0),
        ),
    ]
}

#[test]
fn albers_bounds_reject_branch_cut_for_both_cone_signs() {
    for (_, albers) in projections() {
        let inverse = albers.prepare_inverse();
        let y_p = -albers.rho0;
        assert!(inverse
            .row_path_bounds(-100_000.0, 100_000.0, y_p, 100.0)
            .is_none());
        for (x0, x1) in [(-100_000.0, 100_000.0), (0.0, 100_000.0), (-100_000.0, 0.0)] {
            let coord = |x, y_p| (x + albers.x_0, albers.rho0 - y_p + albers.y_0);
            for (start, end) in [
                (coord(x0, y_p), coord(x1, y_p + 100_000.0)),
                (coord(x1, y_p + 100_000.0), coord(x0, y_p)),
            ] {
                let step = ((end.0 - start.0) / 2000.0, (end.1 - start.1) / 2000.0);
                assert!(inverse.line_path_bounds(start, end, step, 1e8).is_none());
            }
        }
        // The same crossing on the near side of the apex is smooth.
        assert!(inverse
            .row_path_bounds(-100_000.0, 100_000.0, -y_p, 100.0)
            .is_some());
    }
}

/// A north-up row: x(i) = x_start + i * step at constant y.
struct AlbersRow<'a> {
    inverse: AlbersInverse<'a>,
    x_start: f64,
    step: f64,
    y_p: f64,
    y_p_sq: f64,
    bounds: RowPathBounds,
}

impl<'a> AlbersRow<'a> {
    fn new(
        albers: &'a AlbersConicFast,
        x_start: f64,
        y: f64,
        step: f64,
        width: usize,
    ) -> Option<Self> {
        let inverse = albers.prepare_inverse();
        let (y_p, y_p_sq) = inverse.row_yp(y);
        let x_end = x_start + width as f64 * step;
        let bounds = inverse.row_path_bounds(x_start.min(x_end), x_start.max(x_end), y_p, step)?;
        Some(Self {
            inverse,
            x_start,
            step,
            y_p,
            y_p_sq,
            bounds,
        })
    }

    fn lon_lat(&self, i: usize) -> (f64, f64) {
        self.inverse
            .transform_with_yp(self.x_start + i as f64 * self.step, self.y_p, self.y_p_sq)
    }
}

impl RowPath for AlbersRow<'_> {
    fn point(&mut self, i: usize) -> LatLng {
        let (lon, lat) = self.lon_lat(i);
        LatLng::new(lat, lon).expect("finite")
    }
    fn speed_bound(&self) -> f64 {
        self.bounds.speed
    }
    fn accel_bound(&self) -> f64 {
        self.bounds.accel
    }
    fn noise_bound(&self) -> f64 {
        self.bounds.noise
    }
}

fn unit(lon_deg: f64, lat_deg: f64) -> [f64; 3] {
    let (lat, lon) = (lat_deg.to_radians(), lon_deg.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

/// Random rows: a random point of the projection's region, projected
/// forward to pick the row's y and starting x.
fn rows(seed: u64, count: usize) -> Vec<(usize, f64, f64, f64)> {
    let mut next = rng(seed);
    (0..count)
        .map(|k| {
            let p = k % 4;
            let (lat0, lon0) = match p {
                0 => (25.0 + 24.0 * next(), -124.0 + 57.0 * next()),
                1 => (52.0 + 18.0 * next(), -170.0 + 40.0 * next()),
                2 => (-43.0 + 32.0 * next(), 113.0 + 40.0 * next()),
                _ => (-10.0 + 20.0 * next(), 0.0 + 40.0 * next()),
            };
            let step = 10_f64.powf(1.0 + 1.7 * next()); // 10 m .. 500 m
            (p, lat0, lon0, step)
        })
        .collect()
}

#[test]
fn albers_row_bounds_dominate_finite_differences() {
    let projections = projections();
    let width = 2000;
    let mut checked = 0;
    for (p, lat0, lon0, step) in rows(3, 200) {
        let (name, albers) = &projections[p];
        let (x, y) = albers.project_point(lon0, lat0);
        let Some(row) = AlbersRow::new(albers, x - 0.5 * width as f64 * step, y, step, width)
        else {
            continue;
        };
        checked += 1;
        let pts: Vec<[f64; 3]> = (0..=width)
            .map(|i| {
                let (lon, lat) = row.lon_lat(i);
                unit(lon, lat)
            })
            .collect();
        let tol = 1e-12 + 4.0 * row.bounds.noise;
        for i in 1..width {
            let d1 = (0..3)
                .map(|k| (pts[i + 1][k] - pts[i][k]).powi(2))
                .sum::<f64>()
                .sqrt();
            let d2 = (0..3)
                .map(|k| (pts[i + 1][k] - 2.0 * pts[i][k] + pts[i - 1][k]).powi(2))
                .sum::<f64>()
                .sqrt();
            assert!(
                d1 <= row.bounds.speed * (1.0 + 1e-6) + tol,
                "{name}: speed {d1} > {}",
                row.bounds.speed
            );
            assert!(
                d2 <= row.bounds.accel * (1.0 + 1e-6) + tol,
                "{name}: accel {d2} > {}",
                row.bounds.accel
            );
        }
    }
    assert!(checked > 150, "only {checked} rows had bounds");
}

#[test]
fn albers_rows_match_to_cell_with_index_row() {
    let projections = projections();
    let resolutions = [
        Resolution::Six,
        Resolution::Nine,
        Resolution::Ten,
        Resolution::Twelve,
    ];
    let width = 3000;
    let (mut checked, mut decisions) = (0, 0);
    for (p, lat0, lon0, step) in rows(17, 160) {
        let (name, albers) = &projections[p];
        let (x, y) = albers.project_point(lon0, lat0);
        let Some(mut row) = AlbersRow::new(albers, x - 0.5 * width as f64 * step, y, step, width)
        else {
            continue;
        };
        checked += 1;
        let mut cells = vec![vec![None::<CellIndex>; width]; resolutions.len()];
        let stats =
            LatLngIndexer::new().index_row(&mut row, 0..width, &resolutions, |r, cols, cell| {
                for col in cols {
                    cells[r][col] = Some(cell);
                }
            });
        decisions += stats.decisions;
        for (col, ll) in (0..width).map(|col| (col, row.point(col))) {
            for (r, &res) in resolutions.iter().enumerate() {
                assert_eq!(
                    cells[r][col],
                    Some(ll.to_cell(res)),
                    "{name} step {step} col {col} {res:?}"
                );
            }
        }
    }
    assert!(checked > 120, "only {checked} rows had bounds");
    assert!(decisions > 0);
}

// Control: bounds understated 10x (speed, noise; acceleration 100x)
// produce wrong cells, so the derived bounds are within ~10x of what is
// needed and the test above would catch a bound that is too small.
#[test]
fn albers_rows_mismatch_with_understated_bounds() {
    let projections = projections();
    let resolutions = [
        Resolution::Nine,
        Resolution::Ten,
        Resolution::Twelve,
        Resolution::Thirteen,
    ];
    let width = 3000;
    let mut mismatches = 0;
    for (p, lat0, lon0, step) in rows(17, 160) {
        let (_, albers) = &projections[p];
        let (x, y) = albers.project_point(lon0, lat0);
        let Some(mut row) = AlbersRow::new(albers, x - 0.5 * width as f64 * step, y, step, width)
        else {
            continue;
        };
        row.bounds.speed *= 0.1;
        row.bounds.accel *= 0.01;
        row.bounds.noise *= 0.1;
        let mut cells = vec![vec![None::<CellIndex>; width]; resolutions.len()];
        LatLngIndexer::new().index_row(&mut row, 0..width, &resolutions, |r, cols, cell| {
            for col in cols {
                cells[r][col] = Some(cell);
            }
        });
        for col in 0..width {
            let ll = row.point(col);
            for (cells_r, &res) in cells.iter().zip(&resolutions) {
                mismatches += usize::from(cells_r[col] != Some(ll.to_cell(res)));
            }
        }
    }
    assert!(mismatches > 0, "understated bounds never changed a cell");
}
