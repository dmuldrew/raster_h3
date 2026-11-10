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

use super::eviction_space::{CellKeys, EvictionSpace};
use crate::aggregator::cell_hash::H3BuildHasher;
use h3o::{CellIndex, Resolution};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tiff::decoder::DecodingResult;

use crate::aggregator::sampling::SamplingPattern;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::MosaicPrefetchItem;
use crate::raster::prefetch::PrefetchedMosaicReader;
use crate::raster::RasterChunk;

use super::compaction::HierarchicalCompactor;
use super::config::MultiResolutionConfig;
use super::coordinates::eviction_north_bound;
use super::lifecycle::{OutputBuffer, StreamLifecycle};
use super::partition::{restrict_mosaic, BoundaryClassifier, PartialCell};
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
type CellMaps<A> = Vec<HashMap<u64, A, H3BuildHasher>>;

/// A batch's windows, generated on demand without storage per window.
struct WindowJobs<'a> {
    items: &'a [MosaicPrefetchItem],
    mosaic: &'a MosaicReader,
    worker_pixels: usize,
    item: usize,
    row: usize,
    col: usize,
    remaining: usize,
}

impl<'a> WindowJobs<'a> {
    fn new(
        items: &'a [MosaicPrefetchItem],
        mosaic: &'a MosaicReader,
        worker_pixels: usize,
        decoded_bytes: &mut u64,
    ) -> Result<Self> {
        use super::borrowed::BorrowedSamples;
        let mut remaining = 0;
        for item in items {
            let (tile, index, chunk, data, _) = item
                .as_ref()
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
            if chunk.width == 0
                || chunk.height == 0
                || mosaic.tiles[*tile].reader.is_sparse_chunk(*index)
            {
                continue;
            }
            let width = chunk.width as usize;
            let height = chunk.height as usize;
            let spp = mosaic.tiles[*tile].reader.metadata.samples_per_pixel.max(1) as usize;
            let samples = BorrowedSamples::from(data);
            let len = width
                .checked_mul(height)
                .and_then(|pixels| pixels.checked_mul(spp))
                .ok_or_else(|| RasterH3Error::InvalidMetadata("sample window overflow".into()))?;
            samples.window(0, len)?;
            *decoded_bytes += samples.bytes() as u64;
            remaining += if width <= worker_pixels {
                height.div_ceil(worker_pixels / width)
            } else {
                width.div_ceil(worker_pixels) * height
            };
        }
        Ok(Self {
            items,
            mosaic,
            worker_pixels,
            item: 0,
            row: 0,
            col: 0,
            remaining,
        })
    }
}

impl<'a> Iterator for WindowJobs<'a> {
    type Item = WindowJob<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        use super::borrowed::BorrowedSamples;
        while let Some(item) = self.items.get(self.item) {
            let (tile, index, chunk, data, overlap) = item.as_ref().expect("validated batch");
            let width = chunk.width as usize;
            let height = chunk.height as usize;
            // Sparse fills must not be interpreted as observed data.
            if width == 0
                || self.row == height
                || self.mosaic.tiles[*tile].reader.is_sparse_chunk(*index)
            {
                self.item += 1;
                self.row = 0;
                self.col = 0;
                continue;
            }
            let spp = self.mosaic.tiles[*tile]
                .reader
                .metadata
                .samples_per_pixel
                .max(1) as usize;
            let (w, h) = if width <= self.worker_pixels {
                (width, (self.worker_pixels / width).min(height - self.row))
            } else {
                (self.worker_pixels.min(width - self.col), 1)
            };
            let start = (self.row * width + self.col) * spp;
            let len = ((h - 1) * width + w) * spp;
            let samples = BorrowedSamples::from(data)
                .window(start, len)
                .expect("validated sample window");
            let bounds = RasterChunk {
                col_offset: chunk.col_offset + self.col as u32,
                row_offset: chunk.row_offset + self.row as u32,
                width: w as u32,
                height: h as u32,
            };
            if w == width {
                self.row += h;
            } else {
                self.col += w;
                if self.col == width {
                    self.col = 0;
                    self.row += 1;
                }
            }
            self.remaining -= 1;
            return Some((*tile, bounds, samples, width as u32, *overlap));
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for WindowJobs<'_> {}

/// Refuse a `band` (or spectral formula band) that some mosaic tile does not have. The
/// sample kernels index pixels by band without a bounds check that fails the scan, so an
/// out-of-range band would otherwise read another band's values.
fn check_bands_exist(mosaic: &MosaicReader, config: &MultiResolutionConfig) -> Result<()> {
    let bands = match &config.spectral_formula {
        Some(formula) => formula.bands(),
        None => vec![config.band],
    };
    for tile in &mosaic.tiles {
        let available = usize::from(tile.reader.metadata.samples_per_pixel.max(1));
        if let Some(&band) = bands.iter().find(|&&b| b == 0 || b > available) {
            let what = if config.spectral_formula.is_some() {
                "spectral formula band"
            } else {
                "band"
            };
            return Err(RasterH3Error::InvalidParameter(format!(
                "{what} {band} does not exist: the raster has {available} band(s), numbered from 1"
            )));
        }
    }
    Ok(())
}

/// Dedicated aggregation pool of `threads` workers, shared by all live
/// streams requesting the same size so concurrent queries do not each spawn
/// their own threads.
fn shared_aggregation_pool(threads: usize) -> Result<Arc<rayon::ThreadPool>> {
    static POOLS: OnceLock<Mutex<HashMap<usize, Weak<rayon::ThreadPool>>>> = OnceLock::new();
    let mut pools = POOLS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(pool) = pools.get(&threads).and_then(Weak::upgrade) {
        return Ok(pool);
    }
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("raster-h3-agg-{i}"))
            .build()
            .map_err(|e| RasterH3Error::InvalidParameter(format!("aggregation pool: {e}")))?,
    );
    pools.retain(|_, w| w.strong_count() > 0);
    pools.insert(threads, Arc::downgrade(&pool));
    Ok(pool)
}

/// Fixed bytes a worker map and its key list are charged for regardless of
/// entries: the `+ 1` bucket and alignment slack of `spill::table_bytes`.
fn worker_map_fixed_bytes<A>() -> usize {
    2 * (size_of::<(u64, A)>() + 1) + 64
}

/// Worst-case worker bytes per sample for an accumulator that owns no heap
/// state, in the same terms the budget check measures them: a sample adds at
/// most one entry to a map whose reused capacity stays under twice its
/// largest entry count, charged at `spill::table_bytes`'s two
/// buckets per unit of capacity.
pub fn inline_worker_bytes_per_sample<A>() -> usize {
    4 * (size_of::<(u64, A)>() + 1)
}

/// New cells per eviction-key job sent to the workers.
const KEY_CHUNK: usize = 256;

/// Computed eviction keys of one key job: (job number, `(resolution index,
/// cell)` pairs, their keys).
type KeyChunk = (usize, Vec<(usize, u64)>, Vec<CellKeys>);

/// Work handed to a pipeline worker.
enum Work<A, J> {
    /// Aggregate window `index` into `maps`, an empty map set.
    Window {
        index: usize,
        job: J,
        maps: CellMaps<A>,
    },
    /// Compute eviction keys of `(resolution index, cell)` pairs.
    Keys {
        chunk: usize,
        cells: Vec<(usize, u64)>,
    },
}

/// A pipeline worker's reply.
enum Done<A> {
    Window {
        index: usize,
        maps: CellMaps<A>,
        profile: super::profile::WorkerProfile,
        ns: u64,
    },
    Keys {
        chunk: usize,
        cells: Vec<(usize, u64)>,
        keys: Vec<CellKeys>,
    },
    Panicked(Box<dyn std::any::Any + Send>),
}

/// Spill one resolution's horizon map to a sorted run; it is then finalized
/// only by the external merge at EOF.
fn spill_resolution<A: SpillAccumulator>(
    shards: &mut [ShardedResolutionMap<A>],
    spill_runs: &mut [SpillRuns<A>],
    metrics: &mut super::profile::StreamProfile,
    index: usize,
) -> Result<()> {
    let started = std::time::Instant::now();
    let records = shards[index].take_sorted();
    if !records.is_empty() {
        spill_runs[index].push_sorted(records)?;
        shards[index].set_eviction_enabled(false);
    }
    metrics.spill_ns += started.elapsed().as_nanos() as u64;
    Ok(())
}

/// The streamer state a batch's merges write to, borrowed apart from the
/// read-only state the workers use.
struct MergeTarget<'a, A: SpillAccumulator> {
    shards: &'a mut Vec<ShardedResolutionMap<A>>,
    spill_runs: &'a mut Vec<SpillRuns<A>>,
    metrics: &'a mut super::profile::StreamProfile,
    peak_active_bytes: &'a mut usize,
    failure_counter: Option<&'a Arc<std::sync::atomic::AtomicU64>>,
    compactor_bytes: usize,
    accumulator_limit: usize,
    map_budget: usize,
    /// Worker storage allowance of one map set.
    set_budget: usize,
    map_sets: usize,
    /// Cells inserted into a horizon map that still need eviction keys.
    new_cells: Vec<(usize, u64)>,
    /// Computed keys: (chunk number, cells, keys).
    key_chunks: Vec<KeyChunk>,
}

impl<A: SpillAccumulator> MergeTarget<'_, A> {
    fn record(&mut self, profile: super::profile::WorkerProfile, ns: u64) {
        if let (Some(counter), n @ 1..) = (self.failure_counter, profile.transform_failures) {
            counter.fetch_add(n, Ordering::Relaxed);
        }
        self.metrics.worker.merge(profile);
        self.metrics.worker_ns += ns;
        self.metrics.jobs += 1;
    }

    /// Merge one job's map set into the horizon maps, leaving it empty.
    fn merge(&mut self, maps: &mut CellMaps<A>) -> Result<()> {
        let bytes: usize = maps
            .iter()
            .map(|m| {
                super::spill::table_bytes::<u64, A>(m.capacity())
                    + m.values().map(|a| a.heap_bytes()).sum::<usize>()
            })
            .sum();
        // Every set in flight is bounded like this one.
        self.metrics.peak_worker_bytes = self
            .metrics
            .peak_worker_bytes
            .max(bytes.saturating_mul(self.map_sets));
        let t = std::time::Instant::now();
        let result = self.merge_maps(maps);
        self.metrics.merge_ns += t.elapsed().as_nanos() as u64;
        // Retained buckets are part of worker storage, not free capacity.
        if bytes > self.set_budget {
            for map in maps.iter_mut() {
                map.shrink_to_fit();
            }
        }
        result
    }

    fn merge_maps(&mut self, maps: &mut CellMaps<A>) -> Result<()> {
        for (i, map) in maps.iter_mut().enumerate() {
            for (key, acc) in map.drain() {
                if acc.memory_bytes() > self.accumulator_limit {
                    return Err(RasterH3Error::InvalidParameter("single accumulator exceeds aggregation budget; increase aggregation_budget_bytes".into()));
                }
                let (merged, new) = self.shards[i].merge_owned_deferred(key, acc);
                if new {
                    self.new_cells.push((i, key));
                }
                if merged > self.accumulator_limit {
                    return Err(RasterH3Error::InvalidParameter("merged accumulator exceeds aggregation budget; increase aggregation_budget_bytes".into()));
                }
                *self.peak_active_bytes = (*self.peak_active_bytes).max(
                    self.shards
                        .iter()
                        .map(|s| s.estimated_bytes())
                        .sum::<usize>()
                        + self.compactor_bytes,
                );
                if self.shards[i].estimated_bytes() >= self.map_budget {
                    spill_resolution(self.shards, self.spill_runs, self.metrics, i)?;
                }
            }
        }
        Ok(())
    }
}

/// Run a batch on the calling thread: windows in order, then the new cells'
/// eviction keys.
fn run_serial<A, J, W, C>(
    jobs: impl Iterator<Item = J>,
    free: &mut Vec<CellMaps<A>>,
    run_window: &W,
    compute_keys: &C,
    target: &mut MergeTarget<'_, A>,
) -> Result<()>
where
    A: SpillAccumulator,
    W: Fn(J, &mut CellMaps<A>) -> (super::profile::WorkerProfile, u64),
    C: Fn(&[(usize, u64)]) -> Vec<CellKeys>,
{
    let mut maps = free.pop().expect("at least one worker map set");
    let mut result = Ok(());
    for job in jobs {
        let (profile, ns) = run_window(job, &mut maps);
        target.record(profile, ns);
        result = target.merge(&mut maps);
        if result.is_err() {
            maps.iter_mut().for_each(HashMap::clear);
            break;
        }
    }
    free.push(maps);
    result?;
    let cells = {
        let cells = target.new_cells.to_vec();
        target.new_cells.clear();
        cells
    };
    let keys = compute_keys(&cells);
    target.key_chunks.push((0, cells, keys));
    Ok(())
}

/// Run a batch on `workers` tasks of a Rayon scope while the calling thread
/// merges finished windows in window order (see `process_batch`).
/// `finished` holds sets that finished out of order; it is reused across batches.
#[allow(clippy::too_many_arguments)]
fn run_pipelined<'s, A, J, W, C>(
    scope: &rayon::Scope<'s>,
    workers: usize,
    mut jobs: impl ExactSizeIterator<Item = J>,
    free: &mut Vec<CellMaps<A>>,
    run_window: &'s W,
    compute_keys: &'s C,
    target: &mut MergeTarget<'_, A>,
    finished: &mut Vec<Option<CellMaps<A>>>,
) -> Result<()>
where
    A: SpillAccumulator + Send + 's,
    J: Send + 's,
    W: Fn(J, &mut CellMaps<A>) -> (super::profile::WorkerProfile, u64) + Sync,
    C: Fn(&[(usize, u64)]) -> Vec<CellKeys> + Sync,
{
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::mpsc;
    let (work_tx, work_rx) = mpsc::channel::<Work<A, J>>();
    let work_rx = Arc::new(Mutex::new(work_rx));
    let (done_tx, done_rx) = mpsc::channel::<Done<A>>();
    for _ in 0..workers.max(1) {
        let (rx, tx) = (Arc::clone(&work_rx), done_tx.clone());
        scope.spawn(move |_| loop {
            // Holding the lock while waiting lets one idle worker wait in
            // `recv` and the rest on the lock; either way they wake in turn.
            let message = match rx.lock() {
                Ok(guard) => guard.recv(),
                Err(_) => break,
            };
            let Ok(work) = message else { break };
            let reply = catch_unwind(AssertUnwindSafe(|| match work {
                Work::Window {
                    index,
                    job,
                    mut maps,
                } => {
                    let (profile, ns) = run_window(job, &mut maps);
                    Done::Window {
                        index,
                        maps,
                        profile,
                        ns,
                    }
                }
                Work::Keys { chunk, cells } => {
                    let keys = compute_keys(&cells);
                    Done::Keys { chunk, cells, keys }
                }
            }))
            .unwrap_or_else(Done::Panicked);
            if tx.send(reply).is_err() {
                break;
            }
        });
    }
    drop(done_tx);

    finished.clear();
    let slots = free.len();
    finished.resize_with(slots, || None);
    let count = jobs.len();
    let (mut next, mut expected, mut end) = (0, 0, count);
    let (mut key_chunk, mut keys_out) = (0, 0);
    let mut result = Ok(());
    let mut panic = None;
    while next < end {
        let Some(maps) = free.pop() else { break };
        let _ = work_tx.send(Work::Window {
            index: next,
            job: jobs.next().expect("window count"),
            maps,
        });
        next += 1;
    }
    while expected < end || keys_out > 0 {
        let Ok(reply) = done_rx.recv() else { break };
        match reply {
            Done::Panicked(payload) => {
                panic = Some(payload);
                break;
            }
            Done::Keys { chunk, cells, keys } => {
                keys_out -= 1;
                target.key_chunks.push((chunk, cells, keys));
            }
            Done::Window {
                index,
                maps,
                profile,
                ns,
            } => {
                target.record(profile, ns);
                // A slot is reused only after its previous set is merged.
                finished[index % slots] = Some(maps);
                while expected < end {
                    let Some(mut maps) = finished[expected % slots].take() else {
                        break;
                    };
                    if result.is_ok() {
                        result = target.merge(&mut maps);
                    }
                    if result.is_err() {
                        maps.iter_mut().for_each(HashMap::clear);
                        // Stop dispatching; collect what is in flight.
                        end = next;
                    }
                    free.push(maps);
                    expected += 1;
                    if next < end {
                        let maps = free.pop().expect("a set was just returned");
                        let _ = work_tx.send(Work::Window {
                            index: next,
                            job: jobs.next().expect("window count"),
                            maps,
                        });
                        next += 1;
                    }
                }
                let last = expected == count;
                if result.is_ok()
                    && (target.new_cells.len() >= KEY_CHUNK
                        || (last && !target.new_cells.is_empty()))
                {
                    let cells = {
                        let cells = target.new_cells.to_vec();
                        target.new_cells.clear();
                        cells
                    };
                    let _ = work_tx.send(Work::Keys {
                        chunk: key_chunk,
                        cells,
                    });
                    key_chunk += 1;
                    keys_out += 1;
                }
            }
        }
    }
    // Closing the queue lets idle workers exit; the scope joins them.
    drop(work_tx);
    if let Some(payload) = panic {
        std::panic::resume_unwind(payload);
    }
    result
}

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
}

/// Generic single-pass streaming aggregator across multiple H3 resolutions
pub struct MultiHorizonStreamer<K: HorizonStreamKernel> {
    kernel: K,
    prefetcher: Option<PrefetchedMosaicReader>,
    mosaic: Arc<MosaicReader>,
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
        check_bands_exist(&mosaic, config)?;
        let should_compact = config.should_compact_h3_children();
        // A partition streams only its own chunks; cells that chunks outside it may also
        // reach are classified against the full mosaic's chunk list.
        let (mosaic, partition) = match &config.partition {
            Some(spec) => {
                let restricted = Arc::new(restrict_mosaic(&mosaic, spec)?);
                let output = PartitionOutput {
                    classifier: BoundaryClassifier::new(&mosaic, spec),
                    partials: OutputBuffer::with_capacity(16),
                };
                (restricted, Some(output))
            }
            None => (mosaic, None),
        };

        let mut resolutions = Vec::with_capacity(config.resolutions.len());
        let mut resolution_u8s = Vec::with_capacity(config.resolutions.len());
        for &res_u8 in &config.resolutions {
            let res = Resolution::try_from(res_u8).map_err(|_| {
                RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", res_u8))
            })?;
            resolutions.push(res);
            resolution_u8s.push(res_u8);
        }

        let prefetcher = PrefetchedMosaicReader::spawn_with_budget(
            Arc::clone(&mosaic),
            config.prefetch_chunks,
            config.decode_workers,
            config.fetch_workers,
            config.decode_budget_bytes,
        );
        let num_res = resolutions.len();
        let mut resolution_shards: Vec<_> =
            (0..num_res).map(|_| ShardedResolutionMap::new()).collect();
        // Past an opt-in limit, pending sibling groups are decomposed before completion.
        let compactor = HierarchicalCompactor::with_byte_limit(
            should_compact,
            config.compaction_budget_bytes.unwrap_or(usize::MAX),
        );

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
        // Cells are evicted by `eviction_space`; latitude bounds still guard
        // compaction and the horizon published to tilers.
        let eviction_space = EvictionSpace::for_mosaic(&mosaic);
        let suffix_eviction_bound = eviction_space.is_rows().then(|| {
            let mut suffix = vec![f64::NEG_INFINITY; n_chunks + 1];
            for k in (0..n_chunks).rev() {
                let reference = mosaic.chunk_refs[k];
                let tile = &mosaic.tiles[reference.tile_idx];
                let chunk = tile.reader.chunk_layout.get_chunk_bounds(
                    reference.chunk_idx,
                    tile.reader.metadata.width,
                    tile.reader.metadata.height,
                );
                let bound = eviction_space.chunk_bound(
                    &chunk,
                    &tile.reader.metadata.geotransform,
                    &tile.crs_transformer,
                );
                suffix[k] = suffix[k + 1].max(bound);
            }
            suffix
        });
        let can_evict_early = suffix_eviction_bound
            .as_ref()
            .unwrap_or(&suffix_max_north_lat)
            .iter()
            .any(|v| v.is_finite());
        for map in &mut resolution_shards {
            map.set_eviction_enabled(can_evict_early);
            map.set_lat_tracking(eviction_space.is_rows());
        }
        // Leave room for sorted-run construction, merge heads, and bounded output.
        let map_budget = config.aggregation_budget_bytes / num_res / 4;
        let accumulator_limit = config.aggregation_budget_bytes / num_res / 8;
        let spill_runs = (0..num_res)
            .map(|_| SpillRuns::new(accumulator_limit, config.spill_directory.as_deref()))
            .collect();
        // Reserve worker storage by sample cardinality, not TIFF chunk count:
        // each sample can add at most one cell per resolution.
        let per_sample = kernel.worker_bytes_per_sample();
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
        let pool = config
            .aggregation_workers
            .map(shared_aggregation_pool)
            .transpose()?;
        let worker_tasks = match config.aggregation_workers {
            Some(n) => n.clamp(1, 64),
            None => rayon::current_num_threads().clamp(1, 8),
        }
        .min(worker_budget / per_pixel);
        let batch_size = (worker_tasks * 2).min((n_chunks / 4).max(1));
        // Two map sets per worker keep workers busy while earlier windows
        // wait to be merged in order; all sets share the worker allowance.
        let map_sets = (2 * worker_tasks).min(worker_budget / per_pixel).max(1);
        let worker_tasks = worker_tasks.min(map_sets);
        let map_fixed = worker_map_fixed_bytes::<K::Accumulator>().saturating_mul(num_res);
        let worker_pixels =
            ((worker_budget / map_sets).saturating_sub(map_fixed) / per_pixel).clamp(1, 16384);
        // Horizon maps hold every cell of a batch until it is merged, so a
        // batch also stops at about one window per map set (see `next_batch_len`).
        let batch_pixels = map_sets * worker_pixels;
        let output_byte_limit = config.aggregation_budget_bytes / 4;

        Ok(Self {
            kernel,
            prefetcher: Some(prefetcher),
            mosaic,
            resolutions,
            resolution_u8s,
            nodata: config.custom_nodata,
            apply_scale_offset: config.apply_scale_offset,
            transform_failure_counter: config.transform_failure_counter.clone(),
            strict_transforms: config.strict_transforms,
            bbox: config.bbox,
            sampling: config.sampling.clone(),
            resolution_shards,
            compactor,
            output_buffer: OutputBuffer::with_capacity(16),
            lifecycle: StreamLifecycle::new(),
            current_lat_horizon: f64::INFINITY,
            can_evict_early,
            suffix_max_north_lat,
            eviction_space,
            suffix_eviction_bound,
            lat_horizon: f64::INFINITY,
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
            pool,
            worker_budget,
            batch_size,
            batch_pixels,
            output_byte_limit,
            peak_active_bytes: 0,
            finished_sets: Vec::new(),
            new_cell_buffer: Vec::new(),
            key_chunk_buffer: Vec::new(),
            worker_maps: (0..map_sets)
                .map(|_| {
                    (0..num_res)
                        .map(|_| HashMap::with_hasher(H3BuildHasher))
                        .collect()
                })
                .collect(),
            metrics: super::profile::StreamProfile {
                worker_map_sets: map_sets as u64,
                ..Default::default()
            },
            partition,
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
        if let Some(partition) = &mut self.partition {
            if !partition.classifier.is_interior(key) {
                // Other partitions may hold samples of this cell: hand its state on
                // unfiltered, since only the merged state can be filtered.
                let bytes = size_of::<PartialCell<K::Accumulator>>() + acc.heap_bytes();
                partition.partials.push_sized(
                    PartialCell {
                        resolution: self.resolution_u8s[res_idx],
                        h3_index: key,
                        accumulator: acc,
                    },
                    bytes,
                );
                return;
            }
        }
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
        spill_resolution(
            &mut self.resolution_shards,
            &mut self.spill_runs,
            &mut self.metrics,
            index,
        )
    }

    /// Aggregate a batch's windows into the horizon maps.
    ///
    /// Windows run on long-lived workers that take jobs as they become free,
    /// so no worker waits for a wave's slowest window. Each job fills its own
    /// map set, and the calling thread merges the sets strictly in window
    /// order while later windows still run, so results do not depend on
    /// timing. At most `worker_maps.len()` sets are in flight; a set returns
    /// to the pool once merged. Cells new to a horizon map get their eviction
    /// keys (a cell-boundary computation each) from the workers too, as extra
    /// jobs, and the keys are queued in creation order at the end of the
    /// batch, before any eviction can need them.
    fn process_batch(&mut self, items: &mut [MosaicPrefetchItem]) -> Result<()> {
        let mosaic = &*self.mosaic;
        let jobs = WindowJobs::new(
            items,
            mosaic,
            self.worker_pixels,
            &mut self.metrics.decoded_bytes,
        )?;
        let job_count = jobs.len();
        if job_count == 0 {
            return Ok(());
        }
        let mut free = std::mem::take(&mut self.worker_maps);
        let new_cells = std::mem::take(&mut self.new_cell_buffer);
        let key_chunks = std::mem::take(&mut self.key_chunk_buffer);
        let map_sets = free.len();
        let set_budget = self.worker_budget / map_sets.max(1);
        let Self {
            kernel,
            resolutions,
            sampling,
            bbox,
            apply_scale_offset,
            nodata,
            eviction_space,
            resolution_shards,
            spill_runs,
            compactor,
            metrics,
            peak_active_bytes,
            accumulator_limit,
            map_budget,
            pool,
            worker_tasks,
            transform_failure_counter,
            finished_sets,
            ..
        } = self;
        let (kernel, mosaic, resolutions, sampling, eviction_space) = (
            &*kernel,
            mosaic,
            &*resolutions,
            &*sampling,
            &*eviction_space,
        );
        let (active, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let run_window = |job: WindowJob<'_>, maps: &mut CellMaps<K::Accumulator>| {
            peak.fetch_max(
                active.fetch_add(1, Ordering::Relaxed) + 1,
                Ordering::Relaxed,
            );
            let (tile_idx, chunk, samples, stride, overlap) = &job;
            let tile = &mosaic.tiles[*tile_idx];
            let scope = super::profile::WorkerScope::new();
            let start = std::time::Instant::now();
            let window = WalkContext {
                chunk,
                resolutions,
                crs: &tile.crs_transformer,
                gt: &tile.reader.metadata.geotransform,
                sampling,
                bbox: *bbox,
                stride: *stride,
                samples_per_pixel: tile.reader.metadata.samples_per_pixel,
                owner: overlap.then_some((*tile_idx, mosaic)),
                band_scaling: if *apply_scale_offset {
                    &tile.reader.metadata.band_scaling
                } else {
                    &[]
                },
                alpha_sample: tile.reader.metadata.alpha_sample,
            };
            kernel.process_window(
                &window,
                *samples,
                nodata.or(tile.reader.metadata.nodata),
                maps,
            );
            active.fetch_sub(1, Ordering::Relaxed);
            (scope.snapshot(), start.elapsed().as_nanos() as u64)
        };
        let compute_keys = |cells: &[(usize, u64)]| -> Vec<CellKeys> {
            cells
                .iter()
                .map(|&(_, cell)| eviction_space.cell_keys(cell))
                .collect()
        };
        let compactor_bytes = compactor.estimated_bytes();
        let mut target = MergeTarget {
            shards: resolution_shards,
            spill_runs,
            metrics,
            peak_active_bytes,
            failure_counter: transform_failure_counter.as_ref(),
            compactor_bytes,
            accumulator_limit: *accumulator_limit,
            map_budget: *map_budget,
            set_budget,
            map_sets,
            new_cells,
            key_chunks,
        };

        // Execution policy: a Rayon worker waiting on a join steals unrelated
        // tasks from its own pool, which deadlocks if such a task needs a lock
        // the caller holds (e.g. a shared streamer mutex). So callers on a
        // Rayon worker either hand the batch to the dedicated pool through a
        // helper OS thread, which blocks without stealing, or, with no
        // dedicated pool, run it serially.
        let t = std::time::Instant::now();
        target.metrics.waves += 1;
        let workers = (*worker_tasks).min(job_count);
        // A panic unwinds past the restore at the end of this function, and the
        // sets it held (in flight or being merged) are lost with it. Catch it,
        // refill the pool and resume, so a caught panic does not leave the
        // streamer with fewer map sets (or none) for later batches.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &*pool {
            // One window (or one worker) gains nothing from the pipeline;
            // running it here saves the channels and task spawns.
            _ if workers <= 1 => {
                run_serial(jobs, &mut free, &run_window, &compute_keys, &mut target)
            }
            Some(p) if p.current_thread_index().is_some() => {
                target.metrics.serial_waves += 1;
                run_serial(jobs, &mut free, &run_window, &compute_keys, &mut target)
            }
            Some(p) if rayon::current_thread_index().is_some() => {
                target.metrics.bridged_waves += 1;
                std::thread::scope(|s| {
                    s.spawn(|| {
                        p.in_place_scope(|scope| {
                            run_pipelined(
                                scope,
                                workers,
                                jobs,
                                &mut free,
                                &run_window,
                                &compute_keys,
                                &mut target,
                                finished_sets,
                            )
                        })
                    })
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
            }
            Some(p) => p.in_place_scope(|scope| {
                run_pipelined(
                    scope,
                    workers,
                    jobs,
                    &mut free,
                    &run_window,
                    &compute_keys,
                    &mut target,
                    finished_sets,
                )
            }),
            None if rayon::current_thread_index().is_some() => {
                target.metrics.serial_waves += 1;
                run_serial(jobs, &mut free, &run_window, &compute_keys, &mut target)
            }
            None => rayon::in_place_scope(|scope| {
                run_pipelined(
                    scope,
                    workers,
                    jobs,
                    &mut free,
                    &run_window,
                    &compute_keys,
                    &mut target,
                    finished_sets,
                )
            }),
        }));
        let result = match outcome {
            Ok(result) => result,
            Err(panic) => {
                free.iter_mut().flatten().for_each(HashMap::clear);
                free.resize_with(map_sets, || {
                    (0..resolutions.len())
                        .map(|_| HashMap::with_hasher(H3BuildHasher))
                        .collect()
                });
                self.worker_maps = free;
                std::panic::resume_unwind(panic);
            }
        };
        let kernel_ns = t.elapsed().as_nanos() as u64;
        let MergeTarget {
            shards,
            mut key_chunks,
            mut new_cells,
            ..
        } = target;
        // Queue eviction keys in the order the cells were created, so pop
        // order (ties included) does not depend on which worker finished first.
        if result.is_ok() {
            key_chunks.sort_unstable_by_key(|(chunk, _, _)| *chunk);
            for (_, cells, keys) in key_chunks.drain(..) {
                for ((res, cell), keys) in cells.into_iter().zip(keys) {
                    shards[res].push_eviction(cell, keys);
                }
            }
        }
        key_chunks.clear();
        new_cells.clear();
        let metrics = &mut self.metrics;
        metrics.peak_concurrent_jobs = metrics
            .peak_concurrent_jobs
            .max(peak.load(Ordering::Relaxed));
        metrics.kernel_wall_ns += kernel_ns;
        self.worker_maps = free;
        self.new_cell_buffer = new_cells;
        self.key_chunk_buffer = key_chunks;
        result?;
        let failures = self.metrics.worker.transform_failures;
        if self.strict_transforms && failures > 0 {
            return Err(RasterH3Error::TransformFailures(failures));
        }
        Ok(())
    }

    /// Advance until a bounded batch of completed records is available.
    pub fn advance_until_completed(&mut self, min_rows: usize) -> Result<()> {
        if let Some(reason) = self.lifecycle.failure_reason() {
            return Err(RasterH3Error::StreamFailed(reason.to_string()));
        }
        let result = self.latch_panics(|s| s.advance_inner(min_rows.min(2048)));
        self.metrics.peak_compaction_bytes = self.compactor.peak_bytes();
        self.metrics.forced_compaction_flushes = self.compactor.forced_flushes();
        if let Some(prefetcher) = &self.prefetcher {
            self.metrics.peak_decoded_bytes = prefetcher.peak_decoded_bytes();
        }
        match result {
            Ok(()) => Ok(()),
            Err(e @ RasterH3Error::TransformFailures(_)) => {
                // Keep the typed error for this call; later reads get the latched text.
                self.fail(e.to_string());
                Err(e)
            }
            Err(e) => Err(self.fail(e.to_string())),
        }
    }

    /// Records plus (in a partitioned scan) partial cells awaiting the consumer.
    fn buffered_len(&self) -> usize {
        self.output_buffer.len() + self.partition.as_ref().map_or(0, |p| p.partials.len())
    }

    fn buffered_bytes(&self) -> usize {
        self.output_buffer.estimated_bytes()
            + self
                .partition
                .as_ref()
                .map_or(0, |p| p.partials.estimated_bytes())
    }

    fn advance_inner(&mut self, min_rows: usize) -> Result<()> {
        while self.buffered_len() < min_rows
            && !self.lifecycle.is_finished()
            && (self.buffered_len() == 0 || self.buffered_bytes() < self.output_byte_limit)
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
                let lat_guard = self.lat_guard();
                self.compactor.flush_completed(
                    lat_guard,
                    |res| {
                        resolution_u8s
                            .iter()
                            .position(|&r| r == res)
                            .is_some_and(|i| !spill_runs[i].has_spilled())
                    },
                    &self.kernel,
                    &mut self.output_buffer,
                    self.output_byte_limit,
                );
                if self.output_buffer.len() > buffered {
                    continue;
                }
            }
            if self.buffered_len() == 0 && !self.spill_runs.iter().any(|r| r.has_spilled()) {
                // Held sibling groups may still emit records north of the horizon.
                self.current_lat_horizon =
                    self.lat_guard().max(self.compactor.pending_emit_south());
            }

            let t0 = std::time::Instant::now();
            let batch_len = self.next_batch_len();
            let mut items = Vec::with_capacity(batch_len);
            if let Some(prefetcher) = &self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut items, 1, batch_len);
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
                if let Some(prefetcher) = self.prefetcher.take() {
                    self.metrics.peak_decoded_bytes = prefetcher.peak_decoded_bytes();
                }
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
                let k = self.processed_chunk_count;
                self.lat_horizon = self.suffix_max_north_lat[k];
                self.eviction_horizon = self
                    .suffix_eviction_bound
                    .as_ref()
                    .map_or(self.lat_horizon, |suffix| suffix[k]);
            }
        }
        Ok(())
    }

    /// At least the southern latitude of every cell that may still be
    /// emitted: cells of chunks not yet processed and cells held in the
    /// horizon maps. With latitude keys the eviction horizon is that bound,
    /// because every cell south-keyed above it has been popped. With row keys
    /// a held cell can lie north of the remaining chunks, so the held cells'
    /// latitudes are included. Compaction and tilers rely on this bound.
    fn lat_guard(&self) -> f64 {
        if !self.eviction_space.is_rows() {
            return self.eviction_horizon;
        }
        self.resolution_shards
            .iter()
            .zip(&self.spill_runs)
            .filter(|(_, runs)| !runs.has_spilled())
            .filter_map(|(shard, _)| shard.held_south_lat_bound())
            .fold(self.lat_horizon, f64::max)
    }

    /// Chunks to take for the next batch. Horizon maps hold every cell of a
    /// batch until it is merged, so a batch stops once its chunks reach
    /// `batch_pixels` (about two waves of worker windows, enough to keep every
    /// worker busy), or at `batch_size` chunks. Counting only chunks let
    /// batches of large strips or tiles grow with the worker count, and fine
    /// resolutions then spilled. Chunks arrive in `chunk_refs` order, so the
    /// upcoming chunks' sizes are known here, even in mixed mosaics.
    fn next_batch_len(&self) -> usize {
        let upcoming = self
            .mosaic
            .chunk_refs
            .get(self.processed_chunk_count..)
            .unwrap_or_default();
        let mut pixels = 0usize;
        let mut len = 0;
        for chunk in upcoming.iter().take(self.batch_size) {
            let layout = &self.mosaic.tiles[chunk.tile_idx].reader.chunk_layout;
            pixels += layout.chunk_width as usize * layout.chunk_height as usize;
            len += 1;
            if pixels >= self.batch_pixels {
                break;
            }
        }
        len.max(1)
    }

    /// Run `f`, latching the stream as failed if it panics, then resume the
    /// unwind. A panic loses the batch in flight (its chunks are already
    /// taken from the prefetcher), so a caller that catches it must not read
    /// on past the missing samples.
    fn latch_panics<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))) {
            Ok(value) => value,
            Err(panic) => {
                self.fail("aggregation panicked; the stream cannot continue".into());
                std::panic::resume_unwind(panic)
            }
        }
    }

    /// Latch failures so subsequent reads cannot mistake a failed stream for EOF.
    fn fail(&mut self, reason: String) -> RasterH3Error {
        self.prefetcher.take();
        self.output_buffer.clear();
        if let Some(partition) = &mut self.partition {
            partition.partials.clear();
        }
        self.compactor.clear();
        self.resolution_shards.clear();
        self.worker_maps.clear();
        self.spill_runs.clear();
        self.final_reader = None;
        self.lifecycle.latch_failure(reason)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::multi_horizon::continuous_streamer::MultiScanHorizonStreamer;
    use crate::raster::prefetch::MosaicPrefetchItem;
    use tiff::encoder::{colortype, TiffEncoder};
    use tiff::tags::Tag;

    /// Minimal georeferenced Gray8 GeoTIFF — enough for `GeoTiffStreamReader::open`.
    fn write_gray_geotiff(path: &std::path::Path) {
        const GEOKEYS: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = TiffEncoder::new(std::io::BufWriter::new(file)).unwrap();
        let mut image = encoder.new_image::<colortype::Gray8>(8, 8).unwrap();
        image
            .encoder()
            .write_tag(
                Tag::ModelTiepointTag,
                &[0.0, 0.0, 0.0, -122.0, 38.0, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001, 0.001, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(34735), &GEOKEYS[..])
            .unwrap();
        image.write_data(&[1u8; 64]).unwrap();
    }

    /// A panic inside a wave used to leave `worker_maps` empty; every following
    /// wave then zipped jobs against zero maps and silently dropped all
    /// samples. `process_batch` now restores its state before resuming the
    /// unwind, so a caller that catches the panic finds the maps intact.
    #[test]
    fn worker_panic_preserves_worker_maps() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_gray_geotiff(tmp.path());
        let reader = GeoTiffStreamReader::open(tmp.path()).unwrap();
        let config = MultiResolutionConfig::single(5);
        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        let maps_before = streamer.worker_maps.len();
        assert!(maps_before > 0);

        // Without horizon maps, merging the window's map set indexes a
        // missing resolution and panics after the sets are taken from the pool.
        streamer.resolution_shards.clear();
        let mut items: Vec<MosaicPrefetchItem> = vec![Ok((
            0,
            0,
            RasterChunk {
                col_offset: 0,
                row_offset: 0,
                width: 4,
                height: 4,
            },
            DecodingResult::U8(vec![1; 16]),
            false,
        ))];
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = streamer.process_batch(&mut items);
        }));
        assert!(panic.is_err());
        assert_eq!(streamer.worker_maps.len(), maps_before);
    }

    /// A panic while advancing loses the batch in flight, so the stream
    /// latches a failure: later reads return an error instead of continuing
    /// past the missing samples.
    #[test]
    fn panic_while_advancing_latches_failure() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_gray_geotiff(tmp.path());
        let reader = GeoTiffStreamReader::open(tmp.path()).unwrap();
        let config = MultiResolutionConfig::single(5);
        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        // Without horizon maps, the first merge indexes a missing resolution
        // and panics on the calling thread.
        streamer.resolution_shards.clear();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = streamer.advance_until_completed(1);
        }));
        assert!(panic.is_err(), "the panic must still reach the caller");
        let err = streamer.advance_until_completed(1).unwrap_err();
        assert!(err.to_string().contains("panicked"), "{err}");
        assert!(streamer.fetch_next_batch(16).is_err());
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::aggregator::accumulator::H3Accumulator;

    fn test_mosaic() -> (tempfile::NamedTempFile, MosaicReader) {
        use tiff::encoder::{colortype::Gray8, TiffEncoder};
        use tiff::tags::Tag;

        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let mut encoder = TiffEncoder::new(file.reopen().unwrap()).unwrap();
            let mut image = encoder.new_image::<Gray8>(1, 1).unwrap();
            image
                .encoder()
                .write_tag(Tag::ModelPixelScaleTag, &[0.01, 0.01, 0.0][..])
                .unwrap();
            image
                .encoder()
                .write_tag(
                    Tag::ModelTiepointTag,
                    &[0.0, 0.0, 0.0, -100.0, 40.0, 0.0][..],
                )
                .unwrap();
            image
                .encoder()
                .write_tag(
                    Tag::GeoKeyDirectoryTag,
                    &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
                )
                .unwrap();
            image.write_data(&[1]).unwrap();
        }
        let reader = GeoTiffStreamReader::open(file.path()).unwrap();
        (
            file,
            MosaicReader::from_single_reader(reader, None, None).unwrap(),
        )
    }

    #[test]
    fn large_tile_windows_are_lazy_and_cover_each_pixel_once() {
        let (_file, mosaic) = test_mosaic();
        let items = vec![Ok((
            0,
            0,
            RasterChunk {
                col_offset: 7,
                row_offset: 11,
                width: 4096,
                height: 4096,
            },
            DecodingResult::U8(vec![1; 4096 * 4096]),
            true,
        ))];
        let mut decoded_bytes = 0;
        let mut jobs = WindowJobs::new(&items, &mosaic, 33, &mut decoded_bytes).unwrap();
        let count = 4096usize.div_ceil(33) * 4096;
        assert_eq!(jobs.len(), count);
        assert_eq!(decoded_bytes, 4096 * 4096);
        let mut pixels = 0;
        let mut windows = 0;
        while let Some((tile, chunk, samples, stride, overlap)) = jobs.next() {
            assert_eq!(tile, 0);
            assert_eq!(chunk.col_offset as usize - 7, pixels % 4096);
            assert_eq!(chunk.row_offset as usize - 11, pixels / 4096);
            assert_eq!(chunk.height, 1);
            assert!(chunk.width <= 33);
            assert_eq!(samples.bytes(), chunk.width as usize);
            assert_eq!(stride, 4096);
            assert!(overlap);
            pixels += chunk.width as usize;
            windows += 1;
            assert_eq!(jobs.len(), count - windows);
        }
        assert_eq!(windows, count);
        assert_eq!(pixels, 4096 * 4096);
        assert!(jobs.next().is_none());
    }

    #[test]
    fn windows_combine_rows_and_preserve_interleaved_sample_offsets() {
        let (_file, mut mosaic) = test_mosaic();
        mosaic.tiles[0].reader.metadata.samples_per_pixel = 3;
        let items = vec![
            Ok((
                0,
                0,
                RasterChunk {
                    col_offset: 0,
                    row_offset: 0,
                    width: 0,
                    height: 3,
                },
                DecodingResult::U8(Vec::new()),
                false,
            )),
            Ok((
                0,
                0,
                RasterChunk {
                    col_offset: 4,
                    row_offset: 6,
                    width: 3,
                    height: 5,
                },
                DecodingResult::U8((0..45).collect()),
                false,
            )),
            Ok((
                0,
                0,
                RasterChunk {
                    col_offset: 8,
                    row_offset: 9,
                    width: 3,
                    height: 1,
                },
                DecodingResult::U8((45..54).collect()),
                false,
            )),
        ];
        let mut decoded_bytes = 0;
        let jobs = WindowJobs::new(&items, &mosaic, 7, &mut decoded_bytes).unwrap();
        assert_eq!(jobs.len(), 4);
        let windows: Vec<_> = jobs
            .map(|(_, chunk, samples, stride, _)| {
                let super::super::borrowed::BorrowedSamples::U8(samples) = samples else {
                    panic!("wrong sample type")
                };
                assert_eq!(stride, 3);
                (chunk.row_offset, chunk.height, samples.to_vec())
            })
            .collect();
        assert_eq!(
            windows,
            vec![
                (6, 2, (0..18).collect()),
                (8, 2, (18..36).collect()),
                (10, 1, (36..45).collect()),
                (9, 1, (45..54).collect()),
            ]
        );
        assert_eq!(decoded_bytes, 54);
    }

    #[test]
    fn malformed_chunks_fail_before_dispatch() {
        let (_file, mosaic) = test_mosaic();
        let chunk = RasterChunk {
            col_offset: 0,
            row_offset: 0,
            width: 2,
            height: 2,
        };
        let mut items = vec![
            Ok((0, 0, chunk, DecodingResult::U8(vec![0; 4]), false)),
            Ok((0, 0, chunk, DecodingResult::U8(vec![0; 3]), false)),
        ];
        let mut decoded_bytes = 0;
        assert!(matches!(
            WindowJobs::new(&items, &mosaic, 1, &mut decoded_bytes),
            Err(RasterH3Error::InvalidMetadata(_))
        ));
        items[1] = Err(RasterH3Error::StreamFailed("decode failed".into()));
        assert!(matches!(
            WindowJobs::new(&items, &mosaic, 1, &mut decoded_bytes),
            Err(RasterH3Error::StreamFailed(_))
        ));
    }

    #[test]
    fn scheduling_storage_is_bounded_for_half_a_million_windows() {
        struct Job<'a>(&'a AtomicUsize);
        impl Drop for Job<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let live = AtomicUsize::new(0);
        let peak_live = AtomicUsize::new(0);
        let jobs = (0..512_000).map(|_| {
            peak_live.fetch_max(live.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
            Job(&live)
        });
        let run_window = |_: Job<'_>, _: &mut CellMaps<H3Accumulator>| {
            (super::super::profile::WorkerProfile::default(), 0)
        };
        let compute_keys = |_: &[(usize, u64)]| Vec::new();
        let mut shards = vec![ShardedResolutionMap::new()];
        let mut spill_runs = vec![SpillRuns::new(usize::MAX, None)];
        let mut metrics = super::super::profile::StreamProfile::default();
        let mut peak = 0;
        let mut free = (0..8)
            .map(|_| vec![HashMap::with_hasher(H3BuildHasher)])
            .collect();
        let mut finished = Vec::new();
        let mut target = MergeTarget {
            shards: &mut shards,
            spill_runs: &mut spill_runs,
            metrics: &mut metrics,
            peak_active_bytes: &mut peak,
            failure_counter: None,
            compactor_bytes: 0,
            accumulator_limit: usize::MAX,
            map_budget: usize::MAX,
            set_budget: usize::MAX,
            map_sets: 8,
            new_cells: Vec::new(),
            key_chunks: Vec::new(),
        };
        pool.in_place_scope(|scope| {
            run_pipelined(
                scope,
                4,
                jobs,
                &mut free,
                &run_window,
                &compute_keys,
                &mut target,
                &mut finished,
            )
        })
        .unwrap();
        assert_eq!(metrics.jobs, 512_000);
        assert_eq!(live.load(Ordering::Relaxed), 0);
        assert!(peak_live.load(Ordering::Relaxed) <= 8);
        assert_eq!(finished.len(), 8);
        assert_eq!(finished.capacity(), 8);
        assert!(finished.iter().all(Option::is_none));
        assert_eq!(free.len(), 8);
    }

    fn run(jobs: usize, panic_at: Option<usize>) -> std::thread::Result<Result<Vec<usize>>> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let mut shards: Vec<ShardedResolutionMap<H3Accumulator>> =
            vec![ShardedResolutionMap::new()];
        let mut spill_runs = vec![SpillRuns::new(usize::MAX, None)];
        let mut metrics = super::super::profile::StreamProfile::default();
        let mut peak = 0;
        let mut free: Vec<CellMaps<H3Accumulator>> = (0..8)
            .map(|_| vec![HashMap::with_hasher(H3BuildHasher)])
            .collect();
        let order = Mutex::new(Vec::new());
        // Window i adds one cell whose index records i, with a delay that
        // makes later windows finish first.
        let run_window = |index: usize, maps: &mut CellMaps<H3Accumulator>| {
            if Some(index) == panic_at {
                panic!("window {index} failed");
            }
            std::thread::sleep(std::time::Duration::from_micros(
                ((jobs - index) % 5) as u64 * 200,
            ));
            let cell = u64::from(
                h3o::LatLng::new(10.0 + index as f64 * 0.01, 20.0)
                    .unwrap()
                    .to_cell(Resolution::Nine),
            );
            maps[0].insert(cell, H3Accumulator::new(index as f64));
            (super::super::profile::WorkerProfile::default(), 0)
        };
        let compute_keys = |cells: &[(usize, u64)]| -> Vec<CellKeys> {
            cells
                .iter()
                .map(|&(_, c)| {
                    order.lock().unwrap().push(c);
                    CellKeys {
                        key: 0.0,
                        south_lat_upper: 0.0,
                    }
                })
                .collect()
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut target = MergeTarget {
                shards: &mut shards,
                spill_runs: &mut spill_runs,
                metrics: &mut metrics,
                peak_active_bytes: &mut peak,
                failure_counter: None,
                compactor_bytes: 0,
                accumulator_limit: usize::MAX,
                map_budget: usize::MAX,
                set_budget: usize::MAX,
                map_sets: 8,
                new_cells: Vec::new(),
                key_chunks: Vec::new(),
            };
            let result = pool.in_place_scope(|scope| {
                run_pipelined(
                    scope,
                    4,
                    0..jobs,
                    &mut free,
                    &run_window,
                    &compute_keys,
                    &mut target,
                    &mut Vec::new(),
                )
            });
            result.map(|()| {
                let mut chunks = std::mem::take(&mut target.key_chunks);
                chunks.sort_unstable_by_key(|c| c.0);
                // Merge order: the window each new cell came from.
                chunks
                    .into_iter()
                    .flat_map(|(_, cells, _)| cells)
                    .map(|(_, cell)| target.shards[0].get(cell).map(|a| a.sum as usize).unwrap())
                    .collect()
            })
        }))
    }

    #[test]
    fn merges_in_window_order_when_later_windows_finish_first() {
        let order = run(600, None).unwrap().unwrap();
        assert_eq!(order, (0..600).collect::<Vec<_>>());
    }

    #[test]
    fn a_worker_panic_reaches_the_caller() {
        let outcome = run(200, Some(37));
        assert!(outcome.is_err(), "the panic was swallowed");
    }
}
