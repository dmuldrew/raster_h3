//! Generic Multi-Resolution Scanline Horizon Streamer Controller
//!
//! Consolidates chunk prefetch draining, Rayon multi-core chunk dispatch, 32-way shard
//! partitioning, lock-free thread result merging, decompression buffer recycling,
//! southernmost latitude horizon progression, and 7-cell hierarchical compaction.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use fxhash::FxBuildHasher;
use h3o::{CellIndex, Resolution};
use rayon::prelude::*;
use tiff::decoder::DecodingResult;

use crate::aggregator::horizon_streamer::compute_cell_south_lat;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::PrefetchedMosaicReader;
use crate::raster::RasterChunk;

use super::config::MultiResolutionConfig;
use super::sharded_map::{get_shard, AccumulatorMerge, ShardedResolutionMap, NUM_SHARDS};

/// Kernel trait parameterizing data type-specific chunk processing, aggregation, and filtering
pub trait HorizonStreamKernel: Send + Sync + 'static {
    type Accumulator: AccumulatorMerge + Default + 'static;
    type Record: Send + 'static;

    /// Create an accumulator for parent cell during 7-cell compaction
    fn new_parent_accumulator(&self) -> Self::Accumulator;

    /// Check if accumulator passes user filter thresholds
    fn passes_filter(&self, acc: &Self::Accumulator) -> bool;

    /// Construct an output record from cell index, resolution, and accumulator
    fn make_record(&self, resolution: u8, cell_u64: u64, acc: Self::Accumulator) -> Self::Record;

    /// Execute chunk processing kernel into thread-local hash maps
    fn process_chunk(
        &self,
        chunk_bounds: &RasterChunk,
        decoding_result: &mut DecodingResult,
        resolutions: &[Resolution],
        crs_transformer: &CrsTransformer,
        gt: &GeoTransform,
        sampling: &SamplingPattern,
        bbox: Option<[f64; 4]>,
        chunk_stride: u32,
        nodata: Option<f64>,
        samples_per_pixel: u16,
        overlap_ctx: Option<(usize, &MosaicReader)>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, FxBuildHasher>],
    ) -> bool;
}

/// Generic single-pass streaming aggregator across multiple H3 resolutions
pub struct MultiHorizonStreamer<K: HorizonStreamKernel> {
    pub kernel: K,
    pub prefetcher: Option<PrefetchedMosaicReader>,
    pub mosaic: Arc<MosaicReader>,
    pub resolutions: Vec<Resolution>,
    pub resolution_u8s: Vec<u8>,
    pub nodata: Option<f64>,
    pub bbox: Option<[f64; 4]>,
    pub sampling: SamplingPattern,
    pub resolution_shards: Vec<ShardedResolutionMap<K::Accumulator>>,
    pub completed_buffer: VecDeque<K::Record>,
    pub is_finished: bool,
    pub current_lat_horizon: f64,
    pub profile_stats: [u64; 4],
    pub processed_chunk_count: usize,
    pub compact: bool,
    pub pending_compact: HashMap<u64, (K::Accumulator, Vec<(u64, K::Accumulator)>), FxBuildHasher>,
}

impl<K: HorizonStreamKernel> MultiHorizonStreamer<K> {
    /// Initialize a new MultiHorizonStreamer from a single GeoTIFF reader
    pub fn new(
        reader: GeoTiffStreamReader,
        config: &MultiResolutionConfig,
        kernel: K,
    ) -> Result<Self> {
        let mosaic = Arc::new(MosaicReader::from_single_reader(
            reader,
            config.bbox,
            config.custom_crs.as_deref(),
        )?);
        Self::new_mosaic(mosaic, config, kernel)
    }

    /// Initialize a new MultiHorizonStreamer from a multi-file MosaicReader
    pub fn new_mosaic(
        mosaic: Arc<MosaicReader>,
        config: &MultiResolutionConfig,
        kernel: K,
    ) -> Result<Self> {
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
        let resolution_shards = (0..num_res).map(|_| ShardedResolutionMap::new()).collect();
        let compact = config.compact;
        let pending_compact = HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default());

        Ok(Self {
            kernel,
            prefetcher: Some(prefetcher),
            mosaic,
            resolutions,
            resolution_u8s,
            nodata: config.custom_nodata,
            bbox: config.bbox,
            sampling: config.sampling.clone(),
            resolution_shards,
            completed_buffer: VecDeque::with_capacity(2048),
            is_finished: false,
            current_lat_horizon: f64::INFINITY,
            profile_stats: [0; 4],
            processed_chunk_count: 0,
            compact,
            pending_compact,
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

    /// Push an accumulated cell into completed buffer or hierarchical compaction cache
    fn push_record(&mut self, res_u8: u8, cell_u64: u64, acc: K::Accumulator) {
        if !self.kernel.passes_filter(&acc) {
            return;
        }

        if self.compact {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                if let Some(parent_res) = cell.resolution().pred() {
                    if let Some(parent) = cell.parent(parent_res) {
                        let parent_u64: u64 = parent.into();
                        let entry = self.pending_compact.entry(parent_u64).or_insert_with(|| {
                            (self.kernel.new_parent_accumulator(), Vec::with_capacity(7))
                        });
                        entry.0.merge(&acc);
                        entry.1.push((cell_u64, acc));

                        if entry.1.len() == 7 {
                            let (parent_acc, _) = self.pending_compact.remove(&parent_u64).unwrap();
                            let p_res_u8: u8 = parent_res.into();
                            self.completed_buffer.push_back(self.kernel.make_record(
                                p_res_u8,
                                parent_u64,
                                parent_acc,
                            ));
                            return;
                        }
                        return;
                    }
                }
            }
        }

        self.completed_buffer.push_back(self.kernel.make_record(
            res_u8,
            cell_u64,
            acc,
        ));
    }

    /// Flush remaining pending compaction cells at stream termination
    fn flush_pending_compact(&mut self) {
        for (_, (_, children)) in self.pending_compact.drain() {
            for (cell_u64, acc) in children {
                let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                    cell.resolution().into()
                } else {
                    8
                };
                self.completed_buffer.push_back(self.kernel.make_record(
                    res_u8,
                    cell_u64,
                    acc,
                ));
            }
        }
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            let res_u8 = self.resolution_u8s[res_idx];
            let newly_evicted = self.resolution_shards[res_idx].evict_completed(lat_horizon);
            for (cell_u64, acc) in newly_evicted {
                self.push_record(res_u8, cell_u64, acc);
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
                        self.completed_buffer.push_back(self.kernel.make_record(
                            res_u8,
                            cell_u64,
                            acc,
                        ));
                    }
                }
            }
        }
    }

    /// Advance scanline horizon until at least `min_rows` completed records are available or finished
    pub fn advance_until_completed(&mut self, min_rows: usize) {
        let batch_size = (rayon::current_num_threads() * 8).clamp(64, 256);
        let min_batch = (rayon::current_num_threads() * 2).clamp(16, 64);
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
                    let remaining = self.resolution_shards[res_idx].drain_all();
                    for (cell_u64, acc) in remaining {
                        self.push_record(res_u8, cell_u64, acc);
                    }
                }
                self.flush_pending_compact();
                break;
            }

            let resolutions = &self.resolutions;
            let sampling = &self.sampling;
            let bbox = self.bbox;
            let mosaic = Arc::clone(&self.mosaic);
            let user_nodata = self.nodata;
            let kernel = &self.kernel;

            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(Vec<[Vec<(u64, K::Accumulator)>; NUM_SHARDS]>, DecodingResult)> = chunk_items
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

                                let has_data = kernel.process_chunk(
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
                                    overlap_ctx,
                                    local_maps,
                                );

                                let mut chunk_shards = Vec::with_capacity(if has_data { local_maps.len() } else { 0 });
                                if has_data {
                                    for m in local_maps.iter_mut() {
                                        let mut shards: [Vec<(u64, K::Accumulator)>; NUM_SHARDS] =
                                            std::array::from_fn(|_| Vec::new());
                                        for (cell_u64, acc) in m.drain() {
                                            let s = get_shard(cell_u64);
                                            shards[s].push((cell_u64, acc));
                                        }
                                        chunk_shards.push(shards);
                                    }
                                }
                                (chunk_shards, std::mem::replace(decoding_result, DecodingResult::U8(Vec::new())))
                            }
                            Err(_) => (Vec::new(), DecodingResult::U8(Vec::new())),
                        }
                    },
                )
                .collect();
            self.profile_stats[1] += t1.elapsed().as_nanos() as u64;

            let t2 = std::time::Instant::now();
            self.processed_chunk_count += chunk_items.len();

            let num_res = self.resolutions.len();
            for res_idx in 0..num_res {
                self.resolution_shards[res_idx].merge_thread_results(&parallel_results, res_idx);
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
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<K::Record> {
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
        F: FnMut(usize, K::Record),
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
        self.resolution_shards
            .iter()
            .map(|s| s.active_cell_count())
            .sum()
    }

    /// Check if stream is fully drained and finished
    pub fn is_finished(&self) -> bool {
        self.is_finished && self.completed_buffer.is_empty()
    }
}
