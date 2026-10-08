use fxhash::FxBuildHasher;
use h3o::Resolution;
use std::collections::HashMap;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::sampling::SamplingPattern;
use crate::aggregator::simd::SimdSpanAccumulate;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::config::SpectralFormula;
use super::walker::{
    is_slice_all_native_nodata, walk_direct, walk_interleaved, ScanlineEngine, WalkContext,
};

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
}

/// Process a single typed chunk slice for continuous numeric aggregation across resolutions
#[allow(clippy::too_many_arguments)]
fn process_continuous_slice_into_maps<T: SimdSpanAccumulate>(
    ctx: &WalkContext,
    slice: &[T],
    native_nodata: Option<T>,
    samples_per_pixel: usize,
    band: usize,
    spectral_formula: Option<SpectralFormula>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) {
    let engine = ContinuousEngine {
        native_nodata,
        track_quantiles,
    };
    if samples_per_pixel <= 1 && spectral_formula.is_none() {
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
            samples_per_pixel,
            |px| pixel_value(px, native_nodata, band, spectral_formula),
            &engine,
            chunk_maps,
        );
    }
}

struct ContinuousEngine<T> {
    native_nodata: Option<T>,
    track_quantiles: bool,
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
        let span_acc = T::accumulate_span(slice, self.native_nodata);
        if span_acc.count > 0.0 {
            acc.merge(&span_acc);
            if self.track_quantiles {
                if let Some(ref mut q) = acc.quantiles {
                    for &v in slice {
                        if v.is_valid(self.native_nodata) {
                            q.update(v.to_f64_val(), 1.0);
                        }
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn accumulate_span_multi(
        &self,
        run_accs: &mut [H3Accumulator],
        run_cells: &[u64],
        slice: &[T],
    ) {
        let span_acc = T::accumulate_span(slice, self.native_nodata);
        if span_acc.count > 0.0 {
            for i in 0..run_accs.len() {
                if run_cells[i] != 0 {
                    run_accs[i].merge(&span_acc);
                }
            }
            if self.track_quantiles {
                for &v in slice {
                    if v.is_valid(self.native_nodata) {
                        let fv = v.to_f64_val();
                        for i in 0..run_accs.len() {
                            if run_cells[i] != 0 {
                                if let Some(ref mut q) = run_accs[i].quantiles {
                                    q.update(fv, 1.0);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn get_sample(&self, pixel: T) -> Option<f64> {
        if pixel.is_valid(self.native_nodata) {
            Some(pixel.to_f64_val())
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

/// Reduce one pixel's interleaved band values to the selected band or spectral index.
#[inline(always)]
fn pixel_value<T: SimdSpanAccumulate>(
    px: &[T],
    nodata: Option<T>,
    band: usize,
    formula: Option<SpectralFormula>,
) -> Option<f64> {
    let get = |b: usize| {
        let v = px[b.saturating_sub(1).min(px.len() - 1)];
        v.is_valid(nodata).then(|| v.to_f64_val())
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

/// Process a continuous chunk across all resolutions into thread-local hash maps
pub fn process_continuous_chunk_payload_into(
    chunk_bounds: &RasterChunk,
    decoding_result: &DecodingResult,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    samples_per_pixel: u16,
    band: usize,
    spectral_formula: Option<SpectralFormula>,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) -> bool {
    process_continuous_borrowed_into(
        chunk_bounds,
        super::borrowed::BorrowedSamples::from(decoding_result),
        resolutions,
        crs_transformer,
        gt,
        sampling,
        bbox,
        chunk_stride,
        nodata,
        samples_per_pixel,
        band,
        spectral_formula,
        overlap_ctx,
        track_quantiles,
        chunk_maps,
    )
}

pub fn process_continuous_borrowed_into(
    chunk_bounds: &RasterChunk,
    decoding_result: super::borrowed::BorrowedSamples<'_>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    samples_per_pixel: u16,
    band: usize,
    spectral_formula: Option<SpectralFormula>,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) -> bool {
    let spp = samples_per_pixel.max(1) as usize;
    if spp == 1 && spectral_formula.is_none() && decoding_result.all_nodata(nodata) {
        return false;
    }
    let ctx = WalkContext {
        chunk: chunk_bounds,
        resolutions,
        crs: crs_transformer,
        gt,
        sampling,
        bbox,
        stride: chunk_stride,
        owner: overlap_ctx,
    };
    crate::dispatch_samples!(decoding_result, nodata, |slice, nd| {
        process_continuous_slice_into_maps(
            &ctx,
            slice,
            nd,
            spp,
            band,
            spectral_formula,
            track_quantiles,
            chunk_maps,
        );
    });
    true
}
