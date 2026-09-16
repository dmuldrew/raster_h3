//! Coordinate Reference System (CRS) transformations to WGS84.
//!
//! This module implements a 3-tier CRS transformation hierarchy:
//!
//! - **Tier 1: `Wgs84Identity`** — Zero-cost passthrough for EPSG:4326/4269 data.
//! - **Tier 2: `WebMercatorFast` and `AlbersConic`** — Fast analytical transformations with direct formulas (no PROJ4 overhead).
//! - **Tier 3: `Proj4`** — General-purpose fallback using `proj4rs` for arbitrary CRS.

use crate::error::{RasterH3Error, Result};
use proj4rs::proj::Proj;

const WGS84_A: f64 = 6378137.0; // WGS84 semi-major axis in meters
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Precomputed constants for analytical, closed-form inverse Albers Equal Area Conic projection
#[derive(Debug, Clone, Copy)]
pub struct AlbersConicFast {
    /// Latitude of origin in radians.
    pub lat_origin_rad: f64,
    /// Longitude of central meridian in radians.
    pub lon_origin_rad: f64,
    /// False easting in meters.
    pub x_0: f64,
    /// False northing in meters.
    pub y_0: f64,
    /// Cone constant.
    pub n: f64,
    /// Albers constant C.
    pub c: f64,
    /// Polar distance at origin.
    pub rho0: f64,
    /// First eccentricity of the ellipsoid.
    pub e: f64,
    /// Eccentricity squared.
    pub e2: f64,
    /// Semi-major axis in meters.
    pub a: f64,
    /// Authalic latitude parameter at poles.
    pub qp: f64,
}

impl AlbersConicFast {
    /// Initialize with standard 2 parallels and origin (in degrees) on GRS80/WGS84 spheroid
    pub fn new(lat1_deg: f64, lat2_deg: f64, lat0_deg: f64, lon0_deg: f64) -> Self {
        Self::with_offsets(lat1_deg, lat2_deg, lat0_deg, lon0_deg, 0.0, 0.0)
    }

    /// Initialize with standard parallels, origin, and false easting/northing offsets (in meters)
    pub fn with_offsets(
        lat1_deg: f64,
        lat2_deg: f64,
        lat0_deg: f64,
        lon0_deg: f64,
        x_0: f64,
        y_0: f64,
    ) -> Self {
        let a: f64 = 6378137.0; // GRS80/WGS84 semi-major axis
        let f: f64 = 1.0 / 298.257222101; // GRS80 flattening
        let e2: f64 = 2.0 * f - f * f;
        let e: f64 = e2.sqrt();

        let deg_to_rad: f64 = std::f64::consts::PI / 180.0;
        let phi1 = lat1_deg * deg_to_rad;
        let phi2 = lat2_deg * deg_to_rad;
        let phi0 = lat0_deg * deg_to_rad;
        let lam0 = lon0_deg * deg_to_rad;

        let m = |phi: f64| -> f64 {
            let sin_phi = phi.sin();
            phi.cos() / (1.0 - e2 * sin_phi * sin_phi).sqrt()
        };

        let q = |phi: f64| -> f64 {
            let sin_phi = phi.sin();
            let e_sin = e * sin_phi;
            let ratio: f64 = (1.0 - e_sin) / (1.0 + e_sin);
            (1.0 - e2) * (sin_phi / (1.0 - e_sin * e_sin) - (1.0 / (2.0 * e)) * ratio.ln())
        };

        let m1 = m(phi1);
        let m2 = m(phi2);
        let q1 = q(phi1);
        let q2 = q(phi2);
        let q0 = q(phi0);
        let qp = q(std::f64::consts::FRAC_PI_2);

        let n = if (phi1 - phi2).abs() < 1e-10 {
            phi1.sin()
        } else {
            (m1 * m1 - m2 * m2) / (q2 - q1)
        };

        let c = m1 * m1 + n * q1;
        let rho0 = a * (c - n * q0).max(0.0).sqrt() / n;

        Self {
            lat_origin_rad: phi0,
            lon_origin_rad: lam0,
            x_0,
            y_0,
            n,
            c,
            rho0,
            e,
            e2,
            a,
            qp,
        }
    }

    /// Preconfigured for EPSG:5070 (USA_Contiguous_Albers_Equal_Area_Conic)
    pub fn epsg_5070() -> Self {
        Self::new(29.5, 45.5, 23.0, -96.0)
    }

    /// Parse PROJ string parameters for an Albers Equal Area projection (+proj=aea)
    pub fn from_proj_string(src: &str) -> Option<Self> {
        let lower = src.to_lowercase();
        if !lower.contains("proj=aea") {
            return None;
        }

        // Fall back to proj4rs for non-GRS80/WGS84 ellipsoids requiring datum transforms
        if lower.contains("clrk66") || lower.contains("nad27") || lower.contains("bessel") {
            return None;
        }

        let mut lat_1 = None;
        let mut lat_2 = None;
        let mut lat_0 = None;
        let mut lon_0 = None;
        let mut x_0 = 0.0;
        let mut y_0 = 0.0;

        for token in lower.split(|c: char| c.is_whitespace() || c == '+') {
            if token.is_empty() {
                continue;
            }
            if let Some((k, v)) = token.split_once('=') {
                match k.trim() {
                    "lat_1" => lat_1 = v.parse::<f64>().ok(),
                    "lat_2" => lat_2 = v.parse::<f64>().ok(),
                    "lat_0" => lat_0 = v.parse::<f64>().ok(),
                    "lon_0" => lon_0 = v.parse::<f64>().ok(),
                    "x_0" => x_0 = v.parse::<f64>().unwrap_or(0.0),
                    "y_0" => y_0 = v.parse::<f64>().unwrap_or(0.0),
                    _ => {}
                }
            }
        }

        let l2_val = lat_2.or(lat_1);
        let l0_val = lat_0.unwrap_or(0.0);

        match (lat_1, l2_val, lon_0) {
            (Some(l1), Some(l2), Some(ln0)) => {
                Some(Self::with_offsets(l1, l2, l0_val, ln0, x_0, y_0))
            }
            _ => None,
        }
    }

    /// Analytical inverse transformation from projected (x, y) to (lon, lat) in WGS84 degrees
    #[inline(always)]
    pub fn transform_point(&self, x: f64, y: f64) -> (f64, f64) {
        let x_p = x - self.x_0;
        let y_p = self.rho0 - (y - self.y_0);
        let rho = (x_p * x_p + y_p * y_p).sqrt();
        let theta = if self.n >= 0.0 {
            x_p.atan2(y_p)
        } else {
            (-x_p).atan2(-y_p)
        };

        let lon_rad = self.lon_origin_rad + theta / self.n;
        let q = (self.c - (rho * rho * self.n * self.n) / (self.a * self.a)) / self.n;

        // Newton-Raphson inverse for latitude from q
        let sin_beta = (q / self.qp).max(-1.0).min(1.0);
        let mut phi = sin_beta.asin();

        // 2 iterations of Newton-Raphson provide nanometer precision
        for _ in 0..2 {
            let sin_phi = phi.sin();
            let cos_phi = phi.cos();
            if cos_phi.abs() < 1e-12 {
                break;
            }
            let e_sin = self.e * sin_phi;
            let one_minus_e2_sin2 = 1.0 - e_sin * e_sin;
            let ratio: f64 = (1.0 - e_sin) / (1.0 + e_sin);
            let q_curr = (1.0 - self.e2)
                * (sin_phi / one_minus_e2_sin2 - (1.0 / (2.0 * self.e)) * ratio.ln());
            let dq_dphi = 2.0 * (1.0 - self.e2) * cos_phi / (one_minus_e2_sin2 * one_minus_e2_sin2);
            let delta = (q - q_curr) / dq_dphi;
            phi += delta;
            if delta.abs() < 1e-12 {
                break;
            }
        }

        let lon_deg = lon_rad * RAD_TO_DEG;
        let lat_deg = phi * RAD_TO_DEG;
        (lon_deg, lat_deg)
    }
}

/// High-performance CRS to WGS84 coordinate transformer
#[derive(Clone)]
pub enum CrsTransformer {
    /// Native WGS84 (EPSG:4326) - Zero math, zero overhead
    Wgs84Identity,
    /// Fast analytical Web Mercator (EPSG:3857 / EPSG:900913)
    WebMercatorFast,
    /// Fast analytical Albers Equal Area Conic (EPSG:5070 CONUS Albers)
    AlbersConic(AlbersConicFast),
    /// Pure Rust PROJ4 transformation for arbitrary projections
    Proj4 { from: Proj, to: Proj },
}

impl CrsTransformer {
    /// Create a transformer from an optional EPSG code or PROJ string
    pub fn from_crs_or_epsg(epsg: Option<u32>, proj_str: Option<&str>) -> Result<Self> {
        let parsed_epsg = proj_str.and_then(|s| {
            let s = s.trim();
            if let Some(rest) = s.strip_prefix("EPSG:").or_else(|| s.strip_prefix("epsg:")) {
                rest.trim().parse::<u32>().ok()
            } else if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
                s.parse::<u32>().ok()
            } else {
                None
            }
        });

        let effective_epsg = epsg.or(parsed_epsg);

        if let Some(code) = effective_epsg {
            match code {
                4326 | 4269 => return Ok(Self::Wgs84Identity),
                3857 | 900913 | 3785 => return Ok(Self::WebMercatorFast),
                5070 => return Ok(Self::AlbersConic(AlbersConicFast::epsg_5070())),
                3338 => {
                    return Ok(Self::AlbersConic(AlbersConicFast::new(
                        55.0, 65.0, 50.0, -154.0,
                    )))
                }
                32601..=32660 => {
                    let zone = code - 32600;
                    let p_str = format!("+proj=utm +zone={} +datum=WGS84 +units=m +no_defs", zone);
                    return Self::from_proj_string(&p_str);
                }
                32701..=32760 => {
                    let zone = code - 32700;
                    let p_str = format!(
                        "+proj=utm +zone={} +south +datum=WGS84 +units=m +no_defs",
                        zone
                    );
                    return Self::from_proj_string(&p_str);
                }
                _ => {
                    let p_str = format!("+init=epsg:{}", code);
                    if let Ok(transformer) = Self::from_proj_string(&p_str) {
                        return Ok(transformer);
                    }
                    return Err(RasterH3Error::UnsupportedEpsg {
                        code,
                        detail: format!("Unsupported or unrecognized EPSG code: {}", code),
                    });
                }
            }
        }

        if let Some(s) = proj_str {
            let trimmed = s.trim();
            let lower = trimmed.to_lowercase();
            if lower.contains("longlat")
                || lower.contains("latlong")
                || lower == "epsg:4326"
                || lower == "4326"
            {
                return Ok(Self::Wgs84Identity);
            }
            if lower.contains("3857")
                || lower.contains("900913")
                || (lower.contains("proj=merc") && lower.contains("a=6378137"))
            {
                return Ok(Self::WebMercatorFast);
            }
            if let Some(albers) = AlbersConicFast::from_proj_string(trimmed) {
                return Ok(Self::AlbersConic(albers));
            }
            if lower.contains("5070") {
                return Ok(Self::AlbersConic(AlbersConicFast::epsg_5070()));
            }
            return Self::from_proj_string(trimmed);
        }

        Err(RasterH3Error::CrsNotDetected(
            "No CRS detected in raster metadata. A CRS must be specified explicitly (e.g. crs := 'EPSG:4326', crs := 'EPSG:5070').".into(),
        ))
    }

    /// Construct from arbitrary PROJ string to WGS84
    pub fn from_proj_string(src_proj: &str) -> Result<Self> {
        let trimmed = src_proj.trim();
        let lower = trimmed.to_lowercase();
        if lower.contains("longlat")
            || lower.contains("latlong")
            || lower == "epsg:4326"
            || lower == "4326"
        {
            return Ok(Self::Wgs84Identity);
        }
        if lower.contains("3857")
            || lower.contains("900913")
            || (lower.contains("proj=merc") && lower.contains("a=6378137"))
        {
            return Ok(Self::WebMercatorFast);
        }
        if let Some(albers) = AlbersConicFast::from_proj_string(trimmed) {
            return Ok(Self::AlbersConic(albers));
        }
        if lower.contains("5070") {
            return Ok(Self::AlbersConic(AlbersConicFast::epsg_5070()));
        }

        let from = Proj::from_proj_string(src_proj).map_err(|e| {
            RasterH3Error::CrsError(format!(
                "Failed to parse source PROJ string '{}': {:?}",
                src_proj, e
            ))
        })?;
        let to = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").map_err(|e| {
            RasterH3Error::CrsError(format!(
                "Failed to initialize WGS84 target projection: {:?}",
                e
            ))
        })?;

        Ok(Self::Proj4 { from, to })
    }

    /// Transform a single (x, y) point to (lon, lat) in WGS84 degrees
    #[inline(always)]
    pub fn transform_point(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        match self {
            Self::Wgs84Identity => Ok((x, y)),
            Self::WebMercatorFast => {
                let lon = (x / WGS84_A) * RAD_TO_DEG;
                let lat =
                    (2.0 * (y / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
                Ok((lon, lat))
            }
            Self::AlbersConic(albers) => Ok(albers.transform_point(x, y)),
            Self::Proj4 { from, to } => {
                let mut point_3d = (x, y, 0.0);
                proj4rs::transform::transform(from, to, &mut point_3d).map_err(|e| {
                    RasterH3Error::CrsError(format!(
                        "Reprojection error for point ({}, {}): {:?}",
                        x, y, e
                    ))
                })?;
                // proj4rs outputs radians for longlat
                let lon_deg = point_3d.0 * RAD_TO_DEG;
                let lat_deg = point_3d.1 * RAD_TO_DEG;
                Ok((lon_deg, lat_deg))
            }
        }
    }

    /// Transform a batch of (x, y) coordinates into destination (lon, lat) slices
    #[inline]
    pub fn transform_batch(
        &self,
        xs: &[f64],
        ys: &[f64],
        out_lon: &mut [f64],
        out_lat: &mut [f64],
    ) -> Result<()> {
        let count = xs.len().min(ys.len()).min(out_lon.len()).min(out_lat.len());
        for i in 0..count {
            let (lon, lat) = self.transform_point(xs[i], ys[i])?;
            out_lon[i] = lon;
            out_lat[i] = lat;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wgs84_identity() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(4326), None).unwrap();
        let (lon, lat) = tf.transform_point(-122.4194, 37.7749).unwrap();
        assert!((lon - -122.4194).abs() < 1e-6);
        assert!((lat - 37.7749).abs() < 1e-6);
    }

    #[test]
    fn test_web_mercator() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
        // San Francisco Web Mercator coords
        let (lon, lat) = tf.transform_point(-13627665.27, 4547675.35).unwrap();
        assert!((lon - -122.4194).abs() < 1e-2);
        assert!((lat - 37.7749).abs() < 1e-2);
    }

    #[test]
    fn test_albers_epsg5070_accuracy() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
        // Point in Washington DC area: (x = 1580000.0, y = 1940000.0) in EPSG:5070
        let (lon, lat) = tf.transform_point(1580000.0, 1940000.0).unwrap();
        // Should be approximately Lon -77.0, Lat 38.9
        assert!((lon - (-77.05)).abs() < 0.5);
        assert!((lat - 38.88).abs() < 0.5);

        // Compare against PROJ4 reference
        let proj4_tf = CrsTransformer::from_proj_string("+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=GRS80 +units=m +no_defs").unwrap();
        let (p4_lon, p4_lat) = proj4_tf.transform_point(1580000.0, 1940000.0).unwrap();

        assert!(
            (lon - p4_lon).abs() < 1e-5,
            "Lon mismatch: {} vs {}",
            lon,
            p4_lon
        );
        assert!(
            (lat - p4_lat).abs() < 1e-5,
            "Lat mismatch: {} vs {}",
            lat,
            p4_lat
        );
    }

    #[test]
    fn test_utm_reprojection() {
        // UTM Zone 32N (EPSG:32632)
        let tf = CrsTransformer::from_crs_or_epsg(Some(32632), None).unwrap();
        let (lon, lat) = tf.transform_point(500000.0, 4500000.0).unwrap();
        assert!((lon - 9.0).abs() < 0.1);
        assert!((lat - 40.65).abs() < 0.5);
    }

    #[test]
    fn test_hawaii_albers_parsing_and_accuracy() {
        let proj_str = "+proj=aea +lat_1=8 +lat_2=18 +lat_0=13 +lon_0=-157 +x_0=0 +y_0=0 +datum=NAD83 +units=m +no_defs";
        let tf = CrsTransformer::from_crs_or_epsg(None, Some(proj_str)).unwrap();

        // Verify it was parsed as AlbersConicFast
        match tf {
            CrsTransformer::AlbersConic(_) => {}
            _ => panic!("Expected AlbersConicFast for Hawaii Albers PROJ string"),
        }

        // Test origin point (0, 0) -> should be exactly (-157.0, 13.0)
        let (lon, lat) = tf.transform_point(0.0, 0.0).unwrap();
        assert!((lon - -157.0).abs() < 1e-6);
        assert!((lat - 13.0).abs() < 1e-6);
    }

    #[test]
    fn test_antimeridian_utm_zone_1_and_60_continuity() {
        // UTM Zone 1N (EPSG:32601): central meridian = -177°, spans -180° to -174°
        let tf_z1 = CrsTransformer::from_crs_or_epsg(Some(32601), None).unwrap();
        let (lon_cm, lat_cm) = tf_z1.transform_point(500000.0, 0.0).unwrap();
        assert!(
            (lon_cm - -177.0).abs() < 1e-3,
            "Zone 1 central meridian should be -177°"
        );
        assert!(lat_cm.abs() < 1e-3, "Equator latitude should be 0°");

        // West edge of Zone 1 near the antimeridian: spans -180° to -174°
        // Note: PROJ4 normalizes -180° and +180° to the antimeridian meridian
        let (lon_west, lat_w) = tf_z1.transform_point(166021.0, 0.0).unwrap();
        assert!(
            (lon_west.abs() - 180.0).abs() < 0.05,
            "West edge should be at antimeridian (±180°), got {}",
            lon_west
        );
        assert!(lat_w.abs() < 1e-3);

        // Point inside Zone 1: x = 250000m (approx -179.24°)
        let (lon_in1, _) = tf_z1.transform_point(250000.0, 0.0).unwrap();
        assert!(
            lon_in1 > -180.0 && lon_in1 < -177.0,
            "Inside Zone 1 should be between -180° and -177°: got {}",
            lon_in1
        );

        // UTM Zone 60N (EPSG:32660): central meridian = +177°, spans +174° to +180°
        let tf_z60 = CrsTransformer::from_crs_or_epsg(Some(32660), None).unwrap();
        let (lon_cm60, lat_cm60) = tf_z60.transform_point(500000.0, 0.0).unwrap();
        assert!(
            (lon_cm60 - 177.0).abs() < 1e-3,
            "Zone 60 central meridian should be +177°"
        );
        assert!(lat_cm60.abs() < 1e-3, "Equator latitude should be 0°");

        // East edge of Zone 60 near +180° antimeridian
        let (lon_east, _) = tf_z60.transform_point(833978.0, 0.0).unwrap();
        assert!(
            (lon_east.abs() - 180.0).abs() < 0.05,
            "East edge should be at antimeridian (±180°), got {}",
            lon_east
        );

        // Point inside Zone 60: x = 750000m (approx +179.24°)
        let (lon_in60, _) = tf_z60.transform_point(750000.0, 0.0).unwrap();
        assert!(
            lon_in60 > 177.0 && lon_in60 < 180.0,
            "Inside Zone 60 should be between +177° and +180°: got {}",
            lon_in60
        );
    }

    #[test]
    fn test_polar_stereographic_and_latitude_bounds() {
        // South Pole Stereographic (+proj=stere +lat_0=-90)
        let sp_proj = "+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
        let tf_sp = CrsTransformer::from_proj_string(sp_proj).unwrap();
        let (_sp_lon, sp_lat) = tf_sp.transform_point(0.0, 0.0).unwrap();
        assert!(
            (sp_lat - -90.0).abs() < 1e-5,
            "South pole latitude should be -90°: got {}",
            sp_lat
        );

        // North Pole Stereographic (+proj=stere +lat_0=90)
        let np_proj = "+proj=stere +lat_0=90 +lat_ts=71 +lon_0=0 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
        let tf_np = CrsTransformer::from_proj_string(np_proj).unwrap();
        let (_np_lon, np_lat) = tf_np.transform_point(0.0, 0.0).unwrap();
        assert!(
            (np_lat - 90.0).abs() < 1e-5,
            "North pole latitude should be +90°: got {}",
            np_lat
        );

        // Web Mercator extreme Y points approach ±85.051129° without NaN
        let tf_wm = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
        let (_lon_n, lat_n) = tf_wm.transform_point(0.0, 20000000.0).unwrap();
        assert!(!lat_n.is_nan());
        assert!(
            lat_n > 85.0 && lat_n <= 90.0,
            "Latitude must remain bounded: {}",
            lat_n
        );

        let (_lon_s, lat_s) = tf_wm.transform_point(0.0, -20000000.0).unwrap();
        assert!(!lat_s.is_nan());
        assert!(
            lat_s < -85.0 && lat_s >= -90.0,
            "Latitude must remain bounded: {}",
            lat_s
        );
    }

    #[test]
    fn test_malformed_proj_strings_and_error_handling() {
        let invalid_strings = [
            "+proj=nonexistent_projection_abc_123",
            "completely invalid syntax without plus",
            "+proj=utm +zone=999 +datum=WGS84",
        ];

        for s in &invalid_strings {
            let res = CrsTransformer::from_proj_string(s);
            assert!(
                res.is_err(),
                "Expected error for invalid PROJ string: {}",
                s
            );
            match res.err().unwrap() {
                RasterH3Error::CrsError(msg) => {
                    assert!(!msg.is_empty(), "CrsError must contain diagnostic message");
                }
                other => panic!("Expected RasterH3Error::CrsError, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_transform_batch_bounds_safety() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(4326), None).unwrap();
        let xs = [10.0, 20.0, 30.0, 40.0, 50.0];
        let ys = [1.0, 2.0, 3.0, 4.0, 5.0];

        // Mismatched destination buffer: smaller than inputs
        let mut out_lon = [0.0; 3];
        let mut out_lat = [0.0; 3];
        tf.transform_batch(&xs, &ys, &mut out_lon, &mut out_lat)
            .unwrap();

        assert_eq!(out_lon, [10.0, 20.0, 30.0]);
        assert_eq!(out_lat, [1.0, 2.0, 3.0]);

        // Empty slices
        let mut empty_lon = [];
        let mut empty_lat = [];
        tf.transform_batch(&[], &[], &mut empty_lon, &mut empty_lat)
            .unwrap();
    }

    #[test]
    fn test_unsupported_epsg_error() {
        let res = CrsTransformer::from_crs_or_epsg(Some(999999), None);
        assert!(res.is_err(), "Expected error for unsupported EPSG code");
        match res.err().unwrap() {
            RasterH3Error::UnsupportedEpsg { code, detail } => {
                assert_eq!(code, 999999);
                assert!(detail.contains("999999"));
            }
            other => panic!("Expected RasterH3Error::UnsupportedEpsg, got {:?}", other),
        }
    }
}
