//! Worker windows and the pipeline that merges their map sets in window order.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::super::eviction_space::CellKeys;
use super::super::sharded_map::ShardedResolutionMap;
use super::super::spill::{SpillAccumulator, SpillRuns};
use crate::aggregator::cell_hash::H3BuildHasher;
use crate::error::{RasterH3Error, Result};
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::MosaicPrefetchItem;
use crate::raster::RasterChunk;

/// One worker window: (tile index, window bounds, borrowed samples, row stride, overlaps another tile).
pub(super) type WindowJob<'a> = (
    usize,
    RasterChunk,
    super::super::borrowed::BorrowedSamples<'a>,
    u32,
    bool,
);
/// One worker's per-resolution cell maps.
pub(super) type CellMaps<A> = Vec<HashMap<u64, A, H3BuildHasher>>;

/// A batch's windows, generated on demand without storage per window.
pub(super) struct WindowJobs<'a> {
    items: &'a [MosaicPrefetchItem],
    mosaic: &'a MosaicReader,
    worker_pixels: usize,
    item: usize,
    row: usize,
    col: usize,
    remaining: usize,
}

impl<'a> WindowJobs<'a> {
    pub(super) fn new(
        items: &'a [MosaicPrefetchItem],
        mosaic: &'a MosaicReader,
        worker_pixels: usize,
        decoded_bytes: &mut u64,
    ) -> Result<Self> {
        use super::super::borrowed::BorrowedSamples;
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
        use super::super::borrowed::BorrowedSamples;
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

/// Dedicated aggregation pool of `threads` workers, shared by all live
/// streams requesting the same size so concurrent queries do not each spawn
/// their own threads.
pub(super) fn shared_aggregation_pool(threads: usize) -> Result<Arc<rayon::ThreadPool>> {
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
pub(super) fn worker_map_fixed_bytes<A>() -> usize {
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
pub(super) type KeyChunk = (usize, Vec<(usize, u64)>, Vec<CellKeys>);

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
        profile: super::super::profile::WorkerProfile,
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
pub(super) fn spill_resolution<A: SpillAccumulator>(
    shards: &mut [ShardedResolutionMap<A>],
    spill_runs: &mut [SpillRuns<A>],
    metrics: &mut super::super::profile::StreamProfile,
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
pub(super) struct MergeTarget<'a, A: SpillAccumulator> {
    pub(super) shards: &'a mut Vec<ShardedResolutionMap<A>>,
    pub(super) spill_runs: &'a mut Vec<SpillRuns<A>>,
    pub(super) metrics: &'a mut super::super::profile::StreamProfile,
    pub(super) peak_active_bytes: &'a mut usize,
    pub(super) failure_counter: Option<&'a Arc<std::sync::atomic::AtomicU64>>,
    pub(super) compactor_bytes: usize,
    pub(super) accumulator_limit: usize,
    pub(super) map_budget: usize,
    /// Worker storage allowance of one map set.
    pub(super) set_budget: usize,
    pub(super) map_sets: usize,
    /// Cells inserted into a horizon map that still need eviction keys.
    pub(super) new_cells: Vec<(usize, u64)>,
    /// Computed keys: (chunk number, cells, keys).
    pub(super) key_chunks: Vec<KeyChunk>,
}

impl<A: SpillAccumulator> MergeTarget<'_, A> {
    fn record(&mut self, profile: super::super::profile::WorkerProfile, ns: u64) {
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
                super::super::spill::table_bytes::<u64, A>(m.capacity())
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
pub(super) fn run_serial<A, J, W, C>(
    jobs: impl Iterator<Item = J>,
    free: &mut Vec<CellMaps<A>>,
    run_window: &W,
    compute_keys: &C,
    target: &mut MergeTarget<'_, A>,
) -> Result<()>
where
    A: SpillAccumulator,
    W: Fn(J, &mut CellMaps<A>) -> (super::super::profile::WorkerProfile, u64),
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
pub(super) fn run_pipelined<'s, A, J, W, C>(
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
    W: Fn(J, &mut CellMaps<A>) -> (super::super::profile::WorkerProfile, u64) + Sync,
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
