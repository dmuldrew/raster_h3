//! Generic multi-resolution streaming controller.
//!
//! Bounded worker windows merge into resolution maps. Conservative chunk/cell
//! bounds permit early finalization; memory pressure creates sorted partial runs.
//! Once spilled, a resolution is finalized only after external merging at EOF.
//! Output and sibling compaction are incremental: compaction groups complete
//! during streaming or at EOF in sorted order. Failures latch and drop
//! temporary files. The aggregation budget covers active maps, worker maps
//! and buffered output. Decoded chunks have their own budget
//! (`decode_budget_bytes`); pending compaction groups are accounted and
//! optionally limited (`compaction_budget_bytes`). Metadata is not budgeted.

mod advance;
mod batch;
mod construct;
mod pipeline;

#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod tests;

pub use pipeline::inline_worker_bytes_per_sample;

use std::collections::HashMap;
use std::sync::Arc;

use h3o::Resolution;
use pipeline::{CellMaps, KeyChunk};
use tiff::decoder::DecodingResult;

use super::compaction::HierarchicalCompactor;
use super::eviction_space::EvictionSpace;
use super::lifecycle::{OutputBuffer, StreamLifecycle};
use super::partition::{BoundaryClassifier, PartialCell};
use super::sharded_map::{AccumulatorMerge, ShardedResolutionMap};
use super::spill::{RunReader, SpillAccumulator, SpillRuns};
use super::walker::WalkContext;
use crate::aggregator::cell_hash::H3BuildHasher;
use crate::aggregator::sampling::SamplingPattern;
use crate::error::{RasterH3Error, Result};
use crate::grid::GridKind;
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::PrefetchedMosaicReader;

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
        let bytes = size_of::<Self::Record>() + acc.heap_bytes();
        output.push_sized(self.make_record(resolution, key, acc), bytes);
    }

    /// Worst-case worker bytes one sample can add to one resolution's map: a
    /// new table entry, its owned accumulator state and its eviction key.
    /// Window sizes are derived from it, so it must not understate. The
    /// default of 1 KiB covers table slack plus quantile or category state;
    /// kernels with inline-only accumulators can return
    /// [`inline_worker_bytes_per_sample`] instead for larger windows.
    fn worker_bytes_per_sample(&self) -> usize {
        1024usize.max(size_of::<Self::Accumulator>().saturating_mul(8))
    }

    /// Aggregate one decoded window into thread-local cell maps, one per
    /// resolution. Returns false when the window held no valid data.
    fn process_chunk(
        &self,
        window: &WalkContext<'_>,
        decoding_result: &mut DecodingResult,
        nodata: Option<f64>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, H3BuildHasher>],
    ) -> bool;

    /// Borrowed window override. Built-in kernels never allocate a pixel copy.
    fn process_window(
        &self,
        window: &WalkContext<'_>,
        samples: super::borrowed::BorrowedSamples<'_>,
        nodata: Option<f64>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, H3BuildHasher>],
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

    /// Spatial grid system used by this streamer
    fn grid(&self) -> GridKind {
        GridKind::H3
    }
}

/// Generic single-pass streaming aggregator across multiple grid resolutions
pub struct MultiHorizonStreamer<K: HorizonStreamKernel> {
    kernel: K,
    prefetcher: Option<PrefetchedMosaicReader>,
    mosaic: Arc<MosaicReader>,
    grid: GridKind,
    resolutions: Vec<Resolution>,
    resolution_u8s: Vec<u8>,
    nodata: Option<f64>,
    apply_scale_offset: bool,
    /// Shared total that transform failures are also added to.
    transform_failure_counter: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Fail on the first wave that drops a sample to a transform failure.
    strict_transforms: bool,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    resolution_shards: Vec<ShardedResolutionMap<K::Accumulator>>,
    compactor: HierarchicalCompactor<K>,
    output_buffer: OutputBuffer<K::Record>,
    lifecycle: StreamLifecycle,
    current_lat_horizon: f64,
    can_evict_early: bool,
    suffix_max_north_lat: Vec<f64>,
    /// How cells' eviction keys and chunks' bounds are measured.
    eviction_space: EvictionSpace,
    /// Suffix maxima of chunk bounds in `eviction_space` when it is not
    /// latitude (then `suffix_max_north_lat` serves both).
    suffix_eviction_bound: Option<Vec<f64>>,
    /// Latitude bound of every chunk not yet processed. Equals
    /// `eviction_horizon` when keys are latitudes.
    lat_horizon: f64,
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
    /// Dedicated kernel pool (`aggregation_workers`); None uses the ambient pool.
    pool: Option<Arc<rayon::ThreadPool>>,
    worker_budget: usize,
    /// Most chunks per batch.
    batch_size: usize,
    /// Pixels after which a batch stops, about two waves of worker windows.
    batch_pixels: usize,
    output_byte_limit: usize,
    peak_active_bytes: usize,
    /// Map sets for window jobs in flight (twice the workers, so a worker
    /// can start its next window while earlier ones wait to be merged).
    worker_maps: Vec<CellMaps<K::Accumulator>>,
    /// Ring of out-of-order sets, bounded by the map-set count and reused across batches.
    finished_sets: Vec<Option<CellMaps<K::Accumulator>>>,
    /// New-cell and computed-key lists of a batch, reused across batches.
    new_cell_buffer: Vec<(usize, u64)>,
    key_chunk_buffer: Vec<KeyChunk>,
    metrics: super::profile::StreamProfile,
    /// Set for partitioned scans (`MultiResolutionConfig::partition`).
    partition: Option<PartitionOutput<K::Accumulator>>,
}

/// Interior/boundary classification and buffered partial cells of a partitioned scan.
struct PartitionOutput<A> {
    classifier: BoundaryClassifier,
    partials: OutputBuffer<PartialCell<A>>,
}

impl<K: HorizonStreamKernel> MultiHorizonStreamer<K> {
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

    /// Spatial grid system used by this streamer
    pub fn grid(&self) -> GridKind {
        self.grid
    }

    /// Target resolution integer levels
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

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Result<Vec<K::Record>> {
        self.reject_partitioned()?;
        self.advance_until_completed(max_rows)?;
        Ok(self.output_buffer.take_batch(max_rows))
    }

    /// Drain up to `max_rows` completed records directly into a closure with zero heap allocation
    pub fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> Result<usize>
    where
        F: FnMut(usize, K::Record),
    {
        self.reject_partitioned()?;
        self.advance_until_completed(max_rows)?;
        Ok(self.output_buffer.drain_into(max_rows, consumer))
    }

    /// Record-only reads would silently lose a partitioned scan's partial cells.
    fn reject_partitioned(&self) -> Result<()> {
        if self.partition.is_some() {
            return Err(RasterH3Error::InvalidParameter(
                "partitioned scans emit partial cells; drain them with drain_partitioned_into"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Drain a partitioned scan: up to `max_rows` completed (interior) records into
    /// `on_record` and partial (boundary) cells into `on_partial`, in total. Returns the
    /// number drained; with `max_rows >= 1`, 0 means end of stream. Like
    /// [`Self::drain_completed_into`], `max_rows == 0` drains nothing and also returns 0, so
    /// pass at least 1 (or check [`Self::is_finished`]). Also works for unpartitioned
    /// scans, which produce no partial cells.
    pub fn drain_partitioned_into<F, G>(
        &mut self,
        max_rows: usize,
        on_record: F,
        mut on_partial: G,
    ) -> Result<usize>
    where
        F: FnMut(usize, K::Record),
        G: FnMut(PartialCell<K::Accumulator>),
    {
        self.advance_until_completed(max_rows)?;
        let records = self.output_buffer.drain_into(max_rows, on_record);
        let partials = match &mut self.partition {
            Some(p) => p
                .partials
                .drain_into(max_rows - records, |_, cell| on_partial(cell)),
            None => 0,
        };
        Ok(records + partials)
    }

    /// For a partitioned scan, the number of finalized cells classified so far as
    /// `(interior, boundary)`.
    pub fn partition_cell_counts(&self) -> Option<(u64, u64)> {
        self.partition.as_ref().map(|p| p.classifier.counts())
    }

    /// Bytes written to spill files, including intermediate merge outputs.
    pub fn spill_bytes_written(&self) -> u64 {
        self.spill_runs.iter().map(|r| r.bytes_written).sum()
    }

    /// Number of partial sorted runs written, excluding intermediate merge files.
    pub fn spill_run_count(&self) -> u64 {
        self.spill_runs.iter().map(|r| r.runs_written).sum()
    }

    /// [`Self::spill_run_count`] for each configured resolution, in order.
    pub fn spill_run_counts(&self) -> Vec<u64> {
        self.spill_runs.iter().map(|r| r.runs_written).collect()
    }

    /// Valid sample points dropped because their coordinate transform failed
    /// or left the valid latitude/longitude domain; see
    /// [`super::profile::WorkerProfile::transform_failures`]. Samples outside
    /// the bbox or owned by another mosaic tile are not counted.
    pub fn transform_failures(&self) -> u64 {
        self.metrics.worker.transform_failures
    }

    /// Peak estimate of active table/heap allocation, including pending
    /// compaction groups; excludes decoded chunks and output memory.
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
        self.lifecycle.is_finished() && self.buffered_len() == 0
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

    #[inline(always)]
    fn grid(&self) -> GridKind {
        self.grid
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
