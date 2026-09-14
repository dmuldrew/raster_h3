use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use crossbeam_deque::Injector;
use tiff::decoder::DecodingResult;

use crate::error::Result;
use crate::raster::geotiff::{ChunkDecoder, GeoTiffStreamReader};
use crate::raster::mosaic::MosaicReader;
use crate::raster::remote_prefetch::RemoteChunkPrefetchQueue;
use crate::raster::RasterChunk;

/// Item yielded by the chunk prefetch worker
pub type PrefetchItem = Result<(u32, RasterChunk, DecodingResult)>;

/// Item yielded by the mosaic prefetch worker: (tile_idx, chunk_idx, bounds, data, has_overlap)
pub type MosaicPrefetchItem = Result<(usize, u32, RasterChunk, DecodingResult, bool)>;

struct OrderedQueueState<T> {
    slots: Vec<Option<T>>,
    next_read: usize,
    closed: bool,
}

/// High-throughput, zero-allocation bounded ring buffer that delivers parallel background chunk
/// decompression results to the aggregator in strict sequence order without an intermediate collector thread.
pub struct OrderedPrefetchQueue<T> {
    capacity: usize,
    total_jobs: usize,
    state: Mutex<OrderedQueueState<T>>,
    not_empty: Condvar,
    not_full: Condvar,
}

impl<T> OrderedPrefetchQueue<T> {
    pub fn new(capacity: usize, total_jobs: usize) -> Self {
        let cap = capacity.max(16);
        let mut slots = Vec::with_capacity(cap);
        for _ in 0..cap {
            slots.push(None);
        }
        Self {
            capacity: cap,
            total_jobs,
            state: Mutex::new(OrderedQueueState {
                slots,
                next_read: 0,
                closed: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
        }
    }

    /// Worker deposits completed decompressed chunk at `job_id`. Blocks if workers are >= `capacity`
    /// ahead of the consumer, enforcing backpressure. Returns false if closed.
    pub fn push(&self, job_id: usize, item: T) -> bool {
        let mut guard = self.state.lock().unwrap();
        while job_id >= guard.next_read + self.capacity && !guard.closed {
            guard = self.not_full.wait(guard).unwrap();
        }
        if guard.closed {
            return false;
        }
        let slot_idx = job_id % self.capacity;
        guard.slots[slot_idx] = Some(item);
        if job_id == guard.next_read {
            self.not_empty.notify_all();
        }
        true
    }

    /// Signal that prefetching is closed, unblocking all waiting workers and consumers.
    pub fn close(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// Pull next batch of ready chunks directly into `batch`. Drains contiguous sequence-ordered chunks
    /// under a single mutex lock.
    pub fn drain_into(&self, batch: &mut Vec<T>, min_batch: usize, max_batch: usize) -> usize {
        let initial_len = batch.len();
        let target_min = initial_len + min_batch.max(1);
        let target_max = initial_len + max_batch.max(min_batch);

        let mut guard = self.state.lock().unwrap();

        // 1. Wait until at least 1 chunk is ready, or all jobs finished, or closed with no slot
        while guard.next_read < self.total_jobs
            && guard.slots[guard.next_read % self.capacity].is_none()
            && !guard.closed
        {
            guard = self.not_empty.wait(guard).unwrap();
        }

        if guard.next_read >= self.total_jobs
            || (guard.closed && guard.slots[guard.next_read % self.capacity].is_none())
        {
            return 0;
        }

        // 2. Drain contiguous available items up to target_max
        let mut drained_any = false;
        while batch.len() < target_max && guard.next_read < self.total_jobs {
            let slot_idx = guard.next_read % self.capacity;
            if let Some(item) = guard.slots[slot_idx].take() {
                batch.push(item);
                guard.next_read += 1;
                drained_any = true;
            } else {
                break;
            }
        }

        // 3. If below target_min and more jobs remain, wait for additional chunks
        while batch.len() < target_min && guard.next_read < self.total_jobs && !guard.closed {
            while guard.next_read < self.total_jobs
                && guard.slots[guard.next_read % self.capacity].is_none()
                && !guard.closed
            {
                guard = self.not_empty.wait(guard).unwrap();
            }
            if guard.next_read < self.total_jobs {
                let slot_idx = guard.next_read % self.capacity;
                if let Some(item) = guard.slots[slot_idx].take() {
                    batch.push(item);
                    guard.next_read += 1;
                    drained_any = true;
                } else {
                    break;
                }
            }
        }

        if drained_any {
            self.not_full.notify_all();
        }

        batch.len() - initial_len
    }

    /// Pull the next single prefetched chunk (blocks if next chunk is still decoding)
    pub fn next(&self) -> Option<T> {
        let mut batch = Vec::with_capacity(1);
        if self.drain_into(&mut batch, 1, 1) > 0 {
            batch.pop()
        } else {
            None
        }
    }
}

/// Asynchronous double-buffered chunk prefetcher that reuses persistent decoders
/// across one or more background threads with lock-free buffer pooling and bounded in-order delivery.
pub struct PrefetchedChunkReader {
    queue: Arc<OrderedPrefetchQueue<PrefetchItem>>,
    buffer_pool: Arc<Injector<DecodingResult>>,
    _remote_queue: Option<Arc<RemoteChunkPrefetchQueue>>,
    _worker_handles: Vec<JoinHandle<()>>,
}

impl Drop for PrefetchedChunkReader {
    fn drop(&mut self) {
        self.queue.close();
    }
}

impl PrefetchedChunkReader {
    /// Spawn a background prefetch thread pool with hardware-scaled worker count.
    /// Uses persistent decoders per worker thread to avoid IFD header re-parsing.
    pub fn spawn(reader: GeoTiffStreamReader, chunk_indices: Vec<u32>, buffer_capacity: usize) -> Self {
        let default_workers = if reader.is_remote() {
            4
        } else if chunk_indices.len() <= 4 {
            1
        } else {
            std::thread::available_parallelism()
                .map(|p| (p.get().saturating_sub(2)).clamp(4, 12))
                .unwrap_or(6)
        };
        let capacity = buffer_capacity.max((default_workers * 128).max(1024));
        Self::spawn_with_workers(reader, chunk_indices, capacity, default_workers)
    }

    /// Spawn background prefetch workers with an explicit worker thread count.
    pub fn spawn_with_workers(
        reader: GeoTiffStreamReader,
        chunk_indices: Vec<u32>,
        buffer_capacity: usize,
        num_workers: usize,
    ) -> Self {
        let remote_queue = if reader.is_remote() {
            RemoteChunkPrefetchQueue::spawn_single(&reader, &chunk_indices, None).map(Arc::new)
        } else {
            None
        };

        let total_jobs = chunk_indices.len();
        let queue = Arc::new(OrderedPrefetchQueue::new(buffer_capacity.max(16), total_jobs));
        let buffer_pool = Arc::new(Injector::new());

        if total_jobs == 0 {
            return Self {
                queue,
                buffer_pool,
                _remote_queue: remote_queue,
                _worker_handles: Vec::new(),
            };
        }

        let chunk_indices = Arc::new(chunk_indices);
        let next_job_idx = Arc::new(AtomicUsize::new(0));
        let actual_workers = num_workers.max(1).min(total_jobs);
        let mut handles = Vec::with_capacity(actual_workers);

        for _ in 0..actual_workers {
            let worker_reader = reader.clone();
            let worker_indices = Arc::clone(&chunk_indices);
            let worker_job_idx = Arc::clone(&next_job_idx);
            let worker_queue = Arc::clone(&queue);
            let worker_buffer_pool = Arc::clone(&buffer_pool);
            let worker_remote_queue = remote_queue.clone();

            let handle = thread::spawn(move || {
                let mut decoder = match worker_reader.open_decoder() {
                    Ok(d) => d,
                    Err(e) => {
                        let job = worker_job_idx.fetch_add(1, Ordering::Relaxed);
                        if job < worker_indices.len() {
                            worker_queue.push(job, Err(e));
                        }
                        worker_queue.close();
                        return;
                    }
                };

                loop {
                    let job_id = worker_job_idx.fetch_add(1, Ordering::Relaxed);
                    if job_id >= worker_indices.len() {
                        break;
                    }
                    let chunk_idx = worker_indices[job_id];
                    let recycled_buf = match worker_buffer_pool.steal() {
                        crossbeam_deque::Steal::Success(b) => Some(b),
                        _ => None,
                    };
                    let prefetched_bytes = worker_remote_queue
                        .as_ref()
                        .and_then(|q| q.get_chunk_payload(0, chunk_idx).ok().flatten());

                    let item = decoder
                        .read_chunk_with_payload(
                            chunk_idx,
                            prefetched_bytes.as_ref().map(|v| v.as_slice()),
                            recycled_buf,
                        )
                        .map(|(bounds, data)| (chunk_idx, bounds, data));

                    if !worker_queue.push(job_id, item) {
                        break;
                    }
                }
            });
            handles.push(handle);
        }

        Self {
            queue,
            buffer_pool,
            _remote_queue: remote_queue,
            _worker_handles: handles,
        }
    }

    /// Return processed decoding buffers back to the worker pool for zero-allocation reuse
    pub fn recycle_batch(&self, buffers: impl IntoIterator<Item = DecodingResult>) {
        for buf in buffers {
            self.buffer_pool.push(buf);
        }
    }

    /// Pull the next prefetched chunk (blocks if next chunk is still decoding)
    pub fn next_chunk(&self) -> Option<PrefetchItem> {
        self.queue.next()
    }

    /// Pull next batch of ready chunks directly into `batch`.
    pub fn drain_chunk_batch_into(
        &self,
        batch: &mut Vec<PrefetchItem>,
        min_batch: usize,
        max_batch: usize,
    ) -> usize {
        self.queue.drain_into(batch, min_batch, max_batch)
    }

    /// Pull next batch of ready chunks (pulls up to `max_batch` chunks or until EOF)
    pub fn next_chunk_batch(&self, max_batch: usize) -> Vec<PrefetchItem> {
        let mut batch = Vec::with_capacity(max_batch);
        self.drain_chunk_batch_into(&mut batch, max_batch, max_batch);
        batch
    }
}

/// Multi-file mosaic prefetcher providing globally latitude-interleaved chunk decoding
/// with lock-free buffer pooling and single-hop bounded in-order queueing.
pub struct PrefetchedMosaicReader {
    queue: Arc<OrderedPrefetchQueue<MosaicPrefetchItem>>,
    buffer_pool: Arc<Injector<DecodingResult>>,
    _remote_queue: Option<Arc<RemoteChunkPrefetchQueue>>,
    _worker_handles: Vec<JoinHandle<()>>,
}

impl Drop for PrefetchedMosaicReader {
    fn drop(&mut self) {
        self.queue.close();
    }
}

impl PrefetchedMosaicReader {
    /// Spawn background prefetch thread pool for mosaic ingestion
    pub fn spawn(mosaic: Arc<MosaicReader>, buffer_capacity: usize) -> Self {
        let default_workers = if mosaic.tiles.iter().any(|t| t.reader.is_remote()) {
            4
        } else if mosaic.chunk_refs.len() <= 4 {
            1
        } else {
            std::thread::available_parallelism()
                .map(|p| (p.get().saturating_sub(2)).clamp(4, 12))
                .unwrap_or(6)
        };
        let capacity = buffer_capacity.max((default_workers * 128).max(1024));
        Self::spawn_with_workers(mosaic, capacity, default_workers)
    }

    /// Spawn background prefetch workers with an explicit worker thread count
    pub fn spawn_with_workers(
        mosaic: Arc<MosaicReader>,
        buffer_capacity: usize,
        num_workers: usize,
    ) -> Self {
        let remote_queue = RemoteChunkPrefetchQueue::spawn_mosaic(&mosaic, None).map(Arc::new);
        let total_jobs = mosaic.chunk_refs.len();
        let queue = Arc::new(OrderedPrefetchQueue::new(buffer_capacity.max(16), total_jobs));
        let buffer_pool = Arc::new(Injector::new());

        if total_jobs == 0 {
            return Self {
                queue,
                buffer_pool,
                _remote_queue: remote_queue,
                _worker_handles: Vec::new(),
            };
        }

        let next_job_idx = Arc::new(AtomicUsize::new(0));
        let actual_workers = num_workers.max(1).min(total_jobs);
        let mut handles = Vec::with_capacity(actual_workers);

        for _ in 0..actual_workers {
            let worker_mosaic = Arc::clone(&mosaic);
            let worker_job_idx = Arc::clone(&next_job_idx);
            let worker_queue = Arc::clone(&queue);
            let worker_buffer_pool = Arc::clone(&buffer_pool);
            let worker_remote_queue = remote_queue.clone();

            let handle = thread::spawn(move || {
                let mut decoders: Vec<Option<ChunkDecoder>> =
                    (0..worker_mosaic.tiles.len()).map(|_| None).collect();

                loop {
                    let job_id = worker_job_idx.fetch_add(1, Ordering::Relaxed);
                    if job_id >= worker_mosaic.chunk_refs.len() {
                        break;
                    }
                    let chunk_ref = worker_mosaic.chunk_refs[job_id];
                    let tile_idx = chunk_ref.tile_idx;
                    let chunk_idx = chunk_ref.chunk_idx;
                    let has_overlap = chunk_ref.has_overlap;

                    if decoders[tile_idx].is_none() {
                        match worker_mosaic.tiles[tile_idx].reader.open_decoder() {
                            Ok(d) => decoders[tile_idx] = Some(d),
                            Err(e) => {
                                worker_queue.push(job_id, Err(e));
                                worker_queue.close();
                                return;
                            }
                        }
                    }
                    let decoder = decoders[tile_idx].as_mut().unwrap();

                    let recycled_buf = match worker_buffer_pool.steal() {
                        crossbeam_deque::Steal::Success(b) => Some(b),
                        _ => None,
                    };
                    let prefetched_bytes = worker_remote_queue
                        .as_ref()
                        .and_then(|q| q.get_chunk_payload(tile_idx, chunk_idx).ok().flatten());

                    let item = decoder
                        .read_chunk_with_payload(
                            chunk_idx,
                            prefetched_bytes.as_ref().map(|v| v.as_slice()),
                            recycled_buf,
                        )
                        .map(|(bounds, data)| (tile_idx, chunk_idx, bounds, data, has_overlap));

                    if !worker_queue.push(job_id, item) {
                        break;
                    }
                }
            });
            handles.push(handle);
        }

        Self {
            queue,
            buffer_pool,
            _remote_queue: remote_queue,
            _worker_handles: handles,
        }
    }

    /// Return processed decoding buffers back to worker pool for zero-allocation reuse
    pub fn recycle_batch(&self, buffers: impl IntoIterator<Item = DecodingResult>) {
        for buf in buffers {
            self.buffer_pool.push(buf);
        }
    }

    /// Pull next prefetched chunk
    pub fn next_chunk(&self) -> Option<MosaicPrefetchItem> {
        self.queue.next()
    }

    /// Pull next batch of ready chunks directly into `batch`
    pub fn drain_chunk_batch_into(
        &self,
        batch: &mut Vec<MosaicPrefetchItem>,
        min_batch: usize,
        max_batch: usize,
    ) -> usize {
        self.queue.drain_into(batch, min_batch, max_batch)
    }

    /// Pull next batch of ready chunks (pulls up to `max_batch` chunks or until EOF)
    pub fn next_chunk_batch(&self, max_batch: usize) -> Vec<MosaicPrefetchItem> {
        let mut batch = Vec::with_capacity(max_batch);
        self.drain_chunk_batch_into(&mut batch, max_batch, max_batch);
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_ordered_prefetch_queue_sequential() {
        let queue = Arc::new(OrderedPrefetchQueue::new(16, 50));
        let q = Arc::clone(&queue);
        let producer = thread::spawn(move || {
            for i in 0..50 {
                assert!(q.push(i, format!("item_{}", i)));
            }
        });
        for i in 0..50 {
            let item = queue.next().expect("Expected item");
            assert_eq!(item, format!("item_{}", i));
        }
        assert!(queue.next().is_none());
        producer.join().unwrap();
    }

    #[test]
    fn test_ordered_prefetch_queue_batch_draining() {
        let queue = Arc::new(OrderedPrefetchQueue::new(16, 100));
        let q = Arc::clone(&queue);
        let producer = thread::spawn(move || {
            for i in 0..100 {
                assert!(q.push(i, i * 10));
            }
        });
        let mut collected = Vec::new();
        let mut batch = Vec::new();
        while queue.drain_into(&mut batch, 5, 10) > 0 {
            collected.append(&mut batch);
        }
        producer.join().unwrap();
        assert_eq!(collected.len(), 100);
        for (idx, &val) in collected.iter().enumerate() {
            assert_eq!(val, idx * 10);
        }
    }

    #[test]
    fn test_ordered_prefetch_queue_reverse_insertion() {
        let queue = Arc::new(OrderedPrefetchQueue::new(16, 16));
        let q_clone = Arc::clone(&queue);

        // Push in reverse order in a background thread
        let handle = thread::spawn(move || {
            for i in (0..16).rev() {
                assert!(q_clone.push(i, i));
            }
        });

        // Drain from main thread - must come out in strictly ascending order 0..16
        let mut collected = Vec::new();
        for _ in 0..16 {
            collected.push(queue.next().unwrap());
        }
        handle.join().unwrap();

        assert_eq!(collected, (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn test_ordered_prefetch_queue_chaotic_multithreaded() {
        // 16 producer threads pushing 10,000 items in chaotic non-sequential order
        let num_items = 10_000;
        let num_producers = 16;
        let queue = Arc::new(OrderedPrefetchQueue::new(64, num_items));
        let next_job = Arc::new(AtomicUsize::new(0));

        let mut producer_handles = Vec::new();
        for worker_id in 0..num_producers {
            let q = Arc::clone(&queue);
            let job_counter = Arc::clone(&next_job);
            producer_handles.push(thread::spawn(move || {
                loop {
                    let job_id = job_counter.fetch_add(1, Ordering::Relaxed);
                    if job_id >= num_items {
                        break;
                    }
                    // Introduce chaotic jitter / preemption
                    let jitter = (job_id * 17 + worker_id * 31) % 10;
                    if jitter == 0 {
                        thread::yield_now();
                    } else if jitter == 1 {
                        thread::sleep(Duration::from_micros(10));
                    }
                    assert!(q.push(job_id, job_id as u64));
                }
            }));
        }

        // Consumer drains using a variety of batch sizes
        let mut received = Vec::with_capacity(num_items);
        let mut batch = Vec::new();
        let mut batch_size_cycle = 1;
        while received.len() < num_items {
            let min_b = batch_size_cycle;
            let max_b = batch_size_cycle * 2;
            let drained = queue.drain_into(&mut batch, min_b, max_b);
            if drained > 0 {
                received.append(&mut batch);
            }
            batch_size_cycle = (batch_size_cycle % 15) + 1;
        }

        for h in producer_handles {
            h.join().unwrap();
        }

        assert_eq!(received.len(), num_items);
        for (expected, actual) in received.iter().enumerate() {
            assert_eq!(*actual, expected as u64, "Queue must maintain strict sequence order");
        }
    }

    #[test]
    fn test_ordered_prefetch_queue_backpressure_saturation() {
        let capacity = 16;
        let total_jobs = 100;
        let queue = Arc::new(OrderedPrefetchQueue::new(capacity, total_jobs));
        let q_clone = Arc::clone(&queue);

        let pushed_count = Arc::new(AtomicUsize::new(0));
        let p_count = Arc::clone(&pushed_count);

        let producer_handle = thread::spawn(move || {
            for i in 0..total_jobs {
                assert!(q_clone.push(i, i));
                p_count.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Sleep briefly to give producer time to saturate capacity
        thread::sleep(Duration::from_millis(50));

        // Capacity is 16. The producer cannot advance beyond capacity (16 items) ahead of consumer (next_read = 0).
        let count_before_drain = pushed_count.load(Ordering::SeqCst);
        assert!(
            count_before_drain <= capacity,
            "Backpressure failed: pushed {} items but capacity is {}",
            count_before_drain,
            capacity
        );

        // Consumer drains 8 items
        let mut batch = Vec::new();
        queue.drain_into(&mut batch, 8, 8);
        assert_eq!(batch.len(), 8);

        // Allow producer to unblock and push 8 more items
        thread::sleep(Duration::from_millis(50));
        let count_after_drain = pushed_count.load(Ordering::SeqCst);
        assert!(
            count_after_drain >= count_before_drain,
            "Producer should resume after draining"
        );
        assert!(
            count_after_drain <= 8 + capacity,
            "Producer cannot exceed next_read + capacity"
        );

        // Drain the rest
        while queue.drain_into(&mut batch, 1, 32) > 0 {
            batch.clear();
        }

        producer_handle.join().unwrap();
        assert_eq!(pushed_count.load(Ordering::SeqCst), total_jobs);
    }

    #[test]
    fn test_ordered_prefetch_queue_early_close() {
        let capacity = 16;
        let total_jobs = 1000;
        let queue = Arc::new(OrderedPrefetchQueue::new(capacity, total_jobs));

        let num_workers = 4;
        let mut handles = Vec::new();
        let unblocked = Arc::new(AtomicUsize::new(0));

        for w in 0..num_workers {
            let q = Arc::clone(&queue);
            let unb = Arc::clone(&unblocked);
            handles.push(thread::spawn(move || {
                for i in (w..total_jobs).step_by(num_workers) {
                    if !q.push(i, i) {
                        unb.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                }
            }));
        }

        // Read a few items
        let _ = queue.next();
        let _ = queue.next();

        // Close early
        queue.close();

        // All producer threads should terminate cleanly without deadlock
        for h in handles {
            h.join().unwrap();
        }

        // Subsequent drain returns 0 once remaining buffer slots are empty
        let mut batch = Vec::new();
        while queue.drain_into(&mut batch, 1, 10) > 0 {
            batch.clear();
        }
        assert_eq!(queue.drain_into(&mut batch, 1, 10), 0);
    }

    #[test]
    fn test_lock_free_buffer_pool_concurrency() {
        let pool = Arc::new(crossbeam_deque::Injector::new());
        let num_threads = 8;
        let iters_per_thread = 2000;

        // Pre-populate pool with 16 buffers
        for _ in 0..16 {
            pool.push(DecodingResult::U8(vec![0u8; 1024]));
        }

        let mut handles = Vec::new();
        for _ in 0..num_threads {
            let p = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                for _ in 0..iters_per_thread {
                    let buf = match p.steal() {
                        crossbeam_deque::Steal::Success(b) => b,
                        _ => DecodingResult::U8(vec![1u8; 1024]),
                    };
                    // Modify buffer to verify no data race or corruption
                    match buf {
                        DecodingResult::U8(mut v) => {
                            v[0] = v[0].wrapping_add(1);
                            p.push(DecodingResult::U8(v));
                        }
                        _ => unreachable!(),
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Verify pool can be completely drained without crashing
        let mut count = 0;
        while let crossbeam_deque::Steal::Success(_) = pool.steal() {
            count += 1;
        }
        assert!(count >= 16);
    }
}
