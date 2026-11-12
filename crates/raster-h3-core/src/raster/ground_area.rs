//! Ellipsoidal ground area for axis-aligned WGS84 geographic rectangles.
//!
//! These helpers calculate pixel footprints only. They do not enable area-weighted
//! aggregation or calculate exact intersections with H3 cell boundaries.

use crate::error::{RasterH3Error, Result};
use crate::raster::geotransform::GeoTransform;

const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;
const WGS84_E2: f64 = WGS84_F * (2.0 - WGS84_F);

fn invalid(message: &str) -> RasterH3Error {
    RasterH3Error::InvalidParameter(format!("invalid WGS84 footprint: {message}"))
}

/// Area in square metres of a WGS84 rectangle bounded by meridians and parallels.
///
/// Longitude and latitude are degrees. Longitudes must lie in [-180, 180]; an
/// east longitude smaller than west denotes a crossing of the antimeridian.
/// A full longitude band is expressed as west = -180 and east = 180. Latitude
/// must satisfy -90 <= south < north <= 90. Empty and ambiguous zero-width
/// rectangles are rejected. The footprint edges follow parallels, not geodesics.
pub fn wgs84_rectangle_area(west: f64, south: f64, east: f64, north: f64) -> Result<f64> {
    if ![west, south, east, north].iter().all(|v| v.is_finite()) {
        return Err(invalid("coordinates must be finite"));
    }
    if !(-180.0..=180.0).contains(&west) || !(-180.0..=180.0).contains(&east) {
        return Err(invalid("longitude must be in [-180, 180]"));
    }
    if south < -90.0 || north > 90.0 || south >= north {
        return Err(invalid("latitude bounds must satisfy -90 <= south < north <= 90"));
    }
    let mut width = east - west;
    if width < 0.0 {
        width += 360.0;
    }
    if width <= 0.0 {
        return Err(invalid("longitude width must be positive"));
    }

    // Integrate a^2(1-e^2) cos(phi)/(1-e^2 sin^2(phi))^2 over
    // latitude and longitude. Express the antiderivative difference using
    // sin(north)-sin(south) so small pixels do not subtract near-equal areas.
    let span = (north - south).to_radians();
    let midpoint = (north + south) * 0.5;
    let cos_mid = if midpoint > 45.0 {
        (((90.0 - north) + (90.0 - south)) * 0.5)
            .to_radians()
            .sin()
    } else if midpoint < -45.0 {
        (((90.0 + north) + (90.0 + south)) * 0.5)
            .to_radians()
            .sin()
    } else {
        midpoint.to_radians().cos()
    };
    let sin_difference = 2.0 * cos_mid * (span * 0.5).sin();
    let u = south.to_radians().sin();
    let v = north.to_radians().sin();
    let cross = 1.0 - WGS84_E2 * u * v;
    let e = WGS84_E2.sqrt();
    let t = e * sin_difference / cross;
    let atanh_ratio = if t == 0.0 { 1.0 } else { t.atanh() / t };
    let rational = (1.0 + WGS84_E2 * u * v)
        / ((1.0 - WGS84_E2 * u * u) * (1.0 - WGS84_E2 * v * v));
    let integral = 0.5 * sin_difference * (rational + atanh_ratio / cross);
    Ok(WGS84_A * WGS84_A * (1.0 - WGS84_E2) * width.to_radians() * integral)
}

/// Area of a pixel from a north-up, axis-aligned WGS84 geographic transform.
///
/// The caller must establish that the raster CRS is WGS84. Longitudes may use
/// a 0..360 representation, but pixel widths must be in (0, 360] degrees.
/// Rotated, sheared, west-up and south-up transforms are rejected.
pub fn wgs84_pixel_area(gt: &GeoTransform, col: usize, row: usize) -> Result<f64> {
    if ![gt.a, gt.b, gt.c0, gt.d, gt.e, gt.f0]
        .iter()
        .all(|v| v.is_finite())
        || gt.b != 0.0
        || gt.d != 0.0
        || !(gt.a > 0.0 && gt.a <= 360.0)
        || gt.e >= 0.0
    {
        return Err(invalid("expected a finite north-up, axis-aligned transform"));
    }
    let west_unwrapped = gt.c0 + col as f64 * gt.a;
    let north = gt.f0 + row as f64 * gt.e;
    let south = north + gt.e;
    if !west_unwrapped.is_finite() || !north.is_finite() || !south.is_finite() {
        return Err(invalid("pixel coordinates are not finite"));
    }
    if gt.a == 360.0 {
        return wgs84_rectangle_area(-180.0, south, 180.0, north);
    }
    let west = (west_unwrapped + 180.0).rem_euclid(360.0) - 180.0;
    let east_unwrapped = west + gt.a;
    let east = if east_unwrapped > 180.0 {
        east_unwrapped - 360.0
    } else {
        east_unwrapped
    };
    wgs84_rectangle_area(west, south, east, north)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsoidal_area_matches_high_precision_reference() {
        // Independent 70-digit mpmath evaluation of the ellipsoidal surface
        // integral with WGS84 a=6378137 and 1/f=298.257223563.
        let cases = [
            (0.0, 0.0, 1.0, 1.0, 12_308_463_893.975_353),
            (0.0, 59.0, 1.0, 60.0, 6_309_805_669.030_524),
            (0.0, 89.0, 1.0, 90.0, 108_866_681.636_207_67),
            (179.5, -1.0, -179.5, 1.0, 24_616_927_787.950_706),
            (-180.0, -90.0, 180.0, 90.0, 510_065_621_724_088.5),
            (0.0, 89.999_999, 0.000_001, 90.0, 1.088_696_687_240_833_9e-10),
        ];
        for (west, south, east, north, expected) in cases {
            let actual = wgs84_rectangle_area(west, south, east, north).unwrap();
            assert!(
                (actual - expected).abs() <= expected * 1e-8,
                "{west} {south} {east} {north}: {actual} vs {expected}"
            );
        }
    }

    #[test]
    fn pixel_area_wraps_longitudes_and_rejects_unsupported_geometry() {
        let mut gt = GeoTransform::from_gdal_array([179.5, 1.0, 0.0, 1.0, 0.0, -2.0]);
        let expected = wgs84_rectangle_area(179.5, -1.0, -179.5, 1.0).unwrap();
        assert!((wgs84_pixel_area(&gt, 0, 0).unwrap() - expected).abs() < 1e-5);
        gt.c0 = -180.5;
        assert!((wgs84_pixel_area(&gt, 0, 0).unwrap() - expected).abs() < 1e-5);
        gt.b = 0.01;
        assert!(wgs84_pixel_area(&gt, 0, 0).is_err());
        gt.b = 0.0;
        gt.e = 1.0;
        assert!(wgs84_pixel_area(&gt, 0, 0).is_err());
    }

    #[test]
    fn rejects_invalid_bounds_and_preserves_area_additivity() {
        for (west, south, east, north) in [
            (0.0, -91.0, 1.0, 0.0),
            (0.0, 1.0, 1.0, 1.0),
            (0.0, 0.0, 0.0, 1.0),
            (180.0, 0.0, -180.0, 1.0),
            (f64::NAN, 0.0, 1.0, 1.0),
        ] {
            assert!(wgs84_rectangle_area(west, south, east, north).is_err());
        }
        let whole = wgs84_rectangle_area(-1.0, 10.0, 1.0, 12.0).unwrap();
        let parts = wgs84_rectangle_area(-1.0, 10.0, 1.0, 11.0).unwrap()
            + wgs84_rectangle_area(-1.0, 11.0, 1.0, 12.0).unwrap();
        assert!((whole - parts).abs() < whole * 1e-14);
    }
}
