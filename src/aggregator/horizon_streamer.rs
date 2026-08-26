use std::collections::{BinaryHeap, VecDeque};
use h3o::{CellIndex, LatLng, Resolution};
use std::collections::HashMap;
use fxhash::FxBuildHasher;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::coherence::SpatialCoherenceCache;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::geotransform::GeoTransform;
use crate::raster::prefetch::PrefetchedChunkReader;
use crate::raster::RasterChunk;

const WGS84_A: f64 = 6378137.0;
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Priority queue entry for H3 cell eviction ordered by southernmost latitude
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HexEvictionEntry {
    pub south_lat: f64,
    pub cell_u64: u64,
}

impl Eq for HexEvictionEntry {}

impl Ord for HexEvictionEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap: highest south_lat (northernmost southern boundary) is popped first
        self.south_lat
            .partial_cmp(&other.south_lat)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for HexEvictionEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Compute the approximate southernmost latitude for an H3 cell using center lat as a fast proxy.
/// Center lat is always >= true south vertex lat, making eviction conservatively lazy (safe).
#[inline(always)]
pub fn compute_cell_south_lat(cell_u64: u64) -> f64 {
    if let Ok(cell) = CellIndex::try_from(cell_u64) {
        h3o::LatLng::from(cell).lat()
    } else {
        f64::NEG_INFINITY
    }
}

/// Fast check if an entire chunk slice is NoData / NaN
#[inline(always)]
pub fn is_chunk_all_nodata<T, F>(slice: &[T], nodata: Option<f64>, to_f64: F) -> bool
where
    T: Copy,
    F: Fn(T) -> f64,
{
    if slice.is_empty() {
        return true;
    }
    match nodata {
        Some(nd) => {
            let mid = slice.len() / 2;
            let last = slice.len() - 1;
            let s0 = to_f64(slice[0]);
            let s_mid = to_f64(slice[mid]);
            let s_last = to_f64(slice[last]);

            let is_nd = |v: f64| !v.is_finite() || (v - nd).abs() < 1e-6;
            if !is_nd(s0) || !is_nd(s_mid) || !is_nd(s_last) {
                return false;
            }
            slice.iter().all(|&x| is_nd(to_f64(x)))
        }
        None => slice.iter().all(|&x| !to_f64(x).is_finite()),
    }
}

/// Configuration options for raster-to-H3 aggregation
#[derive(Debug, Clone)]
pub struct AggregationConfig {
    pub resolution: u8,
    pub custom_crs: Option<String>,
    pub custom_nodata: Option<f64>,
    pub bbox: Option<[f64; 4]>, // [min_lon, min_lat, max_lon, max_lat]
    pub sampling: SamplingPattern,
}

impl Default for AggregationConfig {
    fn default() -> Self {
        Self {
            resolution: 8,
            custom_crs: None,
            custom_nodata: None,
            bbox: None,
            sampling: SamplingPattern::default(),
        }
    }
}

/// Check if a 2D chunk intersects the given [min_lon, min_lat, max_lon, max_lat] bounding box
pub fn chunk_intersects_bbox(
    chunk: &RasterChunk,
    gt: &GeoTransform,
    transformer: &CrsTransformer,
    bbox: &[f64; 4],
) -> bool {
    let [b_min_lon, b_min_lat, b_max_lon, b_max_lat] = *bbox;

    let corners = [
        (chunk.col_offset as usize, chunk.row_offset as usize),
        ((chunk.col_offset + chunk.width) as usize, chunk.row_offset as usize),
        (chunk.col_offset as usize, (chunk.row_offset + chunk.height) as usize),
        ((chunk.col_offset + chunk.width) as usize, (chunk.row_offset + chunk.height) as usize),
    ];

    let mut c_min_lon = f64::INFINITY;
    let mut c_max_lon = f64::NEG_INFINITY;
    let mut c_min_lat = f64::INFINITY;
    let mut c_max_lat = f64::NEG_INFINITY;

    for (c, r) in corners {
        let (x, y) = gt.pixel_to_coord(c as f64, r as f64);
        if let Ok((lon, lat)) = transformer.transform_point(x, y) {
            if lon < c_min_lon { c_min_lon = lon; }
            if lon > c_max_lon { c_max_lon = lon; }
            if lat < c_min_lat { c_min_lat = lat; }
            if lat > c_max_lat { c_max_lat = lat; }
        }
    }

    if !c_min_lon.is_finite() {
        return true;
    }

    c_min_lon <= b_max_lon && c_max_lon >= b_min_lon && c_min_lat <= b_max_lat && c_max_lat >= b_min_lat
}

/// Streaming aggregator using Southernmost Scan-Line Horizon Eviction,
/// Row-Constant Latitude Hoisting, Zero-Copy mmap, and Identity Hasher.
pub struct ScanHorizonStreamer {
    prefetcher: Option<PrefetchedChunkReader>,
    crs_transformer: CrsTransformer,
    resolution: Resolution,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    gt: GeoTransform,
    chunk_stride: u32,
    active_map: HashMap<u64, H3Accumulator, FxBuildHasher>,
    eviction_queue: BinaryHeap<HexEvictionEntry>,
    completed_buffer: VecDeque<(u64, H3Accumulator)>,
    is_finished: bool,
}

impl ScanHorizonStreamer {
    /// Initialize a new ScanHorizonStreamer with background async prefetching and bbox pruning
    pub fn new(reader: GeoTiffStreamReader, config: &AggregationConfig) -> Result<Self> {
        let resolution = Resolution::try_from(config.resolution)
            .map_err(|_| RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", config.resolution)))?;

        let crs_transformer = CrsTransformer::from_crs_or_epsg(
            reader.metadata.epsg,
            config.custom_crs.as_deref().or(reader.metadata.proj_string.as_deref()),
        )?;

        let nodata = config.custom_nodata.or(reader.metadata.nodata);
        let bbox = config.bbox;
        let gt = reader.metadata.geotransform;
        let chunk_stride = reader.chunk_layout.chunk_width;
        let total_chunks = reader.chunk_layout.total_chunks;

        // Prune chunks upfront against bounding box if specified
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

    /// Process a typed chunk using Row-Constant Latitude Hoisting and Linear Longitude Stepping
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
        // 1. Fast NoData early-exit
        if is_chunk_all_nodata(slice, self.nodata, &to_f64) {
            return;
        }

        // 2. Pre-calculate coordinate step parameters
        let is_wgs84 = matches!(self.crs_transformer, CrsTransformer::Wgs84Identity);
        let is_web_mercator = matches!(self.crs_transformer, CrsTransformer::WebMercatorFast);
        let d_lon_step = if is_wgs84 {
            self.gt.a
        } else if is_web_mercator {
            (self.gt.a / WGS84_A) * RAD_TO_DEG
        } else {
            0.0
        };

        // 3. Row-by-row processing
        for r in 0..chunk.height {
            let row_idx = (chunk.row_offset + r) as usize;
            let slice_row_start = (r * self.chunk_stride) as usize;
            let mut row_cache = SpatialCoherenceCache::default();

            if self.sampling.is_single_point() {
                // Fast-path: Single-point center sampling with scanline run-skipping
                let mut run_cell: u64 = 0;
                let mut run_acc = H3Accumulator::default();

                let (x_start, y_row) = self.gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);

                let (mut lon_curr, lat_row) = if is_wgs84 {
                    (x_start, y_row)
                } else if is_web_mercator {
                    let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
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
                        let (x, y) = self.gt.pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
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
                            if run_cell != 0 && run_acc.count > 0.0 {
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
                            run_cell = cell_u64;
                            run_acc = H3Accumulator::default();
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
                // Multi-point sub-pixel super-sampling
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

                    let col_px = (chunk.col_offset as usize) + c;
                    let (center_x, center_y) = self.gt.pixel_center_to_coord(col_px, row_idx);
                    let (center_lon, center_lat) = match self.crs_transformer.transform_point(center_x, center_y) {
                        Ok(coords) => coords,
                        Err(_) => continue,
                    };

                    if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = self.bbox {
                        if center_lon < b_min_lon || center_lon > b_max_lon || center_lat < b_min_lat || center_lat > b_max_lat {
                            continue;
                        }
                    }

                    if row_cache.contains(center_lat, center_lon) {
                        // Fast-path: 100% of sub-points fall strictly inside cached hexagon
                        let cell_u64 = row_cache.cell_u64;
                        self.active_map
                            .entry(cell_u64)
                            .and_modify(|acc| acc.update_weighted(val, 1.0))
                            .or_insert_with(|| {
                                let south_lat = compute_cell_south_lat(cell_u64);
                                self.eviction_queue.push(HexEvictionEntry {
                                    south_lat,
                                    cell_u64,
                                });
                                H3Accumulator::new_weighted(val, 1.0)
                            });
                    } else {
                        // Boundary zone: evaluate each sub-pixel offset
                        for pt in &self.sampling.points {
                            let (px, py) = self.gt.pixel_to_coord(col_px as f64 + pt.dx, row_idx as f64 + pt.dy);
                            if let Ok((lon_i, lat_i)) = self.crs_transformer.transform_point(px, py) {
                                if let Ok(lat_lng) = LatLng::new(lat_i, lon_i) {
                                    let cell_u64: u64 = lat_lng.to_cell(self.resolution).into();
                                    self.active_map
                                        .entry(cell_u64)
                                        .and_modify(|acc| acc.update_weighted(val, pt.weight))
                                        .or_insert_with(|| {
                                            let south_lat = compute_cell_south_lat(cell_u64);
                                            self.eviction_queue.push(HexEvictionEntry {
                                                south_lat,
                                                cell_u64,
                                            });
                                            H3Accumulator::new_weighted(val, pt.weight)
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

    /// Pull up to `max_rows` completed records from the stream
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<(u64, H3Accumulator)> {
        while self.completed_buffer.len() < max_rows && !self.is_finished {
            let next_item = if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.next_chunk()
            } else {
                None
            };

            match next_item {
                Some(Ok((_chunk_idx, chunk_bounds, decoding_result))) => {
                    // Process native pixels with row-constant latitude hoisting & native NoData
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

                    // 2. Compute the current scan horizon at bottom of this chunk row
                    let lat_horizon = self.compute_chunk_bottom_lat(&chunk_bounds);

                    // 3. Evict completed hexagons
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

    /// Return current number of active cells in memory
    pub fn active_cell_count(&self) -> usize {
        self.active_map.len()
    }
}
