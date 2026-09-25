//! Scanline span discovery and core/boundary classification.
//!
//! Identifies which horizontal samples can safely share an H3 cell assignment,
//! and determines which subpixel samples are strictly interior (core) versus boundary.
//! Does not update statistics, mutate accumulators, or manage streaming state.

use h3o::Resolution;

use crate::aggregator::h3_scanline::H3ScanlineLookahead;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

use super::coordinates::{is_point_in_bbox, RowCoordinates, RowGeometryContext};

/// Optimizer for identifying H3 cell spans and subpixel core intervals on a raster scanline.
pub struct H3SpanOptimizer;

#[allow(clippy::too_many_arguments)]
impl H3SpanOptimizer {
    /// Find the end column `span_end` and optional known next cell where contiguous pixels
    /// share the exact same H3 cell `run_cell`.
    #[inline(always)]
    pub fn find_span_end(
        row_coords: &RowCoordinates,
        row_cache: &mut H3ScanlineLookahead,
        c: usize,
        lon_curr: f64,
        ctx: &RowGeometryContext,
        _crs_transformer: &CrsTransformer,
        res: Resolution,
        run_cell: u64,
        _bbox: Option<[f64; 4]>,
    ) -> (usize, Option<u64>) {
        let r_u8 = res as u8;
        // Lookahead span shortcuts require:
        // 1. Unrotated North-up WGS84 or Web Mercator (ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator))
        // 2. Resolution >= 4 (coarse cells res 0..=3 have large sagitta across cell extent)
        // 3. Moderate latitude |lat| < 70° (polar parallels have high curvature)
        // 4. Away from antimeridian boundaries (|lon| <= 175° and lon does not wrap)
        // Projected and rotated grids are non-convex or sheared; fall back to exact per-sample lookup.
        let is_eligible = ctx.is_north_up
            && (ctx.is_wgs84 || ctx.is_web_mercator)
            && r_u8 >= 4
            && row_coords.lat_row.abs() < 70.0
            && lon_curr.abs() <= 175.0
            && (lon_curr + (row_coords.row_c_end.saturating_sub(c)) as f64 * ctx.d_lon_step).abs()
                <= 175.0;

        if is_eligible {
            row_cache.find_span_end(
                c,
                row_coords.row_c_end,
                lon_curr,
                row_coords.lat_row,
                ctx.d_lon_step,
                res,
                run_cell,
            )
        } else {
            (c + 1, None)
        }
    }

    /// Identify the inner core column range `(core_start, core_end)` where all subpixel sample points
    /// land strictly inside `run_cell`.
    #[inline(always)]
    pub fn find_core_span<FCheck>(
        row_coords: &RowCoordinates,
        row_cache: &H3ScanlineLookahead,
        chunk: &RasterChunk,
        c: usize,
        span_end: usize,
        dx_bounds: (f64, f64),
        dy_bounds: (f64, f64),
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        bbox: Option<[f64; 4]>,
        mut is_in_cell: FCheck,
    ) -> (usize, usize)
    where
        FCheck: FnMut(f64, f64) -> bool,
    {
        // Core span optimization requires unrotated North-up WGS84/Mercator with moderate latitude.
        // Projected and rotated rasters need full per-sample evaluation.
        let is_eligible = ctx.is_north_up
            && (ctx.is_wgs84 || ctx.is_web_mercator)
            && row_coords.lat_row.abs() < 70.0;

        if !is_eligible {
            return (c, c);
        }

        row_cache.find_core_span(c, span_end, dx_bounds, dy_bounds, |px, py| {
            let (lon, lat) = if ctx.is_wgs84 {
                (
                    row_coords.lon_start + (px - 0.5) * ctx.d_lon_step,
                    row_coords.lat_row + (py - 0.5) * gt.e,
                )
            } else {
                let (x, y) = gt.pixel_to_coord(
                    (chunk.col_offset as f64) + px,
                    (row_coords.row_idx as f64) + py,
                );
                match crs_transformer.transform_point(x, y) {
                    Ok(coords) => coords,
                    Err(_) => return false,
                }
            };
            if !is_point_in_bbox(lon, lat, bbox) {
                return false;
            }
            is_in_cell(lat, lon)
        })
    }
}
