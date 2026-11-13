//! Driving the stream forward: batch sizing, EOF finalization and failure latching.

use super::{FinalCells, HorizonStreamKernel, MultiHorizonStreamer};
use crate::error::{RasterH3Error, Result};

impl<K: HorizonStreamKernel> MultiHorizonStreamer<K> {
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
    pub(super) fn buffered_len(&self) -> usize {
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
                if self.final_resolution < self.resolution_u8s.len() {
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
                for i in 0..self.resolution_u8s.len() {
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
}
