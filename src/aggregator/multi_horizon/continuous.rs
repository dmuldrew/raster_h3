use std::collections::HashMap;
use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::h3_scanline::H3ScanlineLookahead;
use crate::aggregator::horizon_streamer::is_chunk_all_nodata;
use crate::aggregator::sampling::SamplingPattern;
use crate::aggregator::simd::SimdSpanAccumulate;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::config::SpectralFormula;

pub const WGS84_A: f64 = 6378137.0;
pub const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
}

/// Fast check if an entire row slice consists purely of NoData values
#[inline(always)]
pub fn is_slice_all_native_nodata<T, N>(slice: &[T], native_nodata: Option<N>) -> bool
where
    T: Copy + PartialEq,
    N: Copy + PartialEq<T>,
{
    if let Some(nd_nat) = native_nodata {
        if slice.is_empty() {
            return true;
        }
        let len = slice.len();
        if nd_nat != slice[0] || nd_nat != slice[len / 2] || nd_nat != slice[len - 1] {
            return false;
        }
        slice.iter().all(|&val| nd_nat == val)
    } else {
        false
    }
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
    let stride = if chunk_stride > 0 && slice.len() >= chunk_stride as usize {
        chunk_stride as usize
    } else {
        (chunk.width as usize).max(1)
    };
    let actual_rows = (slice.len() / stride).min(chunk.height as usize);
    let num_res = resolutions.len();

    for r in 0..actual_rows {
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        for c in 0..row_width {
            let val = slice[slice_row_start + c];
            if !val.is_valid(native_nodata) {
                continue;
            }
            let float_val = val.to_f64_val();

            if sampling.is_single_point() {
                let (x, y) = gt.pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                let (lon, lat) = match crs_transformer.transform_point(x, y) {
                    Ok(coords) => coords,
                    Err(_) => continue,
                };

                if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                    if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                        continue;
                    }
                }

                if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                    continue;
                }

                if let Ok(ll) = LatLng::new(lat, lon) {
                    for res_idx in 0..num_res {
                        let res = resolutions[res_idx];
                        let cell_u64: u64 = ll.to_cell(res).into();
                        chunk_maps[res_idx]
                            .entry(cell_u64)
                            .and_modify(|acc| acc.update(float_val))
                            .or_insert_with(|| {
                                let mut acc = if track_quantiles {
                                    H3Accumulator::with_quantiles()
                                } else {
                                    H3Accumulator::default()
                                };
                                acc.update(float_val);
                                acc
                            });
                    }
                }
            } else {
                for sp in &sampling.points {
                    let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                    let py = (chunk.row_offset as f64) + (r as f64) + sp.dy;
                    let (x, y) = gt.pixel_to_coord(px, py);
                    if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                continue;
                            }
                        }

                        if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                            continue;
                        }

                        if let Ok(ll) = LatLng::new(lat, lon) {
                            for res_idx in 0..num_res {
                                let res = resolutions[res_idx];
                                let cell_u64: u64 = ll.to_cell(res).into();
                                chunk_maps[res_idx]
                                    .entry(cell_u64)
                                    .and_modify(|acc| acc.update_weighted(float_val, sp.weight))
                                    .or_insert_with(|| {
                                        let mut acc = if track_quantiles {
                                            H3Accumulator::with_quantiles()
                                        } else {
                                            H3Accumulator::default()
                                        };
                                        acc.update_weighted(float_val, sp.weight);
                                        acc
                                    });
                            }
                        }
                    }
                }
            }
        }
    }
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

    let is_wgs84 = matches!(crs_transformer, CrsTransformer::Wgs84Identity);
    let is_web_mercator = matches!(crs_transformer, CrsTransformer::WebMercatorFast);
    let d_lon_step = if is_wgs84 {
        gt.a
    } else if is_web_mercator {
        (gt.a / WGS84_A) * RAD_TO_DEG
    } else {
        0.0
    };

    let stride = if chunk_stride > 0 && slice.len() >= chunk_stride as usize {
        chunk_stride as usize
    } else {
        (chunk.width as usize).max(1)
    };
    let actual_rows = (slice.len() / stride).min(chunk.height as usize);
    let num_res = resolutions.len();
    let is_north_up = gt.b == 0.0 && gt.d == 0.0;
    let dx_step = gt.a;

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
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        let slice_row = &slice[slice_row_start..slice_row_start + row_width];
        if is_slice_all_native_nodata(slice_row, native_nodata) {
            continue;
        }

        let (x_start, y_row) = gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);
            let (lon_start, lat_row) = if is_wgs84 {
                (x_start, y_row)
            } else if is_web_mercator {
                let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2)
                    * RAD_TO_DEG;
                let lon = (x_start / WGS84_A) * RAD_TO_DEG;
                (lon, lat)
            } else {
                match crs_transformer.transform_point(x_start, y_row) {
                    Ok(coords) => coords,
                    Err(_) => (x_start, y_row),
                }
            };

            let (d_lon_dx, d_lat_dx, d_lon_dy, d_lat_dy) = if !is_single_point {
                if is_wgs84 {
                    (gt.a, gt.d, gt.b, gt.e)
                } else if is_web_mercator {
                    let lon_dx = ((x_start + gt.a) / WGS84_A) * RAD_TO_DEG;
                    let lat_dy = (2.0 * ((y_row + gt.e) / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
                    (lon_dx - lon_start, 0.0, 0.0, lat_dy - lat_row)
                } else {
                    let (lon_x, lat_x) = match crs_transformer.transform_point(x_start + dx_step, y_row) {
                        Ok(coords) => coords,
                        Err(_) => (lon_start, lat_row),
                    };
                    let (lon_y, lat_y) = match crs_transformer.transform_point(x_start, y_row + gt.e) {
                        Ok(coords) => coords,
                        Err(_) => (lon_start, lat_row),
                    };
                    (lon_x - lon_start, lat_x - lat_row, lon_y - lon_start, lat_y - lat_row)
                }
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };

            if is_wgs84 || is_web_mercator {
                if let Some([_, b_min_lat, _, b_max_lat]) = bbox {
                    if lat_row < b_min_lat || lat_row > b_max_lat {
                        continue;
                    }
                }
            }

            let (row_c_start, row_c_end) = if (is_wgs84 || is_web_mercator) && bbox.is_some() {
                let [b_min_lon, _, b_max_lon, _] = bbox.unwrap();
                if d_lon_step > 0.0 {
                    let c_s = if lon_start < b_min_lon {
                        ((b_min_lon - lon_start) / d_lon_step).ceil().max(0.0) as usize
                    } else {
                        0
                    };
                    let c_e = if lon_start < b_max_lon {
                        (((b_max_lon - lon_start) / d_lon_step).floor().max(0.0) as usize + 1).min(row_width)
                    } else {
                        0
                    };
                    (c_s, c_e)
                } else if d_lon_step < 0.0 {
                    let c_s = if lon_start > b_max_lon {
                        ((b_max_lon - lon_start) / d_lon_step).ceil().max(0.0) as usize
                    } else {
                        0
                    };
                    let c_e = if lon_start > b_min_lon {
                        (((b_min_lon - lon_start) / d_lon_step).floor().max(0.0) as usize + 1).min(row_width)
                    } else {
                        0
                    };
                    (c_s, c_e)
                } else {
                    (0, row_width)
                }
            } else {
                (0, row_width)
            };

            if row_c_start >= row_c_end || row_c_start >= row_width {
                continue;
            }

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

                    let mut lon_curr = lon_start + (row_c_start as f64) * d_lon_step;
                    let mut x_curr = x_start + (row_c_start as f64) * dx_step;
                    let mut c = row_c_start;
                    let mut known_next_cell: Option<u64> = None;

                    while c < row_c_end {
                        let (lon, lat) = if is_wgs84 || is_web_mercator {
                            (lon_curr, lat_row)
                        } else if is_north_up {
                            match crs_transformer.transform_point(x_curr, y_row) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    known_next_cell = None;
                                    c += 1;
                                    x_curr += dx_step;
                                    continue;
                                }
                            }
                        } else {
                            let (x, y) = gt
                                .pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                            match crs_transformer.transform_point(x, y) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    known_next_cell = None;
                                    c += 1;
                                    continue;
                                }
                            }
                        };

                        if !is_wgs84 && !is_web_mercator {
                            if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                    known_next_cell = None;
                                    c += 1;
                                    if is_north_up {
                                        x_curr += dx_step;
                                    }
                                    continue;
                                }
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
                            }

                            let (span_end, next_cell) = if is_wgs84 || is_web_mercator {
                                row_cache.find_span_end(c, row_c_end, lon_curr, lat_row, d_lon_step, res, run_cell)
                            } else if is_north_up {
                                row_cache.find_span_end_projected(
                                    c,
                                    row_c_end,
                                    x_start,
                                    y_row,
                                    dx_step,
                                    |x, y| match crs_transformer.transform_point(x, y) {
                                        Ok((p_lon, p_lat)) => {
                                            if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                                if p_lon < b_min_lon || p_lon > b_max_lon || p_lat < b_min_lat || p_lat > b_max_lat {
                                                    return None;
                                                }
                                            }
                                            LatLng::new(p_lat, p_lon).ok().map(|ll| ll.to_cell(res).into())
                                        }
                                        Err(_) => None,
                                    },
                                    run_cell,
                                )
                            } else {
                                (c + 1, None)
                            };

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
                                let (core_start, core_end) = row_cache.find_core_span(
                                    c,
                                    span_end,
                                    dx_bounds,
                                    dy_bounds,
                                    |px, py| {
                                        let (lon, lat) = if is_wgs84 {
                                            let test_lon = lon_start + (px - 0.5) * d_lon_step;
                                            let test_lat = lat_row + (py - 0.5) * gt.e;
                                            (test_lon, test_lat)
                                        } else if is_web_mercator {
                                            let test_lon = lon_start + (px - 0.5) * d_lon_step;
                                            let test_lat = lat_row + (py - 0.5) * d_lat_dy;
                                            (test_lon, test_lat)
                                        } else {
                                            let d_col = px - 0.5;
                                            let d_row = py - 0.5;
                                            let lon = lon_start + d_col * d_lon_dx + d_row * d_lon_dy;
                                            let lat = lat_row + d_col * d_lat_dx + d_row * d_lat_dy;
                                            (lon, lat)
                                        };
                                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                                return false;
                                            }
                                        }
                                        LatLng::new(lat, lon).ok().map(|ll| ll.to_cell(res).into()) == Some(run_cell)
                                    },
                                );

                                let mut evaluate_boundary = |k: usize| {
                                    let val_raw = slice[slice_row_start + k];
                                    if !val_raw.is_valid(native_nodata) {
                                        return;
                                    }
                                    let val = val_raw.to_f64_val();

                                    let (k_lon, k_lat) = if is_wgs84 || is_web_mercator {
                                        (lon_start + (k as f64) * d_lon_step, lat_row)
                                    } else if is_north_up {
                                        let x_k = x_start + (k as f64) * dx_step;
                                        match crs_transformer.transform_point(x_k, y_row) {
                                            Ok(coords) => coords,
                                            Err(_) => (lon_start + (k as f64) * d_lon_dx, lat_row + (k as f64) * d_lat_dx),
                                        }
                                    } else {
                                        let (x_k, y_k) = gt.pixel_center_to_coord((chunk.col_offset as usize) + k, row_idx);
                                        match crs_transformer.transform_point(x_k, y_k) {
                                            Ok(coords) => coords,
                                            Err(_) => (lon_start + (k as f64) * d_lon_dx, lat_row + (k as f64) * d_lat_dx),
                                        }
                                    };

                                    for sp in &sampling.points {
                                        let d_x = sp.dx - 0.5;
                                        let d_y = sp.dy - 0.5;
                                        let lon = k_lon + d_x * d_lon_dx + d_y * d_lon_dy;
                                        let lat = k_lat + d_x * d_lat_dx + d_y * d_lat_dy;

                                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                                continue;
                                            }
                                        }
                                        if let Ok(ll) = LatLng::new(lat, lon) {
                                            let cell: u64 = ll.to_cell(res).into();
                                            active_map
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
                    let (lon, lat) = if is_wgs84 || is_web_mercator {
                        (lon_curr, lat_row)
                    } else if is_north_up {
                        match crs_transformer.transform_point(x_curr, y_row) {
                            Ok(coords) => coords,
                            Err(_) => {
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
                                x_curr += dx_step;
                                continue;
                            }
                        }
                    } else {
                        let (x, y) = gt
                            .pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                        match crs_transformer.transform_point(x, y) {
                            Ok(coords) => coords,
                            Err(_) => {
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
                                continue;
                            }
                        }
                    };

                    if !is_wgs84 && !is_web_mercator {
                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
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
                                }

                                let (span_end, next_cell) = if is_wgs84 || is_web_mercator {
                                    row_caches[i].find_span_end(
                                        c,
                                        row_c_end,
                                        lon_curr,
                                        lat_row,
                                        d_lon_step,
                                        res,
                                        run_cells[i],
                                    )
                                } else if is_north_up {
                                    row_caches[i].find_span_end_projected(
                                        c,
                                        row_c_end,
                                        x_start,
                                        y_row,
                                        dx_step,
                                        |x, y| match crs_transformer.transform_point(x, y) {
                                            Ok((p_lon, p_lat)) => {
                                                if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                                    if p_lon < b_min_lon
                                                        || p_lon > b_max_lon
                                                        || p_lat < b_min_lat
                                                        || p_lat > b_max_lat
                                                    {
                                                        return None;
                                                    }
                                                }
                                                LatLng::new(p_lat, p_lon).ok().map(|ll| ll.to_cell(res).into())
                                            }
                                            Err(_) => None,
                                        },
                                        run_cells[i],
                                    )
                                } else {
                                    (c + 1, None)
                                };

                                span_ends[i] = span_end;
                                known_next_cells[i] = next_cell;

                                if !is_single_point {
                                    let (c_start, c_end) = row_caches[i].find_core_span(
                                        c,
                                        span_end,
                                        dx_bounds,
                                        dy_bounds,
                                        |px, py| {
                                            let (test_lon, test_lat) = if is_wgs84 {
                                                (lon_start + (px - 0.5) * d_lon_step, lat_row + (py - 0.5) * gt.e)
                                            } else if is_web_mercator {
                                                (lon_start + (px - 0.5) * d_lon_step, lat_row + (py - 0.5) * d_lat_dy)
                                            } else {
                                                let d_col = px - 0.5;
                                                let d_row = py - 0.5;
                                                (
                                                    lon_start + d_col * d_lon_dx + d_row * d_lon_dy,
                                                    lat_row + d_col * d_lat_dx + d_row * d_lat_dy,
                                                )
                                            };
                                            if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                                if test_lon < b_min_lon
                                                    || test_lon > b_max_lon
                                                    || test_lat < b_min_lat
                                                    || test_lat > b_max_lat
                                                {
                                                    return false;
                                                }
                                            }
                                            LatLng::new(test_lat, test_lon).ok().map(|ll| ll.to_cell(res).into())
                                                == Some(run_cells[i])
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

                        let evaluate_boundary_multi = |k: usize,
                                                       run_accs: &mut [H3Accumulator],
                                                       chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>]| {
                            let val_raw = slice[slice_row_start + k];
                            if !val_raw.is_valid(native_nodata) {
                                return;
                            }
                            let val = val_raw.to_f64_val();

                            let (k_lon, k_lat) = if is_wgs84 || is_web_mercator {
                                (lon_start + (k as f64) * d_lon_step, lat_row)
                            } else if is_north_up {
                                let x_k = x_start + (k as f64) * dx_step;
                                match crs_transformer.transform_point(x_k, y_row) {
                                    Ok(coords) => coords,
                                    Err(_) => (
                                        lon_start + (k as f64) * d_lon_dx,
                                        lat_row + (k as f64) * d_lat_dx,
                                    ),
                                }
                            } else {
                                let (x_k, y_k) =
                                    gt.pixel_center_to_coord((chunk.col_offset as usize) + k, row_idx);
                                match crs_transformer.transform_point(x_k, y_k) {
                                    Ok(coords) => coords,
                                    Err(_) => (
                                        lon_start + (k as f64) * d_lon_dx,
                                        lat_row + (k as f64) * d_lat_dx,
                                    ),
                                }
                            };

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
                                } else {
                                    let res = resolutions[i];
                                    for sp in &sampling.points {
                                        let d_x = sp.dx - 0.5;
                                        let d_y = sp.dy - 0.5;
                                        let lon = k_lon + d_x * d_lon_dx + d_y * d_lon_dy;
                                        let lat = k_lat + d_x * d_lat_dx + d_y * d_lat_dy;

                                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                            if lon < b_min_lon
                                                || lon > b_max_lon
                                                || lat < b_min_lat
                                                || lat > b_max_lat
                                            {
                                                continue;
                                            }
                                        }
                                        if let Ok(ll) = LatLng::new(lat, lon) {
                                            let cell: u64 = ll.to_cell(res).into();
                                            chunk_maps[i]
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
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
        }
    } else {
        match decoding_result {
            DecodingResult::U8(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
        }
    }
    true
}
