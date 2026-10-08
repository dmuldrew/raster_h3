use fxhash::FxBuildHasher;
use rayon::prelude::*;
use std::collections::{BinaryHeap, HashMap};

use super::spill::table_bytes;
use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::horizon_streamer::{compute_cell_south_lat, HexEvictionEntry};
use crate::aggregator::quantiles::QuantileSketch;

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
    /// Owned dynamic state used by the aggregation budget. Inline-only custom
    /// accumulators can use the default; heap-owning implementations must override.
    fn heap_bytes(&self) -> usize {
        0
    }
}

impl AccumulatorMerge for H3Accumulator {
    fn heap_bytes(&self) -> usize {
        self.quantiles.as_ref().map_or(0, |q| {
            std::mem::size_of::<QuantileSketch>()
                + table_bytes::<i32, f64>(q.pos_bins.capacity())
                + table_bytes::<i32, f64>(q.neg_bins.capacity())
        })
    }

    #[inline(always)]
    fn merge(&mut self, other: &Self) {
        H3Accumulator::merge(self, other);
    }
}

impl AccumulatorMerge for CategoricalAccumulator {
    fn heap_bytes(&self) -> usize {
        self.heap_counts.as_ref().map_or(0, |h| {
            std::mem::size_of_val(&**h) + table_bytes::<i64, f64>(h.capacity())
        })
    }

    #[inline(always)]
    fn merge(&mut self, other: &Self) {
        CategoricalAccumulator::merge(self, other);
    }
}

/// A 32-way hash-partitioned active map and eviction heap for a single H3 resolution.
///
/// Both batch merges and the budget-aware owned merge require exclusive access.
/// Legacy batch APIs parallelize over disjoint shards. The controller joins
/// bounded worker waves, merges owned states with budget checks, and pops completed
/// cells incrementally. Merge and eviction phases cannot overlap in safe Rust.
pub struct ShardedResolutionMap<A: AccumulatorMerge> {
    pub shards: Vec<HashMap<u64, A, FxBuildHasher>>,
    pub eviction: Vec<BinaryHeap<HexEvictionEntry>>,
    dynamic_bytes: usize,
    /// Table and heap allocation, maintained incrementally so budget checks
    /// stay O(1) per merged cell. Refreshed after every bulk mutation.
    structure_bytes: usize,
    track_eviction: bool,
}

impl<A: AccumulatorMerge> ShardedResolutionMap<A> {
    /// Create a new ShardedResolutionMap with `NUM_SHARDS` shards
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        let mut eviction = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(HashMap::with_capacity_and_hasher(
                0,
                FxBuildHasher::default(),
            ));
            eviction.push(BinaryHeap::new());
        }
        Self {
            shards,
            eviction,
            dynamic_bytes: 0,
            structure_bytes: 0,
            track_eviction: true,
        }
    }

    #[inline]
    fn shard_bytes(&self, s: usize) -> usize {
        super::spill::table_bytes::<u64, A>(self.shards[s].capacity())
            + self.eviction[s].capacity() * std::mem::size_of::<HexEvictionEntry>()
    }

    fn recount_structure(&mut self) {
        self.structure_bytes = (0..NUM_SHARDS).map(|s| self.shard_bytes(s)).sum();
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
        let track_eviction = self.track_eviction;
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
                                    if track_eviction {
                                        let south_lat = compute_cell_south_lat(cell_u64);
                                        shard_evict.push(HexEvictionEntry {
                                            south_lat,
                                            cell_u64,
                                        });
                                    }
                                    acc.clone()
                                });
                        }
                    }
                }
            });
        self.dynamic_bytes = self
            .shards
            .iter()
            .flat_map(|map| map.values())
            .map(A::heap_bytes)
            .sum();
        self.recount_structure();
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

        self.dynamic_bytes -= newly_evicted
            .iter()
            .map(|(_, acc)| acc.heap_bytes())
            .sum::<usize>();
        self.recount_structure();
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

        self.dynamic_bytes = 0;
        self.recount_structure();
        remaining.par_sort_unstable_by_key(|item| item.0);
        remaining
    }

    /// Return total active in-flight cell count across all 32 shards
    pub fn active_cell_count(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }
}

impl<A: super::spill::SpillAccumulator> ShardedResolutionMap<A> {
    pub fn set_eviction_enabled(&mut self, enabled: bool) {
        self.track_eviction = enabled;
        if !enabled {
            for heap in &mut self.eviction {
                *heap = BinaryHeap::new();
            }
            self.recount_structure();
        }
    }

    /// Owned merge avoids cloning a large quantile or categorical state.
    /// Returns the merged accumulator's memory footprint.
    pub fn merge_owned(&mut self, key: u64, acc: A) -> usize {
        let s = get_shard(key);
        let before = self.shard_bytes(s);
        let merged_bytes = match self.shards[s].entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                self.dynamic_bytes -= existing.heap_bytes();
                existing.merge(&acc);
                self.dynamic_bytes += existing.heap_bytes();
                existing.memory_bytes()
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let bytes = acc.memory_bytes();
                self.dynamic_bytes += acc.heap_bytes();
                entry.insert(acc);
                if self.track_eviction {
                    self.eviction[s].push(HexEvictionEntry {
                        cell_u64: key,
                        south_lat: compute_cell_south_lat(key),
                    });
                }
                bytes
            }
        };
        self.structure_bytes = self.structure_bytes - before + self.shard_bytes(s);
        merged_bytes
    }

    pub fn estimated_bytes(&self) -> usize {
        self.dynamic_bytes + self.structure_bytes
    }

    /// Remove at most one completed cell, keeping output backpressure effective.
    pub fn pop_completed(&mut self, horizon: f64) -> Option<(u64, A)> {
        for s in 0..NUM_SHARDS {
            while self.eviction[s]
                .peek()
                .is_some_and(|e| e.south_lat > horizon)
            {
                // Removal can leave a tombstone that lowers reported capacity.
                let before = self.shard_bytes(s);
                let key = self.eviction[s].pop().unwrap().cell_u64;
                let removed = self.shards[s].remove(&key);
                if removed.is_some() && self.shards[s].is_empty() {
                    self.shards[s] = HashMap::with_hasher(FxBuildHasher::default());
                    self.eviction[s] = BinaryHeap::new();
                }
                self.structure_bytes = self.structure_bytes - before + self.shard_bytes(s);
                if let Some(acc) = removed {
                    self.dynamic_bytes -= acc.heap_bytes();
                    return Some((key, acc));
                }
            }
        }
        None
    }

    /// A run is bounded by the active-map budget. Release buckets/heaps rather
    /// than retaining their high-water capacity during external merging.
    pub fn take_sorted(&mut self) -> Vec<(u64, A)> {
        let mut records = Vec::with_capacity(self.active_cell_count());
        for (map, heap) in self.shards.iter_mut().zip(&mut self.eviction) {
            let owned = std::mem::replace(map, HashMap::with_hasher(FxBuildHasher::default()));
            records.extend(owned);
            *heap = BinaryHeap::new();
        }
        self.dynamic_bytes = 0;
        self.structure_bytes = 0;
        records.sort_unstable_by_key(|(key, _)| *key);
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h3o::{LatLng, Resolution};

    fn recounted(map: &ShardedResolutionMap<H3Accumulator>) -> usize {
        let dynamic: usize = map
            .shards
            .iter()
            .flat_map(|s| s.values())
            .map(|a| a.heap_bytes())
            .sum();
        dynamic + (0..NUM_SHARDS).map(|s| map.shard_bytes(s)).sum::<usize>()
    }

    #[test]
    fn incremental_estimate_matches_full_recount() {
        let mut map = ShardedResolutionMap::<H3Accumulator>::new();
        let res = Resolution::try_from(7).unwrap();
        let cells: Vec<u64> = (0..2000)
            .map(|i| {
                let ll = LatLng::new(60.0 - i as f64 * 0.05, -120.0 + (i % 40) as f64 * 0.1);
                u64::from(ll.unwrap().to_cell(res))
            })
            .collect();
        for (i, &cell) in cells.iter().enumerate() {
            let mut acc = if i % 3 == 0 {
                H3Accumulator::with_quantiles()
            } else {
                H3Accumulator::default()
            };
            acc.update(i as f64);
            map.merge_owned(cell, acc);
            assert_eq!(map.estimated_bytes(), recounted(&map));
        }
        // Pop until shards empty out and are replaced.
        while map.pop_completed(f64::NEG_INFINITY).is_some() {
            assert_eq!(map.estimated_bytes(), recounted(&map));
        }
        assert_eq!(map.estimated_bytes(), 0);
        for &cell in &cells[..500] {
            map.merge_owned(cell, H3Accumulator::new(1.0));
        }
        map.set_eviction_enabled(false);
        assert_eq!(map.estimated_bytes(), recounted(&map));
        map.take_sorted();
        assert_eq!(map.estimated_bytes(), 0);
    }
}
