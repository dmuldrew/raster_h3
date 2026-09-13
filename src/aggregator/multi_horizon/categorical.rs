use std::collections::HashMap;
use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use tiff::decoder::DecodingResult;

use crate::aggregator::categorical::{CategoricalAccumulator, CategoricalUniformity};
use crate::aggregator::h3_scanline::{can_use_neighbor_cache, H3NeighborDiskCache, H3ScanlineLookahead};
use crate::aggregator::horizon_streamer::is_chunk_all_nodata;
use crate::aggregator::remap::CategoryRemapper;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::walker::{
    is_point_in_bbox, is_slice_all_native_nodata, resolve_subpixel_cell, walk_overlap_pixel_cells,
    RowCoordinates, RowGeometryContext,
};

/// Categorical record yielded by the multi-resolution categorical streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiCategoricalRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: CategoricalAccumulator,
}

/// Direct pixel-by-pixel categorical slice aggregation with strict tile ownership resolution
fn process_categorical_overlap_slice_into_maps<T, F, N>(
    slice: &[T],
    chunk: &RasterChunk,
    to_i64: F,
    native_nodata: Option<N>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    tile_idx: usize,
    mosaic: &MosaicReader,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) where
    T: Copy + PartialEq,
    F: Fn(T) -> Option<i64>,
    N: Copy + PartialEq<T>,
{
    let resolve_cat = |val: T| -> Option<i64> {
        if let Some(nd) = native_nodata {
            if nd == val {
                return None;
            }
        }
        let raw_cat = to_i64(val)?;
        if let Some(nd_f64) = nodata {
            if (raw_cat as f64 - nd_f64).abs() < 1e-6 {
                return None;
            }
        }
        if let Some(rem) = remapper {
            rem.remap(raw_cat)
        } else {
            Some(raw_cat)
        }
    };

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
        |val| resolve_cat(val).is_some(),
        |res_idx, cell_u64, weight, val| {
            if let Some(cat) = resolve_cat(val) {
                chunk_maps[res_idx]
                    .entry(cell_u64)
                    .and_modify(|acc| acc.update_weighted(cat, weight))
                    .or_insert_with(|| {
                        let mut acc = CategoricalAccumulator::default();
                        acc.update_weighted(cat, weight);
                        acc
                    });
            }
        },
    );
}

/// Process a single typed chunk slice for categorical landcover aggregation across resolutions
fn process_categorical_slice_into_maps<T, F, N>(
    slice: &[T],
    chunk: &RasterChunk,
    to_i64: F,
    native_nodata: Option<N>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) where
    T: CategoricalUniformity,
    F: Fn(T) -> Option<i64>,
    N: Copy + PartialEq<T>,
{
    if slice.is_empty() {
        return;
    }

    if let Some((tile_idx, mosaic)) = overlap_ctx {
        process_categorical_overlap_slice_into_maps(
            slice,
            chunk,
            to_i64,
            native_nodata,
            resolutions,
            crs_transformer,
            gt,
            sampling,
            bbox,
            chunk_stride,
            nodata,
            tile_idx,
            mosaic,
            remapper,
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
    let mut run_accs: Vec<CategoricalAccumulator> = if num_res > 1 {
        vec![CategoricalAccumulator::default(); num_res]
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
                    let mut run_acc = CategoricalAccumulator::default();

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
                                if run_cell != 0 && run_acc.total_count > 0.0 {
                                    active_map
                                        .entry(run_cell)
                                        .and_modify(|acc| acc.merge(&run_acc))
                                        .or_insert_with(|| run_acc);
                                }
                                run_cell = cell_u64;
                                run_acc = CategoricalAccumulator::default();
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

                            let aggregate_cat_span = |sub_slice: &[T], acc: &mut CategoricalAccumulator| {
                                if sub_slice.is_empty() { return; }
                                let first_val = sub_slice[0];
                                let is_uniform = T::is_uniform(sub_slice);

                                if is_uniform {
                                    let mut is_nd = false;
                                    if let Some(nd_nat) = native_nodata {
                                        if nd_nat == first_val {
                                            is_nd = true;
                                        }
                                    }
                                    if !is_nd {
                                        if let Some(raw_cat) = to_i64(first_val) {
                                            let mut is_nd_float = false;
                                            if native_nodata.is_none() {
                                                if let Some(nd) = nodata {
                                                    if (raw_cat as f64 - nd).abs() < 1e-6 {
                                                        is_nd_float = true;
                                                    }
                                                }
                                            }
                                            if !is_nd_float {
                                                let cat_opt = if let Some(rem) = remapper {
                                                    rem.remap(raw_cat)
                                                } else {
                                                    Some(raw_cat)
                                                };
                                                if let Some(cat) = cat_opt {
                                                    acc.update_weighted(cat, sub_slice.len() as f64);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    let mut curr_cat: Option<i64> = None;
                                    let mut curr_cat_count: f64 = 0.0;

                                    for &val_raw in sub_slice {
                                        if let Some(nd_nat) = native_nodata {
                                            if nd_nat == val_raw {
                                                continue;
                                            }
                                        }

                                        if let Some(raw_cat) = to_i64(val_raw) {
                                            if native_nodata.is_none() {
                                                if let Some(nd) = nodata {
                                                    if (raw_cat as f64 - nd).abs() < 1e-6 {
                                                        continue;
                                                    }
                                                }
                                            }
                                            let cat = if let Some(rem) = remapper {
                                                match rem.remap(raw_cat) {
                                                    Some(c) => c,
                                                    None => continue,
                                                }
                                            } else {
                                                raw_cat
                                            };
                                            if Some(cat) == curr_cat {
                                                curr_cat_count += 1.0;
                                            } else {
                                                if let Some(prev) = curr_cat {
                                                    acc.update_weighted(prev, curr_cat_count);
                                                }
                                                curr_cat = Some(cat);
                                                curr_cat_count = 1.0;
                                            }
                                        }
                                    }

                                    if let Some(prev) = curr_cat {
                                        acc.update_weighted(prev, curr_cat_count);
                                    }
                                }
                            };

                            if is_single_point {
                                let span_slice = &slice[slice_row_start + c..slice_row_start + span_end];
                                aggregate_cat_span(span_slice, &mut run_acc);
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
                                    if let Some(nd_nat) = native_nodata {
                                        if nd_nat == val_raw {
                                            return;
                                        }
                                    }

                                    if let Some(raw_cat) = to_i64(val_raw) {
                                        if native_nodata.is_none() {
                                            if let Some(nd) = nodata {
                                                if (raw_cat as f64 - nd).abs() < 1e-6 {
                                                    return;
                                                }
                                            }
                                        }
                                        let cat = if let Some(rem) = remapper {
                                            match rem.remap(raw_cat) {
                                                Some(c) => c,
                                                None => return,
                                            }
                                        } else {
                                            raw_cat
                                        };

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
                                                    .and_modify(|acc| acc.update_weighted(cat, weight))
                                                    .or_insert_with(|| {
                                                        let mut a = CategoricalAccumulator::default();
                                                        a.update_weighted(cat, weight);
                                                        a
                                                    });
                                            },
                                        );
                                    }
                                };

                                for k in c..core_start {
                                    evaluate_boundary(k);
                                }

                                if core_start < core_end {
                                    let core_slice = &slice[slice_row_start + core_start..slice_row_start + core_end];
                                    aggregate_cat_span(core_slice, &mut run_acc);
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

                    if run_cell != 0 && run_acc.total_count > 0.0 {
                        active_map
                            .entry(run_cell)
                            .and_modify(|acc| acc.merge(&run_acc))
                            .or_insert_with(|| run_acc);
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
                    run_accs[i] = CategoricalAccumulator::default();
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
                                if run_cells[i] != 0 && run_accs[i].total_count > 0.0 {
                                    chunk_maps[i]
                                        .entry(run_cells[i])
                                        .and_modify(|acc| acc.merge(&run_accs[i]))
                                        .or_insert_with(|| run_accs[i].clone());
                                    run_accs[i] = CategoricalAccumulator::default();
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
                                if run_cells[i] != 0 && run_accs[i].total_count > 0.0 {
                                    chunk_maps[i]
                                        .entry(run_cells[i])
                                        .and_modify(|acc| acc.merge(&run_accs[i]))
                                        .or_insert_with(|| run_accs[i].clone());
                                    run_accs[i] = CategoricalAccumulator::default();
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
                                    if run_cells[i] != 0 && run_accs[i].total_count > 0.0 {
                                        chunk_maps[i]
                                            .entry(run_cells[i])
                                            .and_modify(|acc| acc.merge(&run_accs[i]))
                                            .or_insert_with(|| run_accs[i].clone());
                                        run_accs[i] = CategoricalAccumulator::default();
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
                                if run_cells[i] != 0 && run_accs[i].total_count > 0.0 {
                                    chunk_maps[i]
                                        .entry(run_cells[i])
                                        .and_modify(|acc| acc.merge(&run_accs[i]))
                                        .or_insert_with(|| run_accs[i].clone());
                                    run_accs[i] = CategoricalAccumulator::default();
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

                    let aggregate_cat_span_multi = |sub_slice: &[T],
                                                    run_accs: &mut [CategoricalAccumulator]| {
                        if sub_slice.is_empty() {
                            return;
                        }
                        let first_val = sub_slice[0];
                        let is_uniform = T::is_uniform(sub_slice);

                        if is_uniform {
                            let mut is_nd = false;
                            if let Some(nd_nat) = native_nodata {
                                if nd_nat == first_val {
                                    is_nd = true;
                                }
                            }
                            if !is_nd {
                                if let Some(raw_cat) = to_i64(first_val) {
                                    let mut is_nd_float = false;
                                    if native_nodata.is_none() {
                                        if let Some(nd) = nodata {
                                            if (raw_cat as f64 - nd).abs() < 1e-6 {
                                                is_nd_float = true;
                                            }
                                        }
                                    }
                                    if !is_nd_float {
                                        let cat_opt = if let Some(rem) = remapper {
                                            rem.remap(raw_cat)
                                        } else {
                                            Some(raw_cat)
                                        };
                                        if let Some(cat) = cat_opt {
                                            let count = sub_slice.len() as f64;
                                            for i in 0..num_res {
                                                if run_cells[i] != 0 {
                                                    run_accs[i].update_weighted(cat, count);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            let mut curr_cat: Option<i64> = None;
                            let mut curr_cat_count: f64 = 0.0;

                            for &val_raw in sub_slice {
                                if let Some(nd_nat) = native_nodata {
                                    if nd_nat == val_raw {
                                        continue;
                                    }
                                }

                                if let Some(raw_cat) = to_i64(val_raw) {
                                    if native_nodata.is_none() {
                                        if let Some(nd) = nodata {
                                            if (raw_cat as f64 - nd).abs() < 1e-6 {
                                                continue;
                                            }
                                        }
                                    }
                                    let cat = if let Some(rem) = remapper {
                                        match rem.remap(raw_cat) {
                                            Some(c) => c,
                                            None => continue,
                                        }
                                    } else {
                                        raw_cat
                                    };
                                    if Some(cat) == curr_cat {
                                        curr_cat_count += 1.0;
                                    } else {
                                        if let Some(prev) = curr_cat {
                                            for i in 0..num_res {
                                                if run_cells[i] != 0 {
                                                    run_accs[i].update_weighted(prev, curr_cat_count);
                                                }
                                            }
                                        }
                                        curr_cat = Some(cat);
                                        curr_cat_count = 1.0;
                                    }
                                }
                            }

                            if let Some(prev) = curr_cat {
                                for i in 0..num_res {
                                    if run_cells[i] != 0 {
                                        run_accs[i].update_weighted(prev, curr_cat_count);
                                    }
                                }
                            }
                        }
                    };

                    if is_single_point {
                        let span_slice = &slice[slice_row_start + c..slice_row_start + step_end];
                        aggregate_cat_span_multi(span_slice, &mut run_accs);
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
                                                           run_accs: &mut [CategoricalAccumulator],
                                                           chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>]| {
                            let val_raw = slice[slice_row_start + k];
                            if let Some(nd_nat) = native_nodata {
                                if nd_nat == val_raw {
                                    return;
                                }
                            }

                            if let Some(raw_cat) = to_i64(val_raw) {
                                if native_nodata.is_none() {
                                    if let Some(nd) = nodata {
                                        if (raw_cat as f64 - nd).abs() < 1e-6 {
                                            return;
                                        }
                                    }
                                }
                                let cat = if let Some(rem) = remapper {
                                    match rem.remap(raw_cat) {
                                        Some(c) => c,
                                        None => return,
                                    }
                                } else {
                                    raw_cat
                                };

                                for i in 0..num_res {
                                    if run_cells[i] == 0 {
                                        continue;
                                    }
                                    if k >= core_starts[i] && k < core_ends[i] {
                                        run_accs[i].update_weighted(cat, 1.0);
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
                                                    .and_modify(|acc| acc.update_weighted(cat, weight))
                                                    .or_insert_with(|| {
                                                        let mut a = CategoricalAccumulator::default();
                                                        a.update_weighted(cat, weight);
                                                        a
                                                    });
                                            }
                                        },
                                    );
                                }
                            }
                        };

                        if sub_core_start < sub_core_end {
                            for k in c..sub_core_start {
                                evaluate_boundary_multi(k, &mut run_accs, chunk_maps);
                            }

                            let core_slice = &slice[slice_row_start + sub_core_start..slice_row_start + sub_core_end];
                            aggregate_cat_span_multi(core_slice, &mut run_accs);

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
                    if run_cells[i] != 0 && run_accs[i].total_count > 0.0 {
                        chunk_maps[i]
                            .entry(run_cells[i])
                            .and_modify(|acc| acc.merge(&run_accs[i]))
                            .or_insert_with(|| run_accs[i].clone());
                    }
                }
            }
        }
    }

/// Process a multi-sample categorical slice into thread-local hash maps for a specific band
fn process_categorical_multisample_slice_into_maps<T, F>(
    slice: &[T],
    chunk: &RasterChunk,
    to_class_fn: F,
    native_nodata: Option<T>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    samples_per_pixel: usize,
    band: usize,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
)
where
    T: Copy + PartialEq,
    F: Fn(T) -> Option<i64>,
{
    let row_width = chunk.width as usize;
    let spp = samples_per_pixel.max(1);
    let b_idx = (band.saturating_sub(1)).min(spp - 1);
    let num_res = resolutions.len();

    for row_idx in 0..chunk.height as usize {
        let slice_row_start = (row_idx * (chunk_stride as usize)) * spp;

        for c in 0..row_width {
            let raw = slice[slice_row_start + c * spp + b_idx];
            if let Some(nd) = native_nodata {
                if raw == nd {
                    continue;
                }
            }

            let raw_cat = match to_class_fn(raw) {
                Some(cls) => cls,
                None => continue,
            };

            let cat = if let Some(rem) = remapper {
                match rem.remap(raw_cat) {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                raw_cat
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
                                .and_modify(|acc| acc.update_weighted(cat, sp.weight))
                                .or_insert_with(|| {
                                    let mut a = CategoricalAccumulator::default();
                                    a.update_weighted(cat, sp.weight);
                                    a
                                });
                        }
                    }
                }
            }
        }
    }
}

/// Process a categorical chunk across all resolutions into thread-local hash maps
pub fn process_categorical_chunk_payload_into(
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
    overlap_ctx: Option<(usize, &MosaicReader)>,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) -> bool {
    let is_multisample = samples_per_pixel > 1 && band > 1;
    let spp = samples_per_pixel.max(1) as usize;

    if !is_multisample {
        let is_all_nodata = match decoding_result {
            DecodingResult::U8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::F32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::F64(slice) => is_chunk_all_nodata(slice, nodata, |x| x),
        };

        if is_all_nodata {
            return false;
        }

        match decoding_result {
            DecodingResult::U8(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x <= i64::MAX as u64 { Some(x as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
        }
    } else {
        match decoding_result {
            DecodingResult::U8(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| if x <= i64::MAX as u64 { Some(x as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
        }
    }
    true
}
