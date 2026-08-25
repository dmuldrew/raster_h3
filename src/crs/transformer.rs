use proj4rs::proj::Proj;
use crate::error::{RasterH3Error, Result};

const WGS84_A: f64 = 6378137.0; // WGS84 semi-major axis in meters
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// High-performance CRS to WGS84 coordinate transformer
pub enum CrsTransformer {
    /// Native WGS84 (EPSG:4326) - Zero math, zero overhead
    Wgs84Identity,
    /// Fast analytical Web Mercator (EPSG:3857 / EPSG:900913)
    WebMercatorFast,
    /// Pure Rust PROJ4 transformation for arbitrary projections
    Proj4 {
        from: Proj,
        to: Proj,
    },
}

impl CrsTransformer {
    /// Create a transformer from an optional EPSG code or PROJ string
    pub fn from_crs_or_epsg(epsg: Option<u32>, proj_str: Option<&str>) -> Result<Self> {
        if let Some(code) = epsg {
            match code {
                4326 | 4269 => return Ok(Self::Wgs84Identity),
                3857 | 900913 | 3785 => return Ok(Self::WebMercatorFast),
                32601..=32660 => {
                    let zone = code - 32600;
                    let p_str = format!("+proj=utm +zone={} +datum=WGS84 +units=m +no_defs", zone);
                    return Self::from_proj_string(&p_str);
                }
                32701..=32760 => {
                    let zone = code - 32700;
                    let p_str = format!("+proj=utm +zone={} +south +datum=WGS84 +units=m +no_defs", zone);
                    return Self::from_proj_string(&p_str);
                }
                _ => {
                    let p_str = format!("+init=epsg:{}", code);
                    if let Ok(transformer) = Self::from_proj_string(&p_str) {
                        return Ok(transformer);
                    }
                }
            }
        }

        if let Some(s) = proj_str {
            let trimmed = s.trim();
            if trimmed.contains("longlat") && (trimmed.contains("WGS84") || trimmed.contains("epsg:4326")) {
                return Ok(Self::Wgs84Identity);
            }
            if trimmed.contains("merc") && trimmed.contains("a=6378137") {
                return Ok(Self::WebMercatorFast);
            }
            return Self::from_proj_string(trimmed);
        }

        // Default to Wgs84Identity if unspec
        Ok(Self::Wgs84Identity)
    }

    /// Construct from arbitrary PROJ string to WGS84
    pub fn from_proj_string(src_proj: &str) -> Result<Self> {
        let from = Proj::from_proj_string(src_proj)
            .map_err(|e| RasterH3Error::CrsError(format!("Failed to parse source PROJ string '{}': {:?}", src_proj, e)))?;
        let to = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
            .map_err(|e| RasterH3Error::CrsError(format!("Failed to initialize WGS84 target projection: {:?}", e)))?;

        Ok(Self::Proj4 { from, to })
    }

    /// Transform a single (x, y) point to (lon, lat) in WGS84 degrees
    #[inline(always)]
    pub fn transform_point(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        match self {
            Self::Wgs84Identity => Ok((x, y)),
            Self::WebMercatorFast => {
                let lon = (x / WGS84_A) * RAD_TO_DEG;
                let lat = (2.0 * (y / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
                Ok((lon, lat))
            }
            Self::Proj4 { from, to } => {
                let mut point_3d = (x, y, 0.0);
                proj4rs::transform::transform(from, to, &mut point_3d)
                    .map_err(|e| RasterH3Error::CrsError(format!("Reprojection error for point ({}, {}): {:?}", x, y, e)))?;
                // proj4rs outputs radians for longlat
                let lon_deg = point_3d.0 * RAD_TO_DEG;
                let lat_deg = point_3d.1 * RAD_TO_DEG;
                Ok((lon_deg, lat_deg))
            }
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
    fn test_utm_reprojection() {
        // UTM Zone 32N (EPSG:32632)
        let tf = CrsTransformer::from_crs_or_epsg(Some(32632), None).unwrap();
        let (lon, lat) = tf.transform_point(500000.0, 4500000.0).unwrap();
        assert!((lon - 9.0).abs() < 0.1);
        assert!((lat - 40.65).abs() < 0.5);
    }
}
