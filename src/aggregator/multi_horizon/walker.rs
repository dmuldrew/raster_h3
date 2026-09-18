//! Unified Geometric Pixel Walker & Spatial Math for Multi-Horizon Aggregators
//!
//! Centralizes raster coordinate projection, scanline derivatives, bounding box pruning,
//! and scanline span accumulation across both continuous (Welford) and categorical engines.

use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use std::collections::HashMap;

use crate::aggregator::h3_scanline::H3ScanlineLookahead;
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

/// Stateful cursor tracking current column and projected coordinate positions along a scanline.
#[derive(Debug, Clone, Copy)]
pub struct ScanlineCursor {
    pub col: usize,
    pub x_curr: f64,
    pub lon_curr: f64,
    pub dx_step: f64,
    pub d_lon_step: f64,
    pub is_north_up: bool,
    pub is_geographic: bool,
}

impl ScanlineCursor {
    #[inline(always)]
    pub fn new(
        col_start: usize,
        x_start: f64,
        lon_start: f64,
        dx_step: f64,
        d_lon_step: f64,
        geom_ctx: &RowGeometryContext,
    ) -> Self {
        Self {
            col: col_start,
            x_curr: x_start + (col_start as f64) * dx_step,
            lon_curr: lon_start + (col_start as f64) * d_lon_step,
            dx_step,
            d_lon_step,
            is_north_up: geom_ctx.is_north_up,
            is_geographic: geom_ctx.is_wgs84 || geom_ctx.is_web_mercator,
        }
    }

    #[inline(always)]
    pub fn advance(&mut self, steps: usize) {
        self.col += steps;
        if self.is_geographic {
            self.lon_curr += (steps as f64) * self.d_lon_step;
        } else if self.is_north_up {
            self.x_curr += (steps as f64) * self.dx_step;
        }
    }

    #[inline(always)]
    pub fn step_one(&mut self) {
        self.advance(1);
    }
}

/// Evaluates subpixel sample points for a boundary pixel in the single-resolution path.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn evaluate_subpixel_pixel<T, Acc, E>(
    val_raw: T,
    k: usize,
    run_cell: u64,
    res: Resolution,
    coords: &RowCoordinates,
    geom_ctx: &RowGeometryContext,
    gt: &GeoTransform,
    crs_transformer: &CrsTransformer,
    col_offset: usize,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    engine: &E,
    run_acc: &mut Acc,
    active_map: &mut HashMap<u64, Acc, FxBuildHasher>,
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
{
    if let Some(sample) = engine.get_sample(val_raw) {
        coords.for_each_subpixel(
            k,
            geom_ctx,
            gt,
            crs_transformer,
            col_offset,
            sampling,
            bbox,
            |lon, lat, d_x, d_y, weight| {
                let cell = match resolve_subpixel_cell(run_cell, lat, lon, d_x, d_y, res) {
                    Some(c) => c,
                    None => return,
                };

                if cell == run_cell {
                    engine.update_sample(run_acc, sample, weight);
                } else {
                    active_map
                        .entry(cell)
                        .and_modify(|acc| engine.update_sample(acc, sample, weight))
                        .or_insert_with(|| {
                            let mut acc = engine.new_acc();
                            engine.update_sample(&mut acc, sample, weight);
                            acc
                        });
                }
            },
        );
    }
}

/// Evaluates subpixel sample points for a boundary pixel across multiple active resolutions.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn evaluate_subpixel_pixel_multi<T, Acc, E>(
    val_raw: T,
    k: usize,
    num_res: usize,
    resolutions: &[Resolution],
    run_cells: &[u64],
    core_starts: &[usize],
    core_ends: &[usize],
    coords: &RowCoordinates,
    geom_ctx: &RowGeometryContext,
    gt: &GeoTransform,
    crs_transformer: &CrsTransformer,
    col_offset: usize,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    engine: &E,
    run_accs: &mut [Acc],
    chunk_maps: &mut [HashMap<u64, Acc, FxBuildHasher>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
{
    if let Some(sample) = engine.get_sample(val_raw) {
        for i in 0..num_res {
            if run_cells[i] == 0 {
                continue;
            }
            if k >= core_starts[i] && k < core_ends[i] {
                engine.update_sample(&mut run_accs[i], sample, 1.0);
            }
        }

        let any_subpixel =
            (0..num_res).any(|i| run_cells[i] != 0 && (k < core_starts[i] || k >= core_ends[i]));
        if any_subpixel {
            coords.for_each_subpixel(
                k,
                geom_ctx,
                gt,
                crs_transformer,
                col_offset,
                sampling,
                bbox,
                |lon, lat, d_x, d_y, weight| {
                    for i in 0..num_res {
                        if run_cells[i] == 0 || (k >= core_starts[i] && k < core_ends[i]) {
                            continue;
                        }
                        let res = resolutions[i];
                        let cell =
                            match resolve_subpixel_cell(run_cells[i], lat, lon, d_x, d_y, res) {
                                Some(c) => c,
                                None => continue,
                            };

                        if cell == run_cells[i] {
                            engine.update_sample(&mut run_accs[i], sample, weight);
                        } else {
                            chunk_maps[i]
                                .entry(cell)
                                .and_modify(|acc| engine.update_sample(acc, sample, weight))
                                .or_insert_with(|| {
                                    let mut a = engine.new_acc();
                                    engine.update_sample(&mut a, sample, weight);
                                    a
                                });
                        }
                    }
                },
            );
        }
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
    row_cache: &mut H3ScanlineLookahead,
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
{
    row_cache.reset_row();
    let is_single_point = sampling.is_single_point();
    let dx_bounds = sampling.dx_bounds();
    let dy_bounds = sampling.dy_bounds();

    let mut run_cell: u64 = 0;
    let mut run_acc = engine.new_acc();
    let mut known_next_cell: Option<u64> = None;

    let mut cursor = ScanlineCursor::new(
        coords.row_c_start,
        coords.x_start,
        coords.lon_start,
        geom_ctx.dx_step,
        geom_ctx.d_lon_step,
        geom_ctx,
    );

    while cursor.col < coords.row_c_end {
        let c = cursor.col;
        let (lon, lat) = match coords.pixel_center_lon_lat(
            c,
            cursor.x_curr,
            cursor.lon_curr,
            geom_ctx,
            gt,
            crs_transformer,
            chunk.col_offset as usize,
        ) {
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

        let cell_opt = known_next_cell
            .take()
            .or_else(|| row_cache.get_or_compute_cell(lat, lon, res));

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
                row_cache.on_cell_changed();
            }

            let (span_end, next_cell) = coords.find_span_end(
                row_cache,
                c,
                cursor.lon_curr,
                geom_ctx,
                crs_transformer,
                res,
                run_cell,
                bbox,
            );

            if is_single_point {
                let span_slice = &slice_row[c..span_end];
                engine.accumulate_span(&mut run_acc, span_slice);
            } else {
                let (core_start, core_end) = coords.find_core_span(
                    row_cache,
                    chunk,
                    c,
                    span_end,
                    dx_bounds,
                    dy_bounds,
                    geom_ctx,
                    gt,
                    crs_transformer,
                    bbox,
                    |lat, lon| {
                        LatLng::new(lat, lon).ok().map(|ll| ll.to_cell(res).into())
                            == Some(run_cell)
                    },
                );

                for k in c..core_start {
                    evaluate_subpixel_pixel(
                        slice_row[k],
                        k,
                        run_cell,
                        res,
                        coords,
                        geom_ctx,
                        gt,
                        crs_transformer,
                        chunk.col_offset as usize,
                        sampling,
                        bbox,
                        engine,
                        &mut run_acc,
                        active_map,
                    );
                }

                if core_end > core_start {
                    let core_slice = &slice_row[core_start..core_end];
                    engine.accumulate_span(&mut run_acc, core_slice);
                }

                for k in core_end..span_end {
                    evaluate_subpixel_pixel(
                        slice_row[k],
                        k,
                        run_cell,
                        res,
                        coords,
                        geom_ctx,
                        gt,
                        crs_transformer,
                        chunk.col_offset as usize,
                        sampling,
                        bbox,
                        engine,
                        &mut run_acc,
                        active_map,
                    );
                }
            }

            let num_stepped = span_end - c;
            row_cache.advance_span(num_stepped);
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
    row_caches: Vec<H3ScanlineLookahead>,
    span_ends: Vec<usize>,
    run_cells: Vec<u64>,
    run_accs: Vec<Acc>,
    known_next_cells: Vec<Option<u64>>,
    core_starts: Vec<usize>,
    core_ends: Vec<usize>,
}

impl<Acc: Clone> MultiResRowBuffers<Acc> {
    fn new<T, E: ScanlineEngine<T, Acc>>(resolutions: &[Resolution], engine: &E) -> Self {
        let num_res = resolutions.len();
        Self {
            row_caches: resolutions
                .iter()
                .map(|&res| H3ScanlineLookahead::for_resolution(res))
                .collect(),
            span_ends: vec![0; num_res],
            run_cells: vec![0; num_res],
            run_accs: (0..num_res).map(|_| engine.new_acc()).collect(),
            known_next_cells: vec![None; num_res],
            core_starts: vec![0; num_res],
            core_ends: vec![0; num_res],
        }
    }

    fn reset_for_row<T, E: ScanlineEngine<T, Acc>>(&mut self, row_c_start: usize, engine: &E) {
        let num_res = self.row_caches.len();
        for i in 0..num_res {
            self.row_caches[i].reset_row();
            self.run_cells[i] = 0;
            engine.clear_acc(&mut self.run_accs[i]);
            self.known_next_cells[i] = None;
            self.span_ends[i] = row_c_start;
            self.core_starts[i] = row_c_start;
            self.core_ends[i] = row_c_start;
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
        self.core_starts[i] = c + 1;
        self.core_ends[i] = c + 1;
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
    let dx_bounds = sampling.dx_bounds();
    let dy_bounds = sampling.dy_bounds();

    let mut cursor = ScanlineCursor::new(
        coords.row_c_start,
        coords.x_start,
        coords.lon_start,
        geom_ctx.dx_step,
        geom_ctx.d_lon_step,
        geom_ctx,
    );

    while cursor.col < coords.row_c_end {
        let c = cursor.col;
        let (lon, lat) = match coords.pixel_center_lon_lat(
            c,
            cursor.x_curr,
            cursor.lon_curr,
            geom_ctx,
            gt,
            crs_transformer,
            chunk.col_offset as usize,
        ) {
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
                    .or_else(|| buf.row_caches[i].get_or_compute_cell(lat, lon, res));

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
                        buf.row_caches[i].on_cell_changed();
                    }

                    let (span_end, next_cell) = coords.find_span_end(
                        &mut buf.row_caches[i],
                        c,
                        cursor.lon_curr,
                        geom_ctx,
                        crs_transformer,
                        res,
                        buf.run_cells[i],
                        bbox,
                    );

                    buf.span_ends[i] = span_end;
                    buf.known_next_cells[i] = next_cell;

                    if !is_single_point {
                        let (c_start, c_end) = coords.find_core_span(
                            &buf.row_caches[i],
                            chunk,
                            c,
                            span_end,
                            dx_bounds,
                            dy_bounds,
                            geom_ctx,
                            gt,
                            crs_transformer,
                            bbox,
                            |test_lat, test_lon| {
                                LatLng::new(test_lat, test_lon)
                                    .ok()
                                    .map(|ll| ll.to_cell(res).into())
                                    == Some(buf.run_cells[i])
                            },
                        );
                        buf.core_starts[i] = c_start;
                        buf.core_ends[i] = c_end;
                    }
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

        if is_single_point {
            let span_slice = &slice_row[c..step_end];
            engine.accumulate_span_multi(&mut buf.run_accs, &buf.run_cells, span_slice);
        } else {
            let mut sub_core_start = c;
            let mut sub_core_end = step_end;
            for i in 0..num_res {
                if buf.run_cells[i] != 0 {
                    sub_core_start = sub_core_start.max(buf.core_starts[i]);
                    sub_core_end = sub_core_end.min(buf.core_ends[i]);
                }
            }

            if sub_core_start < sub_core_end {
                for k in c..sub_core_start {
                    evaluate_subpixel_pixel_multi(
                        slice_row[k],
                        k,
                        num_res,
                        resolutions,
                        &buf.run_cells,
                        &buf.core_starts,
                        &buf.core_ends,
                        coords,
                        geom_ctx,
                        gt,
                        crs_transformer,
                        chunk.col_offset as usize,
                        sampling,
                        bbox,
                        engine,
                        &mut buf.run_accs,
                        chunk_maps,
                    );
                }

                let core_slice = &slice_row[sub_core_start..sub_core_end];
                engine.accumulate_span_multi(&mut buf.run_accs, &buf.run_cells, core_slice);

                for k in sub_core_end..step_end {
                    evaluate_subpixel_pixel_multi(
                        slice_row[k],
                        k,
                        num_res,
                        resolutions,
                        &buf.run_cells,
                        &buf.core_starts,
                        &buf.core_ends,
                        coords,
                        geom_ctx,
                        gt,
                        crs_transformer,
                        chunk.col_offset as usize,
                        sampling,
                        bbox,
                        engine,
                        &mut buf.run_accs,
                        chunk_maps,
                    );
                }
            } else {
                for k in c..step_end {
                    evaluate_subpixel_pixel_multi(
                        slice_row[k],
                        k,
                        num_res,
                        resolutions,
                        &buf.run_cells,
                        &buf.core_starts,
                        &buf.core_ends,
                        coords,
                        geom_ctx,
                        gt,
                        crs_transformer,
                        chunk.col_offset as usize,
                        sampling,
                        bbox,
                        engine,
                        &mut buf.run_accs,
                        chunk_maps,
                    );
                }
            }
        }

        let num_stepped = step_end - c;
        for i in 0..num_res {
            buf.row_caches[i].advance_span(num_stepped);
        }
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
    let geom_ctx = RowGeometryContext::new(chunk, slice.len(), chunk_stride, crs_transformer, gt);
    let RowGeometryContext {
        stride,
        actual_rows,
        ..
    } = geom_ctx;
    let num_res = resolutions.len();

    if num_res == 1 {
        let mut row_cache = H3ScanlineLookahead::for_resolution(resolutions[0]);
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
                    &mut row_cache,
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
        let mut cursor = ScanlineCursor {
            col: 10,
            x_curr: 100.0,
            lon_curr: -122.0,
            dx_step: 2.0,
            d_lon_step: 0.01,
            is_north_up: true,
            is_geographic: true,
        };
        cursor.advance(5);
        assert_eq!(cursor.col, 15);
        assert!((cursor.lon_curr - (-121.95)).abs() < 1e-10);
    }
}
