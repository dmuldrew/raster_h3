//! Merging the partial cells of a partitioned scan (see [`super::partition`]).
//!
//! Partial states travel between workers as bytes ([`encode_partial_state`]) in the same
//! lossless format the controller uses for spill runs, so merging them reproduces exactly
//! what a single scan would have merged in memory, up to floating-point summation order.

use rustc_hash::FxHashMap;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::error::{RasterH3Error, Result};

use super::controller::HorizonStreamKernel;
use super::partition::PartialCell;
use super::sharded_map::AccumulatorMerge;
use super::spill::SpillAccumulator;

/// Magic prefix of an encoded partial state.
const MAGIC: &[u8; 4] = b"RH3P";
/// Encoding version, bumped on any change to the state layout. Version 2 added the
/// continuous state's sum of squared weights; version 1 states are rejected because
/// their weighted variance cannot be recovered.
const VERSION: u8 = 2;

/// Accumulator types whose partial states can be exchanged between workers.
pub trait PartialState: SpillAccumulator + Default {
    /// Distinguishes state kinds so a continuous state is never read as categorical.
    const KIND: u8;
}

impl PartialState for H3Accumulator {
    const KIND: u8 = 1;
}

impl PartialState for CategoricalAccumulator {
    const KIND: u8 = 2;
}

/// Serialize an accumulator for [`decode_partial_state`] on another worker.
pub fn encode_partial_state<A: PartialState>(acc: &A) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(A::KIND);
    acc.write_state(&mut out)
        .expect("writing to a Vec cannot fail");
    out
}

/// Parse a state written by [`encode_partial_state`]. `limit` bounds the decoded state's
/// size in bytes (e.g. the aggregation budget), guarding against corrupt input.
pub fn decode_partial_state<A: PartialState>(bytes: &[u8], limit: usize) -> Result<A> {
    let invalid =
        |why: &str| RasterH3Error::InvalidParameter(format!("invalid partial state: {why}"));
    let Some(rest) = bytes.strip_prefix(MAGIC.as_slice()) else {
        return Err(invalid("missing RH3P prefix"));
    };
    let [version, kind, rest @ ..] = rest else {
        return Err(invalid("truncated header"));
    };
    if *version != VERSION {
        return Err(invalid(&format!("unsupported version {version}")));
    }
    if *kind != A::KIND {
        return Err(invalid(&format!("state kind {kind}, expected {}", A::KIND)));
    }
    let mut input = rest;
    let acc = A::read_state(&mut input, limit).map_err(|e| invalid(&e.to_string()))?;
    if !input.is_empty() {
        return Err(invalid("trailing bytes"));
    }
    Ok(acc)
}

/// Merges the partial cells of all partitions of a scan into final records.
///
/// Feed it every partial cell (any order, any partition) with [`Self::push`] or
/// [`Self::push_encoded`], then call [`Self::finish`]. Filters (`min_count`, `min_mean`,
/// `max_mean`, `min_majority_fraction`) are applied to merged states only. The merger
/// holds every boundary cell in memory, typically a small fraction of the output.
pub struct PartialMerger<K: HorizonStreamKernel> {
    kernel: K,
    cells: FxHashMap<(u8, u64), K::Accumulator>,
}

impl<K> PartialMerger<K>
where
    K: HorizonStreamKernel,
    K::Accumulator: PartialState,
{
    /// A merger finalizing records with `kernel`'s filters and record layout. Use the
    /// `partial_merger` constructor of the matching streamer to build it from a config.
    pub fn new(kernel: K) -> Self {
        Self {
            kernel,
            cells: FxHashMap::default(),
        }
    }

    /// Merge one partition's share of a cell.
    pub fn push(&mut self, cell: PartialCell<K::Accumulator>) {
        match self.cells.entry((cell.resolution, cell.h3_index)) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().merge(&cell.accumulator)
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(cell.accumulator);
            }
        }
    }

    /// Merge one partition's share of a cell from its [`encode_partial_state`] bytes.
    pub fn push_encoded(
        &mut self,
        resolution: u8,
        h3_index: u64,
        state: &[u8],
        limit: usize,
    ) -> Result<()> {
        let accumulator = decode_partial_state(state, limit)?;
        self.push(PartialCell {
            resolution,
            h3_index,
            accumulator,
        });
        Ok(())
    }

    /// Number of distinct cells merged so far.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether nothing has been merged.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Final records for every merged cell passing the filters, sorted by
    /// `(resolution, h3_index)`.
    pub fn finish(self) -> Vec<K::Record> {
        let mut cells: Vec<_> = self.cells.into_iter().collect();
        cells.sort_unstable_by_key(|&(key, _)| key);
        cells
            .into_iter()
            .filter(|(_, acc)| self.kernel.passes_filter(acc))
            .map(|((res, h3), acc)| self.kernel.make_record(res, h3, acc))
            .collect()
    }
}
