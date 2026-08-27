use std::collections::{BinaryHeap, HashMap, VecDeque};
use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use serde::{Deserialize, Serialize};
use tiff::decoder::DecodingResult;

use crate::aggregator::coherence::SpatialCoherenceCache;
use crate::aggregator::horizon_streamer::{
    chunk_intersects_bbox, compute_cell_south_lat, AggregationConfig,
    HexEvictionEntry,
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

/// High-performance accumulator for categorical class frequencies per H3 cell
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoricalAccumulator {
    /// Mapping of category_id -> weighted pixel count
    pub counts: HashMap<i64, f64, FxBuildHasher>,
    /// Total weighted pixel count in this hexagon
    pub total_count: f64,
}

impl Default for CategoricalAccumulator {
    #[inline(always)]
    fn default() -> Self {
        Self {
            counts: HashMap::with_hasher(FxBuildHasher::default()),
            total_count: 0.0,
        }
    }
}

impl CategoricalAccumulator {
    /// Initialize a new empty categorical accumulator
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with a single unweighted category
    #[inline(always)]
    pub fn update(&mut self, category: i64) {
        self.update_weighted(category, 1.0);
    }

    /// Update with a weighted category (e.g. from sub-pixel super-sampling)
    #[inline(always)]
    pub fn update_weighted(&mut self, category: i64, weight: f64) {
        if weight <= 0.0 {
            return;
        }
        *self.counts.entry(category).or_insert(0.0) += weight;
        self.total_count += weight;
    }

    /// Merge another categorical accumulator
    pub fn merge(&mut self, other: &Self) {
        if other.total_count == 0.0 {
            return;
        }
        for (&cat, &cnt) in &other.counts {
            *self.counts.entry(cat).or_insert(0.0) += cnt;
        }
        self.total_count += other.total_count;
    }

    /// Return majority (mode) category, its count, and its fraction of total
    pub fn majority(&self) -> (i64, f64, f64) {
        if self.total_count == 0.0 || self.counts.is_empty() {
            return (0, 0.0, 0.0);
        }
        let mut max_cat = 0;
        let mut max_count = -1.0;
        for (&cat, &cnt) in &self.counts {
            if cnt > max_count {
                max_count = cnt;
                max_cat = cat;
            }
        }
        let frac = if self.total_count > 0.0 {
            max_count / self.total_count
        } else {
            0.0
        };
        (max_cat, max_count, frac)
    }

    /// Return number of unique categories present (richness)
    #[inline(always)]
    pub fn unique_classes(&self) -> usize {
        self.counts.len()
    }

    /// Serialize histogram to a JSON string representation
    pub fn histogram_json(&self) -> String {
        if self.counts.is_empty() {
            return "{}".to_string();
        }
        let mut s = String::with_capacity(32 + self.counts.len() * 20);
        s.push('{');
        let mut first = true;
        let mut sorted_keys: Vec<_> = self.counts.keys().collect();
        sorted_keys.sort_unstable();
        for &k in sorted_keys {
            if !first {
                s.push_str(", ");
            }
            first = false;
            let cnt = self.counts.get(&k).copied().unwrap_or(0.0);
            let frac = if self.total_count > 0.0 {
                cnt / self.total_count
            } else {
                0.0
            };
            s.push('"');
            s.push_str(&k.to_string());
            s.push_str("\": ");
            s.push_str(&format!("{:.4}", frac));
        }
        s.push('}');
        s
    }
}

/// Streaming categorical aggregator using Southernmost Scan-Line Horizon Eviction
pub struct CategoricalHorizonStreamer {
    prefetcher: Option<PrefetchedChunkReader>,
    crs_transformer: CrsTransformer,
    resolution: Resolution,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    gt: GeoTransform,
    chunk_stride: u32,
    active_map: HashMap<u64, CategoricalAccumulator, FxBuildHasher>,
    eviction_queue: BinaryHeap<HexEvictionEntry>,
    completed_buffer: VecDeque<(u64, CategoricalAccumulator)>,
    is_finished: bool,
}

impl CategoricalHorizonStreamer {
    /// Initialize a new CategoricalHorizonStreamer with background async prefetching and bbox pruning
    pub fn new(reader: GeoTiffStreamReader, config: &AggregationConfig) -> Result<Self> {
        let resolution = Resolution::try_from(config.resolution).map_err(|_| {
            RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", config.resolution))
        })?;

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

        Ok(Self {
            prefetcher: Some(prefetcher),
            crs_transformer,
            resolution,
            nodata,
            bbox,
            sampling: config.sampling.clone(),
            gt,
            chunk_stride,
            active_map: HashMap::default(),
            eviction_queue: BinaryHeap::new(),
            completed_buffer: VecDeque::new(),
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

    /// Process a typed chunk for categorical aggregation
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
        // Check if entire chunk is NoData
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

        for r in 0..chunk.height {
            let row_idx = (chunk.row_offset + r) as usize;
            let slice_row_start = (r * self.chunk_stride) as usize;
            let mut row_cache = SpatialCoherenceCache::default();

            if self.sampling.is_single_point() {
                // Fast-path: Single-point center sampling with scanline run-skipping
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

                    if let Some(cell_u64) = row_cache.get_or_compute(lat, lon, self.resolution) {
                        if cell_u64 != run_cell {
                            if run_cell != 0 && run_acc.total_count > 0.0 {
                                self.active_map
                                    .entry(run_cell)
                                    .and_modify(|acc| acc.merge(&run_acc))
                                    .or_insert_with(|| {
                                        let south_lat = compute_cell_south_lat(run_cell);
                                        self.eviction_queue.push(HexEvictionEntry {
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
                                run_acc.update(cat);
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

                if run_cell != 0 && run_acc.total_count > 0.0 {
                    self.active_map
                        .entry(run_cell)
                        .and_modify(|acc| acc.merge(&run_acc))
                        .or_insert_with(|| {
                            let south_lat = compute_cell_south_lat(run_cell);
                            self.eviction_queue.push(HexEvictionEntry {
                                south_lat,
                                cell_u64: run_cell,
                            });
                            run_acc
                        });
                }
            } else {
                // Multi-point sub-pixel super-sampling for categorical data
                for c in 0..chunk.width as usize {
                    let val_raw = slice[slice_row_start + c];

                    if let Some(nd_nat) = native_nodata {
                        if nd_nat == val_raw {
                            continue;
                        }
                    }

                    let cat = match to_i64(val_raw) {
                        Some(v) => v,
                        None => continue,
                    };

                    if let Some(nd) = self.nodata {
                        if (cat as f64 - nd).abs() < 1e-6 {
                            continue;
                        }
                    }

                    let col_px = (chunk.col_offset as usize) + c;
                    let (center_x, center_y) = self.gt.pixel_center_to_coord(col_px, row_idx);
                    let (center_lon, center_lat) =
                        match self.crs_transformer.transform_point(center_x, center_y) {
                            Ok(coords) => coords,
                            Err(_) => continue,
                        };

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
                        self.active_map
                            .entry(cell_u64)
                            .and_modify(|acc| acc.update_weighted(cat, 1.0))
                            .or_insert_with(|| {
                                let south_lat = compute_cell_south_lat(cell_u64);
                                self.eviction_queue.push(HexEvictionEntry {
                                    south_lat,
                                    cell_u64,
                                });
                                let mut acc = CategoricalAccumulator::default();
                                acc.update_weighted(cat, 1.0);
                                acc
                            });
                    } else {
                        for pt in &self.sampling.points {
                            let (px, py) = self
                                .gt
                                .pixel_to_coord(col_px as f64 + pt.dx, row_idx as f64 + pt.dy);
                            if let Ok((lon_i, lat_i)) = self.crs_transformer.transform_point(px, py)
                            {
                                if let Ok(lat_lng) = LatLng::new(lat_i, lon_i) {
                                    let cell_u64: u64 = lat_lng.to_cell(self.resolution).into();
                                    self.active_map
                                        .entry(cell_u64)
                                        .and_modify(|acc| acc.update_weighted(cat, pt.weight))
                                        .or_insert_with(|| {
                                            let south_lat = compute_cell_south_lat(cell_u64);
                                            self.eviction_queue.push(HexEvictionEntry {
                                                south_lat,
                                                cell_u64,
                                            });
                                            let mut acc = CategoricalAccumulator::default();
                                            acc.update_weighted(cat, pt.weight);
                                            acc
                                        });
                                }
                            }
                        }

                        if let Ok(center_ll) = LatLng::new(center_lat, center_lon) {
                            row_cache.update(center_ll.to_cell(self.resolution));
                        }
                    }
                }
            }
        }
    }

    /// Evict all completed hexagons whose southernmost latitude is strictly above lat_horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        while let Some(top) = self.eviction_queue.peek() {
            if top.south_lat > lat_horizon {
                let entry = self.eviction_queue.pop().unwrap();
                if let Some(acc) = self.active_map.remove(&entry.cell_u64) {
                    self.completed_buffer.push_back((entry.cell_u64, acc));
                }
            } else {
                break;
            }
        }
    }

    /// Pull up to `max_rows` completed categorical records from the stream
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<(u64, CategoricalAccumulator)> {
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
                            let nd = self.nodata.and_then(|v| {
                                if (0.0..=255.0).contains(&v) {
                                    Some(v as u8)
                                } else {
                                    None
                                }
                            });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::U16(slice) => {
                            let nd = self.nodata.and_then(|v| {
                                if (0.0..=65535.0).contains(&v) {
                                    Some(v as u16)
                                } else {
                                    None
                                }
                            });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::U32(slice) => {
                            let nd = self.nodata.and_then(|v| {
                                if v >= 0.0 && v <= u32::MAX as f64 {
                                    Some(v as u32)
                                } else {
                                    None
                                }
                            });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::U64(slice) => {
                            let nd = self.nodata.and_then(|v| {
                                if v >= 0.0 {
                                    Some(v as u64)
                                } else {
                                    None
                                }
                            });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::I8(slice) => {
                            let nd = self.nodata.and_then(|v| {
                                if (-128.0..=127.0).contains(&v) {
                                    Some(v as i8)
                                } else {
                                    None
                                }
                            });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::I16(slice) => {
                            let nd = self.nodata.and_then(|v| {
                                if (-32768.0..=32767.0).contains(&v) {
                                    Some(v as i16)
                                } else {
                                    None
                                }
                            });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::I32(slice) => {
                            let nd = self.nodata.and_then(|v| {
                                if v >= i32::MIN as f64 && v <= i32::MAX as f64 {
                                    Some(v as i32)
                                } else {
                                    None
                                }
                            });
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x as i64), nd);
                        }
                        DecodingResult::I64(slice) => {
                            let nd = self.nodata.map(|v| v as i64);
                            self.process_chunk_slice(&slice, &chunk_bounds, |x| Some(x), nd);
                        }
                        DecodingResult::F32(slice) => {
                            let nd = self.nodata.map(|v| v as f32);
                            self.process_chunk_slice(
                                &slice,
                                &chunk_bounds,
                                |x| if x.is_finite() { Some(x.round() as i64) } else { None },
                                nd,
                            );
                        }
                        DecodingResult::F64(slice) => {
                            let nd = self.nodata;
                            self.process_chunk_slice(
                                &slice,
                                &chunk_bounds,
                                |x| if x.is_finite() { Some(x.round() as i64) } else { None },
                                nd,
                            );
                        }
                    }

                    let lat_horizon = self.compute_chunk_bottom_lat(&chunk_bounds);
                    self.evict_completed(lat_horizon);
                }
                Some(Err(_)) | None => {
                    self.is_finished = true;
                    while let Some(entry) = self.eviction_queue.pop() {
                        if let Some(acc) = self.active_map.remove(&entry.cell_u64) {
                            self.completed_buffer.push_back((entry.cell_u64, acc));
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
}
