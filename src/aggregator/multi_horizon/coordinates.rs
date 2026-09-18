//! Coordinate transformation and spatial projection for multi-horizon aggregators.
//!
//! Provides reference coordinate transformation from raster pixel space to WGS84 (EPSG:4326),
//! explicit fast paths for north-up WGS84 and Web Mercator grids, exact per-sample
//! projected CRS transformation, and row-level spatial geometry contexts.

use h3o::{LatLng, Resolution};

use crate::aggregator::sampling::{SamplePoint, SamplingPattern};
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
    pub is_wgs84: bool,
    pub is_web_mercator: bool,
    pub is_north_up: bool,
    pub d_lon_step: f64,
    pub dx_step: f64,
}

impl<'a> CoordinateTransformer<'a> {
    /// Create a new CoordinateTransformer from a GeoTransform and CRS transformer
    pub fn new(gt: &'a GeoTransform, crs_transformer: &'a CrsTransformer) -> Self {
        let is_wgs84 = matches!(crs_transformer, CrsTransformer::Wgs84Identity);
        let is_web_mercator = matches!(crs_transformer, CrsTransformer::WebMercatorFast);
        let is_north_up = gt.b == 0.0 && gt.d == 0.0 && gt.e < 0.0 && gt.a > 0.0;
        let dx_step = gt.a;

        let d_lon_step = if is_wgs84 {
            gt.a
        } else if is_web_mercator {
            (gt.a / WGS84_A) * RAD_TO_DEG
        } else {
            0.0
        };

        Self {
            gt,
            crs_transformer,
            is_wgs84,
            is_web_mercator,
            is_north_up,
            d_lon_step,
            dx_step,
        }
    }

    /// Reference implementation: transform any arbitrary pixel coordinate `(px, py)` to WGS84 `(lon, lat)`
    /// by applying the full affine GeoTransform followed by the CRS projection.
    #[inline(always)]
    pub fn pixel_to_wgs84(&self, px: f64, py: f64) -> Result<(f64, f64)> {
        let (x, y) = self.gt.pixel_to_coord(px, py);
        self.crs_transformer.transform_point(x, y)
    }

    /// Transform a pixel center `(col, row)` to WGS84 `(lon, lat)`
    #[inline(always)]
    pub fn pixel_center_to_wgs84(&self, col: usize, row: usize) -> Result<(f64, f64)> {
        let (x, y) = self.gt.pixel_center_to_coord(col, row);
        self.crs_transformer.transform_point(x, y)
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

/// Resolve boundary samples with exact H3 indexing; reuse only the exact center sample.
#[inline(always)]
pub fn resolve_subpixel_cell(
    run_cell: u64,
    lat: f64,
    lon: f64,
    d_x: f64,
    d_y: f64,
    res: Resolution,
) -> Option<u64> {
    if d_x == 0.0 && d_y == 0.0 {
        Some(run_cell)
    } else if !(-90.0..=90.0).contains(&lat) {
        None
    } else {
        LatLng::new(lat, lon).ok().map(|ll| ll.to_cell(res).into())
    }
}

/// Precomputed chunk geometry context shared across all scanlines in a chunk
#[derive(Debug, Clone)]
pub struct RowGeometryContext {
    pub is_wgs84: bool,
    pub is_web_mercator: bool,
    pub is_north_up: bool,
    pub d_lon_step: f64,
    pub dx_step: f64,
    pub stride: usize,
    pub actual_rows: usize,
}

impl RowGeometryContext {
    pub fn new(
        chunk: &RasterChunk,
        slice_len: usize,
        chunk_stride: u32,
        crs_transformer: &CrsTransformer,
        gt: &GeoTransform,
    ) -> Self {
        let is_wgs84 = matches!(crs_transformer, CrsTransformer::Wgs84Identity);
        let is_web_mercator = matches!(crs_transformer, CrsTransformer::WebMercatorFast);
        let d_lon_step = if is_wgs84 {
            gt.a
        } else if is_web_mercator {
            (gt.a / WGS84_A) * RAD_TO_DEG
        } else {
            0.0
        };

        let stride = if chunk_stride > 0 && slice_len >= chunk_stride as usize {
            chunk_stride as usize
        } else {
            (chunk.width as usize).max(1)
        };
        let actual_rows = (slice_len / stride).min(chunk.height as usize);
        let is_north_up = gt.b == 0.0 && gt.d == 0.0 && gt.e < 0.0 && gt.a > 0.0;
        let dx_step = gt.a;

        Self {
            is_wgs84,
            is_web_mercator,
            is_north_up,
            d_lon_step,
            dx_step,
            stride,
            actual_rows,
        }
    }
}

/// Per-row spatial coordinates, bounds, derivatives, and lookahead parameters
#[derive(Debug, Clone, Copy)]
pub struct RowCoordinates {
    pub row_idx: usize,
    pub x_start: f64,
    pub y_row: f64,
    pub lon_start: f64,
    pub lat_row: f64,
    pub row_c_start: usize,
    pub row_c_end: usize,
}

#[allow(clippy::too_many_arguments)]
impl RowCoordinates {
    pub fn compute(
        r: usize,
        chunk: &RasterChunk,
        row_width: usize,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        sampling: &SamplingPattern,
        bbox: Option<[f64; 4]>,
    ) -> Option<Self> {
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let (x_start, y_row) = gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);

        let (lon_start, lat_row) = if ctx.is_wgs84 {
            (x_start, y_row)
        } else if ctx.is_web_mercator {
            let lat =
                (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
            let lon = (x_start / WGS84_A) * RAD_TO_DEG;
            (lon, lat)
        } else {
            match crs_transformer.transform_point(x_start, y_row) {
                Ok(coords) => coords,
                Err(_) => return None,
            }
        };

        // Center coordinates are sufficient for a single center sample only. A
        // supersampled pixel can overlap the bbox even when its center is out.
        if sampling.is_single_point() && ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) {
            if let Some([_, b_min_lat, _, b_max_lat]) = bbox {
                if lat_row < b_min_lat || lat_row > b_max_lat {
                    return None;
                }
            }
        }

        let (row_c_start, row_c_end) = if sampling.is_single_point()
            && ctx.is_north_up
            && (ctx.is_wgs84 || ctx.is_web_mercator)
        {
            if let Some([b_min_lon, _, b_max_lon, _]) = bbox {
                if ctx.d_lon_step > 0.0 {
                    let c_s = if lon_start < b_min_lon {
                        ((b_min_lon - lon_start) / ctx.d_lon_step).ceil().max(0.0) as usize
                    } else {
                        0
                    };
                    let c_e = if lon_start < b_max_lon {
                        (((b_max_lon - lon_start) / ctx.d_lon_step).floor().max(0.0) as usize + 1)
                            .min(row_width)
                    } else {
                        0
                    };
                    (c_s, c_e)
                } else if ctx.d_lon_step < 0.0 {
                    let c_s = if lon_start > b_max_lon {
                        ((b_max_lon - lon_start) / ctx.d_lon_step).ceil().max(0.0) as usize
                    } else {
                        0
                    };
                    let c_e = if lon_start > b_min_lon {
                        (((b_min_lon - lon_start) / ctx.d_lon_step).floor().max(0.0) as usize + 1)
                            .min(row_width)
                    } else {
                        0
                    };
                    (c_s, c_e)
                } else {
                    (0, row_width)
                }
            } else {
                (0, row_width)
            }
        } else {
            (0, row_width)
        };

        if row_c_start >= row_c_end || row_c_start >= row_width {
            return None;
        }

        Some(Self {
            row_idx,
            x_start,
            y_row,
            lon_start,
            lat_row,
            row_c_start,
            row_c_end,
        })
    }

    /// Compute the projected longitude and latitude for the pixel center at column `c`
    #[inline(always)]
    pub fn pixel_center_lon_lat(
        &self,
        c: usize,
        x_curr: f64,
        lon_curr: f64,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        col_offset: usize,
    ) -> Option<(f64, f64)> {
        if ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) {
            Some((lon_curr, self.lat_row))
        } else if ctx.is_north_up {
            crs_transformer.transform_point(x_curr, self.y_row).ok()
        } else {
            let (x, y) = gt.pixel_center_to_coord(col_offset + c, self.row_idx);
            crs_transformer.transform_point(x, y).ok()
        }
    }

    /// Iterate over all subpixel sampling points for pixel at column `k`, invoking `f(lon, lat, d_x, d_y, weight)`.
    /// For north-up projected CRS (e.g. UTM, Albers), calculates exact per-sample coordinates without linear approximations.
    #[inline(always)]
    pub fn for_each_subpixel<F>(
        &self,
        k: usize,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        col_offset: usize,
        sampling: &SamplingPattern,
        bbox: Option<[f64; 4]>,
        mut f: F,
    ) where
        F: FnMut(f64, f64, f64, f64, f64),
    {
        if ctx.is_north_up && ctx.is_wgs84 {
            let k_lon = self.lon_start + (k as f64) * ctx.d_lon_step;
            let k_lat = self.lat_row;
            for sp in &sampling.points {
                let d_x = sp.dx - 0.5;
                let d_y = sp.dy - 0.5;
                let lon = k_lon + d_x * ctx.d_lon_step;
                let lat = k_lat + d_y * gt.e;
                if is_point_in_bbox(lon, lat, bbox) {
                    f(lon, lat, d_x, d_y, sp.weight);
                }
            }
            return;
        }

        // Projected (including Mercator) and rotated grids require the full
        // affine transform and exact inverse projection at each sample.
        let transformer = CoordinateTransformer::new(gt, crs_transformer);
        for sp in &sampling.points {
            if let Ok((lon, lat)) = transformer.subpixel_to_wgs84(col_offset + k, self.row_idx, *sp)
            {
                if is_point_in_bbox(lon, lat, bbox) {
                    f(lon, lat, sp.dx - 0.5, sp.dy - 0.5, sp.weight);
                }
            }
        }
    }

    /// Delegate span end search to scanline lookahead cache across coordinate reference systems
    #[inline(always)]
    pub fn find_span_end(
        &self,
        row_cache: &mut crate::aggregator::h3_scanline::H3ScanlineLookahead,
        c: usize,
        lon_curr: f64,
        ctx: &RowGeometryContext,
        crs_transformer: &CrsTransformer,
        res: Resolution,
        run_cell: u64,
        bbox: Option<[f64; 4]>,
    ) -> (usize, Option<u64>) {
        super::span::H3SpanOptimizer::find_span_end(
            self,
            row_cache,
            c,
            lon_curr,
            ctx,
            crs_transformer,
            res,
            run_cell,
            bbox,
        )
    }

    /// Identify the inner core column range `(core_start, core_end)` where all subpixel sample points land inside `run_cell`
    #[inline(always)]
    pub fn find_core_span<FCheck>(
        &self,
        row_cache: &crate::aggregator::h3_scanline::H3ScanlineLookahead,
        chunk: &RasterChunk,
        c: usize,
        span_end: usize,
        dx_bounds: (f64, f64),
        dy_bounds: (f64, f64),
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        bbox: Option<[f64; 4]>,
        is_in_cell: FCheck,
    ) -> (usize, usize)
    where
        FCheck: FnMut(f64, f64) -> bool,
    {
        super::span::H3SpanOptimizer::find_core_span(
            self,
            row_cache,
            chunk,
            c,
            span_end,
            dx_bounds,
            dy_bounds,
            ctx,
            gt,
            crs_transformer,
            bbox,
            is_in_cell,
        )
    }
}
