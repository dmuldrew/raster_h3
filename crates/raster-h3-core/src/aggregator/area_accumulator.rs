//! Accumulator state for opt-in ground-area-weighted continuous statistics.
//!
//! This state is not yet connected to the raster walker or output adapters.

use std::io::{self, Read, Write};

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::multi_horizon::partial::PartialState;
use crate::aggregator::multi_horizon::sharded_map::AccumulatorMerge;
use crate::aggregator::multi_horizon::spill::SpillAccumulator;
use crate::error::{RasterH3Error, Result};

/// Pixel contribution count and area-weighted continuous statistics for one cell.
///
/// `pixel_count` is the sum of retained pixel fractions. The underlying
/// accumulator's count is retained area in square metres and its sum is the
/// area-weighted value sum, with units of value times square metres.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AreaAccumulator {
    pixel_count: f64,
    area_stats: H3Accumulator,
}

impl AreaAccumulator {
    /// Add one pixel's total retained fraction in a cell and its full area in m².
    ///
    /// All samples of one pixel assigned to this cell must be combined before
    /// calling this method, so the variance uses one observation per pixel.
    /// Invalid inputs leave the state unchanged.
    pub fn update_pixel(&mut self, value: f64, pixel_fraction: f64, area_m2: f64) -> Result<()> {
        let area = pixel_fraction * area_m2;
        if !value.is_finite()
            || !pixel_fraction.is_finite()
            || !(0.0 < pixel_fraction && pixel_fraction <= 1.0)
            || !area_m2.is_finite()
            || area_m2 <= 0.0
            || !area.is_finite()
            || area <= 0.0
            || !(self.pixel_count + pixel_fraction).is_finite()
            || !(self.area_stats.count + area).is_finite()
            || !(area * value).is_finite()
            || !(self.area_stats.sum + area * value).is_finite()
            || !(area * area).is_finite()
            || !(self.area_stats.sum_w2 + area * area).is_finite()
        {
            return Err(RasterH3Error::InvalidParameter(
                "invalid or overflowing area-weighted pixel contribution".into(),
            ));
        }
        self.pixel_count += pixel_fraction;
        self.area_stats.update_weighted(value, area);
        Ok(())
    }

    /// Sum of retained pixel fractions.
    pub fn pixel_count(&self) -> f64 {
        self.pixel_count
    }

    /// Area-weighted statistics; `count` is retained area in square metres.
    pub fn area_stats(&self) -> &H3Accumulator {
        &self.area_stats
    }

    /// Merge a disjoint contribution state into this state.
    pub fn merge(&mut self, other: &Self) {
        self.pixel_count += other.pixel_count;
        self.area_stats.merge(&other.area_stats);
    }
}

impl AccumulatorMerge for AreaAccumulator {
    fn merge(&mut self, other: &Self) {
        AreaAccumulator::merge(self, other);
    }

    fn heap_bytes(&self) -> usize {
        AccumulatorMerge::heap_bytes(&self.area_stats)
    }
}

impl SpillAccumulator for AreaAccumulator {
    fn write_state(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&self.pixel_count.to_le_bytes())?;
        self.area_stats.write_state(out)
    }

    fn read_state(input: &mut impl Read, limit: usize) -> io::Result<Self> {
        let mut bytes = [0u8; 8];
        input.read_exact(&mut bytes)?;
        let pixel_count = f64::from_le_bytes(bytes);
        if !pixel_count.is_finite() || pixel_count < 0.0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid pixel count"));
        }
        let area_stats = H3Accumulator::read_state(input, limit)?;
        if (pixel_count == 0.0) != (area_stats.count == 0.0) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "inconsistent area state"));
        }
        let acc = Self { pixel_count, area_stats };
        if acc.memory_bytes() > limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "area state exceeds budget"));
        }
        Ok(acc)
    }
}

impl PartialState for AreaAccumulator {
    const KIND: u8 = 3;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::multi_horizon::partial::{decode_partial_state, encode_partial_state};
    use crate::raster::ground_area::wgs84_rectangle_area;

    #[test]
    fn area_weights_keep_pixel_count_separate_and_survive_merge_and_partial_state() {
        let equator = wgs84_rectangle_area(0.0, 0.0, 1.0, 1.0).unwrap();
        let north = wgs84_rectangle_area(0.0, 59.0, 1.0, 60.0).unwrap();
        let mut a = AreaAccumulator::default();
        let mut b = AreaAccumulator::default();
        a.update_pixel(10.0, 1.0, equator).unwrap();
        b.update_pixel(30.0, 0.5, north).unwrap();
        let b: AreaAccumulator = decode_partial_state(&encode_partial_state(&b), 1 << 20).unwrap();
        a.merge(&b);
        let expected_area = equator + north * 0.5;
        let expected_mean = (equator * 10.0 + north * 0.5 * 30.0) / expected_area;
        assert_eq!(a.pixel_count(), 1.5);
        assert!((a.area_stats().count - expected_area).abs() < 1e-5);
        assert!((a.area_stats().mean() - expected_mean).abs() < 1e-12);
        assert!(a.area_stats().variance() > 0.0);
        let back: AreaAccumulator = decode_partial_state(&encode_partial_state(&a), 1 << 20).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn invalid_contributions_do_not_mutate_state() {
        let mut acc = AreaAccumulator::default();
        acc.update_pixel(2.0, 0.5, 4.0).unwrap();
        let before = acc.clone();
        for (value, fraction, area) in [
            (f64::NAN, 1.0, 1.0), (1.0, 0.0, 1.0), (1.0, 1.1, 1.0),
            (1.0, f64::NAN, 1.0), (1.0, 1.0, f64::INFINITY),
            (1.0, 1.0, f64::MAX),
        ] {
            assert!(acc.update_pixel(value, fraction, area).is_err());
            assert_eq!(acc, before);
        }
    }
}
