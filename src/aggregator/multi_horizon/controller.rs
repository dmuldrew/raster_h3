//! Generic Multi-Resolution Scanline Horizon Streamer Controller
//!
//! # Architecture & Responsibilities
//! - **Chunk Prefetch & Dispatch**: Coordinates chunk draining from background prefetchers
//!   and Rayon multi-core parallel chunk-row execution.
//! - **Shard Aggregation**: 32-way partitioned lock-free maps for accumulating cell values.
//! - **Horizon Progression**: Tracks the southernmost latitude reached by scanlines.
//! - **Eviction & Compaction**: Delegates record buffering and lifecycle to [`OutputBuffer`]
//!   and [`StreamLifecycle`], and 7-cell compaction to [`HierarchicalCompactor`].
//!
//! # Invariants for Horizon Eviction and Compaction Ordering
//! 1. **Sharded Eviction**: As the scanline horizon advances southwards (`lat_horizon`),
//!    sharded maps identify all cells whose northern extent lies entirely north of `lat_horizon`.
//!    Because chunks are ordered strictly north-to-south, no subsequent chunk can ever contribute
//!    pixels to these cells. They are evicted from the active shard maps.
//! 2. **Compaction Ingestion**: Evicted cells are fed into [`HierarchicalCompactor`]. If all 7
//!    aperture-7 children for a parent cell arrive, the parent accumulator is merged and emitted
//!    at resolution `R - 1`.
//! 3. **Compactor Horizon Eviction**: Pending parents whose southernmost latitude (`compute_cell_south_lat`)
//!    is strictly north of `lat_horizon` are evicted. Because no further chunks can reach any child
//!    within that parent's footprint, incomplete parents (< 7 children) cannot receive more children.
//!    They are decomposed back into child records and emitted into [`OutputBuffer`].
//! 4. **Latched Failures**: Any error encountered during chunk prefetching, decoding, or
//!    aggregation is latched in [`StreamLifecycle`]. The error state is irreversible, ensuring
//!    downstream consumers never mistake a failure for EOF.

use fxhash::FxBuildHasher;
use h3o::Resolution;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use tiff::decoder::DecodingResult;

use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::PrefetchedMosaicReader;
use crate::raster::RasterChunk;

use super::compaction::HierarchicalCompactor;
use super::config::MultiResolutionConfig;
use super::lifecycle::{OutputBuffer, StreamLifecycle};
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
    #[allow(clippy::too_many_arguments)]
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

/// Unified abstraction for streaming raster aggregators producing completed records.
///
/// Serves as the standard record source interface for serialization sinks
/// (e.g. Parquet exporters, PMTiles tilers, DuckDB table functions).
pub trait RecordStreamer {
    type Record;

    /// Drain up to `max_rows` completed records directly into a consumer closure
    fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> Result<usize>
    where
        F: FnMut(usize, Self::Record);

    /// Current southernmost latitude reached by the scanline horizon
    fn current_lat_horizon(&self) -> f64;

    /// Check if the stream has finished processing all raster chunks and drained all records
    fn is_finished(&self) -> bool;

    /// Target H3 resolution levels
    fn resolution_u8s(&self) -> &[u8];

    /// Optional spatial bounding box in WGS84 [min_lon, min_lat, max_lon, max_lat]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        None
    }
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
    compactor: HierarchicalCompactor<K>,
    output_buffer: OutputBuffer<K::Record>,
    lifecycle: StreamLifecycle,
    pub current_lat_horizon: f64,
    pub profile_stats: [u64; 4],
    pub processed_chunk_count: usize,
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
        config.validate()?;
        let should_compact = config.should_compact_h3_children();

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
        let compactor = HierarchicalCompactor::new(should_compact);

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
            compactor,
            output_buffer: OutputBuffer::with_capacity(2048),
            lifecycle: StreamLifecycle::new(),
            current_lat_horizon: f64::INFINITY,
            profile_stats: [0; 4],
            processed_chunk_count: 0,
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

    /// Whether hierarchical child-to-parent compaction is active
    pub fn is_compact_enabled(&self) -> bool {
        self.compactor.is_enabled()
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            let res_u8 = self.resolution_u8s[res_idx];
            let newly_evicted = self.resolution_shards[res_idx].evict_completed(lat_horizon);
            for (cell_u64, acc) in newly_evicted {
                self.compactor.push_cell(
                    &self.kernel,
                    res_u8,
                    cell_u64,
                    acc,
                    &mut self.output_buffer,
                );
            }
        }

        self.compactor
            .evict_above_horizon(&self.kernel, lat_horizon, &mut self.output_buffer);
    }

    /// Advance scanline horizon until at least `min_rows` completed records are available or finished
    #[allow(clippy::type_complexity)]
    pub fn advance_until_completed(&mut self, min_rows: usize) -> Result<()> {
        if let Some(reason) = self.lifecycle.failure_reason() {
            return Err(RasterH3Error::StreamFailed(reason.to_string()));
        }
        if self.lifecycle.is_finished() {
            return Ok(());
        }

        let batch_size = (rayon::current_num_threads() * 8).clamp(64, 256);
        let min_batch = (rayon::current_num_threads() * 2).clamp(16, 64);
        let mut chunk_items = Vec::with_capacity(batch_size);

        while self.output_buffer.len() < min_rows && !self.lifecycle.is_finished() {
            chunk_items.clear();
            let t0 = std::time::Instant::now();
            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut chunk_items, min_batch, batch_size);
            }
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;

            // Reject the whole batch before merging or emitting any of its records.
            if let Some(error) = chunk_items.iter().find_map(|item| item.as_ref().err()) {
                return Err(self.fail(error.to_string()));
            }

            if chunk_items.is_empty() {
                if self.processed_chunk_count != self.mosaic.chunk_refs.len() {
                    return Err(self.fail(format!(
                        "Prefetch ended after {} of {} chunks",
                        self.processed_chunk_count,
                        self.mosaic.chunk_refs.len()
                    )));
                }
                self.lifecycle.mark_finished();
                self.current_lat_horizon = f64::NEG_INFINITY;
                let num_res = self.resolutions.len();
                for res_idx in 0..num_res {
                    let res_u8 = self.resolution_u8s[res_idx];
                    let remaining = self.resolution_shards[res_idx].drain_all();
                    for (cell_u64, acc) in remaining {
                        self.compactor.push_cell(
                            &self.kernel,
                            res_u8,
                            cell_u64,
                            acc,
                            &mut self.output_buffer,
                        );
                    }
                }
                self.compactor
                    .flush_all(&self.kernel, &mut self.output_buffer);
                break;
            }

            let resolutions = &self.resolutions;
            let sampling = &self.sampling;
            let bbox = self.bbox;
            let mosaic = Arc::clone(&self.mosaic);
            let user_nodata = self.nodata;
            let kernel = &self.kernel;

            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(
                Vec<[Vec<(u64, K::Accumulator)>; NUM_SHARDS]>,
                DecodingResult,
            )> = chunk_items
                .par_iter_mut()
                .map_init(
                    || {
                        let mut maps = Vec::with_capacity(resolutions.len());
                        for _ in 0..resolutions.len() {
                            maps.push(HashMap::with_capacity_and_hasher(
                                128,
                                FxBuildHasher::default(),
                            ));
                        }
                        maps
                    },
                    |local_maps, item| match item {
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

                            let mut chunk_shards =
                                Vec::with_capacity(if has_data { local_maps.len() } else { 0 });
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
                            (
                                chunk_shards,
                                std::mem::replace(decoding_result, DecodingResult::U8(Vec::new())),
                            )
                        }
                        Err(_) => unreachable!("chunk errors were checked before dispatch"),
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

            let recycled_buffers: Vec<DecodingResult> =
                parallel_results.into_iter().map(|(_, dec)| dec).collect();

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
        Ok(())
    }

    /// Latch failures so subsequent reads cannot mistake a failed stream for EOF.
    fn fail(&mut self, reason: String) -> RasterH3Error {
        self.prefetcher.take();
        self.output_buffer.clear();
        self.compactor.clear();
        self.resolution_shards.clear();
        self.lifecycle.latch_failure(reason)
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Result<Vec<K::Record>> {
        self.advance_until_completed(max_rows)?;
        Ok(self.output_buffer.take_batch(max_rows))
    }

    /// Drain up to `max_rows` completed records directly into a closure with zero heap allocation
    pub fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> Result<usize>
    where
        F: FnMut(usize, K::Record),
    {
        self.advance_until_completed(max_rows)?;
        Ok(self.output_buffer.drain_into(max_rows, consumer))
    }

    /// Return total active in-flight cells across all resolutions
    pub fn active_cell_count(&self) -> usize {
        let shard_count: usize = self
            .resolution_shards
            .iter()
            .map(|s| s.active_cell_count())
            .sum();
        shard_count + self.compactor.len()
    }

    /// Check if stream is fully drained and finished
    pub fn is_finished(&self) -> bool {
        self.lifecycle.is_finished() && self.output_buffer.is_empty()
    }
}

impl<K: HorizonStreamKernel> RecordStreamer for MultiHorizonStreamer<K> {
    type Record = K::Record;

    #[inline(always)]
    fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> Result<usize>
    where
        F: FnMut(usize, Self::Record),
    {
        self.drain_completed_into(max_rows, consumer)
    }

    #[inline(always)]
    fn current_lat_horizon(&self) -> f64 {
        self.current_lat_horizon
    }

    #[inline(always)]
    fn is_finished(&self) -> bool {
        self.is_finished()
    }

    #[inline(always)]
    fn resolution_u8s(&self) -> &[u8] {
        &self.resolution_u8s
    }

    #[inline(always)]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        Some(self.mosaic.mosaic_bounds_wgs84)
    }
}
