use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use tiff::decoder::DecodingResult;

use crate::error::Result;
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::RasterChunk;

/// Item yielded by the prefetch worker
pub type PrefetchItem = Result<(u32, RasterChunk, DecodingResult)>;

/// Asynchronous double-buffered chunk prefetcher
pub struct PrefetchedChunkReader {
    receiver: Receiver<PrefetchItem>,
    _worker_handle: JoinHandle<()>,
}

impl PrefetchedChunkReader {
    /// Spawn a background prefetch thread for the given chunk range
    pub fn spawn(reader: GeoTiffStreamReader, chunk_indices: Vec<u32>, buffer_capacity: usize) -> Self {
        let (sender, receiver): (SyncSender<PrefetchItem>, Receiver<PrefetchItem>) =
            sync_channel(buffer_capacity.max(1));

        let worker_handle = thread::spawn(move || {
            for chunk_idx in chunk_indices {
                let item = reader
                    .read_chunk(chunk_idx)
                    .map(|(bounds, data)| (chunk_idx, bounds, data));

                if sender.send(item).is_err() {
                    // Consumer dropped, exit worker gracefully
                    break;
                }
            }
        });

        Self {
            receiver,
            _worker_handle: worker_handle,
        }
    }

    /// Pull the next prefetched chunk (blocks if next chunk is still decoding)
    pub fn next_chunk(&self) -> Option<PrefetchItem> {
        self.receiver.recv().ok()
    }
}
