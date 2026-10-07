//! Exact center-sampling scanline span discovery.
//!
//! Identifies which horizontal samples can safely share an H3 cell assignment,
//! Supersampling uses the fused sample walker.
//! Does not update statistics, mutate accumulators, or manage streaming state.

use h3o::Resolution;

use crate::aggregator::h3_scanline::{cell_at, span_end};
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;

use super::coordinates::{RowCoordinates, RowGeometryContext};

/// Sequential H3 center-span discovery.
pub struct H3SpanOptimizer;

#[allow(clippy::too_many_arguments)]
impl H3SpanOptimizer {
    /// Find the end column `span_end` and optional known next cell where contiguous pixels
    /// share the exact same H3 cell `run_cell`.
    #[inline(always)]
    pub fn find_span_end(
        row_coords: &RowCoordinates,
        c: usize,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs: &CrsTransformer,
        col_offset: usize,
        res: Resolution,
        run_cell: u64,
    ) -> (usize, Option<u64>) {
        if ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) {
            span_end(c, row_coords.row_c_end, run_cell, |next| {
                row_coords
                    .pixel_center_lon_lat(next, gt, crs, col_offset)
                    .and_then(|(lon, lat)| cell_at(lat, lon, res))
            })
        } else {
            (c + 1, None)
        }
    }
}
