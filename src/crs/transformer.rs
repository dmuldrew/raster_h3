//! Coordinate Reference System (CRS) transformations to WGS84.
//!
//! This module implements a 3-tier CRS transformation hierarchy:
//!
//! - **Tier 1: `Wgs84Identity`** — Zero-cost passthrough for EPSG:4326/4269 data.
//! - **Tier 2: `WebMercatorFast` and `AlbersConic`** — Fast analytical transformations with direct formulas (no PROJ4 overhead).
//! - **Tier 3: `Proj4`** — General-purpose fallback using `proj4rs` for arbitrary CRS.

use crate::error::{RasterH3Error, Result};
use crate::raster::geotransform::GeoTransform;
use proj4rs::proj::Proj;

const WGS84_A: f64 = 6378137.0; // WGS84 semi-major axis in meters
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Normalize longitude to [-180.0, 180.0) degrees
#[inline]
pub fn wrap_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

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

/// Tokenize a PROJ string into key-value pairs.
pub(crate) fn tokenize_proj_string(src: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for token in src.split_whitespace() {
        let token = token.trim().trim_start_matches('+');
        if token.is_empty() {
            continue;
        }
        if let Some((k, v)) = token.split_once('=') {
            map.insert(k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
        } else {
            map.insert(token.to_ascii_lowercase(), String::new());
        }
    }
    map
}

/// Helper to check if a +towgs84 parameter list is effectively all-zero (e.g., "0,0,0" or "0,0,0,0,0,0,0").
fn is_all_zero_towgs84(val: &str) -> bool {
    let mut count = 0;
    for part in val.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<f64>() {
            Ok(num) => {
                if num != 0.0 {
                    return false;
                }
                count += 1;
            }
            Err(_) => return false,
        }
    }
    count > 0
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

    /// Parse PROJ string parameters for an Albers Equal Area projection (+proj=aea) from token map
    pub fn from_proj_tokens(tokens: &std::collections::HashMap<String, String>) -> Option<Self> {
        if tokens.get("proj").map(|s| s.as_str()) != Some("aea") {
            return None;
        }

        // Must NOT have nadgrids (unless @null or empty)
        if let Some(nadgrids) = tokens.get("nadgrids") {
            if nadgrids != "@null" && !nadgrids.is_empty() {
                return None;
            }
        }

        // Must NOT have towgs84 (unless all-zero)
        if let Some(towgs84) = tokens.get("towgs84") {
            if !is_all_zero_towgs84(towgs84) {
                return None;
            }
        }

        // Accept the fast path only when the ellipsoid is GRS80 or WGS84
        // (by ellps=, datum=NAD83|WGS84, or a≈6378137 && rf≈298.257).
        let ellps = tokens.get("ellps").map(|s| s.as_str());
        let datum = tokens.get("datum").map(|s| s.as_str());

        let a_val = tokens.get("a").and_then(|v| v.parse::<f64>().ok());
        let rf_val = tokens
            .get("rf")
            .and_then(|v| v.parse::<f64>().ok())
            .or_else(|| {
                tokens
                    .get("f")
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|f| 1.0 / f)
            });

        // If an explicit incompatible ellps or datum is present, reject fast path
        if let Some(e) = ellps {
            if e != "grs80" && e != "wgs84" {
                return None;
            }
        }
        if let Some(d) = datum {
            if d != "nad83" && d != "wgs84" {
                return None;
            }
        }

        let is_grs80_or_wgs84_ellps = match ellps {
            Some("grs80") | Some("wgs84") => true,
            _ => false,
        };

        let is_grs80_or_wgs84_datum = match datum {
            Some("nad83") | Some("wgs84") => true,
            _ => false,
        };

        let is_grs80_or_wgs84_params = match (a_val, rf_val) {
            (Some(a), Some(rf)) => (a - 6378137.0).abs() < 1.0 && (rf - 298.257).abs() < 0.02,
            _ => false,
        };

        let is_valid_ellipsoid =
            is_grs80_or_wgs84_ellps || is_grs80_or_wgs84_datum || is_grs80_or_wgs84_params;

        if !is_valid_ellipsoid {
            return None;
        }

        let lat_1 = tokens.get("lat_1").and_then(|v| v.parse::<f64>().ok());
        let lat_2 = tokens.get("lat_2").and_then(|v| v.parse::<f64>().ok());
        let lat_0 = tokens.get("lat_0").and_then(|v| v.parse::<f64>().ok());
        let lon_0 = tokens.get("lon_0").and_then(|v| v.parse::<f64>().ok());
        let x_0 = tokens
            .get("x_0")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        let y_0 = tokens
            .get("y_0")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

        let l2_val = lat_2.or(lat_1);
        let l0_val = lat_0.unwrap_or(0.0);

        match (lat_1, l2_val, lon_0) {
            (Some(l1), Some(l2), Some(ln0)) => {
                Some(Self::with_offsets(l1, l2, l0_val, ln0, x_0, y_0))
            }
            _ => None,
        }
    }

    /// Parse PROJ string parameters for an Albers Equal Area projection (+proj=aea)
    pub fn from_proj_string(src: &str) -> Option<Self> {
        let tokens = tokenize_proj_string(src);
        Self::from_proj_tokens(&tokens)
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
            return Self::from_epsg_code(code);
        }

        if let Some(s) = proj_str {
            return Self::from_proj_string(s);
        }

        Err(RasterH3Error::CrsNotDetected(
            "No CRS detected in raster metadata. A CRS must be specified explicitly (e.g. crs := 'EPSG:4326', crs := 'EPSG:5070').".into(),
        ))
    }

    /// Resolve an EPSG code to a fast-path or Proj4 transformer.
    pub fn from_epsg_code(code: u32) -> Result<Self> {
        match code {
            // EPSG:4326 is WGS84 native.
            // EPSG:4269 is NAD83 (GRS80 ellipsoid). Treated as Wgs84Identity, accepting the
            // ~1–2 m continental difference between NAD83 and WGS84 as proj4rs does not
            // support grid-shift datum transformations.
            4326 | 4269 => Ok(Self::Wgs84Identity),
            3857 | 900913 | 3785 => Ok(Self::WebMercatorFast),
            5070 => Ok(Self::AlbersConic(AlbersConicFast::epsg_5070())),
            3338 => Ok(Self::AlbersConic(AlbersConicFast::new(
                55.0, 65.0, 50.0, -154.0,
            ))),
            32601..=32660 => {
                let zone = code - 32600;
                let p_str = format!("+proj=utm +zone={} +datum=WGS84 +units=m +no_defs", zone);
                Self::from_proj_string(&p_str)
            }
            32701..=32760 => {
                let zone = code - 32700;
                let p_str = format!(
                    "+proj=utm +zone={} +south +datum=WGS84 +units=m +no_defs",
                    zone
                );
                Self::from_proj_string(&p_str)
            }
            3413 => {
                let p_str = "+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
                Self::from_proj_string(p_str)
            }
            3031 => {
                let p_str = "+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
                Self::from_proj_string(p_str)
            }
            _ => {
                let p_str = format!("+init=epsg:{}", code);
                let from =
                    Proj::from_proj_string(&p_str).map_err(|_| RasterH3Error::UnsupportedEpsg {
                        code,
                        detail: format!("Unsupported or unrecognized EPSG code: {}", code),
                    })?;
                let to =
                    Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").map_err(|e| {
                        RasterH3Error::CrsError(format!(
                            "Failed to initialize WGS84 target projection: {:?}",
                            e
                        ))
                    })?;
                Ok(Self::Proj4 { from, to })
            }
        }
    }

    /// Construct from arbitrary PROJ string to WGS84
    pub fn from_proj_string(src_proj: &str) -> Result<Self> {
        let trimmed = src_proj.trim();

        // Plain EPSG:NNNN or bare integer check
        let parsed_epsg = if let Some(rest) = trimmed
            .strip_prefix("EPSG:")
            .or_else(|| trimmed.strip_prefix("epsg:"))
        {
            rest.trim().parse::<u32>().ok()
        } else if trimmed.chars().all(|c| c.is_ascii_digit()) && !trimmed.is_empty() {
            trimmed.parse::<u32>().ok()
        } else {
            None
        };

        if let Some(code) = parsed_epsg {
            return Self::from_epsg_code(code);
        }

        let tokens = tokenize_proj_string(trimmed);

        // Reject datum transformations on ANY projection (proj4rs has no grid-shift or datum-shift support)
        if let Some(towgs84) = tokens.get("towgs84") {
            if !is_all_zero_towgs84(towgs84) {
                return Err(RasterH3Error::CrsError(format!(
                    "Datum transformations via Helmert shifts (+towgs84={}) are not supported",
                    towgs84
                )));
            }
        }
        if let Some(nadgrids) = tokens.get("nadgrids") {
            if nadgrids != "@null" && !nadgrids.is_empty() {
                return Err(RasterH3Error::CrsError(format!(
                    "Datum transformations via grid shifts (+nadgrids={}) are not supported",
                    nadgrids
                )));
            }
        }

        // +init=epsg:NNNN -> treat as EPSG code
        if let Some(init_val) = tokens.get("init") {
            if let Some(code_str) = init_val.strip_prefix("epsg:") {
                if let Ok(code) = code_str.parse::<u32>() {
                    return Self::from_epsg_code(code);
                }
            }
        }

        if let Some(proj) = tokens.get("proj") {
            match proj.as_str() {
                "longlat" | "latlong" => {
                    let valid_datum = match tokens.get("datum").map(|s| s.as_str()) {
                        None | Some("wgs84") | Some("grs80") | Some("nad83") => true,
                        _ => false,
                    };
                    let valid_ellps = match tokens.get("ellps").map(|s| s.as_str()) {
                        None | Some("wgs84") | Some("grs80") => true,
                        _ => false,
                    };
                    let valid_a = tokens
                        .get("a")
                        .and_then(|v| v.parse::<f64>().ok())
                        .map_or(true, |a| (a - WGS84_A).abs() < 1.0);
                    let valid_rf = tokens
                        .get("rf")
                        .and_then(|v| v.parse::<f64>().ok())
                        .map_or(true, |rf| (rf - 298.257).abs() < 0.01);

                    if valid_datum && valid_ellps && valid_a && valid_rf {
                        return Ok(Self::Wgs84Identity);
                    }
                }
                "merc" => {
                    let a_val = tokens.get("a").and_then(|v| v.parse::<f64>().ok());
                    let b_val = tokens.get("b").and_then(|v| v.parse::<f64>().ok());
                    let rf_val = tokens.get("rf");
                    let f_val = tokens.get("f");
                    let ellps_val = tokens.get("ellps").map(|s| s.as_str());

                    let a_is_6378137 = a_val.map_or(false, |a| (a - 6378137.0).abs() < 1e-3);
                    let b_is_6378137 = b_val.map_or(false, |b| (b - 6378137.0).abs() < 1e-3);
                    let absent_rf_f_b = rf_val.is_none() && f_val.is_none() && b_val.is_none();
                    let ellps_sphere_or_absent = match ellps_val {
                        None | Some("sphere") => true,
                        _ => false,
                    };

                    let is_sphere_6378137 =
                        a_is_6378137 && (b_is_6378137 || (absent_rf_f_b && ellps_sphere_or_absent));

                    let lon_0 = tokens
                        .get("lon_0")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let x_0 = tokens
                        .get("x_0")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let y_0 = tokens
                        .get("y_0")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let k = tokens
                        .get("k")
                        .or_else(|| tokens.get("k_0"))
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(1.0);
                    let lat_ts = tokens
                        .get("lat_ts")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(0.0);

                    let is_standard_merc = lon_0.abs() < 1e-6
                        && x_0.abs() < 1e-6
                        && y_0.abs() < 1e-6
                        && (k - 1.0).abs() < 1e-6
                        && lat_ts.abs() < 1e-6;

                    if is_sphere_6378137 && is_standard_merc {
                        return Ok(Self::WebMercatorFast);
                    }
                }
                "aea" => {
                    if let Some(albers) = AlbersConicFast::from_proj_tokens(&tokens) {
                        return Ok(Self::AlbersConic(albers));
                    }
                }
                _ => {}
            }
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

    /// Calculate WGS84 [min_lon, min_lat, max_lon, max_lat] spatial bounds for a raster pixel rectangle.
    ///
    /// For EPSG:4326 and EPSG:3857 (Web Mercator), latitude is affine or monotonic in y,
    /// so the 4 corners are mathematically exact extrema.
    ///
    /// For Albers Conic projections, parallels of latitude are circular arcs centered at the cone apex.
    /// Along each bounding edge, this computes the exact closed-form analytical critical point
    /// minimizing distance to the apex, capturing the true maximum latitude at the central meridian.
    ///
    /// For arbitrary PROJ4 projections, boundary edges are densified with interior samples
    /// and a conservative safety headroom is applied to guarantee that `max_lat` is a sound upper bound.
    pub fn transform_rect_bounds(
        &self,
        gt: &GeoTransform,
        col: f64,
        row: f64,
        width: f64,
        height: f64,
    ) -> [f64; 4] {
        let p0 = gt.pixel_to_coord(col, row);
        let p1 = gt.pixel_to_coord(col + width, row);
        let p2 = gt.pixel_to_coord(col + width, row + height);
        let p3 = gt.pixel_to_coord(col, row + height);

        let corners = [p0, p1, p2, p3];

        let mut min_lon = f64::INFINITY;
        let mut min_lat = f64::INFINITY;
        let mut max_lon = f64::NEG_INFINITY;
        let mut max_lat = f64::NEG_INFINITY;

        for &(x, y) in &corners {
            if let Ok((lon, lat)) = self.transform_point(x, y) {
                let w_lon = wrap_lon(lon);
                min_lon = min_lon.min(w_lon);
                max_lon = max_lon.max(w_lon);
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
            }
        }

        match self {
            Self::Wgs84Identity | Self::WebMercatorFast => {
                if gt.b != 0.0 || gt.d != 0.0 {
                    let edges = [
                        (p0, p1, 16usize),
                        (p1, p2, 16usize),
                        (p2, p3, 16usize),
                        (p3, p0, 16usize),
                    ];
                    for ((x1, y1), (x2, y2), steps) in edges {
                        let dx = x2 - x1;
                        let dy = y2 - y1;
                        for step in 1..steps {
                            let t = step as f64 / steps as f64;
                            let px = x1 + t * dx;
                            let py = y1 + t * dy;
                            if let Ok((lon, lat)) = self.transform_point(px, py) {
                                let w_lon = wrap_lon(lon);
                                min_lon = min_lon.min(w_lon);
                                max_lon = max_lon.max(w_lon);
                                min_lat = min_lat.min(lat);
                                max_lat = max_lat.max(lat);
                            }
                        }
                    }
                }
            }
            Self::AlbersConic(albers) => {
                // Parallels are circular arcs centered at the cone apex (x_0, y_0 + rho0).
                let apex_x = albers.x_0;
                let apex_y = albers.y_0 + albers.rho0;

                let edges = [(p0, p1), (p1, p2), (p2, p3), (p3, p0)];
                for ((x1, y1), (x2, y2)) in edges {
                    let dx = x2 - x1;
                    let dy = y2 - y1;
                    let denom = dx * dx + dy * dy;
                    if denom > 1e-12 {
                        let t = -(dx * (x1 - apex_x) + dy * (y1 - apex_y)) / denom;
                        if t > 0.0 && t < 1.0 {
                            let cx = x1 + t * dx;
                            let cy = y1 + t * dy;
                            if let Ok((lon, lat)) = self.transform_point(cx, cy) {
                                let w_lon = wrap_lon(lon);
                                min_lon = min_lon.min(w_lon);
                                max_lon = max_lon.max(w_lon);
                                min_lat = min_lat.min(lat);
                                max_lat = max_lat.max(lat);
                            }
                        }
                    }
                }
            }
            Self::Proj4 { .. } => {
                // Densify boundary edges: 32 samples on horizontal edges, 16 on vertical edges
                let edges = [
                    (p0, p1, 32usize),
                    (p1, p2, 16usize),
                    (p2, p3, 32usize),
                    (p3, p0, 16usize),
                ];
                for ((x1, y1), (x2, y2), steps) in edges {
                    let dx = x2 - x1;
                    let dy = y2 - y1;
                    for step in 1..steps {
                        let t = step as f64 / steps as f64;
                        let px = x1 + t * dx;
                        let py = y1 + t * dy;
                        if let Ok((lon, lat)) = self.transform_point(px, py) {
                            let w_lon = wrap_lon(lon);
                            min_lon = min_lon.min(w_lon);
                            max_lon = max_lon.max(w_lon);
                            min_lat = min_lat.min(lat);
                            max_lat = max_lat.max(lat);
                        }
                    }
                }

                // Densify interior with a 5x5 grid (including exact center at i=3, j=3)
                // to detect interior extrema (e.g., polar stereographic or LAEA projections)
                for i in 1..=5 {
                    let tx = i as f64 / 6.0;
                    for j in 1..=5 {
                        let ty = j as f64 / 6.0;
                        let px = p0.0 * (1.0 - tx) * (1.0 - ty)
                            + p1.0 * tx * (1.0 - ty)
                            + p2.0 * tx * ty
                            + p3.0 * (1.0 - tx) * ty;
                        let py = p0.1 * (1.0 - tx) * (1.0 - ty)
                            + p1.1 * tx * (1.0 - ty)
                            + p2.1 * tx * ty
                            + p3.1 * (1.0 - tx) * ty;
                        if let Ok((lon, lat)) = self.transform_point(px, py) {
                            let w_lon = wrap_lon(lon);
                            min_lon = min_lon.min(w_lon);
                            max_lon = max_lon.max(w_lon);
                            min_lat = min_lat.min(lat);
                            max_lat = max_lat.max(lat);
                        }
                    }
                }

                // If any probe approaches the pole (within ~55km / 0.5°), clamp to full polar bounds
                if max_lat >= 89.5 {
                    max_lat = 90.0;
                    min_lon = -180.0;
                    max_lon = 180.0;
                }
                if min_lat <= -89.5 {
                    min_lat = -90.0;
                    min_lon = -180.0;
                    max_lon = 180.0;
                }

                // Add 0.0005° (~55m) safety margin to max_lat/min_lat for Proj4 projections
                if max_lat.is_finite() && max_lat < 90.0 {
                    max_lat = (max_lat + 0.0005).min(90.0);
                }
                if min_lat.is_finite() && min_lat > -90.0 {
                    min_lat = (min_lat - 0.0005).max(-90.0);
                }
            }
        }

        // Handle rasters spanning across the antimeridian
        if max_lon - min_lon > 180.0 {
            min_lon = -180.0;
            max_lon = 180.0;
        }

        if max_lat == f64::NEG_INFINITY {
            // Every probe failed to project. Return the whole globe rather than a
            // plausible-looking `[0,0,0,0]`: this keeps bbox pruning conservative and
            // sorts the chunk first in the north-to-south horizon order, so no cell can
            // be evicted before the chunk is processed.
            [-180.0, -90.0, 180.0, 90.0]
        } else {
            [min_lon, min_lat, max_lon, max_lat]
        }
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

    #[test]
    fn test_transform_rect_bounds_wgs84_and_mercator() {
        let gt = GeoTransform {
            c0: -120.0,
            a: 0.1,
            b: 0.0,
            f0: 38.0,
            d: 0.0,
            e: -0.1,
        };
        let tf_wgs = CrsTransformer::Wgs84Identity;
        let b_wgs = tf_wgs.transform_rect_bounds(&gt, 0.0, 0.0, 10.0, 10.0);
        assert!((b_wgs[0] - (-120.0)).abs() < 1e-6);
        assert!((b_wgs[1] - 37.0).abs() < 1e-6);
        assert!((b_wgs[2] - (-119.0)).abs() < 1e-6);
        assert!((b_wgs[3] - 38.0).abs() < 1e-6);

        let tf_wm = CrsTransformer::WebMercatorFast;
        let gt_wm = GeoTransform {
            c0: -13000000.0,
            a: 1000.0,
            b: 0.0,
            f0: 4500000.0,
            d: 0.0,
            e: -1000.0,
        };
        let b_wm = tf_wm.transform_rect_bounds(&gt_wm, 0.0, 0.0, 10.0, 10.0);
        assert!(b_wm[3] > b_wm[1]);
        assert!(b_wm[2] > b_wm[0]);
    }

    #[test]
    fn test_transform_rect_bounds_conus_albers_strip_captures_central_meridian() {
        // EPSG:5070 CONUS Albers strip matching BP_CONUS strip row 0
        let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
        let gt = GeoTransform {
            c0: -2362395.0,
            a: 30.0,
            b: 0.0,
            f0: 3267405.0,
            d: 0.0,
            e: -30.0,
        };

        // Strip width: 156335 pixels (~4690 km), height: 1 pixel
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 156335.0, 1.0);
        let max_lat = bounds[3];

        // The top corners (west and east) have latitude around 46.0°:
        let (_, lat_nw) = tf.transform_point(-2362395.0, 3267405.0).unwrap();
        let (_, lat_ne) = tf
            .transform_point(-2362395.0 + 156335.0 * 30.0, 3267405.0)
            .unwrap();
        let (_, lat_cm) = tf.transform_point(0.0, 3267405.0).unwrap();
        let corner_max = lat_nw.max(lat_ne);
        assert!((corner_max - 48.856).abs() < 0.01);
        assert!((lat_cm - 52.482).abs() < 0.01);

        // transform_rect_bounds MUST capture the true maximum latitude at the central meridian,
        // which is ~3.6° higher than the corner maximum:
        assert_eq!(max_lat, lat_cm);
        assert!(
            max_lat > corner_max + 3.0,
            "max_lat ({}) should exceed corner max ({}) by at least 3.0°",
            max_lat,
            corner_max
        );
    }

    #[test]
    fn test_transform_rect_bounds_rotated_albers() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
        // Rotated geotransform: 45 degree rotation
        let angle = std::f64::consts::FRAC_PI_4;
        let cos_a = angle.cos() * 30.0;
        let sin_a = angle.sin() * 30.0;
        let gt_rot = GeoTransform {
            c0: -100000.0,
            a: cos_a,
            b: -sin_a,
            f0: 2000000.0,
            d: sin_a,
            e: cos_a,
        };

        let bounds = tf.transform_rect_bounds(&gt_rot, 0.0, 0.0, 1000.0, 1000.0);
        assert!(bounds[3] > bounds[1]);
        assert!(bounds[2] > bounds[0]);
    }

    #[test]
    fn test_transform_rect_bounds_rotated_wgs84() {
        let tf = CrsTransformer::Wgs84Identity;
        // 45-degree rotated geotransform
        let angle = std::f64::consts::FRAC_PI_4;
        let cos_a = angle.cos();
        let sin_a = angle.sin();
        let gt_rot = GeoTransform {
            c0: 0.0,
            a: cos_a,
            b: -sin_a,
            f0: 45.0,
            d: sin_a,
            e: cos_a,
        };

        // For a 45°-rotated WGS84 grid, corner max of baseline corners
        let p0 = gt_rot.pixel_to_coord(0.0, 0.0);
        let p3 = gt_rot.pixel_to_coord(0.0, 10.0);
        let corner_max = p0.1.max(p3.1);

        let bounds = tf.transform_rect_bounds(&gt_rot, 0.0, 0.0, 10.0, 10.0);
        let max_lat = bounds[3];
        let mid_lat = max_lat;

        assert!((bounds[3] - mid_lat).abs() < 1e-9);
        assert!(bounds[3] > corner_max);
    }

    #[test]
    fn test_transform_rect_bounds_antimeridian_wrap() {
        let tf = CrsTransformer::Wgs84Identity;
        // Raster crossing antimeridian: col 0 at 175°, width 10° -> col 10 at 185° (-175°)
        let gt = GeoTransform {
            c0: 175.0,
            a: 1.0,
            b: 0.0,
            f0: 10.0,
            d: 0.0,
            e: -1.0,
        };
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 10.0, 10.0);
        // Span across 180° exceeds 180° when wrapped (from -175° to 175° naive span is 350°),
        // so bounds must report [-180, 0, 180, 10]
        assert_eq!(bounds[0], -180.0);
        assert_eq!(bounds[2], 180.0);
    }

    #[test]
    fn test_transform_rect_bounds_polar_stereographic() {
        // EPSG:3413 NSIDC Sea Ice Polar Stereographic North
        let tf = CrsTransformer::from_crs_or_epsg(Some(3413), None).unwrap();
        // A chunk containing the North Pole at (0, 0) in projected coords
        let gt = GeoTransform {
            c0: -50000.0,
            a: 1000.0,
            b: 0.0,
            f0: 50000.0,
            d: 0.0,
            e: -1000.0,
        };
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 100.0, 100.0);
        // Pole (lat = 90.0) is interior to this chunk. transform_rect_bounds must capture max_lat = 90.0
        assert_eq!(bounds[3], 90.0, "Polar chunk must report max_lat = 90.0");
        assert_eq!(
            bounds[0], -180.0,
            "Polar chunk must span full longitude [-180, 180]"
        );
        assert_eq!(
            bounds[2], 180.0,
            "Polar chunk must span full longitude [-180, 180]"
        );
    }

    #[test]
    fn test_tokenized_crs_detection_utm_with_5070_in_param() {
        let proj_str = "+proj=utm +zone=17 +x_0=5070000 +datum=WGS84 +units=m +no_defs";
        let tf = CrsTransformer::from_crs_or_epsg(None, Some(proj_str)).unwrap();
        match tf {
            CrsTransformer::Proj4 { .. } => {}
            _ => panic!("Expected Proj4 transformer, got non-Proj4 for UTM with +x_0=5070000"),
        }
    }

    #[test]
    fn test_ellipsoidal_mercator_routes_to_proj4() {
        // cs2cs -f "%.10f" "+proj=merc +a=6378137 +rf=298.257223563 +lon_0=0" +to "+proj=longlat +datum=WGS84 +no_defs" <<< "1113194.9079 6800125.4544"
        // Output: 9.9999999997 52.1864260950 0.0000000000
        let proj_str = "+proj=merc +a=6378137 +rf=298.257223563 +lon_0=0";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::Proj4 { .. } => {}
            _ => panic!("Expected Proj4 transformer for ellipsoidal Mercator"),
        }

        let (lon, lat) = tf.transform_point(1113194.9079, 6800125.4544).unwrap();
        let golden_lon = 9.9999999997;
        let golden_lat = 52.1864260950;

        // Ground distance tolerance <= 0.01 m
        let deg_to_rad = std::f64::consts::PI / 180.0;
        let dlat_m = (lat - golden_lat) * deg_to_rad * 6378137.0;
        let dlon_m = (lon - golden_lon) * deg_to_rad * 6378137.0 * (golden_lat * deg_to_rad).cos();
        let dist_m = (dlat_m * dlat_m + dlon_m * dlon_m).sqrt();
        assert!(
            dist_m <= 0.01,
            "Transformed point ({}, {}) differs from golden cs2cs ({}, {}) by {} m (> 0.01 m)",
            lon,
            lat,
            golden_lon,
            golden_lat,
            dist_m
        );
    }

    #[test]
    fn test_spherical_mercator_with_nadgrids_null_is_fast_path() {
        let proj_str = "+proj=merc +a=6378137 +b=6378137 +k=1 +lon_0=0 +nadgrids=@null";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::WebMercatorFast => {}
            _ => panic!("Expected WebMercatorFast transformer for spherical Mercator"),
        }
    }

    #[test]
    fn test_albers_intl_ellipsoid_routes_to_proj4() {
        // cs2cs -f "%.10f" "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=intl +units=m +no_defs" +to "+proj=longlat +ellps=intl +no_defs" <<< "1000000.0 2000000.0"
        // Golden cs2cs output: -84.0779390722 40.4478144304 0.0000000000
        let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=intl +units=m +no_defs";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::Proj4 { .. } => {}
            _ => {
                panic!("Expected Proj4 transformer for Albers on non-GRS80/WGS84 ellipsoid (intl)")
            }
        }

        let (lon, lat) = tf.transform_point(1000000.0, 2000000.0).unwrap();
        let golden_lon = -84.0779390722;
        let golden_lat = 40.4478144304;

        // Ground distance tolerance <= 0.01 m on International 1924 ellipsoid (a = 6378388.0)
        let a_intl = 6378388.0;
        let deg_to_rad = std::f64::consts::PI / 180.0;
        let dlat_m = (lat - golden_lat) * deg_to_rad * a_intl;
        let dlon_m = (lon - golden_lon) * deg_to_rad * a_intl * (golden_lat * deg_to_rad).cos();
        let dist_m = (dlat_m * dlat_m + dlon_m * dlon_m).sqrt();
        assert!(
            dist_m <= 0.01,
            "Transformed point ({}, {}) differs from golden cs2cs ({}, {}) by {} m (> 0.01 m)",
            lon,
            lat,
            golden_lon,
            golden_lat,
            dist_m
        );
    }

    #[test]
    fn test_albers_grs80_with_zero_towgs84_is_fast_path() {
        let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=GRS80 +towgs84=0,0,0 +units=m +no_defs";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::AlbersConic(_) => {}
            _ => panic!("Expected AlbersConic fast path for GRS80 with all-zero towgs84"),
        }
    }

    #[test]
    fn test_albers_with_nonzero_towgs84_is_error() {
        let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +towgs84=-8,160,176 +units=m +no_defs";
        let res = CrsTransformer::from_proj_string(proj_str);
        assert!(
            res.is_err(),
            "Expected error for nonzero +towgs84 parameters"
        );
        match res.err().unwrap() {
            RasterH3Error::CrsError(msg) => {
                assert!(msg.contains("towgs84") || msg.contains("Helmert"));
            }
            other => panic!("Expected RasterH3Error::CrsError, got {:?}", other),
        }
    }
}
