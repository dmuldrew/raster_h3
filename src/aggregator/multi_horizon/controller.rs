//! Generic multi-resolution streaming controller.
//!
//! Bounded worker windows merge into resolution maps. Conservative chunk/cell
//! bounds permit early finalization; memory pressure creates sorted partial runs.
//! Once spilled, a resolution is finalized only after external merging at EOF.
//! Output and sorted sibling compaction are incremental. Failures latch and drop
//! temporary files. The aggregation budget excludes decoded chunks and metadata.

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
use super::sharded_map::{AccumulatorMerge, ShardedResolutionMap};
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
    compactor: HierarchicalCompactor<K>,
    output_buffer: OutputBuffer<K::Record>,
    lifecycle: StreamLifecycle,
    pub current_lat_horizon: f64,
    pub can_evict_early: bool,
    pub suffix_max_north_lat: Vec<f64>,
    pub profile_stats: [u64; 4],
    pub processed_chunk_count: usize,
    spill_runs: Vec<SpillRuns<K::Accumulator>>,
    final_reader: Option<FinalCells<K::Accumulator>>,
    eviction_horizon: f64,
    final_resolution: usize,
    finishing: bool,
    compaction_parent: Option<u64>,
    map_budget: usize,
    accumulator_limit: usize,
    worker_pixels: usize,
    worker_tasks: usize,
    output_byte_limit: usize,
    peak_active_bytes: usize,
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
        // become certifiable. Compaction uses sorted EOF groups, not parent geometry.
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
        let can_evict_early = !should_compact && suffix_max_north_lat.iter().any(|v| v.is_finite());
        for map in &mut resolution_shards {
            map.set_eviction_enabled(can_evict_early);
        }
        // Leave room for sorted-run construction, merge heads, and bounded output.
        let map_budget = config.aggregation_budget_bytes / num_res / 2;
        let accumulator_limit = config.aggregation_budget_bytes / num_res / 8;
        let spill_runs = (0..num_res)
            .map(|_| SpillRuns::new(accumulator_limit, config.spill_directory.as_deref()))
            .collect();
        let worker_tasks = rayon::current_num_threads().clamp(1, 8);
        let worker_pixels = (config.aggregation_budget_bytes
            / 8
            / worker_tasks
            / num_res
            / config.sampling.points.len()
            / 1024)
            .clamp(1, 1024);
        let output_byte_limit = config.aggregation_budget_bytes / 4;

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
            output_buffer: OutputBuffer::with_capacity(16),
            lifecycle: StreamLifecycle::new(),
            current_lat_horizon: f64::INFINITY,
            can_evict_early,
            suffix_max_north_lat,
            profile_stats: [0; 4],
            processed_chunk_count: 0,
            spill_runs,
            final_reader: None,
            eviction_horizon: f64::INFINITY,
            final_resolution: 0,
            finishing: false,
            compaction_parent: None,
            map_budget,
            accumulator_limit,
            worker_pixels,
            worker_tasks,
            output_byte_limit,
            peak_active_bytes: 0,
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

    fn merge_window(
        &mut self,
        maps: Vec<HashMap<u64, K::Accumulator, FxBuildHasher>>,
    ) -> Result<()> {
        for (i, map) in maps.into_iter().enumerate() {
            for (key, acc) in map {
                if acc.memory_bytes() > self.accumulator_limit {
                    return Err(RasterH3Error::InvalidParameter("single accumulator exceeds aggregation budget; increase aggregation_budget_bytes".into()));
                }
                if self.resolution_shards[i].merge_owned(key, acc) > self.accumulator_limit {
                    return Err(RasterH3Error::InvalidParameter("merged accumulator exceeds aggregation budget; increase aggregation_budget_bytes".into()));
                }
                self.peak_active_bytes = self.peak_active_bytes.max(
                    self.resolution_shards
                        .iter()
                        .map(|s| s.estimated_bytes())
                        .sum(),
                );
                if self.resolution_shards[i].estimated_bytes() >= self.map_budget {
                    self.spill_resolution(i)?;
                }
            }
        }
        Ok(())
    }

    /// Budgeted scratch windows prevent a single huge TIFF strip from producing
    /// an unbounded worker map. TIFF decoding itself still owns whole chunks.
    fn process_bounded_chunk(
        &mut self,
        tile_idx: usize,
        chunk: RasterChunk,
        data: &DecodingResult,
        has_overlap: bool,
    ) -> Result<()> {
        let mosaic = Arc::clone(&self.mosaic);
        let tile = &mosaic.tiles[tile_idx];
        let spp = tile.reader.metadata.samples_per_pixel.max(1) as usize;
        let stride = tile.reader.chunk_layout.chunk_width as usize;
        let width = chunk.width as usize;
        if width == 0 {
            return Ok(());
        }
        let windows_per_row = width.div_ceil(self.worker_pixels);
        let total = windows_per_row * chunk.height as usize;
        for first in (0..total).step_by(self.worker_tasks) {
            let t1 = std::time::Instant::now();
            let process_window = |j: usize| {
                let row = j / windows_per_row;
                let col = (j % windows_per_row) * self.worker_pixels;
                let n = self.worker_pixels.min(width - col);
                let start = (row * stride + col) * spp;
                let mut window_data = copy_window(data, start, n * spp)?;
                let window = RasterChunk {
                    col_offset: chunk.col_offset + col as u32,
                    row_offset: chunk.row_offset + row as u32,
                    width: n as u32,
                    height: 1,
                };
                let mut maps: Vec<_> = self
                    .resolutions
                    .iter()
                    .map(|_| HashMap::with_hasher(FxBuildHasher::default()))
                    .collect();
                self.kernel.process_chunk(
                    &window,
                    &mut window_data,
                    &self.resolutions,
                    &tile.crs_transformer,
                    &tile.reader.metadata.geotransform,
                    &self.sampling,
                    self.bbox,
                    n as u32,
                    self.nodata.or(tile.reader.metadata.nodata),
                    spp as u16,
                    if has_overlap {
                        Some((tile_idx, &*mosaic))
                    } else {
                        None
                    },
                    &mut maps,
                );
                Ok(maps)
            };
            let range = first..(first + self.worker_tasks).min(total);
            // A caller may hold a streamer mutex inside a saturated Rayon pool.
            // Nested work-stealing can then steal another caller that waits for
            // that same mutex. Process locally in that case to avoid deadlock.
            let results: Vec<Result<_>> = if rayon::current_thread_index().is_some() {
                range.map(process_window).collect()
            } else {
                range.into_par_iter().map(process_window).collect()
            };
            self.profile_stats[1] += t1.elapsed().as_nanos() as u64;
            let t2 = std::time::Instant::now();
            // Rayon has joined the entire wave before any merge/eviction.
            for result in results {
                self.merge_window(result?)?;
            }
            self.profile_stats[2] += t2.elapsed().as_nanos() as u64;
        }
        Ok(())
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
                if let Some(reader) = &mut self.final_reader {
                    if let Some((key, acc)) = reader.next_record()? {
                        self.emit_final(self.final_resolution - 1, key, acc);
                        continue;
                    }
                    self.final_reader = None;
                    self.compactor
                        .flush_all(&self.kernel, &mut self.output_buffer);
                    self.compaction_parent = None;
                    continue;
                }
                if self.final_resolution < self.resolutions.len() {
                    let i = self.final_resolution;
                    self.final_reader = if self.spill_runs[i].has_spilled() {
                        self.spill_resolution(i)?;
                        self.spill_runs[i].finish()?.map(FinalCells::Disk)
                    } else {
                        Some(FinalCells::Memory(
                            self.resolution_shards[i].take_sorted().into_iter(),
                        ))
                    };
                    self.final_resolution += 1;
                    continue;
                }
                self.current_lat_horizon = f64::NEG_INFINITY;
                self.lifecycle.mark_finished();
                break;
            }

            // Never finalize a cell that may have an earlier partial state on disk.
            let mut emitted = false;
            if self.can_evict_early {
                for i in 0..self.resolutions.len() {
                    if !self.spill_runs[i].has_spilled() {
                        if let Some((key, acc)) =
                            self.resolution_shards[i].pop_completed(self.eviction_horizon)
                        {
                            self.emit_final(i, key, acc);
                            emitted = true;
                            break;
                        }
                    }
                }
            }
            if emitted {
                continue;
            }
            if self.output_buffer.is_empty()
                && !self.spill_runs.iter().any(|r| r.has_spilled())
                && !self.compactor.is_enabled()
            {
                self.current_lat_horizon = self.eviction_horizon;
            }

            let t0 = std::time::Instant::now();
            let mut items = Vec::with_capacity(1);
            if let Some(prefetcher) = &self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut items, 1, 1);
            }
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;
            if items.is_empty() {
                if self.processed_chunk_count != self.mosaic.chunk_refs.len() {
                    return Err(RasterH3Error::StreamFailed(format!(
                        "Prefetch ended after {} of {} chunks",
                        self.processed_chunk_count,
                        self.mosaic.chunk_refs.len()
                    )));
                }
                self.prefetcher.take();
                self.finishing = true;
                // Keep the published horizon conservative until deferred output
                // is consumed; downstream tilers must not flush ahead of it.
                continue;
            }
            for item in items {
                let (tile_idx, _, bounds, data, overlap) = item?;
                self.process_bounded_chunk(tile_idx, bounds, &data, overlap)?;
                self.processed_chunk_count += 1;
                if let Some(prefetcher) = &self.prefetcher {
                    prefetcher.recycle_batch(vec![data]);
                }
            }
            // Every sample of the current chunk has been merged or persisted.
            // Spilling may defer older northern cells, so do not publish a new
            // horizon downstream after any resolution has spilled.
            if !self.spill_runs.iter().any(|r| r.has_spilled()) && !self.compactor.is_enabled() {
                self.eviction_horizon = self.suffix_max_north_lat[self.processed_chunk_count];
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

/// Copy a bounded contiguous sample window, retaining the native TIFF type.
fn copy_window(data: &DecodingResult, start: usize, len: usize) -> Result<DecodingResult> {
    macro_rules! copy { ($($variant:ident),*) => { match data { $(DecodingResult::$variant(values) => {
        let end = start.checked_add(len).ok_or_else(|| RasterH3Error::InvalidMetadata("sample window overflow".into()))?;
        let values = values.get(start..end).ok_or_else(|| RasterH3Error::InvalidMetadata("decoded chunk shorter than its declared dimensions".into()))?;
        Ok(DecodingResult::$variant(values.to_vec()))
    }),* } }; }
    copy!(U8, U16, U32, U64, I8, I16, I32, I64, F32, F64)
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
