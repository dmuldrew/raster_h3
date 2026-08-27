//! Multi-Resolution Direct Ground-Truth Horizon Streaming
//!
//! Provides single-pass streaming aggregation across multiple H3 resolution levels simultaneously
//! while preserving 100% true pixel-in-polygon containment at each resolution.

use std::collections::{BinaryHeap, HashMap, VecDeque};
use fxhash::FxBuildHasher;
use h3o::Resolution;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::coherence::SpatialCoherenceCache;
use crate::aggregator::horizon_streamer::{
    chunk_intersects_bbox, compute_cell_south_lat, HexEvictionEntry,
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

impl Default for MultiResolutionConfig {
    fn default() -> Self {
        Self {
            resolutions: vec![8],
            band: 1,
            custom_nodata: None,
            bbox: None,
            sampling: SamplingPattern::center(),
            custom_crs: None,
        }
    }
}

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
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

        let prefetcher = PrefetchedChunkReader::spawn(reader, chunk_indices, 8);
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
        })
    }

    /// Target H3 resolutions
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Target H3 resolution integer levels
    pub fn resolution_u8s(&self) -> &[u8] {
        &self.resolution_u8s
    }

    /// Calculate the minimum WGS84 latitude reached by the bottom edge of a chunk row
    fn compute_chunk_bottom_lat(&self, chunk_bounds: &RasterChunk) -> f64 {
        let row_bottom = (chunk_bounds.row_offset + chunk_bounds.height) as usize;
        let mut min_lat = f64::INFINITY;

        let col_samples = [
            chunk_bounds.col_offset as usize,
            (chunk_bounds.col_offset + chunk_bounds.width / 2) as usize,
            (chunk_bounds.col_offset + chunk_bounds.width) as usize,
        ];

        for &c in &col_samples {
            let (x, y) = self.gt.pixel_to_coord(c as f64, row_bottom as f64);
            if let Ok((_lon, lat)) = self.crs_transformer.transform_point(x, y) {
                if lat < min_lat {
                    min_lat = lat;
                }
            }
        }

        min_lat
    }

    /// Process a typed chunk across all requested resolutions with direct pixel-to-hex containment
    fn process_chunk_slice<T, F, N>(
        &mut self,
        slice: &[T],
        chunk: &RasterChunk,
        to_f64: F,
        native_nodata: Option<N>,
    ) where
        T: Copy + PartialEq,
        F: Fn(T) -> f64,
        N: Copy + PartialEq<T>,
    {
        if slice.is_empty() {
            return;
        }

        let is_wgs84 = matches!(self.crs_transformer, CrsTransformer::Wgs84Identity);
        let is_web_mercator = matches!(self.crs_transformer, CrsTransformer::WebMercatorFast);
        let d_lon_step = if is_wgs84 {
            self.gt.a
        } else if is_web_mercator {
            (self.gt.a / WGS84_A) * RAD_TO_DEG
        } else {
            0.0
        };

        let num_res = self.resolutions.len();

        for res_idx in 0..num_res {
            let res = self.resolutions[res_idx];
            let active_map = &mut self.active_maps[res_idx];
            let eviction_queue = &mut self.eviction_queues[res_idx];

            for r in 0..chunk.height {
                let row_idx = (chunk.row_offset + r) as usize;
                let slice_row_start = (r * self.chunk_stride) as usize;
                let mut row_cache = SpatialCoherenceCache::default();

                if self.sampling.is_single_point() {
                    let mut run_cell: u64 = 0;
                    let mut run_acc = H3Accumulator::default();

                    let (x_start, y_row) =
                        self.gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);

                    let (mut lon_curr, lat_row) = if is_wgs84 {
                        (x_start, y_row)
                    } else if is_web_mercator {
                        let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2)
                            * RAD_TO_DEG;
                        let lon = (x_start / WGS84_A) * RAD_TO_DEG;
                        (lon, lat)
                    } else {
                        match self.crs_transformer.transform_point(x_start, y_row) {
                            Ok(coords) => coords,
                            Err(_) => (x_start, y_row),
                        }
                    };

                    let mut c = 0;
                    while c < chunk.width as usize {
                        let (lon, lat) = if is_wgs84 || is_web_mercator {
                            (lon_curr, lat_row)
                        } else {
                            let (x, y) = self
                                .gt
                                .pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                            match self.crs_transformer.transform_point(x, y) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    c += 1;
                                    if is_wgs84 || is_web_mercator {
                                        lon_curr += d_lon_step;
                                    }
                                    continue;
                                }
                            }
                        };

                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = self.bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                c += 1;
                                if is_wgs84 || is_web_mercator {
                                    lon_curr += d_lon_step;
                                }
                                continue;
                            }
                        }

                        if let Some(cell_u64) = row_cache.get_or_compute(lat, lon, res) {
                            if cell_u64 != run_cell {
                                if run_cell != 0 && run_acc.count > 0.0 {
                                    active_map
                                        .entry(run_cell)
                                        .and_modify(|acc| acc.merge(&run_acc))
                                        .or_insert_with(|| {
                                            let south_lat = compute_cell_south_lat(run_cell);
                                            eviction_queue.push(HexEvictionEntry {
                                                south_lat,
                                                cell_u64: run_cell,
                                            });
                                            run_acc
                                        });
                                }
                                run_cell = cell_u64;
                                run_acc = H3Accumulator::default();
                            }

                            let safe_span = if is_wgs84 || is_web_mercator {
                                row_cache.safe_span_length(lon_curr, d_lon_step)
                            } else {
                                1
                            };

                            let span_end = (c + safe_span).min(chunk.width as usize);

                            if native_nodata.is_none() && self.nodata.is_none() {
                                for i in c..span_end {
                                    let val = to_f64(slice[slice_row_start + i]);
                                    if val.is_finite() {
                                        run_acc.update(val);
                                    }
                                }
                            } else {
                                for i in c..span_end {
                                    let val_raw = slice[slice_row_start + i];

                                    if let Some(nd_nat) = native_nodata {
                                        if nd_nat == val_raw {
                                            continue;
                                        }
                                    }

                                    let val = to_f64(val_raw);
                                    if !val.is_finite() {
                                        continue;
                                    }
                                    if let Some(nd) = self.nodata {
                                        if (val - nd).abs() < 1e-6 {
                                            continue;
                                        }
                                    }

                                    run_acc.update(val);
                                }
                            }

                            let num_stepped = span_end - c;
                            if is_wgs84 || is_web_mercator {
                                lon_curr += (num_stepped as f64) * d_lon_step;
                            }
                            c = span_end;
                        } else {
                            c += 1;
                            if is_wgs84 || is_web_mercator {
                                lon_curr += d_lon_step;
                            }
                        }
                    }

                    if run_cell != 0 && run_acc.count > 0.0 {
                        active_map
                            .entry(run_cell)
                            .and_modify(|acc| acc.merge(&run_acc))
                            .or_insert_with(|| {
                                let south_lat = compute_cell_south_lat(run_cell);
                                eviction_queue.push(HexEvictionEntry {
                                    south_lat,
                                    cell_u64: run_cell,
                                });
                                run_acc
                            });
                    }
                } else {
                    for c in 0..chunk.width as usize {
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
                        if let Some(nd) = self.nodata {
                            if (val - nd).abs() < 1e-6 {
                                continue;
                            }
                        }

                        let (center_lon, center_lat) =
                            self.gt.pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);

                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = self.bbox {
                            if center_lon < b_min_lon
                                || center_lon > b_max_lon
                                || center_lat < b_min_lat
                                || center_lat > b_max_lat
                            {
                                continue;
                            }
                        }

                        if row_cache.contains(center_lat, center_lon) {
                            let cell_u64 = row_cache.cell_u64;
                            active_map
                                .entry(cell_u64)
                                .and_modify(|acc| acc.update_weighted(val, 1.0))
                                .or_insert_with(|| {
                                    let south_lat = compute_cell_south_lat(cell_u64);
                                    eviction_queue.push(HexEvictionEntry {
                                        south_lat,
                                        cell_u64,
                                    });
                                    H3Accumulator::new_weighted(val, 1.0)
                                });
                        } else {
                            let col_px = (chunk.col_offset as usize) + c;
                            for pt in &self.sampling.points {
                                let (px, py) = self.gt.pixel_to_coord(col_px as f64 + pt.dx, row_idx as f64 + pt.dy);
                                if let Ok((lon_i, lat_i)) = self.crs_transformer.transform_point(px, py) {
                                    if let Ok(lat_lng) = h3o::LatLng::new(lat_i, lon_i) {
                                        let cell_u64: u64 = lat_lng.to_cell(res).into();
                                        active_map
                                            .entry(cell_u64)
                                            .and_modify(|acc| acc.update_weighted(val, pt.weight))
                                            .or_insert_with(|| {
                                                let south_lat = compute_cell_south_lat(cell_u64);
                                                eviction_queue.push(HexEvictionEntry {
                                                    south_lat,
                                                    cell_u64,
                                                });
                                                H3Accumulator::new_weighted(val, pt.weight)
                                            });
                                    }
                                }
                            }

                            if let Ok(center_ll) = h3o::LatLng::new(center_lat, center_lon) {
                                row_cache.update(center_ll.to_cell(res));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Evict completed hexagons across all resolution levels
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

    /// Pull up to `max_rows` completed multi-resolution records
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiContinuousRecord> {
        while self.completed_buffer.len() < max_rows && !self.is_finished {
            let next_item = if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.next_chunk()
            } else {
                None
            };

            match next_item {
                Some(Ok((_chunk_idx, chunk_bounds, decoding_result))) => {
                    match decoding_result {
                        DecodingResult::U8(slice) => {
                            let nd = self.nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::U16(slice) => {
                            let nd = self.nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::U32(slice) => {
                            let nd = self.nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::U64(slice) => {
                            let nd = self.nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::I8(slice) => {
                            let nd = self.nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::I16(slice) => {
                            let nd = self.nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::I32(slice) => {
                            let nd = self.nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::I64(slice) => {
                            let nd = self.nodata.map(|v| v as i64);
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::F32(slice) => {
                            let nd = self.nodata.map(|v| v as f32);
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x as f64, nd);
                        }
                        DecodingResult::F64(slice) => {
                            let nd = self.nodata;
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| x, nd);
                        }
                    }

                    let lat_horizon = self.compute_chunk_bottom_lat(&chunk_bounds);
                    self.evict_completed(lat_horizon);
                }
                Some(Err(_)) | None => {
                    self.is_finished = true;
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
                    break;
                }
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
}

impl MultiCategoricalHorizonStreamer {
    /// Initialize a new MultiCategoricalHorizonStreamer
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

        let prefetcher = PrefetchedChunkReader::spawn(reader, chunk_indices, 8);
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
        })
    }

    /// Calculate the minimum WGS84 latitude reached by the bottom edge of a chunk row
    fn compute_chunk_bottom_lat(&self, chunk_bounds: &RasterChunk) -> f64 {
        let row_bottom = (chunk_bounds.row_offset + chunk_bounds.height) as usize;
        let mut min_lat = f64::INFINITY;

        let col_samples = [
            chunk_bounds.col_offset as usize,
            (chunk_bounds.col_offset + chunk_bounds.width / 2) as usize,
            (chunk_bounds.col_offset + chunk_bounds.width) as usize,
        ];

        for &c in &col_samples {
            let (x, y) = self.gt.pixel_to_coord(c as f64, row_bottom as f64);
            if let Ok((_lon, lat)) = self.crs_transformer.transform_point(x, y) {
                if lat < min_lat {
                    min_lat = lat;
                }
            }
        }

        min_lat
    }

    /// Process a typed chunk across all requested resolutions for categorical landcover
    fn process_chunk_slice<T, F, N>(
        &mut self,
        slice: &[T],
        chunk: &RasterChunk,
        to_i64: F,
        native_nodata: Option<N>,
    ) where
        T: Copy + PartialEq,
        F: Fn(T) -> Option<i64>,
        N: Copy + PartialEq<T>,
    {
        if slice.is_empty() {
            return;
        }

        let is_wgs84 = matches!(self.crs_transformer, CrsTransformer::Wgs84Identity);
        let is_web_mercator = matches!(self.crs_transformer, CrsTransformer::WebMercatorFast);
        let d_lon_step = if is_wgs84 {
            self.gt.a
        } else if is_web_mercator {
            (self.gt.a / WGS84_A) * RAD_TO_DEG
        } else {
            0.0
        };

        let num_res = self.resolutions.len();

        for res_idx in 0..num_res {
            let res = self.resolutions[res_idx];
            let active_map = &mut self.active_maps[res_idx];
            let eviction_queue = &mut self.eviction_queues[res_idx];

            for r in 0..chunk.height {
                let row_idx = (chunk.row_offset + r) as usize;
                let slice_row_start = (r * self.chunk_stride) as usize;
                let mut row_cache = SpatialCoherenceCache::default();

                if self.sampling.is_single_point() {
                    let mut run_cell: u64 = 0;
                    let mut run_acc = CategoricalAccumulator::default();

                    let (x_start, y_row) =
                        self.gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);

                    let (mut lon_curr, lat_row) = if is_wgs84 {
                        (x_start, y_row)
                    } else if is_web_mercator {
                        let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2)
                            * RAD_TO_DEG;
                        let lon = (x_start / WGS84_A) * RAD_TO_DEG;
                        (lon, lat)
                    } else {
                        match self.crs_transformer.transform_point(x_start, y_row) {
                            Ok(coords) => coords,
                            Err(_) => (x_start, y_row),
                        }
                    };

                    let mut c = 0;
                    while c < chunk.width as usize {
                        let (lon, lat) = if is_wgs84 || is_web_mercator {
                            (lon_curr, lat_row)
                        } else {
                            let (x, y) = self
                                .gt
                                .pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                            match self.crs_transformer.transform_point(x, y) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    c += 1;
                                    if is_wgs84 || is_web_mercator {
                                        lon_curr += d_lon_step;
                                    }
                                    continue;
                                }
                            }
                        };

                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = self.bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                c += 1;
                                if is_wgs84 || is_web_mercator {
                                    lon_curr += d_lon_step;
                                }
                                continue;
                            }
                        }

                        if let Some(cell_u64) = row_cache.get_or_compute(lat, lon, res) {
                            if cell_u64 != run_cell {
                                if run_cell != 0 && run_acc.total_count > 0.0 {
                                    active_map
                                        .entry(run_cell)
                                        .and_modify(|acc| acc.merge(&run_acc))
                                        .or_insert_with(|| {
                                            let south_lat = compute_cell_south_lat(run_cell);
                                            eviction_queue.push(HexEvictionEntry {
                                                south_lat,
                                                cell_u64: run_cell,
                                            });
                                            run_acc.clone()
                                        });
                                }
                                run_cell = cell_u64;
                                run_acc = CategoricalAccumulator::default();
                            }

                            let safe_span = if is_wgs84 || is_web_mercator {
                                row_cache.safe_span_length(lon_curr, d_lon_step)
                            } else {
                                1
                            };

                            let span_end = (c + safe_span).min(chunk.width as usize);

                            let mut curr_cat: Option<i64> = None;
                            let mut curr_cat_count = 0.0;

                            for i in c..span_end {
                                let val_raw = slice[slice_row_start + i];

                                if let Some(nd_nat) = native_nodata {
                                    if nd_nat == val_raw {
                                        continue;
                                    }
                                }

                                if let Some(cat) = to_i64(val_raw) {
                                    if let Some(nd) = self.nodata {
                                        if (cat as f64 - nd).abs() < 1e-6 {
                                            continue;
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
                            if is_wgs84 || is_web_mercator {
                                lon_curr += (num_stepped as f64) * d_lon_step;
                            }
                            c = span_end;
                        } else {
                            c += 1;
                            if is_wgs84 || is_web_mercator {
                                lon_curr += d_lon_step;
                            }
                        }
                    }

                    if run_cell != 0 && run_acc.total_count > 0.0 {
                        active_map
                            .entry(run_cell)
                            .and_modify(|acc| acc.merge(&run_acc))
                            .or_insert_with(|| {
                                let south_lat = compute_cell_south_lat(run_cell);
                                eviction_queue.push(HexEvictionEntry {
                                    south_lat,
                                    cell_u64: run_cell,
                                });
                                run_acc
                            });
                    }
                } else {
                    for c in 0..chunk.width as usize {
                        let val_raw = slice[slice_row_start + c];
                        if let Some(nd_nat) = native_nodata {
                            if nd_nat == val_raw {
                                continue;
                            }
                        }

                        if let Some(cat) = to_i64(val_raw) {
                            if let Some(nd) = self.nodata {
                                if (cat as f64 - nd).abs() < 1e-6 {
                                    continue;
                                }
                            }

                            let (center_lon, center_lat) =
                                self.gt.pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);

                            if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = self.bbox {
                                if center_lon < b_min_lon
                                    || center_lon > b_max_lon
                                    || center_lat < b_min_lat
                                    || center_lat > b_max_lat
                                {
                                    continue;
                                }
                            }

                            if row_cache.contains(center_lat, center_lon) {
                                let cell_u64 = row_cache.cell_u64;
                                active_map
                                    .entry(cell_u64)
                                    .and_modify(|acc| acc.update_weighted(cat, 1.0))
                                    .or_insert_with(|| {
                                        let south_lat = compute_cell_south_lat(cell_u64);
                                        eviction_queue.push(HexEvictionEntry {
                                            south_lat,
                                            cell_u64,
                                        });
                                        let mut acc = CategoricalAccumulator::new();
                                        acc.update_weighted(cat, 1.0);
                                        acc
                                    });
                            } else {
                                let col_px = (chunk.col_offset as usize) + c;
                                for pt in &self.sampling.points {
                                    let (px, py) = self.gt.pixel_to_coord(col_px as f64 + pt.dx, row_idx as f64 + pt.dy);
                                    if let Ok((lon_i, lat_i)) = self.crs_transformer.transform_point(px, py) {
                                        if let Ok(lat_lng) = h3o::LatLng::new(lat_i, lon_i) {
                                            let cell_u64: u64 = lat_lng.to_cell(res).into();
                                            active_map
                                                .entry(cell_u64)
                                                .and_modify(|acc| acc.update_weighted(cat, pt.weight))
                                                .or_insert_with(|| {
                                                    let south_lat = compute_cell_south_lat(cell_u64);
                                                    eviction_queue.push(HexEvictionEntry {
                                                        south_lat,
                                                        cell_u64,
                                                    });
                                                    let mut acc = CategoricalAccumulator::new();
                                                    acc.update_weighted(cat, pt.weight);
                                                    acc
                                                });
                                        }
                                    }
                                }

                                if let Ok(center_ll) = h3o::LatLng::new(center_lat, center_lon) {
                                    row_cache.update(center_ll.to_cell(res));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Evict completed hexagons across all resolution levels
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

    /// Pull up to `max_rows` completed categorical multi-resolution records
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiCategoricalRecord> {
        while self.completed_buffer.len() < max_rows && !self.is_finished {
            let next_item = if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.next_chunk()
            } else {
                None
            };

            match next_item {
                Some(Ok((_chunk_idx, chunk_bounds, decoding_result))) => {
                    match decoding_result {
                        DecodingResult::U8(slice) => {
                            let nd = self.nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::U16(slice) => {
                            let nd = self.nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::U32(slice) => {
                            let nd = self.nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::U64(slice) => {
                            let nd = self.nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| if x <= i64::MAX as u64 { Some(x as i64) } else { None }, nd);
                        }
                        DecodingResult::I8(slice) => {
                            let nd = self.nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::I16(slice) => {
                            let nd = self.nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::I32(slice) => {
                            let nd = self.nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::I64(slice) => {
                            let nd = self.nodata.map(|v| v as i64);
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x), nd);
                        }
                        DecodingResult::F32(slice) => {
                            let nd = self.nodata.map(|v| v as f32);
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd);
                        }
                        DecodingResult::F64(slice) => {
                            let nd = self.nodata;
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd);
                        }
                    }

                    let lat_horizon = self.compute_chunk_bottom_lat(&chunk_bounds);
                    self.evict_completed(lat_horizon);
                }
                Some(Err(_)) | None => {
                    self.is_finished = true;
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
                    break;
                }
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
