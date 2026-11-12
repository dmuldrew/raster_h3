//! Processing of one prefetched batch: sampling windows, horizon merge, eviction and spill.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use h3o::CellIndex;

use super::super::eviction_space::CellKeys;
use super::super::partition::PartialCell;
use super::super::sharded_map::AccumulatorMerge;
use super::super::walker::WalkContext;
use super::pipeline::{
    run_pipelined, run_serial, spill_resolution, CellMaps, MergeTarget, WindowJob, WindowJobs,
};
use super::{HorizonStreamKernel, MultiHorizonStreamer};
use crate::aggregator::cell_hash::H3BuildHasher;
use crate::error::{RasterH3Error, Result};
use crate::raster::prefetch::MosaicPrefetchItem;

impl<K: HorizonStreamKernel> MultiHorizonStreamer<K> {
    /// Emit one final cell. In sorted EOF order siblings are contiguous, so a
    /// parent's group is decomposed as soon as the stream moves past it.
    pub(super) fn emit_final(&mut self, res_idx: usize, key: u64, acc: K::Accumulator) {
        if let Some(partition) = &mut self.partition {
            if !partition.classifier.is_interior(key) {
                // Other partitions may hold samples of this cell: hand its state on
                // unfiltered, since only the merged state can be filtered.
                let bytes = size_of::<PartialCell<K::Accumulator>>() + acc.heap_bytes();
                partition.partials.push_sized(
                    PartialCell {
                        resolution: self.resolution_u8s[res_idx],
                        h3_index: key,
                        accumulator: acc,
                    },
                    bytes,
                );
                return;
            }
        }
        if self.compactor.is_enabled() && self.finishing {
            let parent = CellIndex::try_from(key)
                .ok()
                .and_then(|cell| cell.resolution().pred().and_then(|r| cell.parent(r)))
                .map(u64::from);
            if parent != self.compaction_parent {
                if let Some(previous) = self.compaction_parent {
                    self.compactor
                        .flush_parent(previous, &self.kernel, &mut self.output_buffer);
                }
                self.compaction_parent = parent;
            }
        }
        self.compactor.push_cell(
            &self.kernel,
            self.resolution_u8s[res_idx],
            key,
            acc,
            &mut self.output_buffer,
        );
    }

    pub(super) fn spill_resolution(&mut self, index: usize) -> Result<()> {
        spill_resolution(
            &mut self.resolution_shards,
            &mut self.spill_runs,
            &mut self.metrics,
            index,
        )
    }

    /// Aggregate a batch's windows into the horizon maps.
    ///
    /// Windows run on long-lived workers that take jobs as they become free,
    /// so no worker waits for a wave's slowest window. Each job fills its own
    /// map set, and the calling thread merges the sets strictly in window
    /// order while later windows still run, so results do not depend on
    /// timing. At most `worker_maps.len()` sets are in flight; a set returns
    /// to the pool once merged. Cells new to a horizon map get their eviction
    /// keys (a cell-boundary computation each) from the workers too, as extra
    /// jobs, and the keys are queued in creation order at the end of the
    /// batch, before any eviction can need them.
    pub(super) fn process_batch(&mut self, items: &mut [MosaicPrefetchItem]) -> Result<()> {
        let mosaic = &*self.mosaic;
        let jobs = WindowJobs::new(
            items,
            mosaic,
            self.worker_pixels,
            &mut self.metrics.decoded_bytes,
        )?;
        let job_count = jobs.len();
        if job_count == 0 {
            return Ok(());
        }
        let mut free = std::mem::take(&mut self.worker_maps);
        let new_cells = std::mem::take(&mut self.new_cell_buffer);
        let key_chunks = std::mem::take(&mut self.key_chunk_buffer);
        let map_sets = free.len();
        let set_budget = self.worker_budget / map_sets.max(1);
        let Self {
            kernel,
            resolutions,
            sampling,
            bbox,
            apply_scale_offset,
            nodata,
            eviction_space,
            resolution_shards,
            spill_runs,
            compactor,
            metrics,
            peak_active_bytes,
            accumulator_limit,
            map_budget,
            pool,
            worker_tasks,
            transform_failure_counter,
            finished_sets,
            ..
        } = self;
        let (kernel, mosaic, resolutions, sampling, eviction_space) = (
            &*kernel,
            mosaic,
            &*resolutions,
            &*sampling,
            &*eviction_space,
        );
        let (active, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let run_window = |job: WindowJob<'_>, maps: &mut CellMaps<K::Accumulator>| {
            peak.fetch_max(
                active.fetch_add(1, Ordering::Relaxed) + 1,
                Ordering::Relaxed,
            );
            let (tile_idx, chunk, samples, stride, overlap) = &job;
            let tile = &mosaic.tiles[*tile_idx];
            let scope = super::super::profile::WorkerScope::new();
            let start = std::time::Instant::now();
            let window = WalkContext {
                chunk,
                resolutions,
                crs: &tile.crs_transformer,
                gt: &tile.reader.metadata.geotransform,
                sampling,
                bbox: *bbox,
                stride: *stride,
                samples_per_pixel: tile.reader.metadata.samples_per_pixel,
                owner: overlap.then_some((*tile_idx, mosaic)),
                band_scaling: if *apply_scale_offset {
                    &tile.reader.metadata.band_scaling
                } else {
                    &[]
                },
                alpha_sample: tile.reader.metadata.alpha_sample,
            };
            kernel.process_window(
                &window,
                *samples,
                nodata.or(tile.reader.metadata.nodata),
                maps,
            );
            active.fetch_sub(1, Ordering::Relaxed);
            (scope.snapshot(), start.elapsed().as_nanos() as u64)
        };
        let compute_keys = |cells: &[(usize, u64)]| -> Vec<CellKeys> {
            cells
                .iter()
                .map(|&(_, cell)| eviction_space.cell_keys(cell))
                .collect()
        };
        let compactor_bytes = compactor.estimated_bytes();
        let mut target = MergeTarget {
            shards: resolution_shards,
            spill_runs,
            metrics,
            peak_active_bytes,
            failure_counter: transform_failure_counter.as_ref(),
            compactor_bytes,
            accumulator_limit: *accumulator_limit,
            map_budget: *map_budget,
            set_budget,
            map_sets,
            new_cells,
            key_chunks,
        };

        // Execution policy: a Rayon worker waiting on a join steals unrelated
        // tasks from its own pool, which deadlocks if such a task needs a lock
        // the caller holds (e.g. a shared streamer mutex). So callers on a
        // Rayon worker either hand the batch to the dedicated pool through a
        // helper OS thread, which blocks without stealing, or, with no
        // dedicated pool, run it serially.
        let t = std::time::Instant::now();
        target.metrics.waves += 1;
        let workers = (*worker_tasks).min(job_count);
        // A panic unwinds past the restore at the end of this function, and the
        // sets it held (in flight or being merged) are lost with it. Catch it,
        // refill the pool and resume, so a caught panic does not leave the
        // streamer with fewer map sets (or none) for later batches.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &*pool {
            // One window (or one worker) gains nothing from the pipeline;
            // running it here saves the channels and task spawns.
            _ if workers <= 1 => {
                run_serial(jobs, &mut free, &run_window, &compute_keys, &mut target)
            }
            Some(p) if p.current_thread_index().is_some() => {
                target.metrics.serial_waves += 1;
                run_serial(jobs, &mut free, &run_window, &compute_keys, &mut target)
            }
            Some(p) if rayon::current_thread_index().is_some() => {
                target.metrics.bridged_waves += 1;
                std::thread::scope(|s| {
                    s.spawn(|| {
                        p.in_place_scope(|scope| {
                            run_pipelined(
                                scope,
                                workers,
                                jobs,
                                &mut free,
                                &run_window,
                                &compute_keys,
                                &mut target,
                                finished_sets,
                            )
                        })
                    })
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
            }
            Some(p) => p.in_place_scope(|scope| {
                run_pipelined(
                    scope,
                    workers,
                    jobs,
                    &mut free,
                    &run_window,
                    &compute_keys,
                    &mut target,
                    finished_sets,
                )
            }),
            None if rayon::current_thread_index().is_some() => {
                target.metrics.serial_waves += 1;
                run_serial(jobs, &mut free, &run_window, &compute_keys, &mut target)
            }
            None => rayon::in_place_scope(|scope| {
                run_pipelined(
                    scope,
                    workers,
                    jobs,
                    &mut free,
                    &run_window,
                    &compute_keys,
                    &mut target,
                    finished_sets,
                )
            }),
        }));
        let result = match outcome {
            Ok(result) => result,
            Err(panic) => {
                free.iter_mut().flatten().for_each(HashMap::clear);
                free.resize_with(map_sets, || {
                    (0..resolutions.len())
                        .map(|_| HashMap::with_hasher(H3BuildHasher))
                        .collect()
                });
                self.worker_maps = free;
                std::panic::resume_unwind(panic);
            }
        };
        let kernel_ns = t.elapsed().as_nanos() as u64;
        let MergeTarget {
            shards,
            mut key_chunks,
            mut new_cells,
            ..
        } = target;
        // Queue eviction keys in the order the cells were created, so pop
        // order (ties included) does not depend on which worker finished first.
        if result.is_ok() {
            key_chunks.sort_unstable_by_key(|(chunk, _, _)| *chunk);
            for (_, cells, keys) in key_chunks.drain(..) {
                for ((res, cell), keys) in cells.into_iter().zip(keys) {
                    shards[res].push_eviction(cell, keys);
                }
            }
        }
        key_chunks.clear();
        new_cells.clear();
        let metrics = &mut self.metrics;
        metrics.peak_concurrent_jobs = metrics
            .peak_concurrent_jobs
            .max(peak.load(Ordering::Relaxed));
        metrics.kernel_wall_ns += kernel_ns;
        self.worker_maps = free;
        self.new_cell_buffer = new_cells;
        self.key_chunk_buffer = key_chunks;
        result?;
        let failures = self.metrics.worker.transform_failures;
        if self.strict_transforms && failures > 0 {
            return Err(RasterH3Error::TransformFailures(failures));
        }
        Ok(())
    }
}
