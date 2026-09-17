//! Hierarchical H3 child-to-parent compaction component.
//!
//! # Architecture & Correctness Invariants
//!
//! The H3 discrete global grid system supports hierarchical aggregation where 7 aperture-7
//! child cells at resolution `R` compose a single parent cell at resolution `R - 1`.
//!
//! 1. **Disambiguation from Column Omission**:
//!    - **Hierarchical Compaction** (`compact_h3_children` / `compact`): Merges 7 fine-resolution
//!      child cells into a coarser parent hexagon during scanline streaming.
//!    - **Column Omission** (`omit_redundant_columns` / `compact` in Parquet): Omits redundant
//!      metadata columns from Parquet serialization.
//!    - These two concepts are completely decoupled in this module.
//!
//! 2. **Adjacent Resolution Rejection**:
//!    - If a multi-resolution query requests both child `R` and parent `R - 1` (e.g. `[7, 8]`),
//!      hierarchical compaction MUST NOT be enabled because full child sets at `R` would produce
//!      coarser `R - 1` cells that duplicate cells already produced by the explicit `R - 1`
//!      aggregation layer. This invariant is validated when constructing [`MultiHorizonStreamer`].
//!
//! 3. **Completeness Invariant**:
//!    - A parent cell at resolution `R - 1` is emitted if and only if all 7 children of that parent
//!      are accumulated before the scanline horizon passes the parent's southernmost extent.
//!    - When child count reaches 7, the merged parent accumulator is emitted at resolution `R - 1`,
//!      and the pending state for that parent is dropped.
//!
//! 4. **Horizon Eviction Invariant**:
//!    - Each parent's southernmost latitude (`compute_cell_south_lat(parent_u64)`) defines the
//!      absolute lowest latitude of any point inside its 7 child cells.
//!    - When the streaming scanline horizon `lat_horizon` drops strictly south of `parent_south_lat`,
//!      no future raster chunks can intersect any remaining children of this parent.
//!    - Any parent in pending storage with fewer than 7 children when evicted cannot be completed.
//!      The compactor decomposes it and emits its individual child records at resolution `R`.
//!
//! 5. **Terminal Flush**:
//!    - At EOF, all remaining incomplete parents are flushed, decomposing their accumulated
//!      partial sets back into child records.

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
            pending: HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default()),
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

                        if entry.1.len() == 7 {
                            let (parent_acc, _) = self.pending.remove(&parent_u64).unwrap();
                            let p_res_u8: u8 = parent_res.into();
                            output.push(kernel.make_record(p_res_u8, parent_u64, parent_acc));
                            return;
                        }
                        return;
                    }
                }
            }
        }

        output.push(kernel.make_record(res_u8, cell_u64, acc));
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
            let parent_south = compute_cell_south_lat(parent_u64);
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
                    output.push(kernel.make_record(res_u8, cell_u64, acc));
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
                output.push(kernel.make_record(res_u8, cell_u64, acc));
            }
        }
    }
}
