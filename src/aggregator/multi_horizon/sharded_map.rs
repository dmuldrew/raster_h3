use fxhash::FxBuildHasher;
use rayon::prelude::*;
use std::collections::{BinaryHeap, HashMap};

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::horizon_streamer::{compute_cell_south_lat, HexEvictionEntry};

/// Number of concurrent shards for parallel active map merging
pub const NUM_SHARDS: usize = 32;

/// Mix the full H3 index before selecting a power-of-two shard.
/// Unused H3 child digits fill the low bits with ones, so masking the low
/// bits of a simple product sends every cell at resolutions 0..=13 to one shard.
#[inline(always)]
pub fn get_shard(cell_u64: u64) -> usize {
    // SplitMix64 finalizer: fold the varying base-cell and child digits into
    // every output bit. Keep all arithmetic in u64 on both 32- and 64-bit hosts.
    let mut h = cell_u64;
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    (h as usize) & (NUM_SHARDS - 1)
}

/// Trait defining an accumulator that can be merged into another
pub trait AccumulatorMerge: Clone + Send + Sync + 'static {
    fn merge(&mut self, other: &Self);
}

impl AccumulatorMerge for H3Accumulator {
    #[inline(always)]
    fn merge(&mut self, other: &Self) {
        H3Accumulator::merge(self, other);
    }
}

impl AccumulatorMerge for CategoricalAccumulator {
    #[inline(always)]
    fn merge(&mut self, other: &Self) {
        CategoricalAccumulator::merge(self, other);
    }
}

/// A 32-way hash-partitioned active map and eviction heap for a single H3 resolution.
///
/// # Concurrency Architecture
///
/// Rather than using simultaneous concurrent insertion and eviction with mutexes or atomics,
/// `ShardedResolutionMap` operates through **exclusive execution phases with parallel shard ownership**:
///
/// 1. **Worker Aggregation Phase**: Worker threads process raster chunks in parallel, partitioning
///    their outputs into 32 thread-local shard buffers with zero cross-thread coordination.
/// 2. **Merge Phase (`merge_thread_results`)**: Takes `&mut self`. Rayon mutably iterates over the
///    32 disjoint shards in parallel (`par_iter_mut`), with each shard sequentially merging chunk
///    results assigned to its shard index. Disjoint shard ownership guarantees zero race conditions
///    and eliminates lock contention.
/// 3. **Eviction Phase (`evict_completed`)**: Takes `&mut self`. Only after all worker results for
///    completed chunks have been merged does the controller advance the watermark and evict cells
///    past `lat_horizon` across shards in parallel.
/// 4. **Drain Phase (`drain_all`)**: Takes `&mut self` at EOF, draining all remaining active cells
///    across all shards in parallel.
pub struct ShardedResolutionMap<A: AccumulatorMerge> {
    pub shards: Vec<HashMap<u64, A, FxBuildHasher>>,
    pub eviction: Vec<BinaryHeap<HexEvictionEntry>>,
}

impl<A: AccumulatorMerge> ShardedResolutionMap<A> {
    /// Create a new ShardedResolutionMap with `NUM_SHARDS` shards
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        let mut eviction = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(HashMap::with_capacity_and_hasher(
                128,
                FxBuildHasher::default(),
            ));
            eviction.push(BinaryHeap::with_capacity(128));
        }
        Self { shards, eviction }
    }

    /// Merge partial chunk results from parallel worker threads into the 32 shards.
    ///
    /// Executes an exclusive merge phase using Rayon parallel mutable iteration over disjoint shards.
    /// Each Rayon task exclusively owns and mutates one shard map and eviction heap, merging
    /// contributions sequentially within that shard.
    pub fn merge_thread_results<T: Sync>(
        &mut self,
        parallel_results: &[(Vec<[Vec<(u64, A)>; NUM_SHARDS]>, T)],
        res_idx: usize,
    ) {
        self.shards
            .par_iter_mut()
            .zip(self.eviction.par_iter_mut())
            .enumerate()
            .for_each(|(s, (shard_map, shard_evict))| {
                for (chunk_shards, _) in parallel_results {
                    if res_idx < chunk_shards.len() {
                        for &(cell_u64, ref acc) in &chunk_shards[res_idx][s] {
                            shard_map
                                .entry(cell_u64)
                                .and_modify(|existing| existing.merge(acc))
                                .or_insert_with(|| {
                                    let south_lat = compute_cell_south_lat(cell_u64);
                                    shard_evict.push(HexEvictionEntry {
                                        south_lat,
                                        cell_u64,
                                    });
                                    acc.clone()
                                });
                        }
                    }
                }
            });
    }

    /// Evict completed cells that lie north of `lat_horizon` across all 32 shards in parallel,
    /// sorting the evicted batch by H3 cell index.
    pub fn evict_completed(&mut self, lat_horizon: f64) -> Vec<(u64, A)> {
        let mut newly_evicted: Vec<(u64, A)> = self
            .shards
            .par_iter_mut()
            .zip(self.eviction.par_iter_mut())
            .map(|(shard_map, shard_evict)| {
                let mut evicted = Vec::new();
                while let Some(top) = shard_evict.peek() {
                    if top.south_lat > lat_horizon {
                        let entry = shard_evict.pop().unwrap();
                        if let Some(acc) = shard_map.remove(&entry.cell_u64) {
                            evicted.push((entry.cell_u64, acc));
                        }
                    } else {
                        break;
                    }
                }
                evicted
            })
            .flatten()
            .collect();

        newly_evicted.par_sort_unstable_by_key(|item| item.0);
        newly_evicted
    }

    /// Drain all remaining active cells at EOF across all 32 shards in parallel,
    /// sorting the remaining batch by H3 cell index.
    pub fn drain_all(&mut self) -> Vec<(u64, A)> {
        let mut remaining: Vec<(u64, A)> = self
            .shards
            .par_iter_mut()
            .zip(self.eviction.par_iter_mut())
            .map(|(shard_map, shard_evict)| {
                let mut drained = Vec::with_capacity(shard_map.len());
                shard_evict.clear();
                for (cell_u64, acc) in shard_map.drain() {
                    drained.push((cell_u64, acc));
                }
                drained
            })
            .flatten()
            .collect();

        remaining.par_sort_unstable_by_key(|item| item.0);
        remaining
    }

    /// Return total active in-flight cell count across all 32 shards
    pub fn active_cell_count(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }
}
