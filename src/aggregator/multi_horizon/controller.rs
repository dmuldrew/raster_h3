//! Generic multi-resolution streaming controller.
//!
//! Bounded worker windows merge into resolution maps. Conservative chunk/cell
//! bounds permit early finalization; memory pressure creates sorted partial runs.
//! Once spilled, a resolution is finalized only after external merging at EOF.
//! Output and sibling compaction are incremental: compaction groups complete
//! during streaming or at EOF in sorted order. Failures latch and drop
//! temporary files. The aggregation budget excludes decoded chunks and metadata.

use fxhash::FxBuildHasher;
use h3o::{CellIndex, Resolution};
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use tiff::decoder::DecodingResult;

use crate::aggregator::sampling::SamplingPattern;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::PrefetchedMosaicReader;
use crate::raster::RasterChunk;

use super::compaction::HierarchicalCompactor;
use super::config::MultiResolutionConfig;
use super::coordinates::eviction_north_bound;
use super::lifecycle::{OutputBuffer, StreamLifecycle};
use super::sharded_map::{AccumulatorMerge, ShardedResolutionMap};
use super::spill::{RunReader, SpillAccumulator, SpillRuns};
use super::walker::WalkContext;

/// One worker window: (tile index, window bounds, borrowed samples, row stride, overlaps another tile).
type WindowJob<'a> = (
    usize,
    RasterChunk,
    super::borrowed::BorrowedSamples<'a>,
    u32,
    bool,
);
/// One worker's per-resolution cell maps.
type CellMaps<A> = Vec<HashMap<u64, A, FxBuildHasher>>;

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

    /// Aggregate one decoded window into thread-local cell maps, one per
    /// resolution. Returns false when the window held no valid data.
    fn process_chunk(
        &self,
        window: &WalkContext,
        decoding_result: &mut DecodingResult,
        nodata: Option<f64>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, FxBuildHasher>],
    ) -> bool;

    /// Borrowed window override. Built-in kernels never allocate a pixel copy.
    fn process_window(
        &self,
        window: &WalkContext,
        samples: super::borrowed::BorrowedSamples<'_>,
        nodata: Option<f64>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, FxBuildHasher>],
    ) -> bool {
        super::profile::copied(samples.bytes());
        let mut owned = samples.to_owned();
        self.process_chunk(window, &mut owned, nodata, local_maps)
    }
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

    /// Whether complete sibling groups are emitted as parent records at one
    /// resolution coarser than requested
    fn compacts_children(&self) -> bool {
        false
    }
}

/// Generic single-pass streaming aggregator across multiple H3 resolutions
pub struct MultiHorizonStreamer<K: HorizonStreamKernel> {
    kernel: K,
    prefetcher: Option<PrefetchedMosaicReader>,
    mosaic: Arc<MosaicReader>,
    resolutions: Vec<Resolution>,
    resolution_u8s: Vec<u8>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    resolution_shards: Vec<ShardedResolutionMap<K::Accumulator>>,
    compactor: HierarchicalCompactor<K>,
    output_buffer: OutputBuffer<K::Record>,
    lifecycle: StreamLifecycle,
    current_lat_horizon: f64,
    can_evict_early: bool,
    suffix_max_north_lat: Vec<f64>,
    processed_chunk_count: usize,
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
    worker_budget: usize,
    batch_size: usize,
    output_byte_limit: usize,
    peak_active_bytes: usize,
    worker_maps: Vec<CellMaps<K::Accumulator>>,
    metrics: super::profile::StreamProfile,
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
        let can_evict_early = suffix_max_north_lat.iter().any(|v| v.is_finite());
        for map in &mut resolution_shards {
            map.set_eviction_enabled(can_evict_early);
        }
        // Leave room for sorted-run construction, merge heads, and bounded output.
        let map_budget = config.aggregation_budget_bytes / num_res / 4;
        let accumulator_limit = config.aggregation_budget_bytes / num_res / 8;
        let spill_runs = (0..num_res)
            .map(|_| SpillRuns::new(accumulator_limit, config.spill_directory.as_deref()))
            .collect();
        // Reserve worker storage by sample cardinality, not TIFF chunk count.
        // 1 KiB/sample covers standard table slack and quantile/category state.
        // Custom heap-owning kernels must obey the same bound or use a larger allowance.
        let per_sample = 1024usize.max(std::mem::size_of::<K::Accumulator>().saturating_mul(8));
        let per_pixel = per_sample
            .saturating_mul(num_res)
            .saturating_mul(config.sampling.points.len());
        let worker_budget = config.aggregation_budget_bytes / 4;
        if per_pixel > worker_budget {
            return Err(RasterH3Error::InvalidParameter(
                "sampling pattern exceeds worker allowance; increase aggregation_budget_bytes"
                    .into(),
            ));
        }
        let worker_tasks = rayon::current_num_threads()
            .clamp(1, 8)
            .min(worker_budget / per_pixel);
        let batch_size = (worker_tasks * 2).min((n_chunks / 4).max(1));
        let worker_pixels = (worker_budget / worker_tasks / per_pixel).clamp(1, 16384);
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
            worker_budget,
            batch_size,
            output_byte_limit,
            peak_active_bytes: 0,
            worker_maps: (0..worker_tasks)
                .map(|_| {
                    (0..num_res)
                        .map(|_| HashMap::with_hasher(FxBuildHasher::default()))
                        .collect()
                })
                .collect(),
            metrics: super::profile::StreamProfile {
                worker_map_sets: worker_tasks as u64,
                ..Default::default()
            },
        })
    }

    /// Return current southernmost latitude reached by scanline horizon
    pub fn current_lat_horizon(&self) -> f64 {
        if self.is_finished() {
            f64::NEG_INFINITY
        } else {
            self.current_lat_horizon
        }
    }

    /// Target H3 resolutions
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Target H3 resolution integer levels
    pub fn resolution_u8s(&self) -> &[u8] {
        &self.resolution_u8s
    }

    /// The tiles and latitude-ordered chunk list being streamed
    pub fn mosaic(&self) -> &MosaicReader {
        &self.mosaic
    }

    /// Whether some chunk suffix has a certified northern bound, permitting
    /// cells to be finalized before EOF
    pub fn can_evict_early(&self) -> bool {
        self.can_evict_early
    }

    /// Chunks fully merged into the resolution maps so far
    pub fn processed_chunk_count(&self) -> usize {
        self.processed_chunk_count
    }

    /// Stage timings and counters for this stream
    pub fn metrics(&self) -> &super::profile::StreamProfile {
        &self.metrics
    }

    /// Drop the prefetcher as if it ended early, for failure-path tests.
    #[doc(hidden)]
    pub fn abort_prefetch_for_testing(&mut self) {
        self.prefetcher.take();
    }

    /// Whether hierarchical child-to-parent compaction is active
    pub fn is_compact_enabled(&self) -> bool {
        self.compactor.is_enabled()
    }

    /// Emit one final cell. In sorted EOF order siblings are contiguous, so a
    /// parent's group is decomposed as soon as the stream moves past it.
    fn emit_final(&mut self, res_idx: usize, key: u64, acc: K::Accumulator) {
        if self.compactor.is_enabled() && self.finishing {
            let parent = CellIndex::try_from(key)
                .ok()
                .and_then(|cell| cell.resolution().pred().and_then(|r| cell.parent(r)))
                .map(u64::from);
            if parent != self.compaction_parent {
                if let Some(previous) = self.compaction_parent {
                    self.compactor
                        .flush_parent(previous, &self.kernel, &mut self.output_buffer);
                }
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
        let started = std::time::Instant::now();
        let records = self.resolution_shards[index].take_sorted();
        if !records.is_empty() {
            self.spill_runs[index].push_sorted(records)?;
            self.resolution_shards[index].set_eviction_enabled(false);
        }
        self.metrics.spill_ns += started.elapsed().as_nanos() as u64;
        Ok(())
    }

    fn merge_window(
        &mut self,
        maps: &mut [HashMap<u64, K::Accumulator, FxBuildHasher>],
    ) -> Result<()> {
        for (i, map) in maps.iter_mut().enumerate() {
            for (key, acc) in map.drain() {
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

    /// Schedule bounded rectangles borrowing decoded storage. When a full row
    /// fits, combine rows into one job; otherwise use horizontal row segments.
    /// Each wave owns at most worker_tasks maps, reused after owned merging.
    fn process_batch(
        &mut self,
        items: &mut [crate::raster::prefetch::MosaicPrefetchItem],
    ) -> Result<()> {
        use super::borrowed::BorrowedSamples;
        let mut maps = std::mem::take(&mut self.worker_maps);
        let result = (|| {
            let mut index = 0;
            let (mut row, mut col) = (0usize, 0usize);
            let mut jobs = Vec::with_capacity(self.worker_tasks);
            let mut results = Vec::with_capacity(self.worker_tasks);
            while index < items.len() {
                jobs.clear();
                while jobs.len() < self.worker_tasks && index < items.len() {
                    let (tile_idx, chunk_idx, chunk, data, overlap) = items[index]
                        .as_ref()
                        .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
                    let width = chunk.width as usize;
                    let height = chunk.height as usize;
                    // Do not interpret a decoder's sparse fill as observed data:
                    // the effective nodata may differ from the file's sentinel.
                    if row >= height
                        || width == 0
                        || self.mosaic.tiles[*tile_idx]
                            .reader
                            .is_sparse_chunk(*chunk_idx)
                    {
                        index += 1;
                        row = 0;
                        col = 0;
                        continue;
                    }
                    let spp = self.mosaic.tiles[*tile_idx]
                        .reader
                        .metadata
                        .samples_per_pixel
                        .max(1) as usize;
                    if row == 0 && col == 0 {
                        self.metrics.decoded_bytes += BorrowedSamples::from(data).bytes() as u64;
                    }
                    let (w, h) = if width <= self.worker_pixels {
                        (width, (self.worker_pixels / width).min(height - row))
                    } else {
                        (self.worker_pixels.min(width - col), 1)
                    };
                    let start = (row * width + col) * spp;
                    let len = ((h - 1) * width + w) * spp;
                    let samples = BorrowedSamples::from(data).window(start, len)?;
                    let bounds = RasterChunk {
                        col_offset: chunk.col_offset + col as u32,
                        row_offset: chunk.row_offset + row as u32,
                        width: w as u32,
                        height: h as u32,
                    };
                    jobs.push((*tile_idx, bounds, samples, width as u32, *overlap));
                    if w == width {
                        row += h;
                    } else {
                        col += w;
                        if col == width {
                            col = 0;
                            row += 1;
                        }
                    }
                }
                if jobs.is_empty() {
                    continue;
                }
                let t = std::time::Instant::now();
                let process = |(job, maps): (&WindowJob<'_>, &mut CellMaps<K::Accumulator>)| {
                    let (tile_idx, chunk, samples, stride, overlap) = job;
                    let tile = &self.mosaic.tiles[*tile_idx];
                    let scope = super::profile::WorkerScope::new();
                    let start = std::time::Instant::now();
                    let window = WalkContext {
                        chunk,
                        resolutions: &self.resolutions,
                        crs: &tile.crs_transformer,
                        gt: &tile.reader.metadata.geotransform,
                        sampling: &self.sampling,
                        bbox: self.bbox,
                        stride: *stride,
                        samples_per_pixel: tile.reader.metadata.samples_per_pixel,
                        owner: overlap.then_some((*tile_idx, &*self.mosaic)),
                    };
                    self.kernel.process_window(
                        &window,
                        *samples,
                        self.nodata.or(tile.reader.metadata.nodata),
                        maps,
                    );
                    (scope.snapshot(), start.elapsed().as_nanos() as u64)
                };
                results.clear();
                if rayon::current_thread_index().is_some() {
                    results.extend(jobs.iter().zip(maps.iter_mut()).map(process));
                } else {
                    jobs.par_iter()
                        .zip(maps.par_iter_mut())
                        .map(process)
                        .collect_into_vec(&mut results);
                }
                self.metrics.kernel_wall_ns += t.elapsed().as_nanos() as u64;
                for (profile, ns) in results.drain(..) {
                    self.metrics.worker.merge(profile);
                    self.metrics.worker_ns += ns;
                    self.metrics.jobs += 1;
                }
                let bytes: usize = maps
                    .iter()
                    .flatten()
                    .map(|m| {
                        super::spill::table_bytes::<u64, K::Accumulator>(m.capacity())
                            + m.values().map(|a| a.heap_bytes()).sum::<usize>()
                    })
                    .sum();
                self.metrics.peak_worker_bytes = self.metrics.peak_worker_bytes.max(bytes);
                let t = std::time::Instant::now();
                for m in &mut maps {
                    self.merge_window(m)?;
                }
                let ns = t.elapsed().as_nanos() as u64;
                self.metrics.merge_ns += ns;
                // Retained buckets are part of worker storage, not free capacity.
                if bytes > self.worker_budget {
                    for map in maps.iter_mut().flatten() {
                        map.shrink_to_fit();
                    }
                }
            }
            Ok(())
        })();
        self.worker_maps = maps;
        result
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
                    self.compactor.flush_resolution(
                        self.resolution_u8s[self.final_resolution - 1],
                        &self.kernel,
                        &mut self.output_buffer,
                    );
                    self.compaction_parent = None;
                    continue;
                }
                if self.final_resolution < self.resolutions.len() {
                    let i = self.final_resolution;
                    self.final_reader = if self.spill_runs[i].has_spilled() {
                        self.spill_resolution(i)?;
                        {
                            let start = std::time::Instant::now();
                            let reader = self.spill_runs[i].finish()?.map(FinalCells::Disk);
                            self.metrics.spill_ns += start.elapsed().as_nanos() as u64;
                            reader
                        }
                    } else {
                        Some(FinalCells::Memory(
                            self.resolution_shards[i].take_sorted().into_iter(),
                        ))
                    };
                    self.final_resolution += 1;
                    continue;
                }
                // Records still buffered here are drained before is_finished().
                self.compactor
                    .flush_all(&self.kernel, &mut self.output_buffer);
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
            if self.can_evict_early {
                // Every finalizable cell has been popped, so a group whose
                // logical children all lie north of the horizon is final.
                let buffered = self.output_buffer.len();
                let (spill_runs, resolution_u8s) = (&self.spill_runs, &self.resolution_u8s);
                self.compactor.flush_completed(
                    self.eviction_horizon,
                    |res| {
                        resolution_u8s
                            .iter()
                            .position(|&r| r == res)
                            .is_some_and(|i| !spill_runs[i].has_spilled())
                    },
                    &self.kernel,
                    &mut self.output_buffer,
                );
                if self.output_buffer.len() > buffered {
                    continue;
                }
            }
            if self.output_buffer.is_empty() && !self.spill_runs.iter().any(|r| r.has_spilled()) {
                // Held sibling groups may still emit records north of the horizon.
                self.current_lat_horizon = self
                    .eviction_horizon
                    .max(self.compactor.pending_emit_south());
            }

            let t0 = std::time::Instant::now();
            let mut items = Vec::with_capacity(self.batch_size);
            if let Some(prefetcher) = &self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut items, 1, self.batch_size);
            }
            let elapsed = t0.elapsed().as_nanos() as u64;
            self.metrics.prefetch_wait_ns += elapsed;
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
            self.process_batch(&mut items)?;
            self.processed_chunk_count += items.len();
            if let Some(prefetcher) = &self.prefetcher {
                prefetcher.recycle_batch(
                    items
                        .into_iter()
                        .filter_map(|item| item.ok().map(|(_, _, _, data, _)| data)),
                );
            }
            // Every sample of the current chunk has been merged or persisted.
            // Spilling may defer older northern cells, so do not publish a new
            // horizon downstream after any resolution has spilled.
            if !self.spill_runs.iter().any(|r| r.has_spilled()) {
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
        self.worker_maps.clear();
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

    /// Bytes written to spill files, including intermediate merge outputs.
    pub fn spill_bytes_written(&self) -> u64 {
        self.spill_runs.iter().map(|r| r.bytes_written).sum()
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
        if self.is_finished() {
            f64::NEG_INFINITY
        } else {
            self.current_lat_horizon
        }
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

    #[inline(always)]
    fn compacts_children(&self) -> bool {
        self.compactor.is_enabled()
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
