//! Generic multi-resolution streaming controller.
//!
//! Coordinates batched Rayon chunk decoding, parallel 32-shard aggregation,
//! conservative geodetic horizon eviction, aperture-7 child-to-parent compaction,
//! and on-disk spill runs for memory bounding.

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
use super::coordinates::eviction_north_bound;
use super::lifecycle::{OutputBuffer, StreamLifecycle};
use super::sharded_map::{get_shard, AccumulatorMerge, ShardedResolutionMap, NUM_SHARDS};
use super::spill::{RunReader, SpillAccumulator, SpillRuns};

/// Kernel trait parameterizing data type-specific chunk processing, aggregation, and filtering
pub trait HorizonStreamKernel: Send + Sync + 'static {
    type Accumulator: SpillAccumulator + Default + 'static;
    type Record: Send + 'static;

    /// Create an accumulator for parent cell during 7-cell compaction
    fn new_parent_accumulator(&self) -> Self::Accumulator;

    /// Check if accumulator passes user filter thresholds
    fn passes_filter(&self, acc: &Self::Accumulator) -> bool;

    /// Construct an output record from cell index, resolution, and accumulator
    fn make_record(&self, resolution: u8, cell_u64: u64, acc: Self::Accumulator) -> Self::Record;

    /// Construct and account for an output record's owned accumulator state.
    fn buffer_record(
        &self,
        resolution: u8,
        key: u64,
        acc: Self::Accumulator,
        output: &mut OutputBuffer<Self::Record>,
    ) {
        let bytes = std::mem::size_of::<Self::Record>() + acc.heap_bytes();
        output.push_sized(self.make_record(resolution, key, acc), bytes);
    }

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
    pub compactor: HierarchicalCompactor<K>,
    pub output_buffer: OutputBuffer<K::Record>,
    pub lifecycle: StreamLifecycle,
    pub current_lat_horizon: f64,
    pub can_evict_early: bool,
    pub suffix_max_north_lat: Vec<f64>,
    pub profile_stats: [u64; 4],
    pub processed_chunk_count: usize,
    spill_runs: Vec<SpillRuns<K::Accumulator>>,
    final_reader: Option<FinalCells<K::Accumulator>>,
    final_resolution: usize,
    finishing: bool,
    compaction_parent: Option<u64>,
    map_budget: usize,
    output_byte_limit: usize,
    peak_active_bytes: usize,
    batch_size: usize,
    min_batch: usize,
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

        let prefetcher = PrefetchedMosaicReader::spawn_with_workers(
            Arc::clone(&mosaic),
            config.prefetch_chunks,
            config.decode_workers,
        );
        let num_res = resolutions.len();
        let mut resolution_shards: Vec<_> =
            (0..num_res).map(|_| ShardedResolutionMap::new()).collect();
        let compactor = HierarchicalCompactor::new(should_compact);

        // Bounds cover whole chunks and all subpixel offsets. Unknown projected
        // chunks get +infinity; once they are processed a later suffix can still
        // become certifiable.
        let n_chunks = mosaic.chunk_refs.len();
        let mut suffix_max_north_lat = vec![f64::NEG_INFINITY; n_chunks + 1];
        for k in (0..n_chunks).rev() {
            let reference = mosaic.chunk_refs[k];
            let tile = &mosaic.tiles[reference.tile_idx];
            let chunk = tile.reader.chunk_layout.get_chunk_bounds(
                reference.chunk_idx,
                tile.reader.metadata.width,
                tile.reader.metadata.height,
            );
            let north = eviction_north_bound(
                &chunk,
                &tile.reader.metadata.geotransform,
                &tile.crs_transformer,
            );
            suffix_max_north_lat[k] = suffix_max_north_lat[k + 1].max(north);
        }
        let can_evict_early =
            suffix_max_north_lat.iter().any(|v| v.is_finite()) && !config.compact_h3_children;
        for map in &mut resolution_shards {
            map.set_eviction_enabled(can_evict_early);
        }

        // Leave room for sorted-run construction, merge heads, and bounded output.
        let map_budget = config.aggregation_budget_bytes / num_res / 2;
        let accumulator_limit = config.aggregation_budget_bytes / num_res / 8;
        let spill_runs = (0..num_res)
            .map(|_| SpillRuns::new(accumulator_limit, config.spill_directory.as_deref()))
            .collect();
        let output_byte_limit = config.aggregation_budget_bytes / 4;

        // Scale batch size with budget and dataset size:
        // For small rasters (few chunks) or tight budgets, use bounded batches so
        // incremental eviction can yield records before EOF without swallowing the entire file.
        // For large rasters and budgets, scale with Rayon worker threads.
        let batch_size = ((config.aggregation_budget_bytes / (num_res * 64 * 1024))
            .min((rayon::current_num_threads() * 8).clamp(16, 64)))
        .min((n_chunks / 4).max(1))
        .max(1);
        let min_batch = (batch_size / 2).max(1);

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
            can_evict_early,
            suffix_max_north_lat,
            profile_stats: [0; 4],
            processed_chunk_count: 0,
            spill_runs,
            final_reader: None,
            final_resolution: 0,
            finishing: false,
            compaction_parent: None,
            map_budget,
            output_byte_limit,
            peak_active_bytes: 0,
            batch_size,
            min_batch,
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

    /// Emit one final cell. In sorted EOF order siblings are contiguous, so
    /// compaction retains at most one parent's children, including pentagons.
    fn emit_final(&mut self, res_idx: usize, key: u64, acc: K::Accumulator) {
        if self.compactor.is_enabled() {
            let parent = h3o::CellIndex::try_from(key)
                .ok()
                .and_then(|cell| cell.resolution().pred().and_then(|r| cell.parent(r)))
                .map(u64::from);
            if parent != self.compaction_parent {
                self.compactor
                    .flush_all(&self.kernel, &mut self.output_buffer);
                self.compaction_parent = parent;
            }
        }
        self.compactor.push_cell(
            &self.kernel,
            self.resolution_u8s[res_idx],
            key,
            acc,
            &mut self.output_buffer,
        );
    }

    fn spill_resolution(&mut self, index: usize) -> Result<()> {
        let records = self.resolution_shards[index].take_sorted();
        if !records.is_empty() {
            self.spill_runs[index].push_sorted(records)?;
            self.resolution_shards[index].set_eviction_enabled(false);
        }
        Ok(())
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            if !self.spill_runs[res_idx].has_spilled() {
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
        }

        self.compactor
            .evict_above_horizon(&self.kernel, lat_horizon, &mut self.output_buffer);
    }

    /// Advance until a bounded batch of completed records is available.
    pub fn advance_until_completed(&mut self, min_rows: usize) -> Result<()> {
        if let Some(reason) = self.lifecycle.failure_reason() {
            return Err(RasterH3Error::StreamFailed(reason.to_string()));
        }
        let result = self.advance_inner(min_rows.min(2048));
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(self.fail(e.to_string())),
        }
    }

    fn advance_inner(&mut self, min_rows: usize) -> Result<()> {
        while self.output_buffer.len() < min_rows
            && !self.lifecycle.is_finished()
            && (self.output_buffer.is_empty()
                || self.output_buffer.estimated_bytes() < self.output_byte_limit)
        {
            if self.finishing {
                if let Some(mut reader) = self.final_reader.take() {
                    let mut reader_done = false;
                    while self.output_buffer.len() < min_rows
                        && self.output_buffer.estimated_bytes() < self.output_byte_limit
                    {
                        if let Some((key, acc)) = reader.next_record()? {
                            self.emit_final(self.final_resolution - 1, key, acc);
                        } else {
                            reader_done = true;
                            self.compactor
                                .flush_all(&self.kernel, &mut self.output_buffer);
                            self.compaction_parent = None;
                            break;
                        }
                    }
                    if !reader_done {
                        self.final_reader = Some(reader);
                    }
                    if self.output_buffer.len() >= min_rows || self.final_reader.is_some() {
                        continue;
                    }
                }
                if self.final_resolution < self.resolutions.len() {
                    let i = self.final_resolution;
                    self.final_resolution += 1;
                    self.final_reader = if self.spill_runs[i].has_spilled() {
                        self.spill_resolution(i)?;
                        self.spill_runs[i].finish()?.map(FinalCells::Disk)
                    } else {
                        Some(FinalCells::Memory(
                            self.resolution_shards[i].take_sorted().into_iter(),
                        ))
                    };
                    continue;
                }
                self.compactor
                    .flush_all(&self.kernel, &mut self.output_buffer);
                self.current_lat_horizon = f64::NEG_INFINITY;
                self.lifecycle.mark_finished();
                break;
            }

            let t0 = std::time::Instant::now();
            let mut chunk_items = Vec::with_capacity(self.batch_size);
            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut chunk_items, self.min_batch, self.batch_size);
            }
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;

            if let Some(error) = chunk_items.iter().find_map(|item| item.as_ref().err()) {
                return Err(RasterH3Error::StreamFailed(error.to_string()));
            }

            if chunk_items.is_empty() {
                if self.processed_chunk_count != self.mosaic.chunk_refs.len() {
                    return Err(RasterH3Error::StreamFailed(format!(
                        "Prefetch ended after {} of {} chunks",
                        self.processed_chunk_count,
                        self.mosaic.chunk_refs.len()
                    )));
                }
                self.prefetcher.take();
                self.finishing = true;
                continue;
            }

            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(
                Vec<[Vec<(u64, K::Accumulator)>; NUM_SHARDS]>,
                DecodingResult,
            )> = {
                let resolutions = &self.resolutions;
                let sampling = &self.sampling;
                let bbox = self.bbox;
                let mosaic = Arc::clone(&self.mosaic);
                let user_nodata = self.nodata;
                let kernel = &self.kernel;

                chunk_items
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
                    .collect()
            };
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

            if self.can_evict_early && self.processed_chunk_count < self.mosaic.chunk_refs.len() {
                let safe_lat = self.suffix_max_north_lat[self.processed_chunk_count];
                if safe_lat < self.current_lat_horizon {
                    let t3 = std::time::Instant::now();
                    self.current_lat_horizon = safe_lat;
                    self.evict_completed(safe_lat);
                    self.profile_stats[3] += t3.elapsed().as_nanos() as u64;
                }
            }

            let total_active_bytes: usize = self
                .resolution_shards
                .iter()
                .map(|s| s.estimated_bytes())
                .sum();
            self.peak_active_bytes = self.peak_active_bytes.max(total_active_bytes);

            for i in 0..num_res {
                if self.resolution_shards[i].estimated_bytes() >= self.map_budget {
                    self.spill_resolution(i)?;
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
        self.spill_runs.clear();
        self.final_reader = None;
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

    /// Number of partial sorted runs written, excluding intermediate merge files.
    pub fn spill_run_count(&self) -> u64 {
        self.spill_runs.iter().map(|r| r.runs_written).sum()
    }

    /// Peak estimate of active table/heap allocation; excludes decoder and output memory.
    pub fn peak_active_bytes(&self) -> usize {
        self.peak_active_bytes
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

enum FinalCells<A> {
    Memory(std::vec::IntoIter<(u64, A)>),
    Disk(RunReader<A>),
}
impl<A: SpillAccumulator> FinalCells<A> {
    fn next_record(&mut self) -> std::io::Result<Option<(u64, A)>> {
        match self {
            Self::Memory(iter) => Ok(iter.next()),
            Self::Disk(reader) => reader.next_record(),
        }
    }
}
