//! Coordinate transformation and spatial projection for multi-horizon aggregators.
//!
//! Provides reference coordinate transformation from raster pixel space to WGS84 (EPSG:4326),
//! antimeridian-aware bounding box tests, and conservative chunk latitude bounds for eviction.

use crate::aggregator::sampling::SamplePoint;
use crate::crs::transformer::CrsTransformer;
use crate::error::Result;
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

pub const WGS84_A: f64 = 6378137.0;
pub const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Unified coordinate transformer providing exact reference transformations and explicit fast paths.
#[derive(Clone)]
pub struct CoordinateTransformer<'a> {
    pub gt: &'a GeoTransform,
    pub crs_transformer: &'a CrsTransformer,
}

impl<'a> CoordinateTransformer<'a> {
    pub fn new(gt: &'a GeoTransform, crs_transformer: &'a CrsTransformer) -> Self {
        Self {
            gt,
            crs_transformer,
        }
    }

    /// Reference implementation: transform any arbitrary pixel coordinate `(px, py)` to WGS84 `(lon, lat)`
    /// by applying the full affine GeoTransform followed by the CRS projection.
    #[inline(always)]
    pub fn pixel_to_wgs84(&self, px: f64, py: f64) -> Result<(f64, f64)> {
        let (x, y) = self.gt.pixel_to_coord(px, py);
        super::profile::transform(self.crs_transformer, x, y)
    }

    /// Transform a pixel center `(col, row)` to WGS84 `(lon, lat)`
    #[inline(always)]
    pub fn pixel_center_to_wgs84(&self, col: usize, row: usize) -> Result<(f64, f64)> {
        let (x, y) = self.gt.pixel_center_to_coord(col, row);
        super::profile::transform(self.crs_transformer, x, y)
    }

    /// Transform a subpixel sample point at `(col, row)` with offset `sp` to WGS84 `(lon, lat)`
    #[inline(always)]
    pub fn subpixel_to_wgs84(&self, col: usize, row: usize, sp: SamplePoint) -> Result<(f64, f64)> {
        let px = col as f64 + sp.dx;
        let py = row as f64 + sp.dy;
        self.pixel_to_wgs84(px, py)
    }
}

/// Normalize longitude into [-180.0, 180.0) degrees
#[inline]
pub fn wrap_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// Check if a WGS84 point `(lon, lat)` falls within an optional bounding box `[min_lon, min_lat, max_lon, max_lat]`.
/// Handles longitude wrapping and antimeridian-crossing bounding boxes (`min_lon > max_lon`).
#[inline(always)]
pub fn is_point_in_bbox(lon: f64, lat: f64, bbox: Option<[f64; 4]>) -> bool {
    let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox else {
        return true;
    };
    if lat < b_min_lat || lat > b_max_lat {
        return false;
    }
    let lon = wrap_lon(lon);
    if b_min_lon <= b_max_lon {
        (lon >= b_min_lon && lon <= b_max_lon) || (lon == -180.0 && b_max_lon >= 180.0)
    } else {
        // Bounding box crosses the antimeridian (e.g. Fiji [178, -20, -178, -15])
        lon >= b_min_lon || lon <= b_max_lon
    }
}

/// Upper latitude bound for every sample in a chunk's pixel rectangle.
/// Affine latitude (WGS84) and monotone northing (Web Mercator) attain
/// their maxima at corners, including rotated/reversed affine grids.
/// Other projections require domain-specific certificates; sampled bounds
/// and fixed padding must never drive irreversible eviction.
pub fn eviction_north_bound(chunk: &RasterChunk, gt: &GeoTransform, crs: &CrsTransformer) -> f64 {
    if [gt.a, gt.b, gt.c0, gt.d, gt.e, gt.f0]
        .iter()
        .any(|v| !v.is_finite())
    {
        return f64::INFINITY;
    }

    match crs {
        CrsTransformer::Wgs84Identity | CrsTransformer::WebMercatorFast => {
            let mut upper = f64::NEG_INFINITY;
            for col in [
                chunk.col_offset as f64,
                chunk.col_offset as f64 + chunk.width as f64,
            ] {
                for row in [
                    chunk.row_offset as f64,
                    chunk.row_offset as f64 + chunk.height as f64,
                ] {
                    let (x, y) = gt.pixel_to_coord(col, row);
                    let Ok((_, lat)) = crs.transform_point(x, y) else {
                        return f64::INFINITY;
                    };
                    if !lat.is_finite() {
                        return f64::INFINITY;
                    }
                    upper = upper.max(lat.next_up());
                }
            }
            upper
        }
        CrsTransformer::AlbersConic(albers) => {
            // For Albers Equal Area Conic:
            // Parallels are concentric circular arcs centered at the cone apex (apex_x, apex_y).
            // Distance rho to the apex is monotonic with latitude.
            // Outside the chunk, the apex has its nearest point on an edge.
            // A chunk containing the apex can have an interior latitude maximum.
            let apex_x = albers.x_0;
            let apex_y = albers.y_0 + albers.rho0;

            let p0 = gt.pixel_to_coord(chunk.col_offset as f64, chunk.row_offset as f64);
            let p1 = gt.pixel_to_coord(
                chunk.col_offset as f64 + chunk.width as f64,
                chunk.row_offset as f64,
            );
            let p2 = gt.pixel_to_coord(
                chunk.col_offset as f64 + chunk.width as f64,
                chunk.row_offset as f64 + chunk.height as f64,
            );
            let p3 = gt.pixel_to_coord(
                chunk.col_offset as f64,
                chunk.row_offset as f64 + chunk.height as f64,
            );

            let mut upper = f64::NEG_INFINITY;
            let corners = [p0, p1, p2, p3];
            for &(x, y) in &corners {
                let Ok((_, lat)) = crs.transform_point(x, y) else {
                    return f64::INFINITY;
                };
                if !lat.is_finite() {
                    return f64::INFINITY;
                }
                upper = upper.max(lat);
            }

            // A bounding-box containment test is conservative for rotated grids:
            // it may disable eviction unnecessarily, but cannot miss an interior
            // apex. Boundary samples alone cannot certify such a chunk.
            let min_x = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
            let max_x = corners
                .iter()
                .map(|p| p.0)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
            let max_y = corners
                .iter()
                .map(|p| p.1)
                .fold(f64::NEG_INFINITY, f64::max);
            if !apex_x.is_finite()
                || !apex_y.is_finite()
                || (apex_x >= min_x && apex_x <= max_x && apex_y >= min_y && apex_y <= max_y)
            {
                return f64::INFINITY;
            }

            // Test critical point along each edge closest to apex
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
                        let Ok((_, lat)) = crs.transform_point(cx, cy) else {
                            return f64::INFINITY;
                        };
                        if !lat.is_finite() {
                            return f64::INFINITY;
                        }
                        upper = upper.max(lat);
                    }
                }
            }
            upper.next_up()
        }
        // Arbitrary PROJ definitions have no certified scale or domain bound.
        // Boundary samples cannot rule out an interior pole, and a constant
        // degrees-per-meter margin is invalid for arbitrary scales and units.
        // Keep cells until EOF (or spill them) rather than evict irreversibly.
        CrsTransformer::Proj4 { .. } => f64::INFINITY,
    }
}
