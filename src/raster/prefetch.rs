use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tiff::decoder::DecodingResult;

use crate::error::Result;
use crate::raster::geotiff::{ChunkDecoder, GeoTiffStreamReader};
use crate::raster::mosaic::MosaicReader;
use crate::raster::remote_prefetch::RemoteChunkPrefetchQueue;
use crate::raster::RasterChunk;

/// Item yielded by the prefetch worker
pub type PrefetchItem = Result<(u32, RasterChunk, DecodingResult)>;

/// Asynchronous double-buffered chunk prefetcher that reuses persistent decoders
/// across one or more background threads with buffer pooling.
pub struct PrefetchedChunkReader {
    receiver: Receiver<PrefetchItem>,
    recycle_sender: Option<SyncSender<DecodingResult>>,
    _remote_queue: Option<Arc<RemoteChunkPrefetchQueue>>,
    _worker_handles: Vec<JoinHandle<()>>,
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
    ///
    /// When `num_workers > 1`, a lock-free atomic job dispenser feeds the worker pool,
    /// and a lightweight collector thread guarantees that chunks are emitted strictly
    /// in the caller's requested index order.
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

        if num_workers <= 1 || chunk_indices.is_empty() {
            let (sender, receiver): (SyncSender<PrefetchItem>, Receiver<PrefetchItem>) =
                sync_channel(buffer_capacity.max(1));
            let (recycle_sender, recycle_receiver): (SyncSender<DecodingResult>, Receiver<DecodingResult>) =
                sync_channel(buffer_capacity.max(1));

            let worker_remote_queue = remote_queue.clone();
            let worker_handle = thread::spawn(move || {
                let mut decoder = match reader.open_decoder() {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = sender.send(Err(e));
                        return;
                    }
                };

                for chunk_idx in chunk_indices {
                    let recycled_buf = recycle_receiver.try_recv().ok();
                    let prefetched_bytes = worker_remote_queue
                        .as_ref()
                        .and_then(|q| q.get_chunk_payload(0, chunk_idx).ok().flatten());

                    let item = decoder
                        .read_chunk_with_payload(chunk_idx, prefetched_bytes.as_ref().map(|v| v.as_slice()), recycled_buf)
                        .map(|(bounds, data)| (chunk_idx, bounds, data));

                    if sender.send(item).is_err() {
                        break;
                    }
                }
            });

            return Self {
                receiver,
                recycle_sender: Some(recycle_sender),
                _remote_queue: remote_queue,
                _worker_handles: vec![worker_handle],
            };
        }

        let total_jobs = chunk_indices.len();
        let chunk_indices = Arc::new(chunk_indices);
        let next_job_idx = Arc::new(AtomicUsize::new(0));

        let internal_capacity = buffer_capacity.max(num_workers * 4);
        let (result_sender, result_receiver) = sync_channel::<(usize, PrefetchItem)>(internal_capacity);
        let (out_sender, receiver): (SyncSender<PrefetchItem>, Receiver<PrefetchItem>) =
            sync_channel(buffer_capacity.max(1));

        let (recycle_sender, recycle_receiver) = sync_channel::<DecodingResult>(internal_capacity);
        let recycle_receiver = Arc::new(Mutex::new(recycle_receiver));

        let mut handles = Vec::with_capacity(num_workers + 1);

        for _ in 0..num_workers {
            let worker_reader = reader.clone();
            let worker_indices = Arc::clone(&chunk_indices);
            let worker_job_idx = Arc::clone(&next_job_idx);
            let worker_sender = result_sender.clone();
            let worker_recycle_rx = Arc::clone(&recycle_receiver);
            let worker_remote_queue = remote_queue.clone();

            let handle = thread::spawn(move || {
                let mut decoder = match worker_reader.open_decoder() {
                    Ok(d) => d,
                    Err(e) => {
                        let job = worker_job_idx.fetch_add(1, Ordering::Relaxed);
                        if job < worker_indices.len() {
                            let _ = worker_sender.send((job, Err(e)));
                        }
                        return;
                    }
                };

                loop {
                    let job_id = worker_job_idx.fetch_add(1, Ordering::Relaxed);
                    if job_id >= worker_indices.len() {
                        break;
                    }
                    let chunk_idx = worker_indices[job_id];
                    let recycled_buf = worker_recycle_rx.lock().ok().and_then(|rx| rx.try_recv().ok());
                    let prefetched_bytes = worker_remote_queue
                        .as_ref()
                        .and_then(|q| q.get_chunk_payload(0, chunk_idx).ok().flatten());

                    let item = decoder
                        .read_chunk_with_payload(chunk_idx, prefetched_bytes.as_ref().map(|v| v.as_slice()), recycled_buf)
                        .map(|(bounds, data)| (chunk_idx, bounds, data));

                    if worker_sender.send((job_id, item)).is_err() {
                        break; // Collector or downstream dropped
                    }
                }
            });
            handles.push(handle);
        }

        // Drop the original sender so that when all worker threads terminate, result_receiver can disconnect
        drop(result_sender);

        // Collector / reordering thread: ensures strict in-order chunk emission
        let collector_handle = thread::spawn(move || {
            let mut next_expected = 0;
            let mut pending = BTreeMap::new();

            while next_expected < total_jobs {
                if let Some(item) = pending.remove(&next_expected) {
                    next_expected += 1;
                    if out_sender.send(item).is_err() {
                        return; // Downstream consumer dropped
                    }
                    continue;
                }

                match result_receiver.recv() {
                    Ok((job_id, item)) => {
                        if job_id == next_expected {
                            next_expected += 1;
                            if out_sender.send(item).is_err() {
                                return; // Downstream consumer dropped
                            }
                        } else {
                            pending.insert(job_id, item);
                        }
                    }
                    Err(_) => {
                        // Drain any remaining buffered items in order
                        while let Some(&first_key) = pending.keys().next() {
                            let item = pending.remove(&first_key).unwrap();
                            if out_sender.send(item).is_err() {
                                return;
                            }
                        }
                        break;
                    }
                }
            }
        });
        handles.push(collector_handle);

        Self {
            receiver,
            recycle_sender: Some(recycle_sender),
            _remote_queue: remote_queue,
            _worker_handles: handles,
        }
    }

    /// Return processed decoding buffers back to the worker pool for zero-allocation reuse
    pub fn recycle_batch(&self, buffers: impl IntoIterator<Item = DecodingResult>) {
        if let Some(ref sender) = self.recycle_sender {
            for buf in buffers {
                if sender.try_send(buf).is_err() {
                    break;
                }
            }
        }
    }

    /// Pull the next prefetched chunk (blocks if next chunk is still decoding)
    pub fn next_chunk(&self) -> Option<PrefetchItem> {
        self.receiver.recv().ok()
    }

    /// Pull next batch of ready chunks directly into `batch`.
    ///
    /// Guarantees that if any chunks are remaining, at least 1 chunk is fetched (blocking if necessary).
    /// After the first chunk, non-blocking `try_recv()` drains any currently ready chunks up to `max_batch`.
    /// If the count is below `min_batch`, it continues waiting with `recv()` until at least `min_batch`
    /// chunks have been acquired or EOF/disconnection is reached.
    ///
    /// This provides true asynchronous double-buffering: Rayon can immediately begin processing
    /// available chunks without stalling for full buffer filling, while background threads continue
    /// decoding subsequent chunks concurrently.
    pub fn drain_chunk_batch_into(
        &self,
        batch: &mut Vec<PrefetchItem>,
        min_batch: usize,
        max_batch: usize,
    ) -> usize {
        let initial_len = batch.len();
        let target_min = initial_len + min_batch.max(1);
        let target_max = initial_len + max_batch.max(min_batch);

        // 1. First item: blocking wait to ensure we don't return 0 if chunks are still in progress
        match self.receiver.recv() {
            Ok(item) => batch.push(item),
            Err(_) => return batch.len() - initial_len,
        }

        // 2. Non-blocking drain for all chunks already decoded and sitting in the channel
        while batch.len() < target_max {
            match self.receiver.try_recv() {
                Ok(item) => batch.push(item),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Channel is currently empty.
                    // If we have met or exceeded target_min, return immediately to let Rayon crunch!
                    if batch.len() >= target_min {
                        break;
                    }
                    // Otherwise wait for the next chunk
                    match self.receiver.recv() {
                        Ok(item) => batch.push(item),
                        Err(_) => break,
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        batch.len() - initial_len
    }

    /// Pull next batch of ready chunks (pulls up to `max_batch` chunks or until EOF)
    pub fn next_chunk_batch(&self, max_batch: usize) -> Vec<PrefetchItem> {
        let mut batch = Vec::with_capacity(max_batch);
        self.drain_chunk_batch_into(&mut batch, max_batch, max_batch);
        batch
    }
}

/// Item yielded by the mosaic prefetch worker: (tile_idx, chunk_idx, bounds, data, has_overlap)
pub type MosaicPrefetchItem = Result<(usize, u32, RasterChunk, DecodingResult, bool)>;

/// Multi-file mosaic prefetcher providing globally latitude-interleaved chunk decoding
pub struct PrefetchedMosaicReader {
    receiver: Receiver<MosaicPrefetchItem>,
    recycle_sender: Option<SyncSender<DecodingResult>>,
    _remote_queue: Option<Arc<RemoteChunkPrefetchQueue>>,
    _worker_handles: Vec<JoinHandle<()>>,
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

        if num_workers <= 1 || mosaic.chunk_refs.is_empty() {
            let (sender, receiver): (SyncSender<MosaicPrefetchItem>, Receiver<MosaicPrefetchItem>) =
                sync_channel(buffer_capacity.max(1));
            let (recycle_sender, recycle_receiver): (SyncSender<DecodingResult>, Receiver<DecodingResult>) =
                sync_channel(buffer_capacity.max(1));

            let worker_remote_queue = remote_queue.clone();
            let worker_handle = thread::spawn(move || {
                let mut decoders: Vec<Option<ChunkDecoder>> =
                    (0..mosaic.tiles.len()).map(|_| None).collect();

                for chunk_ref in &mosaic.chunk_refs {
                    let tile_idx = chunk_ref.tile_idx;
                    let chunk_idx = chunk_ref.chunk_idx;
                    let has_overlap = chunk_ref.has_overlap;

                    if decoders[tile_idx].is_none() {
                        match mosaic.tiles[tile_idx].reader.open_decoder() {
                            Ok(d) => decoders[tile_idx] = Some(d),
                            Err(e) => {
                                let _ = sender.send(Err(e));
                                return;
                            }
                        }
                    }
                    let decoder = decoders[tile_idx].as_mut().unwrap();

                    let recycled_buf = recycle_receiver.try_recv().ok();
                    let prefetched_bytes = worker_remote_queue
                        .as_ref()
                        .and_then(|q| q.get_chunk_payload(tile_idx, chunk_idx).ok().flatten());

                    let item = decoder
                        .read_chunk_with_payload(chunk_idx, prefetched_bytes.as_ref().map(|v| v.as_slice()), recycled_buf)
                        .map(|(bounds, data)| (tile_idx, chunk_idx, bounds, data, has_overlap));

                    if sender.send(item).is_err() {
                        break;
                    }
                }
            });

            return Self {
                receiver,
                recycle_sender: Some(recycle_sender),
                _remote_queue: remote_queue,
                _worker_handles: vec![worker_handle],
            };
        }

        let total_jobs = mosaic.chunk_refs.len();
        let next_job_idx = Arc::new(AtomicUsize::new(0));

        let internal_capacity = buffer_capacity.max(num_workers * 4);
        let (result_sender, result_receiver) =
            sync_channel::<(usize, MosaicPrefetchItem)>(internal_capacity);
        let (out_sender, receiver): (SyncSender<MosaicPrefetchItem>, Receiver<MosaicPrefetchItem>) =
            sync_channel(buffer_capacity.max(1));

        let (recycle_sender, recycle_receiver) = sync_channel::<DecodingResult>(internal_capacity);
        let recycle_receiver = Arc::new(Mutex::new(recycle_receiver));

        let mut handles = Vec::with_capacity(num_workers + 1);

        for _ in 0..num_workers {
            let worker_mosaic = Arc::clone(&mosaic);
            let worker_job_idx = Arc::clone(&next_job_idx);
            let worker_sender = result_sender.clone();
            let worker_recycle_rx = Arc::clone(&recycle_receiver);
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
                                if worker_sender.send((job_id, Err(e))).is_err() {
                                    break;
                                }
                                return;
                            }
                        }
                    }
                    let decoder = decoders[tile_idx].as_mut().unwrap();

                    let recycled_buf =
                        worker_recycle_rx.lock().ok().and_then(|rx| rx.try_recv().ok());
                    let prefetched_bytes = worker_remote_queue
                        .as_ref()
                        .and_then(|q| q.get_chunk_payload(tile_idx, chunk_idx).ok().flatten());

                    let item = decoder
                        .read_chunk_with_payload(chunk_idx, prefetched_bytes.as_ref().map(|v| v.as_slice()), recycled_buf)
                        .map(|(bounds, data)| (tile_idx, chunk_idx, bounds, data, has_overlap));

                    if worker_sender.send((job_id, item)).is_err() {
                        break;
                    }
                }
            });
            handles.push(handle);
        }

        drop(result_sender);

        let collector_handle = thread::spawn(move || {
            let mut next_expected = 0;
            let mut pending = BTreeMap::new();

            while next_expected < total_jobs {
                if let Some(item) = pending.remove(&next_expected) {
                    next_expected += 1;
                    if out_sender.send(item).is_err() {
                        return;
                    }
                    continue;
                }

                match result_receiver.recv() {
                    Ok((job_id, item)) => {
                        if job_id == next_expected {
                            next_expected += 1;
                            if out_sender.send(item).is_err() {
                                return;
                            }
                        } else {
                            pending.insert(job_id, item);
                        }
                    }
                    Err(_) => {
                        while let Some(&first_key) = pending.keys().next() {
                            let item = pending.remove(&first_key).unwrap();
                            if out_sender.send(item).is_err() {
                                return;
                            }
                        }
                        break;
                    }
                }
            }
        });
        handles.push(collector_handle);

        Self {
            receiver,
            recycle_sender: Some(recycle_sender),
            _remote_queue: remote_queue,
            _worker_handles: handles,
        }
    }

    /// Return processed decoding buffers back to worker pool for zero-allocation reuse
    pub fn recycle_batch(&self, buffers: impl IntoIterator<Item = DecodingResult>) {
        if let Some(ref sender) = self.recycle_sender {
            for buf in buffers {
                if sender.try_send(buf).is_err() {
                    break;
                }
            }
        }
    }

    /// Pull next prefetched chunk
    pub fn next_chunk(&self) -> Option<MosaicPrefetchItem> {
        self.receiver.recv().ok()
    }

    /// Pull next batch of ready chunks directly into `batch`
    pub fn drain_chunk_batch_into(
        &self,
        batch: &mut Vec<MosaicPrefetchItem>,
        min_batch: usize,
        max_batch: usize,
    ) -> usize {
        let initial_len = batch.len();
        let target_min = initial_len + min_batch.max(1);
        let target_max = initial_len + max_batch.max(min_batch);

        match self.receiver.recv() {
            Ok(item) => batch.push(item),
            Err(_) => return batch.len() - initial_len,
        }

        while batch.len() < target_max {
            match self.receiver.try_recv() {
                Ok(item) => batch.push(item),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if batch.len() >= target_min {
                        break;
                    }
                    match self.receiver.recv() {
                        Ok(item) => batch.push(item),
                        Err(_) => break,
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        batch.len() - initial_len
    }

    /// Pull next batch of ready chunks (pulls up to `max_batch` chunks or until EOF)
    pub fn next_chunk_batch(&self, max_batch: usize) -> Vec<MosaicPrefetchItem> {
        let mut batch = Vec::with_capacity(max_batch);
        self.drain_chunk_batch_into(&mut batch, max_batch, max_batch);
        batch
    }
}

