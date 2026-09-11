use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::Arc;
use fxhash::FxBuildHasher;
use h3o::{CellIndex, Resolution};
use rayon::prelude::*;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::horizon_streamer::{compute_cell_south_lat, HexEvictionEntry};
use crate::aggregator::sampling::SamplingPattern;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::PrefetchedMosaicReader;

use super::config::{MultiResolutionConfig, SpectralFormula};
use super::continuous::{process_continuous_chunk_payload_into, MultiContinuousRecord};

/// Number of concurrent shards for parallel active map merging
pub const NUM_SHARDS: usize = 32;

/// Fast, uniform shard partitioner for 64-bit H3 cell indices
#[inline(always)]
pub fn get_shard(cell_u64: u64) -> usize {
    let h = cell_u64.wrapping_mul(0x517c_c1b7_2722_0a95);
    (h as usize) & (NUM_SHARDS - 1)
}

/// Single-pass streaming aggregator across multiple H3 resolutions (Continuous Data)
pub struct MultiScanHorizonStreamer {
    prefetcher: Option<PrefetchedMosaicReader>,
    pub mosaic: Arc<MosaicReader>,
    resolutions: Vec<Resolution>,
    resolution_u8s: Vec<u8>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    active_shards: Vec<Vec<HashMap<u64, H3Accumulator, FxBuildHasher>>>,
    eviction_shards: Vec<Vec<BinaryHeap<HexEvictionEntry>>>,
    completed_buffer: VecDeque<MultiContinuousRecord>,
    is_finished: bool,
    current_lat_horizon: f64,
    pub profile_stats: [u64; 4],
    processed_chunk_count: usize,
    band: usize,
    spectral_formula: Option<SpectralFormula>,
    min_count: Option<f64>,
    min_mean: Option<f64>,
    max_mean: Option<f64>,
    compact: bool,
    pending_compact: HashMap<u64, (H3Accumulator, Vec<(u64, H3Accumulator)>), FxBuildHasher>,
    pub track_quantiles: bool,
}

impl MultiScanHorizonStreamer {
    /// Initialize a new MultiScanHorizonStreamer from a single GeoTIFF reader
    pub fn new(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> Result<Self> {
        let mosaic = Arc::new(MosaicReader::from_single_reader(
            reader,
            config.bbox,
            config.custom_crs.as_deref(),
        )?);
        Self::new_mosaic(mosaic, config)
    }

    /// Initialize a new MultiScanHorizonStreamer from a multi-file MosaicReader
    pub fn new_mosaic(mosaic: Arc<MosaicReader>, config: &MultiResolutionConfig) -> Result<Self> {
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

        let prefetcher = PrefetchedMosaicReader::spawn(Arc::clone(&mosaic), 1024);
        let num_res = resolutions.len();

        let mut active_shards = Vec::with_capacity(num_res);
        let mut eviction_shards = Vec::with_capacity(num_res);
        for _ in 0..num_res {
            let mut res_active = Vec::with_capacity(NUM_SHARDS);
            let mut res_evict = Vec::with_capacity(NUM_SHARDS);
            for _ in 0..NUM_SHARDS {
                res_active.push(HashMap::with_capacity_and_hasher(128, FxBuildHasher::default()));
                res_evict.push(BinaryHeap::with_capacity(128));
            }
            active_shards.push(res_active);
            eviction_shards.push(res_evict);
        }

        let band = config.band;
        let spectral_formula = config.spectral_formula;
        let min_count = config.min_count;
        let min_mean = config.min_mean;
        let max_mean = config.max_mean;
        let compact = config.compact;
        let pending_compact = HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default());

        Ok(Self {
            prefetcher: Some(prefetcher),
            mosaic,
            resolutions,
            resolution_u8s,
            nodata: config.custom_nodata,
            bbox: config.bbox,
            sampling: config.sampling.clone(),
            active_shards,
            eviction_shards,
            completed_buffer: VecDeque::with_capacity(2048),
            is_finished: false,
            current_lat_horizon: f64::INFINITY,
            profile_stats: [0; 4],
            processed_chunk_count: 0,
            band,
            spectral_formula,
            min_count,
            min_mean,
            max_mean,
            compact,
            pending_compact,
            track_quantiles: config.track_quantiles(),
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

    fn push_continuous_record(&mut self, res_u8: u8, cell_u64: u64, acc: H3Accumulator) {
        if let Some(min_c) = self.min_count {
            if acc.count < min_c {
                return;
            }
        }
        if let Some(min_m) = self.min_mean {
            if acc.mean() < min_m {
                return;
            }
        }
        if let Some(max_m) = self.max_mean {
            if acc.mean() > max_m {
                return;
            }
        }

        if self.compact {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                if let Some(parent_res) = cell.resolution().pred() {
                    if let Some(parent) = cell.parent(parent_res) {
                        let parent_u64: u64 = parent.into();
                        let entry = self.pending_compact.entry(parent_u64).or_insert_with(|| {
                            (
                                if self.track_quantiles {
                                    H3Accumulator::with_quantiles()
                                } else {
                                    H3Accumulator::default()
                                },
                                Vec::with_capacity(7),
                            )
                        });
                        entry.0.merge(&acc);
                        entry.1.push((cell_u64, acc));

                        if entry.1.len() == 7 {
                            let (parent_acc, _) = self.pending_compact.remove(&parent_u64).unwrap();
                            let p_res_u8: u8 = parent_res.into();
                            self.completed_buffer.push_back(MultiContinuousRecord {
                                resolution: p_res_u8,
                                h3_index: parent_u64,
                                accumulator: parent_acc,
                            });
                            return;
                        }
                        return;
                    }
                }
            }
        }

        self.completed_buffer.push_back(MultiContinuousRecord {
            resolution: res_u8,
            h3_index: cell_u64,
            accumulator: acc,
        });
    }

    fn flush_pending_compact(&mut self) {
        for (_, (_, children)) in self.pending_compact.drain() {
            for (cell_u64, acc) in children {
                let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                    cell.resolution().into()
                } else {
                    8
                };
                self.completed_buffer.push_back(MultiContinuousRecord {
                    resolution: res_u8,
                    h3_index: cell_u64,
                    accumulator: acc,
                });
            }
        }
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            let res_u8 = self.resolution_u8s[res_idx];
            let mut newly_evicted: Vec<(u8, u64, H3Accumulator)> = self.active_shards[res_idx]
                .par_iter_mut()
                .zip(self.eviction_shards[res_idx].par_iter_mut())
                .map(|(shard_map, shard_evict)| {
                    let mut evicted = Vec::new();
                    while let Some(top) = shard_evict.peek() {
                        if top.south_lat > lat_horizon {
                            let entry = shard_evict.pop().unwrap();
                            if let Some(acc) = shard_map.remove(&entry.cell_u64) {
                                evicted.push((res_u8, entry.cell_u64, acc));
                            }
                        } else {
                            break;
                        }
                    }
                    evicted
                })
                .flatten()
                .collect();

            newly_evicted.par_sort_unstable_by_key(|item| item.1);

            for (r, cell_u64, acc) in newly_evicted {
                self.push_continuous_record(r, cell_u64, acc);
            }
        }

        if self.compact && !self.pending_compact.is_empty() {
            let mut to_flush = Vec::new();
            for (&parent_u64, _) in self.pending_compact.iter() {
                let parent_south = compute_cell_south_lat(parent_u64);
                if parent_south > lat_horizon {
                    to_flush.push(parent_u64);
                }
            }
            for p in to_flush {
                if let Some((_, children)) = self.pending_compact.remove(&p) {
                    for (cell_u64, acc) in children {
                        let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                            cell.resolution().into()
                        } else {
                            8
                        };
                        self.completed_buffer.push_back(MultiContinuousRecord {
                            resolution: res_u8,
                            h3_index: cell_u64,
                            accumulator: acc,
                        });
                    }
                }
            }
        }
    }

    /// Advance scanline horizon until at least `min_rows` completed records are available or finished
    pub fn advance_until_completed(&mut self, min_rows: usize) {
        let batch_size = (rayon::current_num_threads() * 4).max(32);
        let min_batch = rayon::current_num_threads().clamp(4, 16);
        let mut chunk_items = Vec::with_capacity(batch_size);

        while self.completed_buffer.len() < min_rows && !self.is_finished {
            chunk_items.clear();
            let t0 = std::time::Instant::now();
            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut chunk_items, min_batch, batch_size);
            }
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;

            if chunk_items.is_empty() {
                self.is_finished = true;
                self.current_lat_horizon = f64::NEG_INFINITY;
                let num_res = self.resolutions.len();
                for res_idx in 0..num_res {
                    let res_u8 = self.resolution_u8s[res_idx];
                    let mut remaining: Vec<(u8, u64, H3Accumulator)> = self.active_shards[res_idx]
                        .par_iter_mut()
                        .zip(self.eviction_shards[res_idx].par_iter_mut())
                        .map(|(shard_map, shard_evict)| {
                            let mut drained = Vec::with_capacity(shard_map.len());
                            shard_evict.clear();
                            for (cell_u64, acc) in shard_map.drain() {
                                drained.push((res_u8, cell_u64, acc));
                            }
                            drained
                        })
                        .flatten()
                        .collect();

                    remaining.par_sort_unstable_by_key(|item| item.1);

                    for (r, cell_u64, acc) in remaining {
                        self.push_continuous_record(r, cell_u64, acc);
                    }
                }
                self.flush_pending_compact();
                break;
            }

            let resolutions = &self.resolutions;
            let sampling = &self.sampling;
            let bbox = self.bbox;
            let band = self.band;
            let spectral_formula = self.spectral_formula;
            let mosaic = Arc::clone(&self.mosaic);
            let user_nodata = self.nodata;
            let track_quantiles = self.track_quantiles;

            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(Vec<Vec<Vec<(u64, H3Accumulator)>>>, DecodingResult)> = chunk_items
                .par_iter_mut()
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
                            Ok((tile_idx, _chunk_idx, chunk_bounds, decoding_result, has_overlap)) => {
                                for m in local_maps.iter_mut() {
                                    m.clear();
                                }
                                let tile = &mosaic.tiles[*tile_idx];
                                let crs_transformer = &tile.crs_transformer;
                                let gt = &tile.reader.metadata.geotransform;
                                let chunk_stride = tile.reader.chunk_layout.chunk_width;
                                let nodata = user_nodata.or(tile.reader.metadata.nodata);
                                let samples_per_pixel = tile.reader.metadata.samples_per_pixel;

                                let overlap_ctx = if *has_overlap {
                                    Some((*tile_idx, &*mosaic))
                                } else {
                                    None
                                };

                                let has_data = process_continuous_chunk_payload_into(
                                    chunk_bounds,
                                    decoding_result,
                                    resolutions,
                                    crs_transformer,
                                    gt,
                                    sampling,
                                    bbox,
                                    chunk_stride,
                                    nodata,
                                    samples_per_pixel,
                                    band,
                                    spectral_formula,
                                    overlap_ctx,
                                    track_quantiles,
                                    local_maps,
                                );
                                let mut chunk_shards = Vec::with_capacity(if has_data { local_maps.len() } else { 0 });
                                if has_data {
                                    for m in local_maps.iter_mut() {
                                        let mut shards = (0..NUM_SHARDS).map(|_| Vec::new()).collect::<Vec<_>>();
                                        for (cell_u64, acc) in m.drain() {
                                            let s = get_shard(cell_u64);
                                            shards[s].push((cell_u64, acc));
                                        }
                                        chunk_shards.push(shards);
                                    }
                                }
                                let dec = std::mem::replace(decoding_result, DecodingResult::U8(Vec::new()));
                                Some((chunk_shards, dec))
                            }
                            Err(_) => None,
                        }
                    },
                )
                .filter_map(|x| x)
                .collect();
            self.profile_stats[1] += t1.elapsed().as_nanos() as u64;

            let t2 = std::time::Instant::now();
            self.processed_chunk_count += chunk_items.len();

            for res_idx in 0..self.resolutions.len() {
                self.active_shards[res_idx]
                    .par_iter_mut()
                    .zip(self.eviction_shards[res_idx].par_iter_mut())
                    .enumerate()
                    .for_each(|(s, (shard_map, shard_evict))| {
                        for (chunk_shards, _) in &parallel_results {
                            if res_idx < chunk_shards.len() {
                                for &(cell_u64, ref acc) in &chunk_shards[res_idx][s] {
                                    shard_map
                                        .entry(cell_u64)
                                        .and_modify(|existing| existing.merge(acc))
                                        .or_insert_with(|| {
                                            let south_lat = compute_cell_south_lat(cell_u64);
                                            shard_evict.push(HexEvictionEntry {
                                                south_lat,
                                                cell_u64,
                                            });
                                            acc.clone()
                                        });
                                }
                            }
                        }
                    });
            }

            let recycled_buffers: Vec<DecodingResult> = parallel_results
                .into_iter()
                .map(|(_, dec)| dec)
                .collect();

            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.recycle_batch(recycled_buffers);
            }
            self.profile_stats[2] += t2.elapsed().as_nanos() as u64;

            if self.processed_chunk_count < self.mosaic.chunk_refs.len() {
                let next_chunk = &self.mosaic.chunk_refs[self.processed_chunk_count];
                let safe_lat = next_chunk.north_lat;
                if safe_lat < self.current_lat_horizon {
                    let t3 = std::time::Instant::now();
                    self.current_lat_horizon = safe_lat;
                    self.evict_completed(safe_lat);
                    self.profile_stats[3] += t3.elapsed().as_nanos() as u64;
                }
            }
        }
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiContinuousRecord> {
        self.advance_until_completed(max_rows);
        let num_to_take = max_rows.min(self.completed_buffer.len());
        let mut batch = Vec::with_capacity(num_to_take);
        for _ in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                batch.push(record);
            }
        }
        batch
    }

    /// Drain up to `max_rows` completed records directly into a closure with zero heap allocation
    pub fn drain_completed_into<F>(&mut self, max_rows: usize, mut consumer: F) -> usize
    where
        F: FnMut(usize, MultiContinuousRecord),
    {
        self.advance_until_completed(max_rows);
        let num_to_take = max_rows.min(self.completed_buffer.len());
        for i in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                consumer(i, record);
            }
        }
        num_to_take
    }

    /// Return total active in-flight cells across all resolutions
    pub fn active_cell_count(&self) -> usize {
        self.active_shards
            .iter()
            .map(|shards| shards.iter().map(|m| m.len()).sum::<usize>())
            .sum()
    }
}
