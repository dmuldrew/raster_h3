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

use super::coordinates::{RowCoordinates, RowGeometryContext};

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
        bbox: Option<[f64; 4]>,
    ) -> (usize, Option<u64>) {
        let is_eligible =
            ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) && bbox.is_none();

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

    /// The corner-only API cannot certify curved spherical interiors. Return
    /// an empty core so the walker evaluates every actual subsample.
    #[inline(always)]
    pub fn find_core_span<FCheck>(
        _row_coords: &RowCoordinates,
        _row_cache: &H3ScanlineLookahead,
        _chunk: &RasterChunk,
        c: usize,
        _span_end: usize,
        _dx_bounds: (f64, f64),
        _dy_bounds: (f64, f64),
        _ctx: &RowGeometryContext,
        _gt: &GeoTransform,
        _crs_transformer: &CrsTransformer,
        _bbox: Option<[f64; 4]>,
        _is_in_cell: FCheck,
    ) -> (usize, usize)
    where
        FCheck: FnMut(f64, f64) -> bool,
    {
        (c, c)
    }
}
