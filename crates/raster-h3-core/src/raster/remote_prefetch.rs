//! Parallel Asynchronous Chunk Prefetching Queue for Remote COGs
//!
//! Provides high-concurrency background network prefetching with HTTP Range request coalescing.
//! Merges consecutive/nearby chunk byte ranges to cut HTTP round-trips by 2x–5x and streams
//! compressed chunk payloads ahead of scanline processing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    pub queue_capacity: usize,
}

impl Default for RemotePrefetchConfig {
    fn default() -> Self {
        Self {
            num_workers: DEFAULT_PREFETCH_WORKERS,
            max_gap_bytes: DEFAULT_MAX_COALESCE_GAP,
            max_range_bytes: DEFAULT_MAX_COALESCE_BYTES,
            queue_capacity: DEFAULT_PREFETCH_QUEUE_CAPACITY,
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

/// Fetched chunk bytes keyed by (tile, chunk index), shared with the workers.
type ReadyChunks = Arc<Mutex<FxHashMap<(usize, u32), Arc<Vec<u8>>>>>;

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
        let next_job = Arc::new(AtomicUsize::new(0));
        let completed_jobs = Arc::new(AtomicUsize::new(0));
        let error = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let sources_arc = Arc::new(sources);
        let ranges_arc = Arc::new(ranges);

        let num_workers = config.num_workers.clamp(1, 32).min(total_jobs);
        let queue_capacity = config.queue_capacity.max(16);

        let mut handles = Vec::with_capacity(num_workers);

        for _ in 0..num_workers {
            let sources = Arc::clone(&sources_arc);
            let ranges = Arc::clone(&ranges_arc);
            let ready = Arc::clone(&ready_chunks);
            let cv = Arc::clone(&condvar);
            let job_idx = Arc::clone(&next_job);
            let done = Arc::clone(&completed_jobs);
            let err_holder = Arc::clone(&error);
            let term = Arc::clone(&shutdown);

            let handle = thread::spawn(move || {
                struct HttpWorkerPanicGuard {
                    err_holder: Arc<Mutex<Option<String>>>,
                    cv: Arc<Condvar>,
                }
                impl Drop for HttpWorkerPanicGuard {
                    fn drop(&mut self) {
                        if thread::panicking() {
                            if let Ok(mut err) = self.err_holder.lock() {
                                if err.is_none() {
                                    *err = Some("HTTP prefetch worker panicked".to_string());
                                }
                            }
                            self.cv.notify_all();
                        }
                    }
                }

                let _panic_guard = HttpWorkerPanicGuard {
                    err_holder: Arc::clone(&err_holder),
                    cv: Arc::clone(&cv),
                };

                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    loop {
                        if term.load(Ordering::Relaxed) {
                            break;
                        }

                        // Backpressure throttling: pause if buffer is full
                        loop {
                            if term.load(Ordering::Relaxed) {
                                return;
                            }
                            let current_buffered = ready.lock().map(|m| m.len()).unwrap_or(0);
                            if current_buffered < queue_capacity {
                                break;
                            }
                            thread::sleep(Duration::from_millis(5));
                        }

                        let job = job_idx.fetch_add(1, Ordering::Relaxed);
                        if job >= ranges.len() {
                            break;
                        }

                        let range = &ranges[job];
                        let source = match sources.get(range.tile_idx).and_then(|s| s.as_ref()) {
                            Some(s) => s,
                            None => {
                                // Nothing to fetch; still count the range as finished.
                                if let Ok(_map) = ready.lock() {
                                    done.fetch_add(1, Ordering::Relaxed);
                                }
                                cv.notify_all();
                                continue;
                            }
                        };

                        match source.fetch_range(range.start_offset, range.end_offset) {
                            Ok(data) => {
                                let data_len = data.len();
                                let mut map = match ready.lock() {
                                    Ok(m) => m,
                                    Err(_) => break,
                                };

                                for &(chunk_idx, slice_start, slice_len) in &range.chunk_slices {
                                    if slice_start + slice_len <= data_len {
                                        let chunk_payload = Arc::new(
                                            data[slice_start..slice_start + slice_len].to_vec(),
                                        );
                                        map.insert((range.tile_idx, chunk_idx), chunk_payload);
                                    }
                                }
                                done.fetch_add(1, Ordering::Relaxed);
                                drop(map);
                                cv.notify_all();
                            }
                            Err(e) => {
                                if let Ok(mut err) = err_holder.lock() {
                                    if err.is_none() {
                                        *err = Some(e.to_string());
                                    }
                                }
                                cv.notify_all();
                                break;
                            }
                        }
                    }
                }));

                if let Err(payload) = res {
                    let msg = crate::error::panic_payload_to_string(payload);
                    if let Ok(mut err) = err_holder.lock() {
                        if err.is_none() {
                            *err = Some(format!("HTTP prefetch worker panicked: {msg}"));
                        }
                    }
                    cv.notify_all();
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
            _worker_handles: handles,
        }
    }

    /// Retrieve a downloaded chunk payload, blocking until available or error occurs.
    /// Removes the chunk from the ready map to maintain bounded memory footprint.
    pub fn get_chunk_payload(
        &self,
        tile_idx: usize,
        chunk_idx: u32,
    ) -> Result<Option<Arc<Vec<u8>>>> {
        let key = (tile_idx, chunk_idx);
        if !self.scheduled_chunks.contains(&key) {
            return Ok(None);
        }

        let mut map = self.ready_chunks.lock().map_err(|_| {
            RasterH3Error::InvalidParameter("Prefetch queue mutex poisoned".to_string())
        })?;

        loop {
            if let Some(payload) = map.remove(&key) {
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
    pub fn try_get_chunk_payload(&self, tile_idx: usize, chunk_idx: u32) -> Option<Arc<Vec<u8>>> {
        let key = (tile_idx, chunk_idx);
        if !self.scheduled_chunks.contains(&key) {
            return None;
        }
        let mut map = self.ready_chunks.lock().ok()?;
        map.remove(&key)
    }

    /// Returns true if the specified chunk was scheduled for remote prefetching
    pub fn is_chunk_scheduled(&self, tile_idx: usize, chunk_idx: u32) -> bool {
        self.scheduled_chunks.contains(&(tile_idx, chunk_idx))
    }

    /// Total jobs planned for this queue
    pub fn total_jobs(&self) -> usize {
        self.total_jobs
    }

    /// Explicitly signal cancellation, unblocking any waiting consumers or workers.
    pub fn cancel(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.condvar.notify_all();
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

    /// Minimal HTTP range server whose responses for offsets at or past
    /// `gate_offset` are held until the gate opens.
    struct GatedRangeServer {
        url: String,
        gate: Arc<(Mutex<bool>, Condvar)>,
        gated_waiting: Arc<AtomicUsize>,
        requests_by_start: Arc<Mutex<FxHashMap<u64, usize>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl GatedRangeServer {
        fn start(file: Vec<u8>, gate_offset: u64) -> Self {
            use std::io::{BufRead, BufReader, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/test.tif", listener.local_addr().unwrap());
            let file = Arc::new(file);
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let gated_waiting = Arc::new(AtomicUsize::new(0));
            let requests_by_start = Arc::new(Mutex::new(FxHashMap::default()));
            let shutdown = Arc::new(AtomicBool::new(false));

            let (g, w, r, s) = (
                Arc::clone(&gate),
                Arc::clone(&gated_waiting),
                Arc::clone(&requests_by_start),
                Arc::clone(&shutdown),
            );
            let handle = thread::spawn(move || {
                while !s.load(Ordering::SeqCst) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(_) => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                    };
                    let (file, g, w, r) = (
                        Arc::clone(&file),
                        Arc::clone(&g),
                        Arc::clone(&w),
                        Arc::clone(&r),
                    );
                    thread::spawn(move || {
                        let _ = stream.set_nonblocking(false);
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut range = None;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            let line = line.trim_end();
                            if line.is_empty() {
                                break;
                            }
                            if let Some(v) = line
                                .to_ascii_lowercase()
                                .strip_prefix("range: bytes=")
                                .map(str::to_owned)
                            {
                                let (a, b) = v.split_once('-').unwrap();
                                range =
                                    Some((a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap()));
                            }
                        }
                        let total = file.len() as u64;
                        let (start, end) = range.expect("test server expects Range requests");
                        let end = end.min(total - 1);
                        *r.lock().unwrap().entry(start).or_insert(0) += 1;

                        if start >= gate_offset {
                            w.fetch_add(1, Ordering::SeqCst);
                            let (lock, cv) = &*g;
                            let mut open = lock.lock().unwrap();
                            while !*open {
                                open = cv.wait(open).unwrap();
                            }
                        }

                        let body = &file[start as usize..=end as usize];
                        let mut stream = stream;
                        let header = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(body);
                    });
                }
            });

            Self {
                url,
                gate,
                gated_waiting,
                requests_by_start,
                shutdown,
                handle: Some(handle),
            }
        }

        fn open_gate(&self) {
            let (lock, cv) = &*self.gate;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }

        fn requests_for(&self, start: u64) -> usize {
            *self
                .requests_by_start
                .lock()
                .unwrap()
                .get(&start)
                .unwrap_or(&0)
        }
    }

    impl Drop for GatedRangeServer {
        fn drop(&mut self) {
            self.open_gate();
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Regression: once every range has been *claimed* by a worker,
    /// `get_chunk_payload` must keep waiting for ranges still in flight
    /// rather than reporting the chunk as unavailable (which made the decoder
    /// re-fetch the same bytes and orphaned the late payload in the map).
    #[test]
    fn test_waits_for_in_flight_ranges_after_all_jobs_claimed() {
        const GATE: u64 = 200_000;
        let file: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let server = GatedRangeServer::start(file.clone(), GATE);
        let source = Arc::new(RemoteHttpSource::open(&server.url).expect("open test source"));

        // Two chunks far enough apart to stay in separate ranges.
        let chunks = [
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 0,
                offset: 200_000,
                length: 1_000,
            },
            ChunkLocation {
                tile_idx: 0,
                chunk_idx: 1,
                offset: 600_000,
                length: 1_000,
            },
        ];
        let cfg = RemotePrefetchConfig {
            num_workers: 2,
            ..Default::default()
        };
        let ranges = coalesce_chunk_ranges(&chunks, cfg.max_gap_bytes, cfg.max_range_bytes);
        assert_eq!(ranges.len(), 2);
        let queue = RemoteChunkPrefetchQueue::spawn_internal(vec![Some(source)], ranges, cfg);

        // Wait until both ranges are claimed and blocked in flight.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while server.gated_waiting.load(Ordering::SeqCst) < 2 {
            if std::time::Instant::now() >= deadline {
                server.open_gate();
                panic!("ranges were never requested");
            }
            thread::sleep(Duration::from_millis(2));
        }

        let early = thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            let q = &queue;
            s.spawn(move || {
                let result = q
                    .get_chunk_payload(0, 1)
                    .map(|p| p.map(|v| v.to_vec()))
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
            });
            // The consumer must still be waiting while the response is held.
            let early = rx.recv_timeout(Duration::from_millis(200)).ok();
            server.open_gate();
            let late = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("consumer never returned");
            (early, late)
        });
        let (early, late) = early;

        assert!(
            early.is_none(),
            "get_chunk_payload returned {early:?} while its range was still in flight"
        );
        assert_eq!(late.unwrap().as_deref(), Some(&file[600_000..601_000]));
        assert_eq!(
            queue
                .get_chunk_payload(0, 0)
                .unwrap()
                .as_deref()
                .map(|v| v.as_slice()),
            Some(&file[200_000..201_000])
        );
        assert_eq!(server.requests_for(200_000), 1);
        assert_eq!(server.requests_for(600_000), 1);
    }
}
