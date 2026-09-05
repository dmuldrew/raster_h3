//! Multi-Resolution Direct Ground-Truth Horizon Streaming
//!
//! Provides single-pass streaming aggregation across multiple H3 resolution levels simultaneously
//! while preserving 100% true pixel-in-polygon containment at each resolution.
//!
//! Employs multi-core chunk-row parallelism via Rayon to process chunks across all CPU cores
//! lock-free while strictly bounding RAM to the active scanline horizon.

use std::collections::{BinaryHeap, HashMap, VecDeque};
use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use rayon::prelude::*;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::h3_scanline::H3ScanlineLookahead;
use crate::aggregator::horizon_streamer::{
    chunk_intersects_bbox, compute_cell_south_lat, is_chunk_all_nodata, HexEvictionEntry,
};
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::geotransform::GeoTransform;
use crate::raster::prefetch::PrefetchedChunkReader;
use crate::raster::RasterChunk;

const WGS84_A: f64 = 6378137.0;
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Configuration for multi-resolution aggregation
#[derive(Debug, Clone)]
pub struct MultiResolutionConfig {
    pub resolutions: Vec<u8>,
    pub band: usize,
    pub custom_nodata: Option<f64>,
    pub bbox: Option<[f64; 4]>,
    pub sampling: SamplingPattern,
    pub custom_crs: Option<String>,
}

impl MultiResolutionConfig {
    /// Create a new multi-resolution configuration
    pub fn new(resolutions: Vec<u8>) -> Self {
        Self {
            resolutions,
            band: 1,
            custom_nodata: None,
            bbox: None,
            sampling: SamplingPattern::center(),
            custom_crs: None,
        }
    }
}

impl Default for MultiResolutionConfig {
    fn default() -> Self {
        Self::new(vec![8])
    }
}

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
}

/// Calculate the minimum WGS84 latitude reached by the bottom edge of a chunk bounds
fn compute_chunk_bounds_bottom_lat(
    chunk_bounds: &RasterChunk,
    gt: &GeoTransform,
    crs_transformer: &CrsTransformer,
) -> f64 {
    let row_bottom = (chunk_bounds.row_offset + chunk_bounds.height) as usize;
    let mut min_lat = f64::INFINITY;

    let col_samples = [
        chunk_bounds.col_offset as usize,
        (chunk_bounds.col_offset + chunk_bounds.width / 2) as usize,
        (chunk_bounds.col_offset + chunk_bounds.width) as usize,
    ];

    for &c in &col_samples {
        let (x, y) = gt.pixel_to_coord(c as f64, row_bottom as f64);
        if let Ok((_lon, lat)) = crs_transformer.transform_point(x, y) {
            if lat < min_lat {
                min_lat = lat;
            }
        }
    }

    min_lat
}

/// Fast check if an entire row slice consists purely of NoData values
#[inline(always)]
fn is_slice_all_native_nodata<T, N>(slice: &[T], native_nodata: Option<N>) -> bool
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

/// Process a single typed chunk slice for continuous numeric aggregation across resolutions
fn process_continuous_slice_into_maps<T, F, N>(
    slice: &[T],
    chunk: &RasterChunk,
    to_f64: F,
    native_nodata: Option<N>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) where
    T: Copy + PartialEq,
    F: Fn(T) -> f64,
    N: Copy + PartialEq<T>,
{
    if slice.is_empty() {
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

        if sampling.is_single_point() {
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

            for res_idx in 0..num_res {
                let res = resolutions[res_idx];
                let active_map = &mut chunk_maps[res_idx];
                let row_cache = &mut row_caches[res_idx];
                row_cache.reset_row();
                let mut run_cell: u64 = 0;
                let mut run_acc = H3Accumulator::default();

                let mut lon_curr = lon_start;
                let mut x_curr = x_start;
                let mut c = 0;
                let mut known_next_cell: Option<u64> = None;

                while c < row_width {
                    let cell_opt = if let Some(known) = known_next_cell.take() {
                        Some(known)
                    } else {
                        let (lon, lat) = if is_wgs84 || is_web_mercator {
                            (lon_curr, lat_row)
                        } else if is_north_up {
                            match crs_transformer.transform_point(x_curr, y_row) {
                                Ok(coords) => coords,
                                Err(_) => {
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
                                    c += 1;
                                    continue;
                                }
                            }
                        };

                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                c += 1;
                                if is_wgs84 || is_web_mercator {
                                    lon_curr += d_lon_step;
                                } else if is_north_up {
                                    x_curr += dx_step;
                                }
                                continue;
                            }
                        }

                        row_cache.get_or_compute_cell(lat, lon, res)
                    };

                    if let Some(cell_u64) = cell_opt {
                        if cell_u64 != run_cell {
                            if run_cell != 0 && run_acc.count > 0.0 {
                                active_map
                                    .entry(run_cell)
                                    .and_modify(|acc| acc.merge(&run_acc))
                                    .or_insert_with(|| run_acc);
                            }
                            run_cell = cell_u64;
                            run_acc = H3Accumulator::default();
                            row_cache.on_cell_changed();
                        }

                        let (span_end, next_cell) = if is_wgs84 || is_web_mercator {
                            row_cache.find_span_end(c, row_width, lon_curr, lat_row, d_lon_step, res, run_cell)
                        } else if is_north_up {
                            row_cache.find_span_end_projected(
                                c,
                                row_width,
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

                        let span_slice = &slice[slice_row_start + c..slice_row_start + span_end];
                        let mut span_sum = 0.0f64;
                        let mut span_count = 0.0f64;
                        let mut span_min = f64::INFINITY;
                        let mut span_max = f64::NEG_INFINITY;

                        if native_nodata.is_none() && nodata.is_none() {
                            for &val_raw in span_slice {
                                let val = to_f64(val_raw);
                                if val.is_finite() {
                                    span_sum += val;
                                    span_min = span_min.min(val);
                                    span_max = span_max.max(val);
                                    span_count += 1.0;
                                }
                            }
                            if span_count > 0.0 {
                                let span_m2 = if span_min == span_max {
                                    0.0f64
                                } else {
                                    let span_mean = span_sum / span_count;
                                    let mut m2 = 0.0f64;
                                    for &val_raw in span_slice {
                                        let val = to_f64(val_raw);
                                        if val.is_finite() {
                                            let d = val - span_mean;
                                            m2 += d * d;
                                        }
                                    }
                                    m2
                                };
                                let span_acc = H3Accumulator {
                                    sum: span_sum,
                                    count: span_count,
                                    min: span_min,
                                    max: span_max,
                                    m2: span_m2,
                                };
                                run_acc.merge(&span_acc);
                            }
                        } else {
                            for &val_raw in span_slice {
                                if let Some(nd_nat) = native_nodata {
                                    if nd_nat == val_raw {
                                        continue;
                                    }
                                }

                                let val = to_f64(val_raw);
                                if !val.is_finite() {
                                    continue;
                                }
                                if native_nodata.is_none() {
                                    if let Some(nd) = nodata {
                                        if (val - nd).abs() < 1e-6 {
                                            continue;
                                        }
                                    }
                                }

                                span_sum += val;
                                span_min = span_min.min(val);
                                span_max = span_max.max(val);
                                span_count += 1.0;
                            }
                            if span_count > 0.0 {
                                let span_m2 = if span_min == span_max {
                                    0.0f64
                                } else {
                                    let span_mean = span_sum / span_count;
                                    let mut m2 = 0.0f64;
                                    if span_count as usize == span_slice.len() {
                                        for &val_raw in span_slice {
                                            let val = to_f64(val_raw);
                                            let d = val - span_mean;
                                            m2 += d * d;
                                        }
                                    } else {
                                        for &val_raw in span_slice {
                                            if let Some(nd_nat) = native_nodata {
                                                if nd_nat == val_raw {
                                                    continue;
                                                }
                                            }
                                            let val = to_f64(val_raw);
                                            if !val.is_finite() {
                                                continue;
                                            }
                                            if native_nodata.is_none() {
                                                if let Some(nd) = nodata {
                                                    if (val - nd).abs() < 1e-6 {
                                                        continue;
                                                    }
                                                }
                                            }

                                            let d = val - span_mean;
                                            m2 += d * d;
                                        }
                                    }
                                    m2
                                };
                                let span_acc = H3Accumulator {
                                    sum: span_sum,
                                    count: span_count,
                                    min: span_min,
                                    max: span_max,
                                    m2: span_m2,
                                };
                                run_acc.merge(&span_acc);
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
                        .or_insert_with(|| run_acc);
                }
            }
        } else {
            for c in 0..row_width {
                let val_raw = slice[slice_row_start + c];
                if let Some(nd_nat) = native_nodata {
                    if nd_nat == val_raw {
                        continue;
                    }
                }

                let val = to_f64(val_raw);
                if !val.is_finite() {
                    continue;
                }
                if native_nodata.is_none() {
                    if let Some(nd) = nodata {
                        if (val - nd).abs() < 1e-6 {
                            continue;
                        }
                    }
                }

                for sp in &sampling.points {
                    let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                    let py = (row_idx as f64) + sp.dy;
                    let (x, y) = gt.pixel_to_coord(px, py);
                    if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                continue;
                            }
                        }

                        if let Ok(ll) = LatLng::new(lat, lon) {
                            for res_idx in 0..num_res {
                                let res = resolutions[res_idx];
                                let active_map = &mut chunk_maps[res_idx];
                                let cell: u64 = ll.to_cell(res).into();
                                active_map
                                    .entry(cell)
                                    .and_modify(|acc| acc.update_weighted(val, sp.weight))
                                    .or_insert_with(|| {
                                        let mut a = H3Accumulator::default();
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
}

/// Process a continuous chunk across all resolutions into thread-local hash maps
fn process_continuous_chunk_payload_into(
    chunk_bounds: &RasterChunk,
    decoding_result: &DecodingResult,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) -> bool {
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
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::U16(slice) => {
            let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::U32(slice) => {
            let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::U64(slice) => {
            let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I8(slice) => {
            let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I16(slice) => {
            let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I32(slice) => {
            let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I64(slice) => {
            let nd = nodata.map(|v| v as i64);
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::F32(slice) => {
            let nd = nodata.map(|v| v as f32);
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x as f64, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::F64(slice) => {
            let nd = nodata;
            process_continuous_slice_into_maps(slice, chunk_bounds, |x| x, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
    }
    true
}

/// Single-pass streaming aggregator across multiple H3 resolutions (Continuous Data)
pub struct MultiScanHorizonStreamer {
    prefetcher: Option<PrefetchedChunkReader>,
    crs_transformer: CrsTransformer,
    resolutions: Vec<Resolution>,
    resolution_u8s: Vec<u8>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    gt: GeoTransform,
    chunk_stride: u32,
    active_maps: Vec<HashMap<u64, H3Accumulator, FxBuildHasher>>,
    eviction_queues: Vec<BinaryHeap<HexEvictionEntry>>,
    completed_buffer: VecDeque<MultiContinuousRecord>,
    is_finished: bool,
    current_lat_horizon: f64,
    pub profile_stats: [u64; 4],
}

impl MultiScanHorizonStreamer {
    /// Initialize a new MultiScanHorizonStreamer with background chunk prefetching
    pub fn new(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> Result<Self> {
        if config.resolutions.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "Resolutions list cannot be empty".to_string(),
            ));
        }

        let mut resolutions = Vec::with_capacity(config.resolutions.len());
        let mut resolution_u8s = Vec::with_capacity(config.resolutions.len());
        for &res_u8 in &config.resolutions {
            let res = Resolution::try_from(res_u8).map_err(|_| {
                RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", res_u8))
            })?;
            resolutions.push(res);
            resolution_u8s.push(res_u8);
        }

        let crs_transformer = CrsTransformer::from_crs_or_epsg(
            reader.metadata.epsg,
            config
                .custom_crs
                .as_deref()
                .or(reader.metadata.proj_string.as_deref()),
        )?;

        let nodata = config.custom_nodata.or(reader.metadata.nodata);
        let bbox = config.bbox;
        let gt = reader.metadata.geotransform;
        let chunk_stride = reader.chunk_layout.chunk_width;
        let total_chunks = reader.chunk_layout.total_chunks;

        let chunk_indices: Vec<u32> = (0..total_chunks)
            .filter(|&idx| {
                if let Some(ref b) = bbox {
                    let chunk_bounds = reader.chunk_layout.get_chunk_bounds(
                        idx,
                        reader.metadata.width,
                        reader.metadata.height,
                    );
                    chunk_intersects_bbox(&chunk_bounds, &gt, &crs_transformer, b)
                } else {
                    true
                }
            })
            .collect();

        let prefetcher = PrefetchedChunkReader::spawn(reader, chunk_indices, 256);
        let num_res = resolutions.len();

        let mut active_maps = Vec::with_capacity(num_res);
        let mut eviction_queues = Vec::with_capacity(num_res);
        for _ in 0..num_res {
            active_maps.push(HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default()));
            eviction_queues.push(BinaryHeap::with_capacity(1024));
        }

        Ok(Self {
            prefetcher: Some(prefetcher),
            crs_transformer,
            resolutions,
            resolution_u8s,
            nodata,
            bbox,
            sampling: config.sampling.clone(),
            gt,
            chunk_stride,
            active_maps,
            eviction_queues,
            completed_buffer: VecDeque::with_capacity(2048),
            is_finished: false,
            current_lat_horizon: f64::INFINITY,
            profile_stats: [0; 4],
        })
    }

    /// Return current southernmost latitude reached by scanline horizon
    pub fn current_lat_horizon(&self) -> f64 {
        self.current_lat_horizon
    }

    /// Target H3 resolutions
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Target H3 resolution integer levels
    pub fn resolution_u8s(&self) -> &[u8] {
        &self.resolution_u8s
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            let res_u8 = self.resolution_u8s[res_idx];
            while let Some(top) = self.eviction_queues[res_idx].peek() {
                if top.south_lat > lat_horizon {
                    let entry = self.eviction_queues[res_idx].pop().unwrap();
                    if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                        self.completed_buffer.push_back(MultiContinuousRecord {
                            resolution: res_u8,
                            h3_index: entry.cell_u64,
                            accumulator: acc,
                        });
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiContinuousRecord> {
        let batch_size = (rayon::current_num_threads() * 4).max(32);
        while self.completed_buffer.len() < max_rows && !self.is_finished {
            let t0 = std::time::Instant::now();
            let chunk_items = if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.next_chunk_batch(batch_size)
            } else {
                Vec::new()
            };
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;

            if chunk_items.is_empty() {
                self.is_finished = true;
                self.current_lat_horizon = f64::NEG_INFINITY;
                let num_res = self.resolutions.len();
                for res_idx in 0..num_res {
                    let res_u8 = self.resolution_u8s[res_idx];
                    while let Some(entry) = self.eviction_queues[res_idx].pop() {
                        if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                            self.completed_buffer.push_back(MultiContinuousRecord {
                                resolution: res_u8,
                                h3_index: entry.cell_u64,
                                accumulator: acc,
                            });
                        }
                    }
                }
                eprintln!(
                    "FETCH SUB-TIMINGS: prefetch_wait={:.3}s, rayon_compute={:.3}s, merge={:.3}s, evict={:.3}s",
                    self.profile_stats[0] as f64 / 1e9,
                    self.profile_stats[1] as f64 / 1e9,
                    self.profile_stats[2] as f64 / 1e9,
                    self.profile_stats[3] as f64 / 1e9,
                );
                break;
            }

            let resolutions = &self.resolutions;
            let crs_transformer = &self.crs_transformer;
            let gt = &self.gt;
            let sampling = &self.sampling;
            let bbox = self.bbox;
            let chunk_stride = self.chunk_stride;
            let nodata = self.nodata;

            // Parallel process all chunks using thread-local reusable HashMaps to eliminate allocation churn
            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(f64, Vec<Vec<(u64, H3Accumulator)>>, DecodingResult)> = chunk_items
                .into_par_iter()
                .map_init(
                    || {
                        let mut maps = Vec::with_capacity(resolutions.len());
                        for _ in 0..resolutions.len() {
                            maps.push(HashMap::with_capacity_and_hasher(128, FxBuildHasher::default()));
                        }
                        maps
                    },
                    |local_maps, item| {
                        match item {
                            Ok((_chunk_idx, chunk_bounds, decoding_result)) => {
                                let bottom_lat = compute_chunk_bounds_bottom_lat(&chunk_bounds, gt, crs_transformer);
                                for m in local_maps.iter_mut() {
                                    m.clear();
                                }
                                let has_data = process_continuous_chunk_payload_into(
                                    &chunk_bounds,
                                    &decoding_result,
                                    resolutions,
                                    crs_transformer,
                                    gt,
                                    sampling,
                                    bbox,
                                    chunk_stride,
                                    nodata,
                                    local_maps,
                                );
                                let mut chunk_entries = Vec::with_capacity(if has_data { local_maps.len() } else { 0 });
                                if has_data {
                                    for m in local_maps.iter_mut() {
                                        let entries: Vec<(u64, H3Accumulator)> = m.drain().collect();
                                        chunk_entries.push(entries);
                                    }
                                }
                                Some((bottom_lat, chunk_entries, decoding_result))
                            }
                            Err(_) => None,
                        }
                    },
                )
                .filter_map(|x| x)
                .collect();
            self.profile_stats[1] += t1.elapsed().as_nanos() as u64;

            let mut min_batch_lat = f64::INFINITY;
            let mut recycled_buffers = Vec::with_capacity(parallel_results.len());

            let t2 = std::time::Instant::now();
            for (bottom_lat, chunk_entries, decoding_result) in parallel_results {
                if bottom_lat < min_batch_lat {
                    min_batch_lat = bottom_lat;
                }

                if !chunk_entries.is_empty() {
                    for (res_idx, entries) in chunk_entries.into_iter().enumerate() {
                        let active_map = &mut self.active_maps[res_idx];
                        let eviction_queue = &mut self.eviction_queues[res_idx];

                        for (cell_u64, acc) in entries {
                            active_map
                                .entry(cell_u64)
                                .and_modify(|existing| existing.merge(&acc))
                                .or_insert_with(|| {
                                    let south_lat = compute_cell_south_lat(cell_u64);
                                    eviction_queue.push(HexEvictionEntry {
                                        south_lat,
                                        cell_u64,
                                    });
                                    acc
                                });
                        }
                    }
                }

                recycled_buffers.push(decoding_result);
            }

            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.recycle_batch(recycled_buffers);
            }
            self.profile_stats[2] += t2.elapsed().as_nanos() as u64;

            if min_batch_lat.is_finite() {
                let t3 = std::time::Instant::now();
                self.current_lat_horizon = min_batch_lat;
                self.evict_completed(min_batch_lat);
                self.profile_stats[3] += t3.elapsed().as_nanos() as u64;
            }
        }

        let num_to_take = max_rows.min(self.completed_buffer.len());
        let mut batch = Vec::with_capacity(num_to_take);
        for _ in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                batch.push(record);
            }
        }
        batch
    }

    /// Return total active in-flight cells across all resolutions
    pub fn active_cell_count(&self) -> usize {
        self.active_maps.iter().map(|m| m.len()).sum()
    }
}

/// Categorical record yielded by the multi-resolution categorical streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiCategoricalRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: CategoricalAccumulator,
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
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) where
    T: Copy + PartialEq,
    F: Fn(T) -> Option<i64>,
    N: Copy + PartialEq<T>,
{
    if slice.is_empty() {
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

        if sampling.is_single_point() {
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

            for res_idx in 0..num_res {
                let res = resolutions[res_idx];
                let active_map = &mut chunk_maps[res_idx];
                let row_cache = &mut row_caches[res_idx];
                row_cache.reset_row();
                let mut run_cell: u64 = 0;
                let mut run_acc = CategoricalAccumulator::default();

                let mut lon_curr = lon_start;
                let mut x_curr = x_start;
                let mut c = 0;
                let mut known_next_cell: Option<u64> = None;

                while c < row_width {
                    let cell_opt = if let Some(known) = known_next_cell.take() {
                        Some(known)
                    } else {
                        let (lon, lat) = if is_wgs84 || is_web_mercator {
                            (lon_curr, lat_row)
                        } else if is_north_up {
                            match crs_transformer.transform_point(x_curr, y_row) {
                                Ok(coords) => coords,
                                Err(_) => {
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
                                    c += 1;
                                    continue;
                                }
                            }
                        };

                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                c += 1;
                                if is_wgs84 || is_web_mercator {
                                    lon_curr += d_lon_step;
                                } else if is_north_up {
                                    x_curr += dx_step;
                                }
                                continue;
                            }
                        }

                        row_cache.get_or_compute_cell(lat, lon, res)
                    };

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
                        }

                        let (span_end, next_cell) = if is_wgs84 || is_web_mercator {
                            row_cache.find_span_end(c, row_width, lon_curr, lat_row, d_lon_step, res, run_cell)
                        } else if is_north_up {
                            row_cache.find_span_end_projected(
                                c,
                                row_width,
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

                        let mut curr_cat: Option<i64> = None;
                        let mut curr_cat_count: f64 = 0.0;

                        for i in c..span_end {
                            let val_raw = slice[slice_row_start + i];

                            if let Some(nd_nat) = native_nodata {
                                if nd_nat == val_raw {
                                    continue;
                                }
                            }

                            if let Some(cat) = to_i64(val_raw) {
                                if native_nodata.is_none() {
                                    if let Some(nd) = nodata {
                                        if (cat as f64 - nd).abs() < 1e-6 {
                                            continue;
                                        }
                                    }
                                }
                                if Some(cat) == curr_cat {
                                    curr_cat_count += 1.0;
                                } else {
                                    if let Some(prev) = curr_cat {
                                        run_acc.update_weighted(prev, curr_cat_count);
                                    }
                                    curr_cat = Some(cat);
                                    curr_cat_count = 1.0;
                                }
                            }
                        }

                        if let Some(prev) = curr_cat {
                            run_acc.update_weighted(prev, curr_cat_count);
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
            for c in 0..row_width {
                let val_raw = slice[slice_row_start + c];
                if let Some(nd_nat) = native_nodata {
                    if nd_nat == val_raw {
                        continue;
                    }
                }

                if let Some(cat) = to_i64(val_raw) {
                    if native_nodata.is_none() {
                        if let Some(nd) = nodata {
                            if (cat as f64 - nd).abs() < 1e-6 {
                                continue;
                            }
                        }
                    }

                    for sp in &sampling.points {
                        let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                        let py = (row_idx as f64) + sp.dy;
                        let (x, y) = gt.pixel_to_coord(px, py);
                        if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                            if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                    continue;
                                }
                            }

                            if let Ok(ll) = LatLng::new(lat, lon) {
                                for res_idx in 0..num_res {
                                    let res = resolutions[res_idx];
                                    let active_map = &mut chunk_maps[res_idx];
                                    let cell: u64 = ll.to_cell(res).into();
                                    active_map
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
    }
}

/// Process a categorical chunk across all resolutions into thread-local hash maps
fn process_categorical_chunk_payload_into(
    chunk_bounds: &RasterChunk,
    decoding_result: &DecodingResult,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) -> bool {
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
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::U16(slice) => {
            let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::U32(slice) => {
            let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::U64(slice) => {
            let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x <= i64::MAX as u64 { Some(x as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I8(slice) => {
            let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I16(slice) => {
            let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I32(slice) => {
            let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::I64(slice) => {
            let nd = nodata.map(|v| v as i64);
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::F32(slice) => {
            let nd = nodata.map(|v| v as f32);
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
        DecodingResult::F64(slice) => {
            let nd = nodata;
            process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, chunk_maps);
        }
    }
    true
}

/// Single-pass streaming aggregator across multiple H3 resolutions (Categorical Data)
pub struct MultiCategoricalHorizonStreamer {
    prefetcher: Option<PrefetchedChunkReader>,
    crs_transformer: CrsTransformer,
    resolutions: Vec<Resolution>,
    resolution_u8s: Vec<u8>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    gt: GeoTransform,
    chunk_stride: u32,
    active_maps: Vec<HashMap<u64, CategoricalAccumulator, FxBuildHasher>>,
    eviction_queues: Vec<BinaryHeap<HexEvictionEntry>>,
    completed_buffer: VecDeque<MultiCategoricalRecord>,
    is_finished: bool,
    current_lat_horizon: f64,
    pub profile_stats: [u64; 4],
}

impl MultiCategoricalHorizonStreamer {
    /// Initialize a new MultiCategoricalHorizonStreamer with background chunk prefetching
    pub fn new(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> Result<Self> {
        if config.resolutions.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "Resolutions list cannot be empty".to_string(),
            ));
        }

        let mut resolutions = Vec::with_capacity(config.resolutions.len());
        let mut resolution_u8s = Vec::with_capacity(config.resolutions.len());
        for &res_u8 in &config.resolutions {
            let res = Resolution::try_from(res_u8).map_err(|_| {
                RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", res_u8))
            })?;
            resolutions.push(res);
            resolution_u8s.push(res_u8);
        }

        let crs_transformer = CrsTransformer::from_crs_or_epsg(
            reader.metadata.epsg,
            config
                .custom_crs
                .as_deref()
                .or(reader.metadata.proj_string.as_deref()),
        )?;

        let nodata = config.custom_nodata.or(reader.metadata.nodata);
        let bbox = config.bbox;
        let gt = reader.metadata.geotransform;
        let chunk_stride = reader.chunk_layout.chunk_width;
        let total_chunks = reader.chunk_layout.total_chunks;

        let chunk_indices: Vec<u32> = (0..total_chunks)
            .filter(|&idx| {
                if let Some(ref b) = bbox {
                    let chunk_bounds = reader.chunk_layout.get_chunk_bounds(
                        idx,
                        reader.metadata.width,
                        reader.metadata.height,
                    );
                    chunk_intersects_bbox(&chunk_bounds, &gt, &crs_transformer, b)
                } else {
                    true
                }
            })
            .collect();

        let prefetcher = PrefetchedChunkReader::spawn(reader, chunk_indices, 256);
        let num_res = resolutions.len();

        let mut active_maps = Vec::with_capacity(num_res);
        let mut eviction_queues = Vec::with_capacity(num_res);
        for _ in 0..num_res {
            active_maps.push(HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default()));
            eviction_queues.push(BinaryHeap::with_capacity(1024));
        }

        Ok(Self {
            prefetcher: Some(prefetcher),
            crs_transformer,
            resolutions,
            resolution_u8s,
            nodata,
            bbox,
            sampling: config.sampling.clone(),
            gt,
            chunk_stride,
            active_maps,
            eviction_queues,
            completed_buffer: VecDeque::with_capacity(2048),
            is_finished: false,
            current_lat_horizon: f64::INFINITY,
            profile_stats: [0; 4],
        })
    }

    /// Return current southernmost latitude reached by scanline horizon
    pub fn current_lat_horizon(&self) -> f64 {
        self.current_lat_horizon
    }

    /// Target H3 resolutions
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Target H3 resolution integer levels
    pub fn resolution_u8s(&self) -> &[u8] {
        &self.resolution_u8s
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            let res_u8 = self.resolution_u8s[res_idx];
            while let Some(top) = self.eviction_queues[res_idx].peek() {
                if top.south_lat > lat_horizon {
                    let entry = self.eviction_queues[res_idx].pop().unwrap();
                    if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                        self.completed_buffer.push_back(MultiCategoricalRecord {
                            resolution: res_u8,
                            h3_index: entry.cell_u64,
                            accumulator: acc,
                        });
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiCategoricalRecord> {
        let batch_size = (rayon::current_num_threads() * 4).max(32);
        while self.completed_buffer.len() < max_rows && !self.is_finished {
            let t0 = std::time::Instant::now();
            let chunk_items = if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.next_chunk_batch(batch_size)
            } else {
                Vec::new()
            };
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;

            if chunk_items.is_empty() {
                self.is_finished = true;
                self.current_lat_horizon = f64::NEG_INFINITY;
                let num_res = self.resolutions.len();
                for res_idx in 0..num_res {
                    let res_u8 = self.resolution_u8s[res_idx];
                    while let Some(entry) = self.eviction_queues[res_idx].pop() {
                        if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                            self.completed_buffer.push_back(MultiCategoricalRecord {
                                resolution: res_u8,
                                h3_index: entry.cell_u64,
                                accumulator: acc,
                            });
                        }
                    }
                }
                eprintln!(
                    "FETCH SUB-TIMINGS: prefetch_wait={:.3}s, rayon_compute={:.3}s, merge={:.3}s, evict={:.3}s",
                    self.profile_stats[0] as f64 / 1e9,
                    self.profile_stats[1] as f64 / 1e9,
                    self.profile_stats[2] as f64 / 1e9,
                    self.profile_stats[3] as f64 / 1e9,
                );
                break;
            }

            let resolutions = &self.resolutions;
            let crs_transformer = &self.crs_transformer;
            let gt = &self.gt;
            let sampling = &self.sampling;
            let bbox = self.bbox;
            let chunk_stride = self.chunk_stride;
            let nodata = self.nodata;

            // Parallel process all chunks using thread-local reusable HashMaps to eliminate allocation churn
            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(f64, Vec<Vec<(u64, CategoricalAccumulator)>>, DecodingResult)> = chunk_items
                .into_par_iter()
                .map_init(
                    || {
                        let mut maps = Vec::with_capacity(resolutions.len());
                        for _ in 0..resolutions.len() {
                            maps.push(HashMap::with_capacity_and_hasher(128, FxBuildHasher::default()));
                        }
                        maps
                    },
                    |local_maps, item| {
                        match item {
                            Ok((_chunk_idx, chunk_bounds, decoding_result)) => {
                                let bottom_lat = compute_chunk_bounds_bottom_lat(&chunk_bounds, gt, crs_transformer);
                                for m in local_maps.iter_mut() {
                                    m.clear();
                                }
                                let has_data = process_categorical_chunk_payload_into(
                                    &chunk_bounds,
                                    &decoding_result,
                                    resolutions,
                                    crs_transformer,
                                    gt,
                                    sampling,
                                    bbox,
                                    chunk_stride,
                                    nodata,
                                    local_maps,
                                );
                                let mut chunk_entries = Vec::with_capacity(if has_data { local_maps.len() } else { 0 });
                                if has_data {
                                    for m in local_maps.iter_mut() {
                                        let entries: Vec<(u64, CategoricalAccumulator)> = m.drain().collect();
                                        chunk_entries.push(entries);
                                    }
                                }
                                Some((bottom_lat, chunk_entries, decoding_result))
                            }
                            Err(_) => None,
                        }
                    },
                )
                .filter_map(|x| x)
                .collect();
            self.profile_stats[1] += t1.elapsed().as_nanos() as u64;

            let mut min_batch_lat = f64::INFINITY;
            let mut recycled_buffers = Vec::with_capacity(parallel_results.len());

            let t2 = std::time::Instant::now();
            for (bottom_lat, chunk_entries, decoding_result) in parallel_results {
                if bottom_lat < min_batch_lat {
                    min_batch_lat = bottom_lat;
                }

                if !chunk_entries.is_empty() {
                    for (res_idx, entries) in chunk_entries.into_iter().enumerate() {
                        let active_map = &mut self.active_maps[res_idx];
                        let eviction_queue = &mut self.eviction_queues[res_idx];

                        for (cell_u64, acc) in entries {
                            active_map
                                .entry(cell_u64)
                                .and_modify(|existing| existing.merge(&acc))
                                .or_insert_with(|| {
                                    let south_lat = compute_cell_south_lat(cell_u64);
                                    eviction_queue.push(HexEvictionEntry {
                                        south_lat,
                                        cell_u64,
                                    });
                                    acc
                                });
                        }
                    }
                }

                recycled_buffers.push(decoding_result);
            }

            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.recycle_batch(recycled_buffers);
            }
            self.profile_stats[2] += t2.elapsed().as_nanos() as u64;

            if min_batch_lat.is_finite() {
                let t3 = std::time::Instant::now();
                self.current_lat_horizon = min_batch_lat;
                self.evict_completed(min_batch_lat);
                self.profile_stats[3] += t3.elapsed().as_nanos() as u64;
            }
        }

        let num_to_take = max_rows.min(self.completed_buffer.len());
        let mut batch = Vec::with_capacity(num_to_take);
        for _ in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                batch.push(record);
            }
        }
        batch
    }

    /// Return total active in-flight cells across all resolutions
    pub fn active_cell_count(&self) -> usize {
        self.active_maps.iter().map(|m| m.len()).sum()
    }
}
