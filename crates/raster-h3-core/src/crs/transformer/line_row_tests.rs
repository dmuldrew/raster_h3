use super::{
    mercator_line_bounds, web_mercator_lat, web_mercator_lon, wgs84_line_bounds, AlbersConicFast,
    AlbersInverse, RowPathBounds,
};
use h3o::{CellIndex, LatLng, LatLngIndexer, Resolution, RowPath, SpanBounds};

/// Deterministic PRNG in [0, 1).
fn rng(mut state: u64) -> impl FnMut() -> f64 {
    move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1_u64 << 53) as f64
    }
}

enum Crs<'a> {
    Wgs84,
    Mercator,
    Albers(AlbersInverse<'a>),
}

/// A rotated raster row: projected (x, y) = start + i * step.
struct LinePath<'a> {
    crs: Crs<'a>,
    start: (f64, f64),
    step: (f64, f64),
    width: usize,
    /// Bounds scale, < 1 to understate them.
    scale: (f64, f64, f64),
}

impl LinePath<'_> {
    fn coord(&self, i: usize) -> (f64, f64) {
        let i = i as f64;
        (
            self.start.0 + i * self.step.0,
            self.start.1 + i * self.step.1,
        )
    }

    fn lon_lat(&self, i: usize) -> (f64, f64) {
        let (x, y) = self.coord(i);
        match &self.crs {
            Crs::Wgs84 => (x, y),
            Crs::Mercator => (web_mercator_lon(x), web_mercator_lat(y)),
            Crs::Albers(inverse) => inverse.transform_point(x, y),
        }
    }

    fn bounds(&self, first: usize, last: usize) -> Option<RowPathBounds> {
        let (start, end) = (self.coord(first), self.coord(last));
        let n = last.max(first) as f64;
        let mag = self.start.0.abs()
            + (n * self.step.0).abs()
            + self.start.1.abs()
            + (n * self.step.1).abs();
        let b = match &self.crs {
            Crs::Wgs84 => wgs84_line_bounds(start, end, self.step, mag),
            Crs::Mercator => mercator_line_bounds(start, end, self.step, mag),
            Crs::Albers(inverse) => inverse.line_path_bounds(start, end, self.step, mag),
        }?;
        Some(RowPathBounds {
            speed: b.speed * self.scale.0,
            accel: b.accel * self.scale.1,
            noise: b.noise * self.scale.2,
        })
    }

    fn whole(&self) -> RowPathBounds {
        self.bounds(0, self.width).expect("bounds")
    }
}

impl RowPath for LinePath<'_> {
    fn point(&mut self, i: usize) -> LatLng {
        let (lon, lat) = self.lon_lat(i);
        LatLng::new(lat, lon).expect("finite")
    }
    fn speed_bound(&self) -> f64 {
        self.whole().speed
    }
    fn accel_bound(&self) -> f64 {
        self.whole().accel
    }
    fn noise_bound(&self) -> f64 {
        self.whole().noise
    }
    fn span_bounds(&mut self, first: usize, last: usize) -> Option<SpanBounds> {
        let b = self.bounds(first, last)?;
        Some(SpanBounds {
            speed: b.speed,
            accel: b.accel,
            noise: b.noise,
        })
    }
}

/// Random rotated rows over the three CRSs: (crs, start, step).
fn rows<'a>(
    albers: &'a [AlbersConicFast],
    seed: u64,
    count: usize,
    width: usize,
) -> Vec<LinePath<'a>> {
    let mut next = rng(seed);
    let mut out = Vec::new();
    for k in 0..count {
        let angle = std::f64::consts::TAU * next();
        let (sin, cos) = angle.sin_cos();
        let (crs, center, size) = match k % 4 {
            0 => {
                // Up to 85°, so some rows approach the poles.
                let lat = -85.0 + 170.0 * next();
                let lon = -180.0 + 360.0 * next();
                (Crs::Wgs84, (lon, lat), 10_f64.powf(-4.5 + 1.7 * next()))
            }
            1 => {
                let y = (-1.9e7 + 3.8e7 * next()) * 0.95;
                let x = -2.0e7 + 4.0e7 * next();
                (Crs::Mercator, (x, y), 10_f64.powf(1.0 + 1.7 * next()))
            }
            p => {
                let albers = &albers[p - 2];
                let (lat, lon) = if p == 2 {
                    (25.0 + 24.0 * next(), -124.0 + 57.0 * next())
                } else {
                    (-43.0 + 32.0 * next(), 113.0 + 40.0 * next())
                };
                let center = albers.project_point(lon, lat);
                (
                    Crs::Albers(albers.prepare_inverse()),
                    center,
                    10_f64.powf(1.0 + 1.7 * next()),
                )
            }
        };
        let step = (size * cos, size * sin);
        let half = 0.5 * width as f64;
        out.push(LinePath {
            crs,
            start: (center.0 - half * step.0, center.1 - half * step.1),
            step,
            width,
            scale: (1.0, 1.0, 1.0),
        });
    }
    out
}

fn albers() -> [AlbersConicFast; 2] {
    [
        AlbersConicFast::epsg_5070(),
        AlbersConicFast::new(-18.0, -36.0, 0.0, 132.0),
    ]
}

fn unit(lon_deg: f64, lat_deg: f64) -> [f64; 3] {
    let (lat, lon) = (lat_deg.to_radians(), lon_deg.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

fn norm(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

#[test]
fn line_bounds_dominate_finite_differences() {
    let albers = albers();
    let width = 2000;
    let mut checked = 0;
    let mut worst: [f64; 2] = [0.0; 2];
    for row in rows(&albers, 5, 400, width) {
        if row.bounds(0, width).is_none() {
            continue;
        }
        checked += 1;
        let pts: Vec<[f64; 3]> = (0..=width)
            .map(|i| {
                let (lon, lat) = row.lon_lat(i);
                unit(lon, lat)
            })
            .collect();
        // Check each 100-column window against its own (span) bounds.
        for w in (0..width).step_by(100) {
            let b = row.bounds(w, w + 100).expect("sub-span bounds");
            let tol = 1e-12 + 4.0 * b.noise;
            for i in (w + 1)..(w + 100) {
                let d1 = norm(std::array::from_fn(|k| pts[i + 1][k] - pts[i][k]));
                let d2 = norm(std::array::from_fn(|k| {
                    pts[i + 1][k] - 2.0 * pts[i][k] + pts[i - 1][k]
                }));
                assert!(
                    d1 <= b.speed * (1.0 + 1e-6) + tol,
                    "speed {d1} > {}",
                    b.speed
                );
                assert!(
                    d2 <= b.accel * (1.0 + 1e-6) + tol,
                    "accel {d2} > {}",
                    b.accel
                );
                worst[0] = worst[0].max(d1 / b.speed);
                worst[1] = worst[1].max(d2 / b.accel);
            }
        }
    }
    assert!(checked > 350, "only {checked} rows had bounds");
    // The speed bounds are tight; acceleration within a few x.
    assert!(worst[0] > 0.5 && worst[1] > 0.05, "worst ratios {worst:?}");
}

fn mismatches(rows: &mut [LinePath<'_>], resolutions: &[Resolution]) -> (usize, usize) {
    let (mut mismatches, mut decisions) = (0, 0);
    for row in rows.iter_mut() {
        if row.bounds(0, row.width).is_none() {
            continue;
        }
        let width = row.width;
        let mut cells = vec![vec![None::<CellIndex>; width]; resolutions.len()];
        let stats = LatLngIndexer::new().index_row(row, 0..width, resolutions, |r, cols, cell| {
            cells[r][cols].fill(Some(cell));
        });
        decisions += stats.decisions;
        for col in 0..width {
            let ll = row.point(col);
            for (cells_r, &res) in cells.iter().zip(resolutions) {
                mismatches += usize::from(cells_r[col] != Some(ll.to_cell(res)));
            }
        }
    }
    (mismatches, decisions)
}

#[test]
fn line_rows_match_to_cell_with_index_row() {
    let albers = albers();
    let resolutions = [
        Resolution::Six,
        Resolution::Nine,
        Resolution::Ten,
        Resolution::Twelve,
    ];
    let mut rows = rows(&albers, 23, 240, 3000);
    let (mismatches, decisions) = mismatches(&mut rows, &resolutions);
    assert_eq!(mismatches, 0);
    assert!(decisions > 100_000, "only {decisions} decisions");
}

// Control: bounds understated 10x (speed, noise; acceleration 100x)
// produce wrong cells, so the test above would catch a bound that is
// too small.
#[test]
fn line_rows_mismatch_with_understated_bounds() {
    let albers = albers();
    let resolutions = [
        Resolution::Nine,
        Resolution::Ten,
        Resolution::Twelve,
        Resolution::Thirteen,
    ];
    let mut rows = rows(&albers, 23, 240, 3000);
    for row in &mut rows {
        row.scale = (0.1, 0.01, 0.1);
    }
    let (mismatches, _) = mismatches(&mut rows, &resolutions);
    assert!(mismatches > 0, "understated bounds never changed a cell");
}
