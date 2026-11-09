use crate::aggregator::cell_hash::{mix_h3, H3BuildHasher};
use std::collections::{BinaryHeap, HashMap};

use super::eviction_space::CellKeys;
use super::spill::table_bytes;
use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::horizon_streamer::compute_cell_south_lat;
use crate::aggregator::quantiles::QuantileSketch;

/// Number of concurrent shards for parallel active map merging
pub const NUM_SHARDS: usize = 32;

/// Mix the full H3 index before selecting a power-of-two shard.
/// Unused H3 child digits fill the low bits with ones, so masking the low
/// bits of a simple product sends every cell at resolutions 0..=13 to one shard.
///
/// Shard maps hash with the same finalizer ([`H3BuildHasher`]), and hashbrown
/// takes bucket positions from the low bits and tags from the top 7. The shard
/// therefore comes from bits 32.., so keys within one shard still spread over
/// every bucket and tag.
#[inline(always)]
pub fn get_shard(cell_u64: u64) -> usize {
    ((mix_h3(cell_u64) >> 32) as usize) & (NUM_SHARDS - 1)
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
            size_of::<QuantileSketch>()
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
            size_of_val(&**h) + table_bytes::<i64, f64>(h.capacity())
        })
    }

    #[inline(always)]
    fn merge(&mut self, other: &Self) {
        CategoricalAccumulator::merge(self, other);
    }
}

/// A pending eviction: the cell's key (see [`super::eviction_space`]) and the
/// latitude-histogram bucket it was counted in.
#[derive(Debug, Clone, Copy)]
struct EvictionEntry {
    key: f64,
    cell_u64: u64,
    lat_bucket: u32,
}

impl PartialEq for EvictionEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for EvictionEntry {}

impl Ord for EvictionEntry {
    /// Max-heap on the key: the earliest-completing cell is popped first.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.total_cmp(&other.key)
    }
}

impl PartialOrd for EvictionEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Buckets per degree of [`LatHistogram`].
const LAT_BUCKETS_PER_DEG: f64 = 100.0;
const LAT_BUCKETS: usize = 180 * LAT_BUCKETS_PER_DEG as usize + 1;

/// Counts of held cells by an upper bound of their southern latitude, rounded
/// up to 0.01°, so the largest such bound among held cells is cheap to read.
#[derive(Debug, Clone)]
struct LatHistogram {
    counts: Vec<u32>,
    held: usize,
    /// No bucket above this index is non-empty.
    top: usize,
}

impl LatHistogram {
    fn new() -> Self {
        Self {
            counts: vec![0; LAT_BUCKETS],
            held: 0,
            top: 0,
        }
    }

    fn bucket(lat: f64) -> u32 {
        let b = ((lat + 90.0) * LAT_BUCKETS_PER_DEG).ceil();
        if b.is_nan() {
            (LAT_BUCKETS - 1) as u32
        } else {
            b.clamp(0.0, (LAT_BUCKETS - 1) as f64) as u32
        }
    }

    fn insert(&mut self, bucket: u32) {
        let b = bucket as usize;
        self.counts[b] += 1;
        self.held += 1;
        self.top = self.top.max(b);
    }

    fn remove(&mut self, bucket: u32) {
        let b = bucket as usize;
        self.counts[b] -= 1;
        self.held -= 1;
        while self.top > 0 && self.counts[self.top] == 0 {
            self.top -= 1;
        }
    }

    /// At least the southern latitude of every held cell; `-inf` when empty.
    fn max_south_lat(&self) -> f64 {
        if self.held == 0 {
            f64::NEG_INFINITY
        } else {
            // Bucket b covers latitudes up to b / 100 - 90; round outwards.
            (self.top as f64 / LAT_BUCKETS_PER_DEG - 90.0).next_up()
        }
    }

    fn clear(&mut self) {
        self.counts.iter_mut().for_each(|c| *c = 0);
        self.held = 0;
        self.top = 0;
    }
}

/// A 32-way hash-partitioned active map and eviction heap for a single H3 resolution.
///
/// The controller joins bounded worker waves, merges owned states with budget
/// checks, and pops completed cells incrementally. Merging and eviction both
/// require exclusive access, so the phases cannot overlap in safe Rust.
pub struct ShardedResolutionMap<A: AccumulatorMerge> {
    shards: Vec<HashMap<u64, A, H3BuildHasher>>,
    eviction: Vec<BinaryHeap<EvictionEntry>>,
    /// Southern-latitude bounds of held cells, when eviction keys are not
    /// latitudes (see [`Self::set_lat_tracking`]).
    lat_histogram: Option<LatHistogram>,
    dynamic_bytes: usize,
    /// Table and heap allocation, maintained incrementally so budget checks
    /// stay O(1) per merged cell.
    structure_bytes: usize,
    track_eviction: bool,
}

impl<A: AccumulatorMerge> Default for ShardedResolutionMap<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: AccumulatorMerge> ShardedResolutionMap<A> {
    /// Create a new ShardedResolutionMap with `NUM_SHARDS` shards
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        let mut eviction = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(HashMap::with_capacity_and_hasher(0, H3BuildHasher));
            eviction.push(BinaryHeap::new());
        }
        Self {
            shards,
            eviction,
            dynamic_bytes: 0,
            structure_bytes: 0,
            track_eviction: true,
            lat_histogram: None,
        }
    }

    #[inline]
    fn shard_bytes(&self, s: usize) -> usize {
        table_bytes::<u64, A>(self.shards[s].capacity())
            + self.eviction[s].capacity() * size_of::<EvictionEntry>()
    }

    fn recount_structure(&mut self) {
        self.structure_bytes = (0..NUM_SHARDS).map(|s| self.shard_bytes(s)).sum();
    }

    /// Return total active in-flight cell count across all 32 shards
    pub fn active_cell_count(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    /// The active accumulator for `cell`, if any
    pub fn get(&self, cell: u64) -> Option<&A> {
        self.shards[get_shard(cell)].get(&cell)
    }

    /// Whether `cell` is active in this map.
    pub fn contains(&self, cell: u64) -> bool {
        self.shards[get_shard(cell)].contains_key(&cell)
    }

    /// Whether newly inserted cells get an eviction key (false once spilled).
    pub fn tracks_eviction(&self) -> bool {
        self.track_eviction
    }

    /// Count held cells by southern latitude, so [`Self::held_south_lat_bound`]
    /// works when eviction keys are not latitudes. Set before inserting.
    pub fn set_lat_tracking(&mut self, enabled: bool) {
        self.lat_histogram = enabled.then(LatHistogram::new);
    }

    /// At least the southern latitude of every cell still awaiting eviction,
    /// `-inf` when none is; `None` without [`Self::set_lat_tracking`].
    pub fn held_south_lat_bound(&self) -> Option<f64> {
        self.lat_histogram.as_ref().map(LatHistogram::max_south_lat)
    }

    /// Per shard: (active cells, pending eviction entries)
    pub fn shard_occupancy(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.shards
            .iter()
            .zip(&self.eviction)
            .map(|(map, heap)| (map.len(), heap.len()))
    }
}

impl<A: super::spill::SpillAccumulator> ShardedResolutionMap<A> {
    pub fn set_eviction_enabled(&mut self, enabled: bool) {
        self.track_eviction = enabled;
        if !enabled {
            for heap in &mut self.eviction {
                *heap = BinaryHeap::new();
            }
            if let Some(h) = &mut self.lat_histogram {
                h.clear();
            }
            self.recount_structure();
        }
    }

    /// Owned merge avoids cloning a large quantile or categorical state.
    /// Returns the merged accumulator's memory footprint.
    /// New cells are keyed by latitude ([`compute_cell_south_lat`]).
    pub fn merge_owned(&mut self, key: u64, acc: A) -> usize {
        self.merge_owned_with(key, acc, || {
            let south = compute_cell_south_lat(key);
            CellKeys {
                key: south,
                south_lat_upper: south,
            }
        })
    }

    /// [`Self::merge_owned`] taking the cell's eviction keys from `keys`,
    /// which must return the stream's [`super::eviction_space::EvictionSpace::cell_keys`]
    /// of `key`. It is called only when the cell is new and eviction is
    /// tracked, so callers can compute the keys ahead of time, off the
    /// merging thread.
    pub fn merge_owned_with(&mut self, key: u64, acc: A, keys: impl FnOnce() -> CellKeys) -> usize {
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
                    let keys = keys();
                    let lat_bucket = LatHistogram::bucket(keys.south_lat_upper);
                    if let Some(h) = &mut self.lat_histogram {
                        h.insert(lat_bucket);
                    }
                    self.eviction[s].push(EvictionEntry {
                        key: keys.key,
                        cell_u64: key,
                        lat_bucket,
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
            while self.eviction[s].peek().is_some_and(|e| e.key > horizon) {
                // Removal can leave a tombstone that lowers reported capacity.
                let before = self.shard_bytes(s);
                let entry = self.eviction[s].pop().unwrap();
                if let Some(h) = &mut self.lat_histogram {
                    h.remove(entry.lat_bucket);
                }
                let key = entry.cell_u64;
                let removed = self.shards[s].remove(&key);
                if removed.is_some() && self.shards[s].is_empty() {
                    self.shards[s] = HashMap::with_hasher(H3BuildHasher);
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
            let owned = std::mem::replace(map, HashMap::with_hasher(H3BuildHasher));
            records.extend(owned);
            *heap = BinaryHeap::new();
        }
        if let Some(h) = &mut self.lat_histogram {
            h.clear();
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
