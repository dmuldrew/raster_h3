//! One-level logical H3 child-to-parent compaction.
//!
//! Final cells are grouped by logical parent. A group emits its parent record
//! once all six (pentagon) or seven children have arrived. Otherwise it is
//! decomposed into child records when no sibling can still arrive: during
//! streaming, once the scanline horizon passes every logical child's southern
//! bound; at EOF, once the sorted stream passes the parent. Grouping is
//! logical, so a parent polygon is never assumed to cover its children.

use fxhash::FxBuildHasher;
use h3o::CellIndex;
use std::collections::HashMap;

use super::controller::HorizonStreamKernel;
use super::lifecycle::OutputBuffer;
use super::sharded_map::AccumulatorMerge;
use crate::aggregator::horizon_streamer::compute_cell_south_lat;

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
}

/// Generic hierarchical H3 7-to-1 compaction aggregator.
pub struct HierarchicalCompactor<K: HorizonStreamKernel> {
    enabled: bool,
    pending: HashMap<u64, Group<K::Accumulator>, FxBuildHasher>,
}

impl<K: HorizonStreamKernel> HierarchicalCompactor<K> {
    /// Initialize a new hierarchical compactor.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pending: HashMap::with_hasher(FxBuildHasher::default()),
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

    /// Clear all pending compaction state.
    pub fn clear(&mut self) {
        self.pending.clear();
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
        let group = self.pending.entry(parent_u64).or_insert_with(|| Group {
            parent_acc: kernel.new_parent_accumulator(),
            children: Vec::with_capacity(7),
            child_res: res_u8,
            siblings_south: parent
                .children(cell.resolution())
                .map(|child| compute_cell_south_lat(child.into()))
                .fold(f64::INFINITY, f64::min),
            emit_south: compute_cell_south_lat(parent_u64),
        });
        group.parent_acc.merge(&acc);
        group.emit_south = group.emit_south.max(compute_cell_south_lat(cell_u64));
        group.children.push((cell_u64, acc));

        // A pentagon has six children at the next resolution.
        let siblings = if parent.is_pentagon() { 6 } else { 7 };
        if group.children.len() == siblings {
            let group = self.pending.remove(&parent_u64).unwrap();
            let parent_res = u8::from(parent.resolution());
            kernel.buffer_record(parent_res, parent_u64, group.parent_acc, output);
        }
    }

    /// Decompose the group for `parent_u64`, if pending. Used when the sorted
    /// EOF stream has moved past that parent.
    pub fn flush_parent(
        &mut self,
        parent_u64: u64,
        kernel: &K,
        output: &mut OutputBuffer<K::Record>,
    ) {
        if let Some(group) = self.pending.remove(&parent_u64) {
            decompose(group, kernel, output);
        }
    }

    /// Decompose groups that can no longer gain a sibling because the horizon
    /// has passed all of their logical children. `streaming(child_res)` must be
    /// false for resolutions whose remaining cells arrive only at EOF.
    pub fn flush_completed(
        &mut self,
        horizon: f64,
        streaming: impl Fn(u8) -> bool,
        kernel: &K,
        output: &mut OutputBuffer<K::Record>,
    ) {
        if self.pending.is_empty() {
            return;
        }
        let done: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, g)| g.siblings_south > horizon && streaming(g.child_res))
            .map(|(&parent, _)| parent)
            .collect();
        for parent in done {
            self.flush_parent(parent, kernel, output);
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
        let done: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, g)| g.child_res == child_res)
            .map(|(&parent, _)| parent)
            .collect();
        for parent in done {
            self.flush_parent(parent, kernel, output);
        }
    }

    /// Northernmost southern bound of any record still pending, or negative
    /// infinity. A published horizon must not pass it.
    pub fn pending_emit_south(&self) -> f64 {
        self.pending
            .values()
            .map(|g| g.emit_south)
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// Flush all remaining pending parents at EOF, decomposing them into child records.
    pub fn flush_all(&mut self, kernel: &K, output: &mut OutputBuffer<K::Record>) {
        for (_, group) in self.pending.drain() {
            decompose(group, kernel, output);
        }
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
