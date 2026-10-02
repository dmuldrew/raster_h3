//! One-level logical H3 child-to-parent compaction.
//!
//! The streaming controller feeds complete cells in H3 order and flushes when
//! the logical parent changes. Siblings are contiguous at a fixed resolution,
//! bounding pending storage to one group (seven hexagon or six pentagon children).
//! This avoids assuming that geographic parent polygons contain logical children.
//! Legacy horizon-based methods remain available but are not used by the controller.

use fxhash::FxBuildHasher;
use h3o::CellIndex;
use std::collections::HashMap;

use super::controller::HorizonStreamKernel;
use super::lifecycle::OutputBuffer;
use super::sharded_map::AccumulatorMerge;
use crate::aggregator::horizon_streamer::compute_cell_south_lat;

/// Generic hierarchical H3 7-to-1 compaction aggregator.
#[allow(clippy::type_complexity)]
pub struct HierarchicalCompactor<K: HorizonStreamKernel> {
    enabled: bool,
    pending: HashMap<u64, (K::Accumulator, Vec<(u64, K::Accumulator)>), FxBuildHasher>,
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

    /// Push an accumulated cell into the compactor or directly into the output buffer.
    ///
    /// If compaction is disabled or the cell cannot be compacted, it is immediately
    /// converted to an output record and pushed to `output`.
    ///
    /// If compaction is enabled:
    /// - Accumulates into the parent cell's merged accumulator and child list.
    /// - If the child count reaches 7, the parent is immediately emitted at resolution `R - 1`.
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

        if self.enabled {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                if let Some(parent_res) = cell.resolution().pred() {
                    if let Some(parent) = cell.parent(parent_res) {
                        let parent_u64: u64 = parent.into();
                        let entry = self.pending.entry(parent_u64).or_insert_with(|| {
                            (kernel.new_parent_accumulator(), Vec::with_capacity(7))
                        });
                        entry.0.merge(&acc);
                        entry.1.push((cell_u64, acc));

                        if entry.1.len() == parent.children(cell.resolution()).count() {
                            let (parent_acc, _) = self.pending.remove(&parent_u64).unwrap();
                            let p_res_u8: u8 = parent_res.into();
                            kernel.buffer_record(p_res_u8, parent_u64, parent_acc, output);
                            return;
                        }
                        return;
                    }
                }
            }
        }

        kernel.buffer_record(res_u8, cell_u64, acc, output);
    }

    /// Evict pending parents that lie strictly north of the scanline latitude horizon.
    ///
    /// Incomplete parents (those with < 7 children) cannot receive any further child cells
    /// because the scanline horizon has passed their southernmost latitude. They are decomposed
    /// and emitted as individual child records at their native resolution.
    pub fn evict_above_horizon(
        &mut self,
        kernel: &K,
        lat_horizon: f64,
        output: &mut OutputBuffer<K::Record>,
    ) {
        if !self.enabled || self.pending.is_empty() {
            return;
        }

        let mut to_flush = Vec::new();
        for &parent_u64 in self.pending.keys() {
            // Logical children protrude outside a parent's geographic polygon.
            // Bound their union instead of treating the parent polygon as a cover.
            let parent_south = CellIndex::try_from(parent_u64)
                .ok()
                .and_then(|parent| {
                    parent.resolution().succ().map(|res| {
                        parent
                            .children(res)
                            .map(|child| compute_cell_south_lat(child.into()))
                            .fold(f64::INFINITY, f64::min)
                    })
                })
                .unwrap_or(-90.0);
            if parent_south > lat_horizon {
                to_flush.push(parent_u64);
            }
        }

        for p in to_flush {
            if let Some((_, children)) = self.pending.remove(&p) {
                for (cell_u64, acc) in children {
                    let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                        cell.resolution().into()
                    } else {
                        8
                    };
                    kernel.buffer_record(res_u8, cell_u64, acc, output);
                }
            }
        }
    }

    /// Flush all remaining pending parents at EOF, decomposing them into child records.
    pub fn flush_all(&mut self, kernel: &K, output: &mut OutputBuffer<K::Record>) {
        for (_, (_, children)) in self.pending.drain() {
            for (cell_u64, acc) in children {
                let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                    cell.resolution().into()
                } else {
                    8
                };
                kernel.buffer_record(res_u8, cell_u64, acc, output);
            }
        }
    }
}
