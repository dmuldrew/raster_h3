use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tiff::decoder::DecodingResult;

use crate::error::Result;
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::RasterChunk;

/// Item yielded by the prefetch worker
pub type PrefetchItem = Result<(u32, RasterChunk, DecodingResult)>;

/// Asynchronous double-buffered chunk prefetcher that reuses persistent decoders
/// across one or more background threads with buffer pooling.
pub struct PrefetchedChunkReader {
    receiver: Receiver<PrefetchItem>,
    recycle_sender: Option<SyncSender<DecodingResult>>,
    _worker_handles: Vec<JoinHandle<()>>,
}

impl PrefetchedChunkReader {
    /// Spawn a background prefetch thread pool with hardware-scaled worker count.
    /// Uses persistent decoders per worker thread to avoid IFD header re-parsing.
    pub fn spawn(reader: GeoTiffStreamReader, chunk_indices: Vec<u32>, buffer_capacity: usize) -> Self {
        let default_workers = if chunk_indices.len() <= 4 {
            1
        } else {
            std::thread::available_parallelism()
                .map(|p| (p.get() / 3).clamp(2, 4))
                .unwrap_or(3)
        };
        let capacity = buffer_capacity.max(256);
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
        if num_workers <= 1 || chunk_indices.is_empty() {
            let (sender, receiver): (SyncSender<PrefetchItem>, Receiver<PrefetchItem>) =
                sync_channel(buffer_capacity.max(1));
            let (recycle_sender, recycle_receiver): (SyncSender<DecodingResult>, Receiver<DecodingResult>) =
                sync_channel(buffer_capacity.max(1));

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
                    let item = match recycled_buf {
                        Some(buf) => decoder
                            .read_chunk_into(chunk_idx, buf)
                            .map(|(bounds, data)| (chunk_idx, bounds, data)),
                        None => decoder
                            .read_chunk(chunk_idx)
                            .map(|(bounds, data)| (chunk_idx, bounds, data)),
                    };

                    if sender.send(item).is_err() {
                        break;
                    }
                }
            });

            return Self {
                receiver,
                recycle_sender: Some(recycle_sender),
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
                    let item = match recycled_buf {
                        Some(buf) => decoder
                            .read_chunk_into(chunk_idx, buf)
                            .map(|(bounds, data)| (chunk_idx, bounds, data)),
                        None => decoder
                            .read_chunk(chunk_idx)
                            .map(|(bounds, data)| (chunk_idx, bounds, data)),
                    };

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

    /// Pull next batch of ready chunks (pulls up to `max_batch` chunks or until EOF)
    pub fn next_chunk_batch(&self, max_batch: usize) -> Vec<PrefetchItem> {
        let mut batch = Vec::with_capacity(max_batch);
        while batch.len() < max_batch {
            match self.receiver.recv() {
                Ok(item) => batch.push(item),
                Err(_) => break, // Channel disconnected / EOF
            }
        }
        batch
    }
}
