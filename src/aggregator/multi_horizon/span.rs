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
use crate::aggregator::sampling::SamplingPattern;

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
        // Exact center runs currently use the constant-latitude fast transforms.
        // Geographic cutoffs are performance policy, not a containment proof.
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
        _row_cache: &H3ScanlineLookahead,
        chunk: &RasterChunk,
        c: usize,
        span_end: usize,
        _dx_bounds: (f64, f64),
        _dy_bounds: (f64, f64),
        sampling: &SamplingPattern,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        bbox: Option<[f64; 4]>,
        mut is_in_cell: FCheck,
    ) -> (usize, usize)
    where
        FCheck: FnMut(f64, f64) -> bool,
    {
        // Bulk accumulation assumes total sample weight exactly one. Certify
        // actual samples, never the four corners of their bounding rectangle.
        if !ctx.is_north_up
            || !(ctx.is_wgs84 || ctx.is_web_mercator)
            || sampling.points.iter().map(|p| p.weight).sum::<f64>() != 1.0
            || span_end.saturating_sub(c) <= 2
        {
            return (c, c);
        }
        let start = c + 1;
        let end = span_end - 1;
        for k in start..end {
            let mut count = 0;
            let mut certified = true;
            row_coords.for_each_subpixel(
                k,
                ctx,
                gt,
                crs_transformer,
                chunk.col_offset as usize,
                sampling,
                bbox,
                |lon, lat, _, _, _| {
                    count += 1;
                    certified &= (-90.0..=90.0).contains(&lat) && is_in_cell(lat, lon);
                },
            );
            if !certified || count != sampling.points.len() {
                return (c, c);
            }
        }
        (start, end)
    }
}
