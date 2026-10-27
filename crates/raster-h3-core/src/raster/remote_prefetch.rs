//! Parallel Asynchronous Chunk Prefetching Queue for Remote COGs
//!
//! Provides high-concurrency background network prefetching with HTTP Range request coalescing.
//! Merges consecutive/nearby chunk byte ranges to cut HTTP round-trips by 2x–5x and streams
//! compressed chunk payloads ahead of scanline processing.
//!
//! Memory is bounded by bytes: before each request a worker reserves the
//! range's size against [`RemotePrefetchConfig::max_buffered_bytes`], and the
//! reservation lasts until the last chunk sliced from that response is
//! dropped. Chunks share their response buffer instead of being copied.

use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::http_range::RemoteHttpSource;
use crate::raster::mosaic::MosaicReader;

/// Default configuration parameters for remote prefetching
pub const DEFAULT_PREFETCH_WORKERS: usize = 8;
pub const DEFAULT_MAX_COALESCE_GAP: u64 = 32768; // 32 KB
pub const DEFAULT_MAX_COALESCE_BYTES: u64 = 2 * 1024 * 1024; // 2 MB
pub const DEFAULT_PREFETCH_QUEUE_CAPACITY: usize = 64;
/// Default cap on response bytes held: requests in flight plus fetched
/// responses still referenced (enough for 8 workers × 2 MB ranges, twice).
pub const DEFAULT_PREFETCH_BUDGET_BYTES: u64 = 32 * 1024 * 1024;

/// Location of a chunk in its container file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLocation {
    pub tile_idx: usize,
    pub chunk_idx: u32,
    pub offset: u64,
    pub length: u64,
}

/// A coalesced byte range merging one or more nearby chunks into a single HTTP request
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedRange {
    pub tile_idx: usize,
    pub start_offset: u64,
    pub end_offset: u64, // inclusive: start..=end
    /// Slices inside the fetched response: (chunk_idx, slice_start, slice_len)
    pub chunk_slices: Vec<(u32, usize, usize)>,
}

/// Configuration for the asynchronous remote prefetch queue
#[derive(Debug, Clone)]
pub struct RemotePrefetchConfig {
    pub num_workers: usize,
    pub max_gap_bytes: u64,
    pub max_range_bytes: u64,
    /// Fetched chunks that may wait to be taken (at least 16). Checked before
    /// each request, so one coalesced response can exceed it.
    pub queue_capacity: usize,
    /// Response bytes the queue may hold: requests in flight plus fetched
    /// responses still referenced by untaken chunks or by callers. A single
    /// range larger than this is still fetched, but only when nothing else is
    /// held.
    pub max_buffered_bytes: u64,
}

impl Default for RemotePrefetchConfig {
    fn default() -> Self {
        Self {
            num_workers: DEFAULT_PREFETCH_WORKERS,
            max_gap_bytes: DEFAULT_MAX_COALESCE_GAP,
            max_range_bytes: DEFAULT_MAX_COALESCE_BYTES,
            queue_capacity: DEFAULT_PREFETCH_QUEUE_CAPACITY,
            max_buffered_bytes: DEFAULT_PREFETCH_BUDGET_BYTES,
        }
    }
}

/// Group consecutive or near-consecutive chunk byte ranges into coalesced requests.
///
/// Chunks are coalesced when they belong to the same tile, `next_offset >= current_end + 1`,
/// the gap between them does not exceed `max_gap_bytes`, and the total merged byte length
/// does not exceed `max_range_bytes`.
pub fn coalesce_chunk_ranges(
    chunks: &[ChunkLocation],
    max_gap_bytes: u64,
    max_range_bytes: u64,
) -> Vec<CoalescedRange> {
    if chunks.is_empty() {
        return Vec::new();
    }

    let mut ranges: Vec<CoalescedRange> = Vec::new();
    let mut current_range: Option<CoalescedRange> = None;

    for chunk in chunks {
        if chunk.length == 0 {
            continue;
        }

        let chunk_end = chunk.offset + chunk.length - 1;

        if let Some(ref mut curr) = current_range {
            // Must belong to the same tile to coalesce
            let same_tile = curr.tile_idx == chunk.tile_idx;
            let monotonic_offset = chunk.offset > curr.end_offset;

            if same_tile && monotonic_offset {
                let gap = chunk.offset - (curr.end_offset + 1);
                let new_total_len = (chunk_end + 1) - curr.start_offset;

                if gap <= max_gap_bytes && new_total_len <= max_range_bytes {
                    let slice_start = (chunk.offset - curr.start_offset) as usize;
                    let slice_len = chunk.length as usize;
                    curr.end_offset = chunk_end;
                    curr.chunk_slices
                        .push((chunk.chunk_idx, slice_start, slice_len));
                    continue;
                }
            }

            // Finish current range and prepare to start new range
            ranges.push(curr.clone());
        }

        current_range = Some(CoalescedRange {
            tile_idx: chunk.tile_idx,
            start_offset: chunk.offset,
            end_offset: chunk_end,
            chunk_slices: vec![(chunk.chunk_idx, 0, chunk.length as usize)],
        });
    }

    if let Some(last) = current_range {
        ranges.push(last);
    }

    ranges
}

/// Byte budget shared by workers and every live response buffer. Kept apart
/// from the ready map so buffers can release it without a reference cycle.
#[derive(Default)]
struct Budget {
    state: Mutex<BudgetState>,
    /// Signalled when bytes or chunk slots are released, or on cancellation.
    condvar: Condvar,
}

#[derive(Default)]
struct BudgetState {
    /// Bytes reserved by requests in flight plus fetched responses still alive.
    held_bytes: u64,
    peak_held_bytes: u64,
    /// Chunks in the ready map not yet taken by a consumer.
    buffered_chunks: usize,
    /// Next range to claim. Ranges are claimed strictly in order, so the
    /// earliest range any consumer can be waiting for is always claimed
    /// before a later one can take budget.
    next_job: usize,
}

impl Budget {
    fn lock(&self) -> MutexGuard<'_, BudgetState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release(&self, bytes: u64) {
        let mut state = self.lock();
        state.held_bytes = state.held_bytes.saturating_sub(bytes);
        drop(state);
        self.condvar.notify_all();
    }

    fn chunks_taken(&self, count: usize) {
        let mut state = self.lock();
        state.buffered_chunks = state.buffered_chunks.saturating_sub(count);
        drop(state);
        self.condvar.notify_all();
    }

    /// Wake waiting workers after `shutdown` changes.
    fn wake(&self) {
        drop(self.lock());
        self.condvar.notify_all();
    }
}

/// Bytes charged to a [`Budget`], released when dropped.
struct BudgetCharge {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Drop for BudgetCharge {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

/// One fetched (possibly coalesced) HTTP response, shared by every chunk
/// sliced from it; its size stays charged until the last slice is dropped.
struct ResponseBuffer {
    bytes: Vec<u8>,
    _charge: BudgetCharge,
}

/// A prefetched chunk: a slice of a shared response buffer.
///
/// Dereferences to the chunk's bytes and is cheap to clone. The whole
/// response stays allocated, and counted against
/// [`RemotePrefetchConfig::max_buffered_bytes`], while any chunk sliced from
/// it is alive, so drop chunks once they are decoded.
#[derive(Clone)]
pub struct PrefetchedChunk {
    buffer: Arc<ResponseBuffer>,
    start: usize,
    len: usize,
}

impl PrefetchedChunk {
    /// Length of the shared response this chunk was sliced from.
    pub fn response_len(&self) -> usize {
        self.buffer.bytes.len()
    }
}

impl Deref for PrefetchedChunk {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buffer.bytes[self.start..self.start + self.len]
    }
}

impl AsRef<[u8]> for PrefetchedChunk {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl PartialEq for PrefetchedChunk {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for PrefetchedChunk {}

impl fmt::Debug for PrefetchedChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrefetchedChunk")
            .field("len", &self.len)
            .field("response_len", &self.response_len())
            .finish()
    }
}

/// Fetched chunks keyed by (tile, chunk index), shared with the workers.
type ReadyChunks = Arc<Mutex<FxHashMap<(usize, u32), PrefetchedChunk>>>;

/// Bytes requested for a range (`end_offset` is inclusive).
fn range_bytes(range: &CoalescedRange) -> u64 {
    range.end_offset - range.start_offset + 1
}

fn publish_error(
    ready: &ReadyChunks,
    error: &Mutex<Option<String>>,
    condvar: &Condvar,
    message: String,
) {
    // Serialize terminal predicate updates with the consumer's check and wait.
    let map = ready
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Ok(mut error) = error.lock() {
        if error.is_none() {
            *error = Some(message);
        }
    }
    drop(map);
    condvar.notify_all();
}

/// Parallel asynchronous prefetch queue managing concurrent HTTP Range requests
pub struct RemoteChunkPrefetchQueue {
    ready_chunks: ReadyChunks,
    condvar: Arc<Condvar>,
    /// Ranges whose fetch has finished and whose chunks are in `ready_chunks`.
    /// Updated while holding the `ready_chunks` lock so waiting consumers
    /// observe it consistently with the map.
    completed_jobs: Arc<AtomicUsize>,
    total_jobs: usize,
    scheduled_chunks: FxHashSet<(usize, u32)>,
    error: Arc<Mutex<Option<String>>>,
    shutdown: Arc<AtomicBool>,
    budget: Arc<Budget>,
    _worker_handles: Vec<JoinHandle<()>>,
}

impl RemoteChunkPrefetchQueue {
    /// Spawn background prefetch queue for a single remote GeoTIFF reader
    pub fn spawn_single(
        reader: &GeoTiffStreamReader,
        chunk_indices: &[u32],
        config: Option<RemotePrefetchConfig>,
    ) -> Option<Self> {
        let remote_source = reader.remote_source()?;
        let info = reader.chunk_info.as_ref()?;
        let cfg = config.unwrap_or_default();

        let mut locations = Vec::with_capacity(chunk_indices.len());
        for &chunk_idx in chunk_indices {
            let idx = chunk_idx as usize;
            if let (Some(&offset), Some(&length)) =
                (info.chunk_offsets.get(idx), info.chunk_bytes.get(idx))
            {
                locations.push(ChunkLocation {
                    tile_idx: 0,
                    chunk_idx,
                    offset,
                    length,
                });
            }
        }

        if locations.is_empty() {
            return None;
        }

        let coalesced = coalesce_chunk_ranges(&locations, cfg.max_gap_bytes, cfg.max_range_bytes);
        if coalesced.is_empty() {
            return None;
        }
        let sources = vec![Some(Arc::clone(remote_source))];
        Some(Self::spawn_internal(sources, coalesced, cfg))
    }

    /// Spawn background prefetch queue for a mosaic containing remote tiles
    pub fn spawn_mosaic(
        mosaic: &MosaicReader,
        config: Option<RemotePrefetchConfig>,
    ) -> Option<Self> {
        let mut has_remote = false;
        let mut sources = Vec::with_capacity(mosaic.tiles.len());

        for tile in &mosaic.tiles {
            if let Some(src) = tile.reader.remote_source() {
                has_remote = true;
                sources.push(Some(Arc::clone(src)));
            } else {
                sources.push(None);
            }
        }

        if !has_remote {
            return None;
        }

        let cfg = config.unwrap_or_default();
        let mut locations = Vec::with_capacity(mosaic.chunk_refs.len());

        for chunk_ref in &mosaic.chunk_refs {
            let tile_idx = chunk_ref.tile_idx;
            if sources[tile_idx].is_none() {
                continue; // Skip local tiles, they stream zero-copy from mmap
            }

            let reader = &mosaic.tiles[tile_idx].reader;
            if let Some(ref info) = reader.chunk_info {
                let idx = chunk_ref.chunk_idx as usize;
                if let (Some(&offset), Some(&length)) =
                    (info.chunk_offsets.get(idx), info.chunk_bytes.get(idx))
                {
                    locations.push(ChunkLocation {
                        tile_idx,
                        chunk_idx: chunk_ref.chunk_idx,
                        offset,
                        length,
                    });
                }
            }
        }

        if locations.is_empty() {
            return None;
        }

        let coalesced = coalesce_chunk_ranges(&locations, cfg.max_gap_bytes, cfg.max_range_bytes);
        if coalesced.is_empty() {
            return None;
        }
        Some(Self::spawn_internal(sources, coalesced, cfg))
    }

    fn spawn_internal(
        sources: Vec<Option<Arc<RemoteHttpSource>>>,
        ranges: Vec<CoalescedRange>,
        config: RemotePrefetchConfig,
    ) -> Self {
        let total_jobs = ranges.len();
        let mut scheduled_chunks = FxHashSet::default();
        for range in &ranges {
            for &(chunk_idx, _, _) in &range.chunk_slices {
                scheduled_chunks.insert((range.tile_idx, chunk_idx));
            }
        }

        let ready_chunks = Arc::new(Mutex::new(FxHashMap::default()));
        let condvar = Arc::new(Condvar::new());
        let budget = Arc::new(Budget::default());
        let completed_jobs = Arc::new(AtomicUsize::new(0));
        let error = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let sources_arc = Arc::new(sources);
        let ranges_arc = Arc::new(ranges);

        let num_workers = config.num_workers.clamp(1, 32).min(total_jobs);
        let queue_capacity = config.queue_capacity.max(16);
        let max_bytes = config.max_buffered_bytes;

        let mut handles = Vec::with_capacity(num_workers);

        for _ in 0..num_workers {
            let sources = Arc::clone(&sources_arc);
            let ranges = Arc::clone(&ranges_arc);
            let ready = Arc::clone(&ready_chunks);
            let cv = Arc::clone(&condvar);
            let budget = Arc::clone(&budget);
            let done = Arc::clone(&completed_jobs);
            let err_holder = Arc::clone(&error);
            let term = Arc::clone(&shutdown);

            let handle = thread::spawn(move || {
                struct HttpWorkerPanicGuard {
                    ready: ReadyChunks,
                    err_holder: Arc<Mutex<Option<String>>>,
                    cv: Arc<Condvar>,
                }
                impl Drop for HttpWorkerPanicGuard {
                    fn drop(&mut self) {
                        if thread::panicking() {
                            publish_error(
                                &self.ready,
                                &self.err_holder,
                                &self.cv,
                                "HTTP prefetch worker panicked".to_string(),
                            );
                        }
                    }
                }

                let _panic_guard = HttpWorkerPanicGuard {
                    ready: Arc::clone(&ready),
                    err_holder: Arc::clone(&err_holder),
                    cv: Arc::clone(&cv),
                };

                // Claim the next range in order once the budget admits it,
                // reserving its size until its response buffer is dropped.
                let claim = || -> Option<(usize, BudgetCharge)> {
                    let mut state = budget.lock();
                    loop {
                        if term.load(Ordering::SeqCst) || state.next_job >= ranges.len() {
                            return None;
                        }
                        let need = range_bytes(&ranges[state.next_job]);
                        // With nothing held, take the range even if it alone
                        // exceeds the budget; otherwise it could never run.
                        let fits = state.held_bytes == 0
                            || (state.held_bytes + need <= max_bytes
                                && state.buffered_chunks < queue_capacity);
                        if fits {
                            let job = state.next_job;
                            state.next_job += 1;
                            state.held_bytes += need;
                            state.peak_held_bytes = state.peak_held_bytes.max(state.held_bytes);
                            let charge = BudgetCharge {
                                budget: Arc::clone(&budget),
                                bytes: need,
                            };
                            return Some((job, charge));
                        }
                        state = budget
                            .condvar
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                };

                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while let Some((job, charge)) = claim() {
                        let range = &ranges[job];
                        let source = match sources.get(range.tile_idx).and_then(|s| s.as_ref()) {
                            Some(s) => s,
                            None => {
                                // Nothing to fetch; still count the range as finished.
                                drop(charge);
                                if let Ok(_map) = ready.lock() {
                                    done.fetch_add(1, Ordering::Relaxed);
                                }
                                cv.notify_all();
                                continue;
                            }
                        };

                        match source.fetch_range(range.start_offset, range.end_offset) {
                            Ok(bytes) => {
                                let data_len = bytes.len();
                                let buffer = Arc::new(ResponseBuffer {
                                    bytes,
                                    _charge: charge,
                                });
                                let chunks: Vec<((usize, u32), PrefetchedChunk)> = range
                                    .chunk_slices
                                    .iter()
                                    .filter(|&&(_, start, len)| start + len <= data_len)
                                    .map(|&(chunk_idx, start, len)| {
                                        let chunk = PrefetchedChunk {
                                            buffer: Arc::clone(&buffer),
                                            start,
                                            len,
                                        };
                                        ((range.tile_idx, chunk_idx), chunk)
                                    })
                                    .collect();
                                // From here only the chunks own the response, so
                                // its bytes are released as soon as the last one
                                // is dropped (or now, outside any lock, if none).
                                drop(buffer);
                                // Count before inserting so a fast consumer
                                // cannot take a chunk that is not yet counted.
                                budget.lock().buffered_chunks += chunks.len();
                                let mut replaced = Vec::new();
                                let mut map = match ready.lock() {
                                    Ok(m) => m,
                                    Err(_) => break,
                                };
                                for (key, chunk) in chunks {
                                    replaced.extend(map.insert(key, chunk));
                                }
                                done.fetch_add(1, Ordering::Relaxed);
                                drop(map);
                                cv.notify_all();
                                // Drop displaced chunks outside the ready lock:
                                // releasing budget takes the budget lock.
                                drop(replaced);
                            }
                            Err(e) => {
                                drop(charge);
                                publish_error(&ready, &err_holder, &cv, e.to_string());
                                break;
                            }
                        }
                    }
                }));

                if let Err(payload) = res {
                    let msg = crate::error::panic_payload_to_string(payload);
                    publish_error(
                        &ready,
                        &err_holder,
                        &cv,
                        format!("HTTP prefetch worker panicked: {msg}"),
                    );
                }
            });

            handles.push(handle);
        }

        Self {
            ready_chunks,
            condvar,
            completed_jobs,
            total_jobs,
            scheduled_chunks,
            error,
            shutdown,
            budget,
            _worker_handles: handles,
        }
    }

    /// Retrieve a downloaded chunk payload, blocking until available or error occurs.
    /// Removes the chunk from the ready map to maintain bounded memory footprint.
    pub fn get_chunk_payload(
        &self,
        tile_idx: usize,
        chunk_idx: u32,
    ) -> Result<Option<PrefetchedChunk>> {
        let key = (tile_idx, chunk_idx);
        if !self.scheduled_chunks.contains(&key) {
            return Ok(None);
        }

        let mut map = self.ready_chunks.lock().map_err(|_| {
            RasterH3Error::InvalidParameter("Prefetch queue mutex poisoned".to_string())
        })?;

        loop {
            if let Some(payload) = map.remove(&key) {
                drop(map);
                self.budget.chunks_taken(1);
                return Ok(Some(payload));
            }

            if let Ok(err_guard) = self.error.lock() {
                if let Some(ref e) = *err_guard {
                    return Err(RasterH3Error::InvalidParameter(e.clone()));
                }
            }

            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(None);
            }

            // Only give up once every range has finished fetching. Ranges that
            // are merely claimed may still be in flight and deliver this chunk.
            if self.completed_jobs.load(Ordering::Relaxed) >= self.total_jobs {
                return Ok(None);
            }

            map = self.condvar.wait(map).map_err(|_| {
                RasterH3Error::InvalidParameter("Prefetch queue condvar poisoned".to_string())
            })?;
        }
    }

    /// Non-blocking check for a ready chunk payload
    pub fn try_get_chunk_payload(
        &self,
        tile_idx: usize,
        chunk_idx: u32,
    ) -> Option<PrefetchedChunk> {
        let key = (tile_idx, chunk_idx);
        if !self.scheduled_chunks.contains(&key) {
            return None;
        }
        let mut map = self.ready_chunks.lock().ok()?;
        let payload = map.remove(&key);
        drop(map);
        if payload.is_some() {
            self.budget.chunks_taken(1);
        }
        payload
    }

    /// Returns true if the specified chunk was scheduled for remote prefetching
    pub fn is_chunk_scheduled(&self, tile_idx: usize, chunk_idx: u32) -> bool {
        self.scheduled_chunks.contains(&(tile_idx, chunk_idx))
    }

    /// Total jobs planned for this queue
    pub fn total_jobs(&self) -> usize {
        self.total_jobs
    }

    /// Response bytes currently held: requests in flight plus fetched
    /// responses still referenced by untaken chunks or by callers.
    pub fn held_bytes(&self) -> u64 {
        self.budget.lock().held_bytes
    }

    /// Highest [`Self::held_bytes`] observed so far.
    pub fn peak_held_bytes(&self) -> u64 {
        self.budget.lock().peak_held_bytes
    }

    /// Explicitly signal cancellation, unblocking any waiting consumers or workers.
    pub fn cancel(&self) {
        let map = self
            .ready_chunks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shutdown.store(true, Ordering::SeqCst);
        drop(map);
        self.condvar.notify_all();
        // Workers waiting for budget re-check `shutdown` under the budget lock.
        self.budget.wake();
    }
}

impl Drop for RemoteChunkPrefetchQueue {
    fn drop(&mut self) {
        self.cancel();
        for handle in self._worker_handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_terminal_updates_hold_ready_chunks_lock() {
        for cancel in [false, true] {
            let queue = Arc::new(RemoteChunkPrefetchQueue::spawn_internal(
                vec![],
                vec![],
                RemotePrefetchConfig::default(),
            ));
            // The check-to-wait gap is only accessible through this private mutex.
            let map = queue.ready_chunks.lock().unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let worker_queue = Arc::clone(&queue);
            let worker = thread::spawn(move || {
                started_tx.send(()).unwrap();
                if cancel {
                    worker_queue.cancel();
                } else {
                    publish_error(
                        &worker_queue.ready_chunks,
                        &worker_queue.error,
                        &worker_queue.condvar,
                        "fetch failed".to_string(),
                    );
                }
                done_tx.send(()).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let early = done_rx.recv_timeout(Duration::from_millis(200));
            let error_before_unlock = queue.error.lock().unwrap().clone();
            let shutdown_before_unlock = queue.shutdown.load(Ordering::SeqCst);
            drop(map);
            if early.is_err() {
                done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            worker.join().unwrap();
            assert!(matches!(
                early,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(error_before_unlock, None);
            assert!(!shutdown_before_unlock);
            if cancel {
                assert!(queue.shutdown.load(Ordering::SeqCst));
            } else {
                assert_eq!(queue.error.lock().unwrap().as_deref(), Some("fetch failed"));
            }
        }
    }

    #[test]
    fn test_coalesce_adjacent_chunks() {
        let chunks = vec![
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 0,
                offset: 1000,
                length: 4000,
            },
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 1,
                offset: 5000,
                length: 4000,
            },
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 2,
                offset: 9000,
                length: 2000,
            },
        ];

        let ranges = coalesce_chunk_ranges(&chunks, 32768, 2 * 1024 * 1024);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start_offset, 1000);
        assert_eq!(ranges[0].end_offset, 10999);
        assert_eq!(ranges[0].chunk_slices.len(), 3);
        assert_eq!(ranges[0].chunk_slices[0], (0, 0, 4000));
        assert_eq!(ranges[0].chunk_slices[1], (1, 4000, 4000));
        assert_eq!(ranges[0].chunk_slices[2], (2, 8000, 2000));
    }

    #[test]
    fn test_coalesce_with_gap() {
        let chunks = vec![
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 0,
                offset: 1000,
                length: 4000,
            }, // 1000..4999
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 1,
                offset: 6000,
                length: 4000,
            }, // gap = 1000 (<= 32KB)
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 2,
                offset: 50000,
                length: 4000,
            }, // gap = 40000 (> 32KB) -> split
        ];

        let ranges = coalesce_chunk_ranges(&chunks, 32768, 2 * 1024 * 1024);
        assert_eq!(ranges.len(), 2);

        // Range 0: chunks 0 and 1
        assert_eq!(ranges[0].start_offset, 1000);
        assert_eq!(ranges[0].end_offset, 9999);
        assert_eq!(ranges[0].chunk_slices.len(), 2);
        assert_eq!(ranges[0].chunk_slices[0], (0, 0, 4000));
        assert_eq!(ranges[0].chunk_slices[1], (1, 5000, 4000));

        // Range 1: chunk 2
        assert_eq!(ranges[1].start_offset, 50000);
        assert_eq!(ranges[1].end_offset, 53999);
        assert_eq!(ranges[1].chunk_slices.len(), 1);
        assert_eq!(ranges[1].chunk_slices[0], (2, 0, 4000));
    }

    #[test]
    fn test_coalesce_max_range_limit() {
        let chunks = vec![
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 0,
                offset: 0,
                length: 600,
            },
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 1,
                offset: 600,
                length: 600,
            }, // cumulative 1200 > max_range 1000 -> split
        ];

        let ranges = coalesce_chunk_ranges(&chunks, 1024, 1000);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start_offset, 0);
        assert_eq!(ranges[0].end_offset, 599);
        assert_eq!(ranges[1].start_offset, 600);
        assert_eq!(ranges[1].end_offset, 1199);
    }

    #[test]
    fn test_coalesce_multi_tile_split() {
        let chunks = vec![
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 0,
                offset: 1000,
                length: 4000,
            },
            ChunkLocation {
                tile_idx: 1,
                chunk_idx: 0,
                offset: 5000,
                length: 4000,
            }, // different tile -> split
        ];

        let ranges = coalesce_chunk_ranges(&chunks, 32768, 2 * 1024 * 1024);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].tile_idx, 0);
        assert_eq!(ranges[1].tile_idx, 1);
    }

    #[test]
    fn test_coalesce_skips_zero_byte_chunks() {
        let chunks = vec![
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 0,
                offset: 1000,
                length: 0, // Sparse/empty chunk
            },
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 1,
                offset: 1000,
                length: 4000,
            },
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 2,
                offset: 5000,
                length: 0, // Trailing empty chunk
            },
        ];

        let ranges = coalesce_chunk_ranges(&chunks, 32768, 2 * 1024 * 1024);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].chunk_slices.len(), 1);
        assert_eq!(ranges[0].chunk_slices[0], (1, 0, 4000));
    }

    #[test]
    fn test_unscheduled_chunk_fast_path_returns_none() {
        let ranges = vec![CoalescedRange {
            tile_idx: 0,
            start_offset: 1000,
            end_offset: 4999,
            chunk_slices: vec![(1, 0, 4000)],
        }];

        let queue = RemoteChunkPrefetchQueue::spawn_internal(
            vec![None], // No network source needed for unscheduled query
            ranges,
            RemotePrefetchConfig::default(),
        );

        // Chunk 1 is scheduled
        assert!(queue.is_chunk_scheduled(0, 1));
        // Chunk 0 (e.g. zero-length sparse chunk) was not scheduled
        assert!(!queue.is_chunk_scheduled(0, 0));
        // Local tile chunk on tile 1 was not scheduled
        assert!(!queue.is_chunk_scheduled(1, 0));

        // get_chunk_payload must return Ok(None) immediately without blocking
        assert_eq!(queue.get_chunk_payload(0, 0).unwrap(), None);
        assert_eq!(queue.get_chunk_payload(1, 0).unwrap(), None);
        assert_eq!(queue.try_get_chunk_payload(0, 0), None);
        assert_eq!(queue.try_get_chunk_payload(1, 0), None);
    }
}
