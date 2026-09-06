//! Parallel Asynchronous Chunk Prefetching Queue for Remote COGs
//!
//! Provides high-concurrency background network prefetching with HTTP Range request coalescing.
//! Merges consecutive/nearby chunk byte ranges to cut HTTP round-trips by 2x–5x and streams
//! compressed chunk payloads ahead of scanline processing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fxhash::FxHashMap;

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
            let monotonic_offset = chunk.offset >= curr.end_offset + 1;

            if same_tile && monotonic_offset {
                let gap = chunk.offset - (curr.end_offset + 1);
                let new_total_len = (chunk_end + 1) - curr.start_offset;

                if gap <= max_gap_bytes && new_total_len <= max_range_bytes {
                    let slice_start = (chunk.offset - curr.start_offset) as usize;
                    let slice_len = chunk.length as usize;
                    curr.end_offset = chunk_end;
                    curr.chunk_slices.push((chunk.chunk_idx, slice_start, slice_len));
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

/// Parallel asynchronous prefetch queue managing concurrent HTTP Range requests
pub struct RemoteChunkPrefetchQueue {
    ready_chunks: Arc<Mutex<FxHashMap<(usize, u32), Arc<Vec<u8>>>>>,
    condvar: Arc<Condvar>,
    next_job: Arc<AtomicUsize>,
    total_jobs: usize,
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
            if let (Some(&offset), Some(&length)) = (info.chunk_offsets.get(idx), info.chunk_bytes.get(idx)) {
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
                if let (Some(&offset), Some(&length)) = (info.chunk_offsets.get(idx), info.chunk_bytes.get(idx)) {
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
        Some(Self::spawn_internal(sources, coalesced, cfg))
    }

    fn spawn_internal(
        sources: Vec<Option<Arc<RemoteHttpSource>>>,
        ranges: Vec<CoalescedRange>,
        config: RemotePrefetchConfig,
    ) -> Self {
        let total_jobs = ranges.len();
        let ready_chunks = Arc::new(Mutex::new(FxHashMap::default()));
        let condvar = Arc::new(Condvar::new());
        let next_job = Arc::new(AtomicUsize::new(0));
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
            let err_holder = Arc::clone(&error);
            let term = Arc::clone(&shutdown);

            let handle = thread::spawn(move || {
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
                        None => continue,
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
            });

            handles.push(handle);
        }

        Self {
            ready_chunks,
            condvar,
            next_job,
            total_jobs,
            error,
            shutdown,
            _worker_handles: handles,
        }
    }

    /// Retrieve a downloaded chunk payload, blocking until available or error occurs.
    /// Removes the chunk from the ready map to maintain bounded memory footprint.
    pub fn get_chunk_payload(&self, tile_idx: usize, chunk_idx: u32) -> Result<Option<Arc<Vec<u8>>>> {
        let key = (tile_idx, chunk_idx);
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

            // If all ranges have been fetched and the chunk was not found, return None
            if self.next_job.load(Ordering::Relaxed) >= self.total_jobs && !map.contains_key(&key) {
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
        let mut map = self.ready_chunks.lock().ok()?;
        map.remove(&key)
    }

    /// Total jobs planned for this queue
    pub fn total_jobs(&self) -> usize {
        self.total_jobs
    }
}

impl Drop for RemoteChunkPrefetchQueue {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.condvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coalesce_adjacent_chunks() {
        let chunks = vec![
            ChunkLocation { tile_idx: 0, chunk_idx: 0, offset: 1000, length: 4000 },
            ChunkLocation { tile_idx: 0, chunk_idx: 1, offset: 5000, length: 4000 },
            ChunkLocation { tile_idx: 0, chunk_idx: 2, offset: 9000, length: 2000 },
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
            ChunkLocation { tile_idx: 0, chunk_idx: 0, offset: 1000, length: 4000 },  // 1000..4999
            ChunkLocation { tile_idx: 0, chunk_idx: 1, offset: 6000, length: 4000 },  // gap = 1000 (<= 32KB)
            ChunkLocation { tile_idx: 0, chunk_idx: 2, offset: 50000, length: 4000 }, // gap = 40000 (> 32KB) -> split
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
            ChunkLocation { tile_idx: 0, chunk_idx: 0, offset: 0, length: 600 },
            ChunkLocation { tile_idx: 0, chunk_idx: 1, offset: 600, length: 600 }, // cumulative 1200 > max_range 1000 -> split
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
            ChunkLocation { tile_idx: 0, chunk_idx: 0, offset: 1000, length: 4000 },
            ChunkLocation { tile_idx: 1, chunk_idx: 0, offset: 5000, length: 4000 }, // different tile -> split
        ];

        let ranges = coalesce_chunk_ranges(&chunks, 32768, 2 * 1024 * 1024);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].tile_idx, 0);
        assert_eq!(ranges[1].tile_idx, 1);
    }
}
