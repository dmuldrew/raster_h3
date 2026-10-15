//! One-level logical H3 child-to-parent compaction.
//!
//! Final cells are grouped by logical parent. A group emits its parent record
//! once all six (pentagon) or seven children have arrived. Otherwise it is
//! decomposed into child records when no sibling can still arrive: during
//! streaming, once the scanline horizon passes every logical child's southern
//! bound; at EOF, once the sorted stream passes the parent. Grouping is
//! logical, so a parent polygon is never assumed to cover its children.
//!
//! Pending groups are indexed by heaps so completion checks cost
//! O(log pending) per finished group instead of a scan per batch. Their
//! owned state is accounted in bytes; past the byte limit the groups closest
//! to completion are decomposed early. Early decomposition only forgoes
//! compaction: those children are emitted individually and their remaining
//! siblings can never form a complete group, so no parent duplicates them.

use crate::aggregator::cell_hash::H3BuildHasher;
use h3o::CellIndex;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use super::controller::HorizonStreamKernel;
use super::lifecycle::OutputBuffer;
use super::sharded_map::AccumulatorMerge;
use super::spill::table_bytes;
use crate::aggregator::horizon_streamer::compute_cell_south_lat;

/// H3 resolutions 0..=15.
const RESOLUTIONS: usize = 16;

/// Children of one logical parent awaiting completion.
struct Group<A> {
    parent_acc: A,
    children: Vec<(u64, A)>,
    child_res: u8,
    /// Southern bound of every logical child: once the horizon is south of
    /// it, no sibling can still be finalized.
    siblings_south: f64,
    /// Northernmost southern bound of any record this group may still emit.
    emit_south: f64,
    /// Owned bytes outside the pending table: child storage and heap state.
    bytes: usize,
}

impl<A: AccumulatorMerge> Group<A> {
    fn owned_bytes(&self) -> usize {
        self.children.capacity() * std::mem::size_of::<(u64, A)>()
            + self.parent_acc.heap_bytes()
            + self
                .children
                .iter()
                .map(|(_, a)| a.heap_bytes())
                .sum::<usize>()
    }
}

/// Max-heap entry keyed by a latitude. Entries are invalidated lazily: an
/// entry is live only while its parent is pending with the same key.
#[derive(Clone, Copy)]
struct HeapEntry {
    lat: f64,
    parent: u64,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.lat
            .total_cmp(&other.lat)
            .then(self.parent.cmp(&other.parent))
    }
}

/// Generic hierarchical H3 7-to-1 compaction aggregator.
pub struct HierarchicalCompactor<K: HorizonStreamKernel> {
    enabled: bool,
    pending: HashMap<u64, Group<K::Accumulator>, H3BuildHasher>,
    /// Per child resolution, groups by `siblings_south`, northernmost first.
    completion: Vec<BinaryHeap<HeapEntry>>,
    /// Groups by `emit_south`, northernmost first.
    emit: BinaryHeap<HeapEntry>,
    /// Sum of `Group::bytes` over pending groups.
    group_bytes: usize,
    byte_limit: usize,
    forced_flushes: u64,
    peak_bytes: usize,
}

impl<K: HorizonStreamKernel> HierarchicalCompactor<K> {
    /// Initialize a new hierarchical compactor without a byte limit.
    pub fn new(enabled: bool) -> Self {
        Self::with_byte_limit(enabled, usize::MAX)
    }

    /// Initialize a compactor that decomposes groups early once its estimated
    /// pending state exceeds `byte_limit`.
    pub fn with_byte_limit(enabled: bool, byte_limit: usize) -> Self {
        Self {
            enabled,
            pending: HashMap::with_hasher(H3BuildHasher),
            completion: (0..RESOLUTIONS).map(|_| BinaryHeap::new()).collect(),
            emit: BinaryHeap::new(),
            group_bytes: 0,
            byte_limit,
            forced_flushes: 0,
            peak_bytes: 0,
        }
    }

    /// Whether hierarchical 7-cell compaction is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Number of incomplete parent cells currently pending compaction.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether no parent cells are currently pending.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Estimated bytes held by pending groups, their table and indexes.
    pub fn estimated_bytes(&self) -> usize {
        let heap_entries = self.emit.capacity()
            + self
                .completion
                .iter()
                .map(BinaryHeap::capacity)
                .sum::<usize>();
        table_bytes::<u64, Group<K::Accumulator>>(self.pending.capacity())
            + self.group_bytes
            + heap_entries * std::mem::size_of::<HeapEntry>()
    }

    /// Highest `estimated_bytes` observed.
    pub fn peak_bytes(&self) -> usize {
        self.peak_bytes
    }

    /// Groups decomposed before completion to stay within the byte limit.
    pub fn forced_flushes(&self) -> u64 {
        self.forced_flushes
    }

    /// Clear all pending compaction state.
    pub fn clear(&mut self) {
        self.pending = HashMap::with_hasher(H3BuildHasher);
        self.completion
            .iter_mut()
            .for_each(|h| *h = BinaryHeap::new());
        self.emit = BinaryHeap::new();
        self.group_bytes = 0;
    }

    /// Push a final cell into its parent group, or straight to `output` when
    /// compaction is disabled or the cell has no parent. A complete group is
    /// emitted immediately as one parent record at resolution `R - 1`.
    pub fn push_cell(
        &mut self,
        kernel: &K,
        res_u8: u8,
        cell_u64: u64,
        acc: K::Accumulator,
        output: &mut OutputBuffer<K::Record>,
    ) {
        if !kernel.passes_filter(&acc) {
            return;
        }
        let cell = CellIndex::try_from(cell_u64).ok().filter(|_| self.enabled);
        let Some((cell, parent)) =
            cell.and_then(|cell| Some((cell, cell.parent(cell.resolution().pred()?)?)))
        else {
            kernel.buffer_record(res_u8, cell_u64, acc, output);
            return;
        };

        let parent_u64 = u64::from(parent);
        let group = match self.pending.entry(parent_u64) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let siblings_south = parent
                    .children(cell.resolution())
                    .map(|child| compute_cell_south_lat(child.into()))
                    .fold(f64::INFINITY, f64::min);
                self.completion[usize::from(res_u8) % RESOLUTIONS].push(HeapEntry {
                    lat: siblings_south,
                    parent: parent_u64,
                });
                entry.insert(Group {
                    parent_acc: kernel.new_parent_accumulator(),
                    children: Vec::with_capacity(7),
                    child_res: res_u8,
                    siblings_south,
                    emit_south: compute_cell_south_lat(parent_u64),
                    bytes: 0,
                })
            }
        };
        group.parent_acc.merge(&acc);
        let cell_south = compute_cell_south_lat(cell_u64);
        if cell_south > group.emit_south {
            group.emit_south = cell_south;
        }
        group.children.push((cell_u64, acc));
        self.emit.push(HeapEntry {
            lat: group.emit_south,
            parent: parent_u64,
        });

        // A pentagon has six children at the next resolution.
        let siblings = if parent.is_pentagon() { 6 } else { 7 };
        if group.children.len() == siblings {
            let group = self.remove(parent_u64).unwrap();
            let parent_res = u8::from(parent.resolution());
            kernel.buffer_record(parent_res, parent_u64, group.parent_acc, output);
        } else {
            let bytes = group.owned_bytes();
            self.group_bytes = self.group_bytes - group.bytes + bytes;
            group.bytes = bytes;
        }
        self.compact_indexes();
        self.enforce_limit(kernel, output);
    }

    fn remove(&mut self, parent_u64: u64) -> Option<Group<K::Accumulator>> {
        let group = self.pending.remove(&parent_u64)?;
        self.group_bytes -= group.bytes;
        Some(group)
    }

    /// Decompose the group for `parent_u64`, if pending. Used when the sorted
    /// EOF stream has moved past that parent.
    pub fn flush_parent(
        &mut self,
        parent_u64: u64,
        kernel: &K,
        output: &mut OutputBuffer<K::Record>,
    ) {
        if let Some(group) = self.remove(parent_u64) {
            decompose(group, kernel, output);
        }
    }

    /// Decompose groups that can no longer gain a sibling because the horizon
    /// has passed all of their logical children. `streaming(child_res)` must be
    /// false for resolutions whose remaining cells arrive only at EOF. Stops
    /// once `output` holds at least `max_output_bytes`; later calls resume.
    pub fn flush_completed(
        &mut self,
        horizon: f64,
        streaming: impl Fn(u8) -> bool,
        kernel: &K,
        output: &mut OutputBuffer<K::Record>,
        max_output_bytes: usize,
    ) {
        for res in 0..RESOLUTIONS {
            if self.completion[res].is_empty() || !streaming(res as u8) {
                continue;
            }
            while let Some(&top) = self.completion[res].peek() {
                if top.lat <= horizon || output.estimated_bytes() >= max_output_bytes {
                    break;
                }
                self.completion[res].pop();
                if self.is_live_completion(res, top) {
                    self.flush_parent(top.parent, kernel, output);
                }
            }
        }
    }

    /// Decompose every group whose children are at `child_res`, after that
    /// resolution's final sorted stream has been consumed.
    pub fn flush_resolution(
        &mut self,
        child_res: u8,
        kernel: &K,
        output: &mut OutputBuffer<K::Record>,
    ) {
        let res = usize::from(child_res) % RESOLUTIONS;
        for entry in std::mem::take(&mut self.completion[res]).into_vec() {
            if self.is_live_completion(res, entry) {
                self.flush_parent(entry.parent, kernel, output);
            }
        }
    }

    /// Northernmost southern bound of any record still pending, or negative
    /// infinity. A published horizon must not pass it.
    pub fn pending_emit_south(&mut self) -> f64 {
        while let Some(&top) = self.emit.peek() {
            if self
                .pending
                .get(&top.parent)
                .is_some_and(|g| g.emit_south == top.lat)
            {
                return top.lat;
            }
            self.emit.pop();
        }
        f64::NEG_INFINITY
    }

    /// Flush all remaining pending parents at EOF, decomposing them into child records.
    pub fn flush_all(&mut self, kernel: &K, output: &mut OutputBuffer<K::Record>) {
        for (_, group) in self.pending.drain() {
            decompose(group, kernel, output);
        }
        self.clear();
    }

    fn is_live_completion(&self, res: usize, entry: HeapEntry) -> bool {
        self.pending.get(&entry.parent).is_some_and(|g| {
            usize::from(g.child_res) % RESOLUTIONS == res && g.siblings_south == entry.lat
        })
    }

    /// Rebuild the lazily invalidated heaps once stale entries dominate, so
    /// index memory stays proportional to pending groups.
    fn compact_indexes(&mut self) {
        let live = self.pending.len();
        let completion: usize = self.completion.iter().map(BinaryHeap::len).sum();
        if completion > 2 * live + 64 {
            for heap in &mut self.completion {
                heap.clear();
            }
            for (&parent, g) in &self.pending {
                self.completion[usize::from(g.child_res) % RESOLUTIONS].push(HeapEntry {
                    lat: g.siblings_south,
                    parent,
                });
            }
        }
        if self.emit.len() > 4 * live + 64 {
            self.emit = self
                .pending
                .iter()
                .map(|(&parent, g)| HeapEntry {
                    lat: g.emit_south,
                    parent,
                })
                .collect();
        }
    }

    /// Decompose the groups nearest completion until the estimate fits.
    fn enforce_limit(&mut self, kernel: &K, output: &mut OutputBuffer<K::Record>) {
        let mut bytes = self.estimated_bytes();
        self.peak_bytes = self.peak_bytes.max(bytes);
        if bytes <= self.byte_limit {
            return;
        }
        // Removals leave tombstones that can grow the table without adding
        // groups, and decomposing groups never shrinks it. Reclaim first so
        // excess capacity is not paid for with groups that could still compact.
        self.reclaim();
        bytes = self.estimated_bytes();
        while bytes > self.byte_limit && !self.pending.is_empty() {
            let Some((res, entry)) = self.next_to_complete() else {
                break;
            };
            self.completion[res].pop();
            self.flush_parent(entry.parent, kernel, output);
            self.forced_flushes += 1;
            bytes = self.estimated_bytes();
        }
        if bytes > self.byte_limit {
            self.reclaim();
        }
    }

    /// Release table capacity and stale heap entries left behind by removal.
    fn reclaim(&mut self) {
        self.pending.shrink_to_fit();
        self.compact_indexes();
        self.emit.shrink_to_fit();
        self.completion
            .iter_mut()
            .for_each(BinaryHeap::shrink_to_fit);
    }

    /// Live group with the northernmost `siblings_south` over all resolutions.
    fn next_to_complete(&mut self) -> Option<(usize, HeapEntry)> {
        let mut best: Option<(usize, HeapEntry)> = None;
        for res in 0..RESOLUTIONS {
            while let Some(&top) = self.completion[res].peek() {
                if self.is_live_completion(res, top) {
                    if best.is_none_or(|(_, b)| top > b) {
                        best = Some((res, top));
                    }
                    break;
                }
                self.completion[res].pop();
            }
        }
        best
    }
}

fn decompose<K: HorizonStreamKernel>(
    group: Group<K::Accumulator>,
    kernel: &K,
    output: &mut OutputBuffer<K::Record>,
) {
    for (cell_u64, acc) in group.children {
        kernel.buffer_record(group.child_res, cell_u64, acc, output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::accumulator::H3Accumulator;
    use crate::aggregator::multi_horizon::continuous_streamer::ContinuousKernel;
    use h3o::{LatLng, Resolution};

    fn kernel() -> ContinuousKernel {
        ContinuousKernel {
            band: 1,
            spectral_formula: None,
            min_count: None,
            min_mean: None,
            max_mean: None,
            track_quantiles: false,
        }
    }

    /// Res-7 parents along a meridian, north to south, none at a pentagon.
    fn parents(n: usize) -> Vec<CellIndex> {
        let mut out: Vec<CellIndex> = Vec::new();
        let mut lat = 40.0;
        while out.len() < n {
            let cell = LatLng::new(lat, -100.0).unwrap().to_cell(Resolution::Seven);
            if out.last() != Some(&cell) && !cell.is_pentagon() {
                out.push(cell);
            }
            lat -= 0.005;
        }
        out
    }

    fn drain(
        output: &mut OutputBuffer<<ContinuousKernel as HorizonStreamKernel>::Record>,
    ) -> Vec<(u8, u64, f64)> {
        let mut records = Vec::new();
        output.drain_into(usize::MAX, |_, r| {
            records.push((r.resolution, r.h3_index, r.accumulator.count))
        });
        records
    }

    #[test]
    fn heap_completion_matches_scan_semantics() {
        let k = kernel();
        let mut compactor = HierarchicalCompactor::<ContinuousKernel>::new(true);
        let mut output = OutputBuffer::with_capacity(16);
        let groups = parents(40);
        let mut south = HashMap::new();
        let mut emit_south = f64::NEG_INFINITY;
        for parent in &groups {
            // Six of seven children: never complete.
            let children: Vec<CellIndex> = parent.children(Resolution::Eight).collect();
            south.insert(
                u64::from(*parent),
                children
                    .iter()
                    .map(|c| compute_cell_south_lat((*c).into()))
                    .fold(f64::INFINITY, f64::min),
            );
            emit_south = emit_south.max(compute_cell_south_lat((*parent).into()));
            for child in &children[..6] {
                emit_south = emit_south.max(compute_cell_south_lat((*child).into()));
                compactor.push_cell(&k, 8, (*child).into(), H3Accumulator::new(1.0), &mut output);
            }
        }
        assert!(output.is_empty());
        assert_eq!(compactor.pending_emit_south(), emit_south);

        let mut lats: Vec<f64> = south.values().copied().collect();
        lats.sort_by(f64::total_cmp);
        let horizon = lats[lats.len() / 2];
        compactor.flush_completed(horizon, |_| true, &k, &mut output, usize::MAX);
        let flushed: std::collections::HashSet<u64> = drain(&mut output)
            .into_iter()
            .map(|(_, cell, _)| {
                u64::from(
                    CellIndex::try_from(cell)
                        .unwrap()
                        .parent(Resolution::Seven)
                        .unwrap(),
                )
            })
            .collect();
        let expected: std::collections::HashSet<u64> = south
            .iter()
            .filter(|(_, &s)| s > horizon)
            .map(|(&p, _)| p)
            .collect();
        assert_eq!(flushed, expected);
        assert_eq!(compactor.len(), groups.len() - expected.len());

        // Non-streaming resolutions are never flushed by the horizon.
        compactor.flush_completed(f64::NEG_INFINITY, |_| false, &k, &mut output, usize::MAX);
        assert!(output.is_empty());

        // An output cap stops flushing early; later calls resume.
        compactor.flush_completed(f64::NEG_INFINITY, |_| true, &k, &mut output, 1);
        assert_eq!(output.len(), 6);
        compactor.flush_completed(f64::NEG_INFINITY, |_| true, &k, &mut output, usize::MAX);
        assert!(compactor.is_empty());
        assert_eq!(compactor.pending_emit_south(), f64::NEG_INFINITY);
    }

    #[test]
    fn byte_limit_decomposes_groups_and_preserves_every_cell() {
        let k = kernel();
        let limit = 16 * 1024;
        let mut compactor = HierarchicalCompactor::<ContinuousKernel>::with_byte_limit(true, limit);
        let mut output = OutputBuffer::with_capacity(16);
        let groups = parents(400);
        let mut pushed = 0;
        for (i, parent) in groups.iter().enumerate() {
            let children: Vec<CellIndex> = parent.children(Resolution::Eight).collect();
            // Every fifth group is complete and should still compact when it
            // fits; the rest stay pending until forced out.
            let n = if i % 5 == 0 { 7 } else { 6 };
            for child in &children[..n] {
                compactor.push_cell(&k, 8, (*child).into(), H3Accumulator::new(1.0), &mut output);
                pushed += 1;
                assert!(
                    compactor.estimated_bytes() <= limit,
                    "{}",
                    compactor.estimated_bytes()
                );
            }
        }
        assert!(compactor.forced_flushes() > 0);
        compactor.flush_all(&k, &mut output);
        let records = drain(&mut output);
        let total: f64 = records.iter().map(|r| r.2).sum();
        assert_eq!(total, pushed as f64, "every sample emitted once");
        let mut seen = std::collections::HashSet::new();
        for (res, cell, count) in &records {
            assert!(seen.insert(*cell), "duplicate {cell:x}");
            if *res == 7 {
                assert_eq!(*count, 7.0, "only complete groups compact");
            }
        }
        assert!(
            records.iter().any(|r| r.0 == 7),
            "complete groups still compact"
        );
        for (_, cell, _) in &records {
            let c = CellIndex::try_from(*cell).unwrap();
            if c.resolution() == Resolution::Eight {
                let parent = u64::from(c.parent(Resolution::Seven).unwrap());
                assert!(!seen.contains(&parent), "child and parent both emitted");
            }
        }
    }
}
