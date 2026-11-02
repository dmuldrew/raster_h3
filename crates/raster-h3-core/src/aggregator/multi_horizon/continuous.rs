use crate::aggregator::cell_hash::H3BuildHasher;
use std::collections::HashMap;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::simd::SimdSpanAccumulate;

use super::config::SpectralFormula;
use super::walker::{
    is_slice_all_native_nodata, is_transparent, walk_direct, walk_interleaved, ScanlineEngine,
    WalkContext,
};
use crate::raster::metadata::BandScaling;

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
}

/// Per-stream options for continuous aggregation.
#[derive(Debug, Clone, Copy)]
pub struct ContinuousOptions {
    /// 1-based band read when no spectral formula is set.
    pub band: usize,
    pub spectral_formula: Option<SpectralFormula>,
    pub track_quantiles: bool,
}

/// Process a single typed chunk slice for continuous numeric aggregation across resolutions
fn process_continuous_slice_into_maps<T: SimdSpanAccumulate + Default>(
    ctx: &WalkContext<'_>,
    slice: &[T],
    native_nodata: Option<T>,
    options: ContinuousOptions,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, H3BuildHasher>],
) {
    let engine = ContinuousEngine {
        native_nodata,
        track_quantiles: options.track_quantiles,
        scaling: ctx.scaling(1),
    };
    if ctx.samples_per_pixel <= 1 && options.spectral_formula.is_none() {
        walk_direct(
            ctx,
            slice,
            |s| is_slice_all_native_nodata(s, native_nodata),
            &engine,
            chunk_maps,
        );
    } else {
        walk_interleaved(
            ctx,
            slice,
            |px| {
                pixel_value(
                    ctx,
                    px,
                    native_nodata,
                    options.band,
                    options.spectral_formula,
                )
            },
            &engine,
            chunk_maps,
        );
    }
}

struct ContinuousEngine<T> {
    native_nodata: Option<T>,
    track_quantiles: bool,
    /// Calibration of single-band samples; interleaved pixels are
    /// calibrated per band by [`pixel_value`].
    scaling: BandScaling,
}

impl<T: SimdSpanAccumulate> ContinuousEngine<T> {
    /// Accumulate a span of stored values as calibrated values. The SIMD
    /// kernel sums stored values; an affine map of those statistics equals
    /// accumulating the calibrated values.
    #[inline(always)]
    fn span_accumulator(&self, slice: &[T]) -> H3Accumulator {
        let mut span_acc = T::accumulate_span(slice, self.native_nodata);
        if !self.scaling.is_identity() {
            span_acc.apply_affine(self.scaling.scale, self.scaling.offset);
        }
        span_acc
    }
}

impl<T: SimdSpanAccumulate> ScanlineEngine<T, H3Accumulator> for ContinuousEngine<T> {
    type Sample = f64;

    fn combine_sample_weights(&self) -> bool {
        !self.track_quantiles
    }

    #[inline(always)]
    fn new_acc(&self) -> H3Accumulator {
        if self.track_quantiles {
            H3Accumulator::with_quantiles()
        } else {
            H3Accumulator::default()
        }
    }

    #[inline(always)]
    fn clear_acc(&self, acc: &mut H3Accumulator) {
        acc.clear();
    }

    #[inline(always)]
    fn has_samples(&self, acc: &H3Accumulator) -> bool {
        acc.count > 0.0
    }

    #[inline(always)]
    fn merge_acc(&self, dest: &mut H3Accumulator, src: &H3Accumulator) {
        dest.merge(src);
    }

    #[inline(always)]
    fn accumulate_span(&self, acc: &mut H3Accumulator, slice: &[T]) {
        let span_acc = self.span_accumulator(slice);
        if span_acc.count > 0.0 {
            acc.merge(&span_acc);
            if self.track_quantiles {
                if let Some(ref mut q) = acc.quantiles {
                    for &v in slice {
                        if v.is_valid(self.native_nodata) {
                            q.update(self.scaling.apply(v.to_f64_val()), 1.0);
                        }
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn accumulate_span_multi(&self, run_accs: &mut [H3Accumulator], slice: &[T]) {
        let span_acc = self.span_accumulator(slice);
        if span_acc.count > 0.0 {
            for acc in run_accs.iter_mut() {
                acc.merge(&span_acc);
            }
            if self.track_quantiles {
                for &v in slice {
                    if v.is_valid(self.native_nodata) {
                        let fv = self.scaling.apply(v.to_f64_val());
                        for q in run_accs.iter_mut().filter_map(|a| a.quantiles.as_mut()) {
                            q.update(fv, 1.0);
                        }
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn get_sample(&self, pixel: T) -> Option<f64> {
        if pixel.is_valid(self.native_nodata) {
            Some(self.scaling.apply(pixel.to_f64_val()))
        } else {
            None
        }
    }

    #[inline(always)]
    fn update_sample(&self, acc: &mut H3Accumulator, sample: f64, weight: f64) {
        if weight == 1.0 {
            acc.update(sample);
        } else {
            acc.update_weighted(sample, weight);
        }
        // update/update_weighted already update the optional quantile sketch.
    }
}

/// Reduce one pixel's interleaved band values to the selected band or
/// spectral index. Nodata is tested on stored values; formulas see
/// calibrated values. Pixels with zero alpha have no value.
#[inline(always)]
fn pixel_value<T: SimdSpanAccumulate + Default>(
    ctx: &WalkContext<'_>,
    px: &[T],
    nodata: Option<T>,
    band: usize,
    formula: Option<SpectralFormula>,
) -> Option<f64> {
    if is_transparent(ctx, px) {
        return None;
    }
    let get = |b: usize| {
        let b = b.saturating_sub(1).min(px.len() - 1);
        let v = px[b];
        v.is_valid(nodata)
            .then(|| ctx.scaling(b + 1).apply(v.to_f64_val()))
    };
    let ratio = |num: f64, den: f64| (den.abs() > 1e-12).then(|| num / den);
    match formula {
        Some(SpectralFormula::Ndvi { nir_band, red_band }) => {
            let (nir, red) = (get(nir_band)?, get(red_band)?);
            ratio(nir - red, nir + red)
        }
        Some(SpectralFormula::Ndwi {
            green_band,
            nir_band,
        }) => {
            let (green, nir) = (get(green_band)?, get(nir_band)?);
            ratio(green - nir, green + nir)
        }
        Some(SpectralFormula::Nbr {
            nir_band,
            swir_band,
        }) => {
            let (nir, swir) = (get(nir_band)?, get(swir_band)?);
            ratio(nir - swir, nir + swir)
        }
        Some(SpectralFormula::Evi {
            nir_band,
            red_band,
            blue_band,
        }) => {
            let (nir, red, blue) = (get(nir_band)?, get(red_band)?, get(blue_band)?);
            ratio(2.5 * (nir - red), nir + 6.0 * red - 7.5 * blue + 1.0)
        }
        None => get(band),
    }
}

/// Process a continuous chunk across all resolutions into thread-local hash maps.
/// Returns false when a single-band chunk is entirely nodata.
pub fn process_continuous_chunk_payload_into(
    window: &WalkContext<'_>,
    decoding_result: &DecodingResult,
    nodata: Option<f64>,
    options: ContinuousOptions,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, H3BuildHasher>],
) -> bool {
    process_continuous_borrowed_into(
        window,
        super::borrowed::BorrowedSamples::from(decoding_result),
        nodata,
        options,
        chunk_maps,
    )
}

/// Borrowed-window form of [`process_continuous_chunk_payload_into`].
pub fn process_continuous_borrowed_into(
    window: &WalkContext<'_>,
    samples: super::borrowed::BorrowedSamples<'_>,
    nodata: Option<f64>,
    options: ContinuousOptions,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, H3BuildHasher>],
) -> bool {
    if window.samples_per_pixel <= 1
        && options.spectral_formula.is_none()
        && samples.all_nodata(nodata)
    {
        return false;
    }
    crate::dispatch_samples!(samples, nodata, |slice, nd| {
        process_continuous_slice_into_maps(window, slice, nd, options, chunk_maps);
    });
    true
}
