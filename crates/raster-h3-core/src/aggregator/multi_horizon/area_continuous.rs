//! Continuous, ground-area-weighted window aggregation.
//!
//! This is a window-level kernel; it is not yet connected to the streaming API.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::aggregator::area_accumulator::AreaAccumulator;
use crate::aggregator::cell_hash::H3BuildHasher;
use crate::aggregator::simd::SimdSpanAccumulate;
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::ground_area::wgs84_pixel_area;
use crate::raster::metadata::BandScaling;

use super::walker::{is_slice_all_native_nodata, walk_direct, ScanlineEngine, WalkContext};

/// A window's per-resolution area-weighted H3 maps.
pub type AreaWindowMaps = Vec<HashMap<u64, AreaAccumulator, H3BuildHasher>>;

struct AreaEngine<'a, T> {
    ctx: &'a WalkContext<'a>,
    nodata: Option<T>,
    scaling: BandScaling,
    error: RefCell<Option<RasterH3Error>>,
}

impl<T: SimdSpanAccumulate> ScanlineEngine<T, AreaAccumulator> for AreaEngine<'_, T> {
    type Sample = f64;

    fn combine_sample_weights(&self) -> bool { true }
    fn needs_pixel_coordinates(&self) -> bool { true }
    fn new_acc(&self) -> AreaAccumulator { AreaAccumulator::default() }
    fn clear_acc(&self, acc: &mut AreaAccumulator) { *acc = AreaAccumulator::default(); }
    fn has_samples(&self, acc: &AreaAccumulator) -> bool { acc.pixel_count() > 0.0 }
    fn merge_acc(&self, dest: &mut AreaAccumulator, src: &AreaAccumulator) { dest.merge(src); }
    fn accumulate_span(&self, _: &mut AreaAccumulator, _: &[T]) {
        unreachable!("area aggregation requires pixel coordinates")
    }
    fn accumulate_span_multi(&self, _: &mut [AreaAccumulator], _: &[T]) {
        unreachable!("area aggregation requires pixel coordinates")
    }
    fn get_sample(&self, pixel: T) -> Option<f64> {
        if !pixel.is_valid(self.nodata) { return None; }
        let value = self.scaling.apply(pixel.to_f64_val());
        value.is_finite().then_some(value)
    }
    fn update_sample(&self, _: &mut AreaAccumulator, _: f64, _: f64) {
        unreachable!("area aggregation requires pixel coordinates")
    }
    fn update_sample_at(&self, acc: &mut AreaAccumulator, value: f64, fraction: f64, col: usize, row: usize) {
        if self.error.borrow().is_some() { return; }
        // Normalized equal weights can round just above one when combined.
        let fraction = if fraction > 1.0 && fraction <= 1.0 + 1e-12 { 1.0 } else { fraction };
        let result = wgs84_pixel_area(self.ctx.gt, col, row)
            .and_then(|area| acc.update_pixel(value, fraction, area));
        if let Err(error) = result { *self.error.borrow_mut() = Some(error); }
    }
}

/// Aggregate a single-band WGS84 window into fresh area-weighted maps.
///
/// `source_epsg` must be the effective source CRS, including any user override;
/// the caller is responsible for resolving it from the raster metadata. This
/// currently accepts only EPSG:4326, north-up geometry and uniform sampling
/// patterns whose weights sum to one. Outputs remain private until the entire
/// window succeeds. Nodata is matched before calibration.
pub fn aggregate_wgs84_area_window<T: SimdSpanAccumulate>(
    ctx: &WalkContext<'_>,
    values: &[T],
    nodata: Option<T>,
    source_epsg: u32,
) -> Result<AreaWindowMaps> {
    if source_epsg != 4326 || !matches!(ctx.crs, CrsTransformer::Wgs84Identity) {
        return Err(RasterH3Error::InvalidParameter("area weighting requires EPSG:4326".into()));
    }
    if ctx.samples_per_pixel != 1 || ctx.alpha_sample.is_some() {
        return Err(RasterH3Error::InvalidParameter("area window kernel requires a single band without alpha".into()));
    }
    let points = &ctx.sampling.points;
    if points.is_empty()
        || points.iter().any(|p| !p.weight.is_finite() || p.weight <= 0.0
            || !p.dx.is_finite() || !p.dy.is_finite()
            || !(0.0..=1.0).contains(&p.dx) || !(0.0..=1.0).contains(&p.dy))
    {
        return Err(RasterH3Error::InvalidParameter("invalid area sampling pattern".into()));
    }
    let first = points[0].weight;
    let total: f64 = points.iter().map(|p| p.weight).sum();
    if points.iter().any(|p| (p.weight - first).abs() > 1e-12)
        || (total - 1.0).abs() > 1e-12
    {
        return Err(RasterH3Error::InvalidParameter("area weighting requires uniform sample weights summing to one".into()));
    }
    let width = ctx.chunk.width as usize;
    let height = ctx.chunk.height as usize;
    if width == 0 || height == 0 || ctx.resolutions.is_empty() {
        return Err(RasterH3Error::InvalidParameter("area window must be non-empty".into()));
    }
    let stride = if ctx.stride == 0 { width } else { ctx.stride as usize };
    let required = (height - 1).checked_mul(stride).and_then(|n| n.checked_add(width))
        .ok_or_else(|| RasterH3Error::InvalidParameter("area window size overflow".into()))?;
    if stride < width || values.len() < required {
        return Err(RasterH3Error::InvalidParameter("truncated or invalid area window".into()));
    }
    let last_col = (ctx.chunk.col_offset as usize).checked_add(width - 1)
        .ok_or_else(|| RasterH3Error::InvalidParameter("area column overflow".into()))?;
    let last_row = (ctx.chunk.row_offset as usize).checked_add(height - 1)
        .ok_or_else(|| RasterH3Error::InvalidParameter("area row overflow".into()))?;
    // Validate the window endpoints even when all values are nodata or filtered.
    wgs84_pixel_area(ctx.gt, ctx.chunk.col_offset as usize, ctx.chunk.row_offset as usize)?;
    wgs84_pixel_area(ctx.gt, last_col, last_row)?;
    let engine = AreaEngine { ctx, nodata, scaling: ctx.scaling(1), error: RefCell::new(None) };
    let mut maps: AreaWindowMaps = (0..ctx.resolutions.len()).map(|_| HashMap::default()).collect();
    walk_direct(ctx, values, |s| is_slice_all_native_nodata(s, nodata), &engine, &mut maps);
    if let Some(error) = engine.error.into_inner() { return Err(error); }
    Ok(maps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::sampling::SamplingPattern;
    use crate::raster::{GeoTransform, RasterChunk};
    use h3o::Resolution;

    #[test]
    fn area_window_conserves_weighted_values_and_pixel_counts() {
        let chunk = RasterChunk { col_offset: 10, row_offset: 20, width: 1, height: 2 };
        let gt = GeoTransform::from_gdal_array([0.0, 1.0, 0.0, 81.0, 0.0, -1.0]);
        let resolutions = [Resolution::try_from(2).unwrap(), Resolution::try_from(5).unwrap()];
        let crs = CrsTransformer::Wgs84Identity;
        let areas = [wgs84_pixel_area(&gt, 10, 20).unwrap(), wgs84_pixel_area(&gt, 10, 21).unwrap()];
        for sampling in [SamplingPattern::center(), SamplingPattern::rgss(), SamplingPattern::nine_point()] {
            let ctx = WalkContext {
                chunk: &chunk, resolutions: &resolutions, crs: &crs, gt: &gt,
                sampling: &sampling, bbox: None, stride: 0, samples_per_pixel: 1,
                owner: None, band_scaling: &[], alpha_sample: None,
            };
            let maps = aggregate_wgs84_area_window(&ctx, &[10.0_f64, 30.0], None, 4326).unwrap();
            for map in &maps {
                let count: f64 = map.values().map(AreaAccumulator::pixel_count).sum();
                let area: f64 = map.values().map(|a| a.area_stats().count).sum();
                let sum: f64 = map.values().map(|a| a.area_stats().sum).sum();
                assert!((count - 2.0).abs() < 1e-12);
                assert!((area - (areas[0] + areas[1])).abs() < (areas[0] + areas[1]) * 1e-12);
                assert!((sum - (10.0 * areas[0] + 30.0 * areas[1])).abs() < sum.abs() * 1e-12);
            }
            let nodata = aggregate_wgs84_area_window(&ctx, &[10.0_f64, -9999.0], Some(-9999.0), 4326).unwrap();
            for map in &nodata {
                assert!((map.values().map(AreaAccumulator::pixel_count).sum::<f64>() - 1.0).abs() < 1e-12);
                assert!((map.values().map(|a| a.area_stats().count).sum::<f64>() - areas[0]).abs() < areas[0] * 1e-12);
            }
        }
    }

    #[test]
    fn rejects_unsupported_crs_geometry_and_nonuniform_sampling() {
        let chunk = RasterChunk { col_offset: 0, row_offset: 0, width: 1, height: 1 };
        let resolutions = [Resolution::try_from(5).unwrap()];
        let crs = CrsTransformer::Wgs84Identity;
        let gt = GeoTransform::from_gdal_array([0.0, 1.0, 0.0, 1.0, 0.0, -1.0]);
        let sampling = SamplingPattern::center();
        let ctx = WalkContext { chunk: &chunk, resolutions: &resolutions, crs: &crs, gt: &gt,
            sampling: &sampling, bbox: None, stride: 0, samples_per_pixel: 1, owner: None,
            band_scaling: &[], alpha_sample: None };
        assert!(aggregate_wgs84_area_window(&ctx, &[1.0_f64], None, 4269).is_err());
        assert!(aggregate_wgs84_area_window::<f64>(&ctx, &[], None, 4326).is_err());
        assert!(aggregate_wgs84_area_window(&ctx, &[f64::MAX], None, 4326).is_err());
        let rotated = GeoTransform { b: 0.1, ..gt };
        assert!(aggregate_wgs84_area_window(&WalkContext { gt: &rotated, ..ctx }, &[1.0_f64], None, 4326).is_err());
        let gaussian = SamplingPattern::gaussian_five_point();
        assert!(aggregate_wgs84_area_window(&WalkContext { sampling: &gaussian, ..ctx }, &[1.0_f64], None, 4326).is_err());
    }
}
