//! Row-path bounds for h3o's exact row indexer (`h3o::RowPath`).

use super::{web_mercator_lat, WGS84_A};

/// Bounds describing how the unit vector of a row's locations moves per
/// column, for `h3o::RowPath`: speed |dP/di| (rad/column), acceleration
/// |d²P/di²| (rad/column²) and the floating-point jitter of the computed
/// coordinates around the smooth path (rad).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RowPathBounds {
    pub speed: f64,
    pub accel: f64,
    pub noise: f64,
}

/// Bounds for a path whose latitude phi stays in `[phi_lo, phi_hi]`
/// (radians) and whose longitude is linear in the column, advancing by
/// `lam1` rad/column, given bounds `phi1 ≥ |phi'|` and `phi2 ≥ |phi''|`.
///
/// With P = (cos phi cos lambda, cos phi sin lambda, sin phi) and lambda'' =
/// 0: |P_phi| = |P_phiphi| = 1, |P_lambda| = |P_lambdalambda| = cos phi and
/// |P_philambda| = |sin phi|, so
///
/// - |P'| ≤ |phi'| + cos phi |lambda'|;
/// - |P''| ≤ |phi''| + phi'² + 2 |phi' lambda'| |sin phi| + lambda'² cos phi.
fn latlng_path_bounds(
    (phi_lo, phi_hi): (f64, f64),
    phi1: impl Fn(f64) -> f64,
    phi2: impl Fn(f64, f64) -> f64,
    lam1: f64,
    noise: f64,
) -> Option<RowPathBounds> {
    let (near, far) = if phi_lo <= 0.0 && phi_hi >= 0.0 {
        (0.0, phi_lo.abs().max(phi_hi.abs()))
    } else {
        (
            phi_lo.abs().min(phi_hi.abs()),
            phi_lo.abs().max(phi_hi.abs()),
        )
    };
    // Largest cos phi and |sin phi| over the range, padded for rounding.
    let cos_max = (near.cos() + 1e-12).min(1.0);
    let sin_max = (far.sin() + 1e-12).min(1.0);
    let phi1 = phi1(cos_max);
    let speed = phi1 + lam1 * cos_max;
    let accel =
        phi2(cos_max, sin_max) + phi1 * phi1 + 2.0 * phi1 * lam1 * sin_max + lam1 * lam1 * cos_max;
    let inflate = 1.0 + 1e-9;
    let bounds = RowPathBounds {
        speed: speed * inflate + 1e-300,
        accel: accel * inflate + 1e-300,
        noise: noise * inflate,
    };
    [phi_lo, phi_hi, bounds.speed, bounds.accel, bounds.noise]
        .iter()
        .all(|v| v.is_finite())
        .then_some(bounds)
}

/// Bounds for WGS84 locations (degrees) along a straight line of
/// coordinates (a rotated or sheared raster row) from `start` to `end`
/// (each `(lon, lat)`), advancing by `step` = (dlon, dlat) degrees per
/// column. Latitude and longitude are both linear in the column, so phi'' =
/// 0. `coord_mag` bounds the magnitude of the terms summed into each
/// coordinate (it sizes their rounding). `None` if a latitude leaves
/// [-90, 90].
pub(crate) fn wgs84_line_bounds(
    start: (f64, f64),
    end: (f64, f64),
    step: (f64, f64),
    coord_mag: f64,
) -> Option<RowPathBounds> {
    let range = -90.0..=90.0;
    if !range.contains(&start.1) || !range.contains(&end.1) {
        return None;
    }
    let (lat_lo, lat_hi) = (start.1.min(end.1), start.1.max(end.1));
    let phi1 = step.1.abs().to_radians();
    let lam1 = step.0.abs().to_radians();
    // A few ulps of `coord_mag` on each coordinate.
    let noise = 2.0 * (8.0 * f64::EPSILON * coord_mag).to_radians();
    latlng_path_bounds(
        (lat_lo.to_radians(), lat_hi.to_radians()),
        |_| phi1,
        |_, _| 0.0,
        lam1,
        noise,
    )
}

/// Bounds for Web Mercator locations along a straight line of projected
/// coordinates from `start` to `end` (metres), advancing by `step` = (dx,
/// dy) metres per column. `coord_mag` bounds the magnitude of the terms
/// summed into each coordinate.
///
/// lambda = x / R is linear; phi = gd(psi) with psi = y / R linear, so
/// phi' = cos phi psi' and phi'' = -sin phi cos phi psi'². `None` beyond
/// ±89° (no Web Mercator data goes there), which also keeps the per-pixel
/// latitude filter from ever rejecting a location in the line.
pub(crate) fn mercator_line_bounds(
    start: (f64, f64),
    end: (f64, f64),
    step: (f64, f64),
    coord_mag: f64,
) -> Option<RowPathBounds> {
    let (lat0, lat1) = (web_mercator_lat(start.1), web_mercator_lat(end.1));
    if !(lat0.abs() <= 89.0 && lat1.abs() <= 89.0) {
        return None;
    }
    let psi1 = step.1.abs() / WGS84_A;
    let lam1 = step.0.abs() / WGS84_A;
    let psi_max = start.1.abs().max(end.1.abs()) / WGS84_A;
    // x and y round by a few ulps of `coord_mag`; then psi = y / R, exp,
    // atan, the subtraction of pi/2 and the degree conversions.
    let noise = 2.0 * 8.0 * f64::EPSILON * coord_mag / WGS84_A
        + 4.0 * f64::EPSILON * (psi_max + 1.0)
        + 16.0 * f64::EPSILON * std::f64::consts::PI;
    latlng_path_bounds(
        (lat0.min(lat1).to_radians(), lat0.max(lat1).to_radians()),
        |cos_max| cos_max * psi1,
        |cos_max, sin_max| sin_max * cos_max * psi1 * psi1,
        lam1,
        noise,
    )
}
