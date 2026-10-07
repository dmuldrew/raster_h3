//! Unified Geometric Pixel Walker & Spatial Math for Multi-Horizon Aggregators
//!
//! Centralizes raster coordinate projection, scanline derivatives, bounding box pruning,
//! and scanline span accumulation across both continuous (Welford) and categorical engines.

use fxhash::FxBuildHasher;
use h3o::Resolution;
use std::collections::HashMap;

use crate::aggregator::h3_scanline::cell_at;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

pub use super::coordinates::{
    is_point_in_bbox, resolve_subpixel_cell, CoordinateTransformer, RowCoordinates,
    RowGeometryContext, RAD_TO_DEG, WGS84_A,
};
pub use super::overlap_walker::walk_overlap_pixel_cells;
pub use super::span::H3SpanOptimizer;

/// Fast check if an entire row slice consists purely of NoData values using centralized NoDataRule
pub use crate::aggregator::nodata::is_slice_all_native_nodata;

/// Trait implemented by accumulator engines (continuous and categorical) to drive the generic scanline walker
pub trait ScanlineEngine<T, Acc> {
    type Sample: Copy;

    /// Opt in only when combining weights for the same pixel preserves semantics.
    fn combine_sample_weights(&self) -> bool {
        false
    }

    /// Allocate a fresh empty accumulator (or with quantiles if configured)
    fn new_acc(&self) -> Acc;

    /// Reset an accumulator to its zero/empty state
    fn clear_acc(&self, acc: &mut Acc);

    /// Check if accumulator contains valid observations
    fn has_samples(&self, acc: &Acc) -> bool;

    /// Merge the source accumulator into the destination
    fn merge_acc(&self, dest: &mut Acc, src: &Acc);

    /// Accumulate a span of pixels into a single accumulator
    fn accumulate_span(&self, acc: &mut Acc, slice: &[T]);

    /// Accumulate a span of pixels across multiple active resolution accumulators
    fn accumulate_span_multi(&self, run_accs: &mut [Acc], run_cells: &[u64], slice: &[T]);

    /// Extract a valid sample from a raw pixel, or None if nodata
    fn get_sample(&self, pixel: T) -> Option<Self::Sample>;

    /// Update an accumulator with a sample and weight
    fn update_sample(&self, acc: &mut Acc, sample: Self::Sample, weight: f64);
}

/// Column position only; coordinates are derived from absolute pixel positions.
#[derive(Debug, Clone, Copy)]
pub struct ScanlineCursor {
    pub col: usize,
}
impl ScanlineCursor {
    pub fn new(col: usize) -> Self {
        Self { col }
    }
    pub fn advance(&mut self, steps: usize) {
        self.col += steps;
    }
    pub fn step_one(&mut self) {
        self.advance(1);
    }
}

/// Dedicated scalar-register hot path for single resolution scanline processing.
#[inline(always)]
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn walk_single_res_row<T, Acc, E>(
    slice_row: &[T],
    chunk: &RasterChunk,
    res: Resolution,
    coords: &RowCoordinates,
    geom_ctx: &RowGeometryContext,
    gt: &GeoTransform,
    crs_transformer: &CrsTransformer,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    engine: &E,
    active_map: &mut HashMap<u64, Acc, FxBuildHasher>,
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
{
    let is_single_point = sampling.is_single_point();

    let mut run_cell: u64 = 0;
    let mut run_acc = engine.new_acc();
    let mut known_next_cell: Option<u64> = None;

    let mut cursor = ScanlineCursor::new(coords.row_c_start);

    while cursor.col < coords.row_c_end {
        let c = cursor.col;
        let (lon, lat) =
            match coords.pixel_center_lon_lat(c, gt, crs_transformer, chunk.col_offset as usize) {
                Some(ll) => ll,
                None => {
                    known_next_cell = None;
                    cursor.step_one();
                    continue;
                }
            };

        if is_single_point
            && ((geom_ctx.is_north_up && !geom_ctx.is_wgs84 && !geom_ctx.is_web_mercator)
                || !geom_ctx.is_north_up)
            && !is_point_in_bbox(lon, lat, bbox)
        {
            known_next_cell = None;
            cursor.step_one();
            continue;
        }

        let cell_opt = known_next_cell.take().or_else(|| cell_at(lat, lon, res));

        if let Some(cell_u64) = cell_opt {
            if cell_u64 != run_cell {
                if run_cell != 0 && engine.has_samples(&run_acc) {
                    active_map
                        .entry(run_cell)
                        .and_modify(|acc| engine.merge_acc(acc, &run_acc))
                        .or_insert_with(|| run_acc.clone());
                }
                run_cell = cell_u64;
                engine.clear_acc(&mut run_acc);
            }

            let (span_end, next_cell) = coords.find_span_end(
                c,
                geom_ctx,
                gt,
                crs_transformer,
                chunk.col_offset as usize,
                res,
                run_cell,
            );

            let span_slice = &slice_row[c..span_end];
            engine.accumulate_span(&mut run_acc, span_slice);

            let num_stepped = span_end - c;
            cursor.advance(num_stepped);
            known_next_cell = next_cell;
        } else {
            cursor.step_one();
        }
    }

    if run_cell != 0 && engine.has_samples(&run_acc) {
        active_map
            .entry(run_cell)
            .and_modify(|acc| engine.merge_acc(acc, &run_acc))
            .or_insert_with(|| run_acc.clone());
    }
}

/// Reusable state buffers for multi-resolution scanline processing across chunks.
struct MultiResRowBuffers<Acc> {
    span_ends: Vec<usize>,
    run_cells: Vec<u64>,
    run_accs: Vec<Acc>,
    known_next_cells: Vec<Option<u64>>,
}

impl<Acc: Clone> MultiResRowBuffers<Acc> {
    fn new<T, E: ScanlineEngine<T, Acc>>(resolutions: &[Resolution], engine: &E) -> Self {
        let num_res = resolutions.len();
        Self {
            span_ends: vec![0; num_res],
            run_cells: vec![0; num_res],
            run_accs: (0..num_res).map(|_| engine.new_acc()).collect(),
            known_next_cells: vec![None; num_res],
        }
    }

    fn reset_for_row<T, E: ScanlineEngine<T, Acc>>(&mut self, row_c_start: usize, engine: &E) {
        let num_res = self.run_cells.len();
        for i in 0..num_res {
            self.run_cells[i] = 0;
            engine.clear_acc(&mut self.run_accs[i]);
            self.known_next_cells[i] = None;
            self.span_ends[i] = row_c_start;
        }
    }

    fn flush_inactive_cell<T, E: ScanlineEngine<T, Acc>>(
        &mut self,
        i: usize,
        c: usize,
        engine: &E,
        chunk_map: &mut HashMap<u64, Acc, FxBuildHasher>,
    ) {
        if self.run_cells[i] != 0 && engine.has_samples(&self.run_accs[i]) {
            chunk_map
                .entry(self.run_cells[i])
                .and_modify(|acc| engine.merge_acc(acc, &self.run_accs[i]))
                .or_insert_with(|| self.run_accs[i].clone());
            engine.clear_acc(&mut self.run_accs[i]);
        }
        self.run_cells[i] = 0;
        self.span_ends[i] = c + 1;
        self.known_next_cells[i] = None;
    }
}

/// Multi-resolution scanline processing row driver.
#[inline(always)]
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn walk_multi_res_row<T, Acc, E>(
    slice_row: &[T],
    chunk: &RasterChunk,
    resolutions: &[Resolution],
    coords: &RowCoordinates,
    geom_ctx: &RowGeometryContext,
    gt: &GeoTransform,
    crs_transformer: &CrsTransformer,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    engine: &E,
    chunk_maps: &mut [HashMap<u64, Acc, FxBuildHasher>],
    buf: &mut MultiResRowBuffers<Acc>,
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
{
    let num_res = resolutions.len();
    buf.reset_for_row(coords.row_c_start, engine);
    let is_single_point = sampling.is_single_point();

    let mut cursor = ScanlineCursor::new(coords.row_c_start);

    while cursor.col < coords.row_c_end {
        let c = cursor.col;
        let (lon, lat) =
            match coords.pixel_center_lon_lat(c, gt, crs_transformer, chunk.col_offset as usize) {
                Some(ll) => ll,
                None => {
                    for i in 0..num_res {
                        buf.flush_inactive_cell(i, c, engine, &mut chunk_maps[i]);
                    }
                    cursor.step_one();
                    continue;
                }
            };

        if is_single_point
            && ((geom_ctx.is_north_up && !geom_ctx.is_wgs84 && !geom_ctx.is_web_mercator)
                || !geom_ctx.is_north_up)
            && !is_point_in_bbox(lon, lat, bbox)
        {
            for i in 0..num_res {
                buf.flush_inactive_cell(i, c, engine, &mut chunk_maps[i]);
            }
            cursor.step_one();
            continue;
        }

        for i in 0..num_res {
            if c >= buf.span_ends[i] {
                let res = resolutions[i];
                let cell_opt = buf.known_next_cells[i]
                    .take()
                    .or_else(|| cell_at(lat, lon, res));

                if let Some(cell_u64) = cell_opt {
                    if cell_u64 != buf.run_cells[i] {
                        if buf.run_cells[i] != 0 && engine.has_samples(&buf.run_accs[i]) {
                            chunk_maps[i]
                                .entry(buf.run_cells[i])
                                .and_modify(|acc| engine.merge_acc(acc, &buf.run_accs[i]))
                                .or_insert_with(|| buf.run_accs[i].clone());
                            engine.clear_acc(&mut buf.run_accs[i]);
                        }
                        buf.run_cells[i] = cell_u64;
                    }

                    let (span_end, next_cell) = coords.find_span_end(
                        c,
                        geom_ctx,
                        gt,
                        crs_transformer,
                        chunk.col_offset as usize,
                        res,
                        buf.run_cells[i],
                    );

                    buf.span_ends[i] = span_end;
                    buf.known_next_cells[i] = next_cell;
                } else {
                    buf.flush_inactive_cell(i, c, engine, &mut chunk_maps[i]);
                }
            }
        }

        let mut step_end = coords.row_c_end;
        for i in 0..num_res {
            step_end = step_end.min(buf.span_ends[i]);
        }
        let step_end = step_end.max(c + 1).min(coords.row_c_end);

        let span_slice = &slice_row[c..step_end];
        engine.accumulate_span_multi(&mut buf.run_accs, &buf.run_cells, span_slice);

        let num_stepped = step_end - c;
        cursor.advance(num_stepped);
    }

    for i in 0..num_res {
        if buf.run_cells[i] != 0 && engine.has_samples(&buf.run_accs[i]) {
            chunk_maps[i]
                .entry(buf.run_cells[i])
                .and_modify(|acc| engine.merge_acc(acc, &buf.run_accs[i]))
                .or_insert_with(|| buf.run_accs[i].clone());
        }
    }
}

/// Unified generic scanline walker across all resolutions and sampling patterns
#[allow(clippy::too_many_arguments)]
pub fn scanline_walk<T, Acc, E, FNoData>(
    slice: &[T],
    chunk: &RasterChunk,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    is_row_all_nodata: FNoData,
    engine: &E,
    chunk_maps: &mut [HashMap<u64, Acc, FxBuildHasher>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
    FNoData: Fn(&[T]) -> bool,
{
    if !sampling.is_single_point() {
        super::supersampling::walk_samples(
            slice,
            chunk,
            resolutions,
            crs_transformer,
            gt,
            sampling,
            bbox,
            chunk_stride,
            is_row_all_nodata,
            engine,
            chunk_maps,
        );
        return;
    }
    let geom_ctx = RowGeometryContext::new(chunk, slice.len(), chunk_stride, crs_transformer, gt);
    let RowGeometryContext {
        stride,
        actual_rows,
        ..
    } = geom_ctx;
    let num_res = resolutions.len();

    if num_res == 1 {
        for r in 0..actual_rows {
            let slice_row_start = r * stride;
            let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
            if row_width == 0 {
                continue;
            }

            let slice_row = &slice[slice_row_start..slice_row_start + row_width];
            if is_row_all_nodata(slice_row) {
                continue;
            }

            if let Some(coords) = RowCoordinates::compute(
                r,
                chunk,
                row_width,
                &geom_ctx,
                gt,
                crs_transformer,
                sampling,
                bbox,
            ) {
                walk_single_res_row(
                    slice_row,
                    chunk,
                    resolutions[0],
                    &coords,
                    &geom_ctx,
                    gt,
                    crs_transformer,
                    sampling,
                    bbox,
                    engine,
                    &mut chunk_maps[0],
                );
            }
        }
    } else {
        let mut buffers = MultiResRowBuffers::new(resolutions, engine);
        for r in 0..actual_rows {
            let slice_row_start = r * stride;
            let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
            if row_width == 0 {
                continue;
            }

            let slice_row = &slice[slice_row_start..slice_row_start + row_width];
            if is_row_all_nodata(slice_row) {
                continue;
            }

            if let Some(coords) = RowCoordinates::compute(
                r,
                chunk,
                row_width,
                &geom_ctx,
                gt,
                crs_transformer,
                sampling,
                bbox,
            ) {
                walk_multi_res_row(
                    slice_row,
                    chunk,
                    resolutions,
                    &coords,
                    &geom_ctx,
                    gt,
                    crs_transformer,
                    sampling,
                    bbox,
                    engine,
                    chunk_maps,
                    &mut buffers,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_point_in_bbox() {
        let bbox = Some([-122.5, 37.5, -122.0, 38.0]);
        assert!(is_point_in_bbox(-122.25, 37.75, bbox));
        assert!(is_point_in_bbox(-122.5, 37.5, bbox));
        assert!(is_point_in_bbox(-122.0, 38.0, bbox));
        assert!(!is_point_in_bbox(-122.6, 37.75, bbox));
        assert!(!is_point_in_bbox(-122.25, 38.1, bbox));
        assert!(is_point_in_bbox(0.0, 0.0, None));

        // Antimeridian-crossing bbox (e.g. Fiji [178.0, -20.0, -178.0, -15.0])
        let fiji_bbox = Some([178.0, -20.0, -178.0, -15.0]);
        assert!(is_point_in_bbox(179.0, -18.0, fiji_bbox));
        assert!(is_point_in_bbox(-179.0, -18.0, fiji_bbox));
        // Unnormalized longitude wrapping (181.0 -> -179.0)
        assert!(is_point_in_bbox(181.0, -18.0, fiji_bbox));
        // Outside longitude
        assert!(!is_point_in_bbox(175.0, -18.0, fiji_bbox));
        assert!(!is_point_in_bbox(-175.0, -18.0, fiji_bbox));
        // Outside latitude
        assert!(!is_point_in_bbox(179.0, -21.0, fiji_bbox));
    }

    #[test]
    fn test_resolve_subpixel_cell_center_fastpath() {
        let run_cell = 0x8828308281ffffff;
        let res = Resolution::try_from(8).unwrap();
        let cell = resolve_subpixel_cell(run_cell, 37.75, -122.25, 0.0, 0.0, res);
        assert_eq!(cell, Some(run_cell));

        // Out-of-range latitude must return None rather than wrapping across pole
        let cell_out_of_bounds = resolve_subpixel_cell(run_cell, 95.0, 0.0, 0.1, 0.1, res);
        assert_eq!(cell_out_of_bounds, None);
    }

    #[test]
    fn test_scanline_cursor_advancement() {
        let mut cursor = ScanlineCursor::new(10);
        cursor.advance(5);
        assert_eq!(cursor.col, 15);
    }
}
