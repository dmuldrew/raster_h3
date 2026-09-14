use std::collections::HashMap;
use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::h3_scanline::{can_use_neighbor_cache, H3NeighborDiskCache, H3ScanlineLookahead};
use crate::aggregator::horizon_streamer::{is_decoding_result_all_nodata, NodataCast};
use crate::aggregator::sampling::SamplingPattern;
use crate::aggregator::simd::SimdSpanAccumulate;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::config::SpectralFormula;
use super::walker::{
    is_point_in_bbox, is_slice_all_native_nodata, resolve_subpixel_cell, RowCoordinates,
    RowGeometryContext,
};
use super::walker::walk_overlap_pixel_cells;

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
}

/// Direct pixel-by-pixel continuous slice aggregation with strict tile ownership resolution
fn process_continuous_overlap_slice_into_maps<T>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<T>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    tile_idx: usize,
    mosaic: &MosaicReader,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) where
    T: SimdSpanAccumulate,
{
    walk_overlap_pixel_cells(
        slice,
        chunk,
        chunk_stride,
        resolutions,
        crs_transformer,
        gt,
        sampling,
        bbox,
        tile_idx,
        mosaic,
        |val| val.is_valid(native_nodata),
        |res_idx, cell_u64, weight, val| {
            let float_val = val.to_f64_val();
            chunk_maps[res_idx]
                .entry(cell_u64)
                .and_modify(|acc| {
                    if weight == 1.0 {
                        acc.update(float_val);
                    } else {
                        acc.update_weighted(float_val, weight);
                    }
                })
                .or_insert_with(|| {
                    let mut acc = if track_quantiles {
                        H3Accumulator::with_quantiles()
                    } else {
                        H3Accumulator::default()
                    };
                    if weight == 1.0 {
                        acc.update(float_val);
                    } else {
                        acc.update_weighted(float_val, weight);
                    }
                    acc
                });
        },
    );
}

/// Process a single typed chunk slice for continuous numeric aggregation across resolutions
fn process_continuous_slice_into_maps<T>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<T>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) where
    T: SimdSpanAccumulate,
{
    if slice.is_empty() {
        return;
    }

    if let Some((tile_idx, mosaic)) = overlap_ctx {
        process_continuous_overlap_slice_into_maps(
            slice,
            chunk,
            native_nodata,
            resolutions,
            crs_transformer,
            gt,
            sampling,
            bbox,
            chunk_stride,
            tile_idx,
            mosaic,
            track_quantiles,
            chunk_maps,
        );
        return;
    }

    let geom_ctx = RowGeometryContext::new(chunk, slice.len(), chunk_stride, crs_transformer, gt);
    let RowGeometryContext {
        is_wgs84,
        is_web_mercator,
        is_north_up,
        d_lon_step,
        dx_step,
        stride,
        actual_rows,
    } = geom_ctx;
    let num_res = resolutions.len();

    let mut row_caches: Vec<H3ScanlineLookahead> = resolutions
        .iter()
        .map(|&res| H3ScanlineLookahead::for_resolution(res))
        .collect();

    let is_single_point = sampling.is_single_point();
    let dx_bounds = sampling.dx_bounds();
    let dy_bounds = sampling.dy_bounds();

    let mut span_ends = if num_res > 1 { vec![0usize; num_res] } else { Vec::new() };
    let mut run_cells = if num_res > 1 { vec![0u64; num_res] } else { Vec::new() };
    let mut run_accs: Vec<H3Accumulator> = if num_res > 1 {
        (0..num_res)
            .map(|_| {
                if track_quantiles {
                    H3Accumulator::with_quantiles()
                } else {
                    H3Accumulator::default()
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut known_next_cells: Vec<Option<u64>> = if num_res > 1 { vec![None; num_res] } else { Vec::new() };
    let mut core_starts = if num_res > 1 { vec![0usize; num_res] } else { Vec::new() };
    let mut core_ends = if num_res > 1 { vec![0usize; num_res] } else { Vec::new() };

    for r in 0..actual_rows {
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        let slice_row = &slice[slice_row_start..slice_row_start + row_width];
        if is_slice_all_native_nodata(slice_row, native_nodata) {
            continue;
        }

        let coords = match RowCoordinates::compute(
            r,
            chunk,
            row_width,
            &geom_ctx,
            gt,
            crs_transformer,
            sampling,
            bbox,
        ) {
            Some(c) => c,
            None => continue,
        };

        let RowCoordinates {
            x_start,
            lon_start,
            row_c_start,
            row_c_end,
            cos_lat_sq,
            px_diag_m,
            ..
        } = coords;

            if num_res == 1 {
                for res_idx in 0..num_res {
                    let res = resolutions[res_idx];
                    let active_map = &mut chunk_maps[res_idx];
                    let row_cache = &mut row_caches[res_idx];
                    row_cache.reset_row();
                    let mut run_cell: u64 = 0;
                    let mut run_acc = if track_quantiles {
                        H3Accumulator::with_quantiles()
                    } else {
                        H3Accumulator::default()
                    };

                    let use_neighbor_cache = !is_single_point && can_use_neighbor_cache(px_diag_m, res);
                    let mut disk_cache = H3NeighborDiskCache::default();

                    let mut lon_curr = lon_start + (row_c_start as f64) * d_lon_step;
                    let mut x_curr = x_start + (row_c_start as f64) * dx_step;
                    let mut c = row_c_start;
                    let mut known_next_cell: Option<u64> = None;

                    while c < row_c_end {
                        let (lon, lat) = match coords.pixel_center_lon_lat(
                            c,
                            x_curr,
                            lon_curr,
                            &geom_ctx,
                            gt,
                            crs_transformer,
                            chunk.col_offset as usize,
                        ) {
                            Some(ll) => ll,
                            None => {
                                known_next_cell = None;
                                c += 1;
                                if is_north_up {
                                    x_curr += dx_step;
                                }
                                continue;
                            }
                        };

                        if !is_wgs84 && !is_web_mercator {
                            if !is_point_in_bbox(lon, lat, bbox) {
                                known_next_cell = None;
                                c += 1;
                                if is_north_up {
                                    x_curr += dx_step;
                                }
                                continue;
                            }
                        }

                        let cell_opt = known_next_cell.take().or_else(|| row_cache.get_or_compute_cell(lat, lon, res));

                        if let Some(cell_u64) = cell_opt {
                            if cell_u64 != run_cell {
                                if run_cell != 0 && run_acc.count > 0.0 {
                                    active_map
                                        .entry(run_cell)
                                        .and_modify(|acc| acc.merge(&run_acc))
                                        .or_insert_with(|| run_acc.clone());
                                }
                                run_cell = cell_u64;
                                run_acc.clear();
                                row_cache.on_cell_changed();
                                if use_neighbor_cache {
                                    disk_cache.update(run_cell, cos_lat_sq);
                                }
                            }

                            let (span_end, next_cell) = coords.find_span_end(
                                row_cache,
                                c,
                                lon_curr,
                                &geom_ctx,
                                crs_transformer,
                                res,
                                run_cell,
                                bbox,
                            );

                            if is_single_point {
                                let span_slice = &slice[slice_row_start + c..slice_row_start + span_end];
                                let span_acc = T::accumulate_span(span_slice, native_nodata);
                                if span_acc.count > 0.0 {
                                    run_acc.merge(&span_acc);
                                    if track_quantiles {
                                        if let Some(ref mut q) = run_acc.quantiles {
                                            for &v in span_slice {
                                                if v.is_valid(native_nodata) {
                                                    q.update(v.to_f64_val(), 1.0);
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                let (core_start, core_end) = coords.find_core_span(
                                    row_cache,
                                    c,
                                    span_end,
                                    dx_bounds,
                                    dy_bounds,
                                    &geom_ctx,
                                    gt,
                                    bbox,
                                    |lat, lon| {
                                        if use_neighbor_cache {
                                            disk_cache.is_in_run_cell(lat, lon)
                                        } else {
                                            LatLng::new(lat, lon).ok().map(|ll| ll.to_cell(res).into()) == Some(run_cell)
                                        }
                                    },
                                );

                                let mut evaluate_boundary = |k: usize| {
                                    let val_raw = slice[slice_row_start + k];
                                    if !val_raw.is_valid(native_nodata) {
                                        return;
                                    }
                                    let val = val_raw.to_f64_val();

                                    coords.for_each_subpixel(
                                        k,
                                        &geom_ctx,
                                        gt,
                                        crs_transformer,
                                        chunk.col_offset as usize,
                                        sampling,
                                        bbox,
                                        |lon, lat, d_x, d_y, weight| {
                                            let cell = match resolve_subpixel_cell(
                                                run_cell,
                                                lat,
                                                lon,
                                                d_x,
                                                d_y,
                                                res,
                                                use_neighbor_cache,
                                                &mut disk_cache,
                                            ) {
                                                Some(c) => c,
                                                None => return,
                                            };

                                            active_map
                                                .entry(cell)
                                                .and_modify(|acc| acc.update_weighted(val, weight))
                                                .or_insert_with(|| {
                                                    let mut a = if track_quantiles {
                                                        H3Accumulator::with_quantiles()
                                                    } else {
                                                        H3Accumulator::default()
                                                    };
                                                    a.update_weighted(val, weight);
                                                    a
                                                });
                                        },
                                    );
                                };

                                for k in c..core_start {
                                    evaluate_boundary(k);
                                }

                                if core_start < core_end {
                                    let core_slice = &slice[slice_row_start + core_start..slice_row_start + core_end];
                                    let core_acc = T::accumulate_span(core_slice, native_nodata);
                                    if core_acc.count > 0.0 {
                                        run_acc.merge(&core_acc);
                                        if track_quantiles {
                                            if let Some(ref mut q) = run_acc.quantiles {
                                                for &v in core_slice {
                                                    if v.is_valid(native_nodata) {
                                                        q.update(v.to_f64_val(), 1.0);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                for k in core_end..span_end {
                                    evaluate_boundary(k);
                                }
                            }

                            let num_stepped = span_end - c;
                            row_cache.advance_span(num_stepped);
                            if is_wgs84 || is_web_mercator {
                                lon_curr += (num_stepped as f64) * d_lon_step;
                            } else if is_north_up {
                                x_curr += (num_stepped as f64) * dx_step;
                            }
                            c = span_end;
                            known_next_cell = next_cell;
                        } else {
                            c += 1;
                            if is_wgs84 || is_web_mercator {
                                lon_curr += d_lon_step;
                            } else if is_north_up {
                                x_curr += dx_step;
                            }
                        }
                    }

                    if run_cell != 0 && run_acc.count > 0.0 {
                        active_map
                            .entry(run_cell)
                            .and_modify(|acc| acc.merge(&run_acc))
                            .or_insert_with(|| run_acc.clone());
                    }
                }
            } else {
                let use_neighbor_caches: Vec<bool> = resolutions
                    .iter()
                    .map(|&r| !is_single_point && can_use_neighbor_cache(px_diag_m, r))
                    .collect();
                let mut disk_caches = vec![H3NeighborDiskCache::default(); num_res];

                for i in 0..num_res {
                    row_caches[i].reset_row();
                    run_cells[i] = 0;
                    run_accs[i].clear();
                    known_next_cells[i] = None;
                    span_ends[i] = row_c_start;
                    core_starts[i] = row_c_start;
                    core_ends[i] = row_c_start;
                }

                let mut lon_curr = lon_start + (row_c_start as f64) * d_lon_step;
                let mut x_curr = x_start + (row_c_start as f64) * dx_step;
                let mut c = row_c_start;

                while c < row_c_end {
                    let (lon, lat) = match coords.pixel_center_lon_lat(
                        c,
                        x_curr,
                        lon_curr,
                        &geom_ctx,
                        gt,
                        crs_transformer,
                        chunk.col_offset as usize,
                    ) {
                        Some(ll) => ll,
                        None => {
                            for i in 0..num_res {
                                if run_cells[i] != 0 && run_accs[i].count > 0.0 {
                                    chunk_maps[i]
                                        .entry(run_cells[i])
                                        .and_modify(|acc| acc.merge(&run_accs[i]))
                                        .or_insert_with(|| run_accs[i].clone());
                                    run_accs[i].clear();
                                }
                                run_cells[i] = 0;
                                known_next_cells[i] = None;
                                span_ends[i] = c + 1;
                            }
                            c += 1;
                            if is_north_up {
                                x_curr += dx_step;
                            }
                            continue;
                        }
                    };

                    if !is_wgs84 && !is_web_mercator {
                        if !is_point_in_bbox(lon, lat, bbox) {
                            for i in 0..num_res {
                                if run_cells[i] != 0 && run_accs[i].count > 0.0 {
                                    chunk_maps[i]
                                        .entry(run_cells[i])
                                        .and_modify(|acc| acc.merge(&run_accs[i]))
                                        .or_insert_with(|| run_accs[i].clone());
                                    run_accs[i].clear();
                                }
                                run_cells[i] = 0;
                                known_next_cells[i] = None;
                                span_ends[i] = c + 1;
                            }
                            c += 1;
                            if is_north_up {
                                x_curr += dx_step;
                            }
                            continue;
                        }
                    }

                    for i in 0..num_res {
                        if c >= span_ends[i] {
                            let res = resolutions[i];
                            let cell_opt = known_next_cells[i]
                                .take()
                                .or_else(|| row_caches[i].get_or_compute_cell(lat, lon, res));

                            if let Some(cell_u64) = cell_opt {
                                if cell_u64 != run_cells[i] {
                                    if run_cells[i] != 0 && run_accs[i].count > 0.0 {
                                        chunk_maps[i]
                                            .entry(run_cells[i])
                                            .and_modify(|acc| acc.merge(&run_accs[i]))
                                            .or_insert_with(|| run_accs[i].clone());
                                        run_accs[i].clear();
                                    }
                                    run_cells[i] = cell_u64;
                                    row_caches[i].on_cell_changed();
                                    if use_neighbor_caches[i] {
                                        disk_caches[i].update(run_cells[i], cos_lat_sq);
                                    }
                                }

                                let (span_end, next_cell) = coords.find_span_end(
                                    &mut row_caches[i],
                                    c,
                                    lon_curr,
                                    &geom_ctx,
                                    crs_transformer,
                                    res,
                                    run_cells[i],
                                    bbox,
                                );

                                span_ends[i] = span_end;
                                known_next_cells[i] = next_cell;

                                if !is_single_point {
                                    let (c_start, c_end) = coords.find_core_span(
                                        &row_caches[i],
                                        c,
                                        span_end,
                                        dx_bounds,
                                        dy_bounds,
                                        &geom_ctx,
                                        gt,
                                        bbox,
                                        |test_lat, test_lon| {
                                            if use_neighbor_caches[i] {
                                                disk_caches[i].is_in_run_cell(test_lat, test_lon)
                                            } else {
                                                LatLng::new(test_lat, test_lon).ok().map(|ll| ll.to_cell(res).into())
                                                    == Some(run_cells[i])
                                            }
                                        },
                                    );
                                    core_starts[i] = c_start;
                                    core_ends[i] = c_end;
                                }
                            } else {
                                if run_cells[i] != 0 && run_accs[i].count > 0.0 {
                                    chunk_maps[i]
                                        .entry(run_cells[i])
                                        .and_modify(|acc| acc.merge(&run_accs[i]))
                                        .or_insert_with(|| run_accs[i].clone());
                                    run_accs[i].clear();
                                }
                                run_cells[i] = 0;
                                span_ends[i] = c + 1;
                                known_next_cells[i] = None;
                                core_starts[i] = c + 1;
                                core_ends[i] = c + 1;
                            }
                        }
                    }

                    let mut step_end = row_c_end;
                    for i in 0..num_res {
                        step_end = step_end.min(span_ends[i]);
                    }
                    let step_end = step_end.max(c + 1).min(row_c_end);

                    if is_single_point {
                        let span_slice = &slice[slice_row_start + c..slice_row_start + step_end];
                        let span_acc = T::accumulate_span(span_slice, native_nodata);
                        if span_acc.count > 0.0 {
                            for i in 0..num_res {
                                if run_cells[i] != 0 {
                                    run_accs[i].merge(&span_acc);
                                }
                            }
                            if track_quantiles {
                                for &v in span_slice {
                                    if v.is_valid(native_nodata) {
                                        let fv = v.to_f64_val();
                                        for i in 0..num_res {
                                            if run_cells[i] != 0 {
                                                if let Some(ref mut q) = run_accs[i].quantiles {
                                                    q.update(fv, 1.0);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        let mut sub_core_start = c;
                        let mut sub_core_end = step_end;
                        for i in 0..num_res {
                            if run_cells[i] != 0 {
                                sub_core_start = sub_core_start.max(core_starts[i]);
                                sub_core_end = sub_core_end.min(core_ends[i]);
                            }
                        }

                        let mut evaluate_boundary_multi = |k: usize,
                                                       run_accs: &mut [H3Accumulator],
                                                       chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>]| {
                            let val_raw = slice[slice_row_start + k];
                            if !val_raw.is_valid(native_nodata) {
                                return;
                            }
                            let val = val_raw.to_f64_val();

                            for i in 0..num_res {
                                if run_cells[i] == 0 {
                                    continue;
                                }
                                if k >= core_starts[i] && k < core_ends[i] {
                                    run_accs[i].update_weighted(val, 1.0);
                                    if track_quantiles {
                                        if let Some(ref mut q) = run_accs[i].quantiles {
                                            q.update(val, 1.0);
                                        }
                                    }
                                }
                            }

                            let any_subpixel = (0..num_res).any(|i| run_cells[i] != 0 && (k < core_starts[i] || k >= core_ends[i]));
                            if any_subpixel {
                                coords.for_each_subpixel(
                                    k,
                                    &geom_ctx,
                                    gt,
                                    crs_transformer,
                                    chunk.col_offset as usize,
                                    sampling,
                                    bbox,
                                    |lon, lat, d_x, d_y, weight| {
                                        for i in 0..num_res {
                                            if run_cells[i] == 0 || (k >= core_starts[i] && k < core_ends[i]) {
                                                continue;
                                            }
                                            let res = resolutions[i];
                                            let cell = match resolve_subpixel_cell(
                                                run_cells[i],
                                                lat,
                                                lon,
                                                d_x,
                                                d_y,
                                                res,
                                                use_neighbor_caches[i],
                                                &mut disk_caches[i],
                                            ) {
                                                Some(c) => c,
                                                None => continue,
                                            };

                                            chunk_maps[i]
                                                .entry(cell)
                                                .and_modify(|acc| acc.update_weighted(val, weight))
                                                .or_insert_with(|| {
                                                    let mut a = if track_quantiles {
                                                        H3Accumulator::with_quantiles()
                                                    } else {
                                                        H3Accumulator::default()
                                                    };
                                                    a.update_weighted(val, weight);
                                                    a
                                                });
                                        }
                                    },
                                );
                            }
                        };

                        if sub_core_start < sub_core_end {
                            for k in c..sub_core_start {
                                evaluate_boundary_multi(k, &mut run_accs, chunk_maps);
                            }

                            let core_slice = &slice[slice_row_start + sub_core_start..slice_row_start + sub_core_end];
                            let core_acc = T::accumulate_span(core_slice, native_nodata);
                            if core_acc.count > 0.0 {
                                for i in 0..num_res {
                                    if run_cells[i] != 0 {
                                        run_accs[i].merge(&core_acc);
                                    }
                                }
                                if track_quantiles {
                                    for &v in core_slice {
                                        if v.is_valid(native_nodata) {
                                            let fv = v.to_f64_val();
                                            for i in 0..num_res {
                                                if run_cells[i] != 0 {
                                                    if let Some(ref mut q) = run_accs[i].quantiles {
                                                        q.update(fv, 1.0);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            for k in sub_core_end..step_end {
                                evaluate_boundary_multi(k, &mut run_accs, chunk_maps);
                            }
                        } else {
                            for k in c..step_end {
                                evaluate_boundary_multi(k, &mut run_accs, chunk_maps);
                            }
                        }
                    }

                    let num_stepped = step_end - c;
                    for i in 0..num_res {
                        row_caches[i].advance_span(num_stepped);
                    }
                    if is_wgs84 || is_web_mercator {
                        lon_curr += (num_stepped as f64) * d_lon_step;
                    } else if is_north_up {
                        x_curr += (num_stepped as f64) * dx_step;
                    }
                    c = step_end;
                }

                for i in 0..num_res {
                    if run_cells[i] != 0 && run_accs[i].count > 0.0 {
                        chunk_maps[i]
                            .entry(run_cells[i])
                            .and_modify(|acc| acc.merge(&run_accs[i]))
                            .or_insert_with(|| run_accs[i].clone());
                    }
                }
            }
        }
    }

/// Process a multi-sample or spectral formula continuous slice into thread-local hash maps
fn process_continuous_multisample_slice_into_maps<T: SimdSpanAccumulate>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<T>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    samples_per_pixel: usize,
    band: usize,
    spectral_formula: Option<SpectralFormula>,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) {
    let row_width = chunk.width as usize;
    let spp = samples_per_pixel.max(1);
    let num_res = resolutions.len();

    for row_idx in 0..chunk.height as usize {
        let slice_row_start = (row_idx * (chunk_stride as usize)) * spp;

        for c in 0..row_width {
            let pixel_base = slice_row_start + c * spp;

            let val_opt = match spectral_formula {
                Some(SpectralFormula::Ndvi { nir_band, red_band }) => {
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let red_idx = (red_band.saturating_sub(1)).min(spp - 1);
                    let nir_raw = slice[pixel_base + nir_idx];
                    let red_raw = slice[pixel_base + red_idx];
                    if nir_raw.is_valid(native_nodata) && red_raw.is_valid(native_nodata) {
                        let nir = nir_raw.to_f64_val();
                        let red = red_raw.to_f64_val();
                        let denom = nir + red;
                        if denom.abs() > 1e-12 {
                            Some((nir - red) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Some(SpectralFormula::Ndwi { green_band, nir_band }) => {
                    let green_idx = (green_band.saturating_sub(1)).min(spp - 1);
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let green_raw = slice[pixel_base + green_idx];
                    let nir_raw = slice[pixel_base + nir_idx];
                    if green_raw.is_valid(native_nodata) && nir_raw.is_valid(native_nodata) {
                        let green = green_raw.to_f64_val();
                        let nir = nir_raw.to_f64_val();
                        let denom = green + nir;
                        if denom.abs() > 1e-12 {
                            Some((green - nir) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Some(SpectralFormula::Nbr { nir_band, swir_band }) => {
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let swir_idx = (swir_band.saturating_sub(1)).min(spp - 1);
                    let nir_raw = slice[pixel_base + nir_idx];
                    let swir_raw = slice[pixel_base + swir_idx];
                    if nir_raw.is_valid(native_nodata) && swir_raw.is_valid(native_nodata) {
                        let nir = nir_raw.to_f64_val();
                        let swir = swir_raw.to_f64_val();
                        let denom = nir + swir;
                        if denom.abs() > 1e-12 {
                            Some((nir - swir) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Some(SpectralFormula::Evi { nir_band, red_band, blue_band }) => {
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let red_idx = (red_band.saturating_sub(1)).min(spp - 1);
                    let blue_idx = (blue_band.saturating_sub(1)).min(spp - 1);
                    let nir_raw = slice[pixel_base + nir_idx];
                    let red_raw = slice[pixel_base + red_idx];
                    let blue_raw = slice[pixel_base + blue_idx];
                    if nir_raw.is_valid(native_nodata) && red_raw.is_valid(native_nodata) && blue_raw.is_valid(native_nodata) {
                        let nir = nir_raw.to_f64_val();
                        let red = red_raw.to_f64_val();
                        let blue = blue_raw.to_f64_val();
                        let denom = nir + 6.0 * red - 7.5 * blue + 1.0;
                        if denom.abs() > 1e-12 {
                            Some(2.5 * (nir - red) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                None => {
                    let b_idx = (band.saturating_sub(1)).min(spp - 1);
                    let raw = slice[pixel_base + b_idx];
                    if raw.is_valid(native_nodata) {
                        Some(raw.to_f64_val())
                    } else {
                        None
                    }
                }
            };

            let val = match val_opt {
                Some(v) => v,
                None => continue,
            };

            for sp in &sampling.points {
                let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                let py = (chunk.row_offset as f64) + (row_idx as f64) + sp.dy;
                let (x, y) = gt.pixel_to_coord(px, py);
                if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                    if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                        if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                            continue;
                        }
                    }

                    if let Some((tile_idx, mosaic)) = overlap_ctx {
                        if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                            continue;
                        }
                    }

                    if let Ok(ll) = LatLng::new(lat, lon) {
                        for res_idx in 0..num_res {
                            let res = resolutions[res_idx];
                            let cell: u64 = ll.to_cell(res).into();
                            chunk_maps[res_idx]
                                .entry(cell)
                                .and_modify(|acc| acc.update_weighted(val, sp.weight))
                                .or_insert_with(|| {
                                    let mut a = if track_quantiles {
                                        H3Accumulator::with_quantiles()
                                    } else {
                                        H3Accumulator::default()
                                    };
                                    a.update_weighted(val, sp.weight);
                                    a
                                });
                        }
                    }
                }
            }
        }
    }
}

/// Process a continuous chunk across all resolutions into thread-local hash maps
pub fn process_continuous_chunk_payload_into(
    chunk_bounds: &RasterChunk,
    decoding_result: &DecodingResult,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    samples_per_pixel: u16,
    band: usize,
    spectral_formula: Option<SpectralFormula>,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) -> bool {
    let is_multisample = samples_per_pixel > 1 || spectral_formula.is_some() || band > 1;
    let spp = samples_per_pixel.max(1) as usize;

    macro_rules! dispatch_continuous {
        ($dr:expr, $nodata:expr, |$slice:ident, $nd:ident| $body:expr) => {
            match $dr {
                DecodingResult::U8($slice) => { let $nd = <u8 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::U16($slice) => { let $nd = <u16 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::U32($slice) => { let $nd = <u32 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::U64($slice) => { let $nd = <u64 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::I8($slice) => { let $nd = <i8 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::I16($slice) => { let $nd = <i16 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::I32($slice) => { let $nd = <i32 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::I64($slice) => { let $nd = <i64 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::F32($slice) => { let $nd = <f32 as NodataCast>::from_nodata_f64($nodata); $body }
                DecodingResult::F64($slice) => { let $nd = <f64 as NodataCast>::from_nodata_f64($nodata); $body }
            }
        };
    }

    if !is_multisample {
        if is_decoding_result_all_nodata(decoding_result, nodata) {
            return false;
        }

        dispatch_continuous!(decoding_result, nodata, |slice, nd| {
            process_continuous_slice_into_maps(
                slice, chunk_bounds, nd, resolutions, crs_transformer, gt,
                sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps,
            );
        });
    } else {
        dispatch_continuous!(decoding_result, nodata, |slice, nd| {
            process_continuous_multisample_slice_into_maps(
                slice, chunk_bounds, nd, resolutions, crs_transformer, gt,
                sampling, bbox, chunk_stride, spp, band, spectral_formula,
                overlap_ctx, track_quantiles, chunk_maps,
            );
        });
    }
    true
}
