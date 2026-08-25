use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use tiff::decoder::DecodingResult;

use crate::error::Result;
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::RasterChunk;

/// Item yielded by the prefetch worker
pub type PrefetchItem = Result<(u32, RasterChunk, DecodingResult)>;

/// Asynchronous double-buffered chunk prefetcher that reuses a persistent decoder
pub struct PrefetchedChunkReader {
    receiver: Receiver<PrefetchItem>,
    _worker_handle: JoinHandle<()>,
}

impl PrefetchedChunkReader {
    /// Spawn a background prefetch thread for the given chunk range.
    /// Creates a single ChunkDecoder at thread start and reuses it for all reads,
    /// avoiding TIFF header re-parsing on every chunk.
    pub fn spawn(reader: GeoTiffStreamReader, chunk_indices: Vec<u32>, buffer_capacity: usize) -> Self {
        let (sender, receiver): (SyncSender<PrefetchItem>, Receiver<PrefetchItem>) =
            sync_channel(buffer_capacity.max(1));

        let worker_handle = thread::spawn(move || {
            // Create a persistent decoder once — parses IFD headers only here
            let mut decoder = match reader.open_decoder() {
                Ok(d) => d,
                Err(e) => {
                    // If decoder creation fails, send the error and exit
                    let _ = sender.send(Err(e));
                    return;
                }
            };

            for chunk_idx in chunk_indices {
                let item = decoder
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
