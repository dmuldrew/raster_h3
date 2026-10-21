//! Lossless, externally merged overview aggregates. Both parent accumulation and
//! tile grouping spill, so neither retains a whole dataset in memory. Overview
//! zooms are disjoint from detail zooms and are written exactly once per tile.
use std::io::{self, Read, Write};

use h3o::{CellIndex, LatLng, Resolution};

use super::{
    mvt::{FeatureProperties, MercatorPoint, MvtLayer, PropertyFilter},
    pyramid::{
        cell_boundary_mercator, for_each_cell_tile_mercator, h3_res_for_zoom,
        Wgs84BoundsAccumulator,
    },
    writer::{tile_id_to_zxy, zxy_to_tile_id, PmtilesWriter},
};
use crate::aggregator::multi_horizon::{
    sharded_map::{AccumulatorMerge, ShardedResolutionMap},
    spill::{SpillAccumulator, SpillRuns},
};

// Each phase holds a bounded map plus merge readers and one accumulator/tile.
const BUFFER_BYTES: usize = 16 * 1024 * 1024;

struct BufferedMerge<A: SpillAccumulator> {
    map: ShardedResolutionMap<A>,
    runs: SpillRuns<A>,
    budget: usize,
}

impl<A: SpillAccumulator> BufferedMerge<A> {
    fn new(budget: usize) -> Self {
        let mut map = ShardedResolutionMap::new();
        map.set_eviction_enabled(false);
        Self {
            map,
            runs: SpillRuns::new(usize::MAX, None),
            budget,
        }
    }

    fn push(&mut self, key: u64, value: A) -> io::Result<()> {
        self.map.merge_owned(key, value);
        if self.map.estimated_bytes() >= self.budget {
            self.runs.push_sorted(self.map.take_sorted())?;
        }
        Ok(())
    }

    fn finish(mut self, mut consume: impl FnMut(u64, A) -> io::Result<()>) -> io::Result<()> {
        if !self.runs.has_spilled() {
            for (key, acc) in self.map.take_sorted() {
                consume(key, acc)?;
            }
        } else {
            self.runs.push_sorted(self.map.take_sorted())?;
            if let Some(mut reader) = self.runs.finish()? {
                while let Some((key, acc)) = reader.next_record()? {
                    consume(key, acc)?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct TileCells<A>(Vec<(u64, A)>);

impl<A: SpillAccumulator> AccumulatorMerge for TileCells<A> {
    fn merge(&mut self, other: &Self) {
        self.0.extend(other.0.iter().cloned());
    }
    fn heap_bytes(&self) -> usize {
        self.0.capacity() * size_of::<(u64, A)>()
            + self.0.iter().map(|(_, a)| a.heap_bytes()).sum::<usize>()
    }
}

impl<A: SpillAccumulator> SpillAccumulator for TileCells<A> {
    fn write_state(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&(self.0.len() as u64).to_le_bytes())?;
        for (id, acc) in &self.0 {
            out.write_all(&id.to_le_bytes())?;
            acc.write_state(out)?;
        }
        Ok(())
    }
    fn read_state(input: &mut impl Read, limit: usize) -> io::Result<Self> {
        let mut bytes = [0; 8];
        input.read_exact(&mut bytes)?;
        let count = usize::try_from(u64::from_le_bytes(bytes)).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "overview tile length overflow")
        })?;
        if count > limit / size_of::<(u64, A)>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "overview tile exceeds budget",
            ));
        }
        let mut cells = Vec::new();
        for _ in 0..count {
            input.read_exact(&mut bytes)?;
            cells.push((u64::from_le_bytes(bytes), A::read_state(input, limit)?));
        }
        Ok(Self(cells))
    }
}

pub(super) struct Overview<A: SpillAccumulator> {
    parents: BufferedMerge<A>,
    resolutions: Vec<Resolution>,
}

impl<A: SpillAccumulator> Overview<A> {
    pub fn new(resolutions: &[u8]) -> Self {
        Self {
            parents: BufferedMerge::new(BUFFER_BYTES),
            resolutions: resolutions
                .iter()
                .map(|&r| Resolution::try_from(r).expect("H3 resolution"))
                .collect(),
        }
    }

    pub fn push(&mut self, cell: CellIndex, value: &A) -> io::Result<()> {
        for &res in &self.resolutions {
            if let Some(parent) = cell.parent(res) {
                self.parents.push(parent.into(), value.clone())?;
            }
        }
        Ok(())
    }

    pub fn write(
        self,
        zooms: &[u8],
        filter: &PropertyFilter,
        writer: &mut PmtilesWriter,
        bounds: &mut Wgs84BoundsAccumulator,
        properties: impl Fn(u64, u8, &A) -> FeatureProperties,
    ) -> io::Result<()> {
        let mut tiles = BufferedMerge::new(BUFFER_BYTES);
        self.parents.finish(|id, acc| {
            let cell = CellIndex::try_from(id).expect("validated parent");
            let center: LatLng = cell.into();
            let center = MercatorPoint::from_lat_lng(center.lat(), center.lng());
            let (vertices, len) = cell_boundary_mercator(cell);
            let mut emitted = false;
            for &z in zooms {
                if h3_res_for_zoom(z) != u8::from(cell.resolution()) {
                    continue;
                }
                emitted = true;
                let mut result = Ok(());
                for_each_cell_tile_mercator(center, &vertices[..len], z, |x, y| {
                    if result.is_ok() {
                        result =
                            tiles.push(zxy_to_tile_id(z, x, y), TileCells(vec![(id, acc.clone())]));
                    }
                });
                result?;
            }
            if emitted {
                bounds.record_cell(cell);
            }
            Ok(())
        })?;
        tiles.finish(|tile_id, cells| {
            let (z, x, y) = tile_id_to_zxy(tile_id);
            let mut layer = MvtLayer::with_filter("h3_hexagons", filter.clone());
            for (id, acc) in cells.0 {
                let cell = CellIndex::try_from(id).expect("validated parent");
                let (vertices, len) = cell_boundary_mercator(cell);
                layer.add_hexagon_mercator(
                    id,
                    &vertices[..len],
                    z,
                    x,
                    y,
                    properties(id, cell.resolution().into(), &acc),
                );
            }
            writer.add_tile(z, x, y, &layer.encode())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::accumulator::H3Accumulator;

    #[test]
    fn merges_repeated_keys_across_spills_with_bounded_buffer() {
        let mut buffer = BufferedMerge::new(4096);
        for pass in 0..4 {
            for key in 0..1000 {
                buffer.push(key, H3Accumulator::new(pass as f64)).unwrap();
                assert!(buffer.map.estimated_bytes() < 4096);
            }
        }
        assert!(buffer.runs.runs_written > 1);
        let mut count = 0;
        buffer
            .finish(|key, acc| {
                assert_eq!(key, count);
                assert_eq!(acc.count, 4.0);
                assert_eq!(acc.sum, 6.0);
                assert_eq!(acc.min, 0.0);
                assert_eq!(acc.max, 3.0);
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 1000);
    }

    #[test]
    fn tile_spills_preserve_ids_and_categorical_histograms() {
        use crate::aggregator::categorical::CategoricalAccumulator;
        let mut buffer = BufferedMerge::new(2048);
        for id in 0..200_u64 {
            let mut acc = CategoricalAccumulator::new();
            acc.update_weighted((id % 3) as i64, 2.5);
            acc.update_weighted(99, 1.0);
            buffer.push(id % 4, TileCells(vec![(id, acc)])).unwrap();
            assert!(buffer.map.estimated_bytes() < 2048);
        }
        assert!(buffer.runs.runs_written > 1);
        let mut seen = std::collections::HashSet::new();
        buffer
            .finish(|tile, cells| {
                assert_eq!(cells.0.len(), 50);
                for (id, acc) in cells.0 {
                    assert_eq!(id % 4, tile);
                    assert!(seen.insert(id));
                    assert_eq!(acc.total_count, 3.5);
                    assert_eq!(acc.majority().0, (id % 3) as i64);
                    assert_eq!(acc.unique_classes(), 2);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(seen.len(), 200);
    }
}
