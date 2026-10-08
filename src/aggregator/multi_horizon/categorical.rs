use fxhash::FxBuildHasher;
use h3o::Resolution;
use std::collections::HashMap;
use tiff::decoder::DecodingResult;

use crate::aggregator::categorical::{CategoricalAccumulator, CategoricalUniformity};
use crate::aggregator::nodata::PixelValidity;
use crate::aggregator::remap::CategoryRemapper;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::walker::{
    is_slice_all_native_nodata, walk_direct, walk_interleaved, ScanlineEngine, WalkContext,
};
/// Categorical record yielded by the multi-resolution categorical streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiCategoricalRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: CategoricalAccumulator,
}

/// Process a single typed chunk slice for categorical landcover aggregation across resolutions
fn process_categorical_slice_into_maps<T: CategoricalUniformity>(
    ctx: &WalkContext,
    slice: &[T],
    native_nodata: Option<T>,
    samples_per_pixel: usize,
    band: usize,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) {
    let engine = CategoricalEngine::new(native_nodata, remapper);
    if samples_per_pixel <= 1 {
        walk_direct(
            ctx,
            slice,
            |s| is_slice_all_native_nodata(s, native_nodata),
            &engine,
            chunk_maps,
        );
    } else {
        let b = band.saturating_sub(1).min(samples_per_pixel - 1);
        walk_interleaved(
            ctx,
            slice,
            samples_per_pixel,
            |px| engine.get_sample(px[b]),
            &engine,
            chunk_maps,
        );
    }
}

struct CategoricalEngine<'a, T> {
    native_nodata: Option<T>,
    remapper: Option<&'a CategoryRemapper>,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: CategoricalUniformity> CategoricalEngine<'a, T> {
    fn new(native_nodata: Option<T>, remapper: Option<&'a CategoryRemapper>) -> Self {
        Self {
            native_nodata,
            remapper,
            _marker: std::marker::PhantomData,
        }
    }

    #[inline(always)]
    fn resolve_category(&self, val: T) -> Option<i64> {
        let rule = PixelValidity::new(self.native_nodata);
        if !rule.is_valid(val) {
            return None;
        }
        let raw_cat = val.to_category()?;
        if let Some(rem) = self.remapper {
            rem.remap(raw_cat)
        } else {
            Some(raw_cat)
        }
    }
}

impl<'a, T: CategoricalUniformity> ScanlineEngine<T, CategoricalAccumulator>
    for CategoricalEngine<'a, T>
{
    type Sample = i64;

    fn combine_sample_weights(&self) -> bool {
        true
    }

    #[inline(always)]
    fn new_acc(&self) -> CategoricalAccumulator {
        CategoricalAccumulator::default()
    }

    #[inline(always)]
    fn clear_acc(&self, acc: &mut CategoricalAccumulator) {
        *acc = CategoricalAccumulator::default();
    }

    #[inline(always)]
    fn has_samples(&self, acc: &CategoricalAccumulator) -> bool {
        acc.total_count > 0.0
    }

    #[inline(always)]
    fn merge_acc(&self, dest: &mut CategoricalAccumulator, src: &CategoricalAccumulator) {
        dest.merge(src);
    }

    #[inline(always)]
    fn accumulate_span(&self, acc: &mut CategoricalAccumulator, sub_slice: &[T]) {
        if sub_slice.is_empty() {
            return;
        }
        let first_val = sub_slice[0];
        let is_uniform = T::is_uniform(sub_slice);

        if is_uniform {
            if let Some(cat) = self.resolve_category(first_val) {
                acc.update_weighted(cat, sub_slice.len() as f64);
            }
        } else {
            let mut curr_cat: Option<i64> = None;
            let mut curr_cat_count: f64 = 0.0;

            for &val_raw in sub_slice {
                let cat = match self.resolve_category(val_raw) {
                    Some(c) => c,
                    None => continue,
                };
                if Some(cat) == curr_cat {
                    curr_cat_count += 1.0;
                } else {
                    if let Some(prev) = curr_cat {
                        acc.update_weighted(prev, curr_cat_count);
                    }
                    curr_cat = Some(cat);
                    curr_cat_count = 1.0;
                }
            }
            if let Some(last_cat) = curr_cat {
                if curr_cat_count > 0.0 {
                    acc.update_weighted(last_cat, curr_cat_count);
                }
            }
        }
    }

    #[inline(always)]
    fn accumulate_span_multi(
        &self,
        run_accs: &mut [CategoricalAccumulator],
        run_cells: &[u64],
        sub_slice: &[T],
    ) {
        if sub_slice.is_empty() {
            return;
        }
        let first_val = sub_slice[0];
        let is_uniform = T::is_uniform(sub_slice);

        if is_uniform {
            if let Some(cat) = self.resolve_category(first_val) {
                let weight = sub_slice.len() as f64;
                for i in 0..run_accs.len() {
                    if run_cells[i] != 0 {
                        run_accs[i].update_weighted(cat, weight);
                    }
                }
            }
        } else {
            let mut curr_cat: Option<i64> = None;
            let mut curr_cat_count: f64 = 0.0;

            for &val_raw in sub_slice {
                let cat = match self.resolve_category(val_raw) {
                    Some(c) => c,
                    None => continue,
                };
                if Some(cat) == curr_cat {
                    curr_cat_count += 1.0;
                } else {
                    if let Some(prev) = curr_cat {
                        for i in 0..run_accs.len() {
                            if run_cells[i] != 0 {
                                run_accs[i].update_weighted(prev, curr_cat_count);
                            }
                        }
                    }
                    curr_cat = Some(cat);
                    curr_cat_count = 1.0;
                }
            }

            if let Some(last_cat) = curr_cat {
                if curr_cat_count > 0.0 {
                    for i in 0..run_accs.len() {
                        if run_cells[i] != 0 {
                            run_accs[i].update_weighted(last_cat, curr_cat_count);
                        }
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn get_sample(&self, pixel: T) -> Option<i64> {
        self.resolve_category(pixel)
    }

    #[inline(always)]
    fn update_sample(&self, acc: &mut CategoricalAccumulator, sample: i64, weight: f64) {
        acc.update_weighted(sample, weight);
    }
}

/// Process a categorical chunk across all resolutions into thread-local hash maps
pub fn process_categorical_chunk_payload_into(
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
    overlap_ctx: Option<(usize, &MosaicReader)>,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) -> bool {
    process_categorical_borrowed_into(
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
        overlap_ctx,
        remapper,
        chunk_maps,
    )
}

pub fn process_categorical_borrowed_into(
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
    overlap_ctx: Option<(usize, &MosaicReader)>,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) -> bool {
    let spp = samples_per_pixel.max(1) as usize;
    if spp == 1 && decoding_result.all_nodata(nodata) {
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
        process_categorical_slice_into_maps(&ctx, slice, nd, spp, band, remapper, chunk_maps);
    });
    true
}
