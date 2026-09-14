use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use std::collections::HashMap;
use tiff::decoder::DecodingResult;

use crate::aggregator::categorical::{CategoricalAccumulator, CategoricalUniformity};
use crate::aggregator::horizon_streamer::{is_decoding_result_all_nodata, NodataCast};
use crate::aggregator::remap::CategoryRemapper;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::walker::{
    is_slice_all_native_nodata, scanline_walk, walk_overlap_pixel_cells, ScanlineEngine,
};
/// Categorical record yielded by the multi-resolution categorical streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiCategoricalRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: CategoricalAccumulator,
}

/// Direct pixel-by-pixel categorical slice aggregation with strict tile ownership resolution
fn process_categorical_overlap_slice_into_maps<T, N>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<N>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    tile_idx: usize,
    mosaic: &MosaicReader,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) where
    T: CategoricalUniformity,
    N: Copy + PartialEq<T>,
{
    let resolve_cat = |val: T| -> Option<i64> {
        if let Some(nd) = native_nodata {
            if nd == val {
                return None;
            }
        }
        let raw_cat = val.to_category()?;
        if let Some(nd_f64) = nodata {
            if (raw_cat as f64 - nd_f64).abs() < 1e-6 {
                return None;
            }
        }
        if let Some(rem) = remapper {
            rem.remap(raw_cat)
        } else {
            Some(raw_cat)
        }
    };

    walk_overlap_pixel_cells(
        slice,
        chunk,
        chunk_stride,
        resolutions,
        crs_transformer,
        gt,
        sampling,
        bbox,
        tile_idx,
        mosaic,
        |val| resolve_cat(val).is_some(),
        |res_idx, cell_u64, weight, val| {
            if let Some(cat) = resolve_cat(val) {
                chunk_maps[res_idx]
                    .entry(cell_u64)
                    .and_modify(|acc| acc.update_weighted(cat, weight))
                    .or_insert_with(|| {
                        let mut acc = CategoricalAccumulator::default();
                        acc.update_weighted(cat, weight);
                        acc
                    });
            }
        },
    );
}

/// Process a single typed chunk slice for categorical landcover aggregation across resolutions
fn process_categorical_slice_into_maps<T, N>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<N>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    nodata: Option<f64>,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) where
    T: CategoricalUniformity,
    N: Copy + PartialEq<T>,
{
    if slice.is_empty() {
        return;
    }

    if let Some((tile_idx, mosaic)) = overlap_ctx {
        process_categorical_overlap_slice_into_maps(
            slice,
            chunk,
            native_nodata,
            resolutions,
            crs_transformer,
            gt,
            sampling,
            bbox,
            chunk_stride,
            nodata,
            tile_idx,
            mosaic,
            remapper,
            chunk_maps,
        );
        return;
    }

    let engine = CategoricalEngine::new(native_nodata, nodata, remapper);
    scanline_walk(
        slice,
        chunk,
        resolutions,
        crs_transformer,
        gt,
        sampling,
        bbox,
        chunk_stride,
        |s| is_slice_all_native_nodata(s, native_nodata),
        &engine,
        chunk_maps,
    );
}

struct CategoricalEngine<'a, T, N> {
    native_nodata: Option<N>,
    nodata: Option<f64>,
    remapper: Option<&'a CategoryRemapper>,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T, N> CategoricalEngine<'a, T, N>
where
    T: CategoricalUniformity,
    N: Copy + PartialEq<T>,
{
    fn new(
        native_nodata: Option<N>,
        nodata: Option<f64>,
        remapper: Option<&'a CategoryRemapper>,
    ) -> Self {
        Self {
            native_nodata,
            nodata,
            remapper,
            _marker: std::marker::PhantomData,
        }
    }

    #[inline(always)]
    fn resolve_category(&self, val: T) -> Option<i64> {
        if let Some(nd_nat) = self.native_nodata {
            if nd_nat == val {
                return None;
            }
        }
        let raw_cat = val.to_category()?;
        if self.native_nodata.is_none() {
            if let Some(nd) = self.nodata {
                if (raw_cat as f64 - nd).abs() < 1e-6 {
                    return None;
                }
            }
        }
        if let Some(rem) = self.remapper {
            rem.remap(raw_cat)
        } else {
            Some(raw_cat)
        }
    }
}

impl<'a, T, N> ScanlineEngine<T, CategoricalAccumulator> for CategoricalEngine<'a, T, N>
where
    T: CategoricalUniformity,
    N: Copy + PartialEq<T>,
{
    type Sample = i64;

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

/// Process a multi-sample categorical slice into thread-local hash maps for a specific band
fn process_categorical_multisample_slice_into_maps<T>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<T>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    samples_per_pixel: usize,
    band: usize,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    remapper: Option<&CategoryRemapper>,
    chunk_maps: &mut [HashMap<u64, CategoricalAccumulator, FxBuildHasher>],
) where
    T: CategoricalUniformity,
{
    let row_width = chunk.width as usize;
    let spp = samples_per_pixel.max(1);
    let b_idx = (band.saturating_sub(1)).min(spp - 1);
    let num_res = resolutions.len();

    for row_idx in 0..chunk.height as usize {
        let slice_row_start = (row_idx * (chunk_stride as usize)) * spp;

        for c in 0..row_width {
            let raw = slice[slice_row_start + c * spp + b_idx];
            if let Some(nd) = native_nodata {
                if raw == nd {
                    continue;
                }
            }

            let raw_cat = match raw.to_category() {
                Some(cls) => cls,
                None => continue,
            };

            let cat = if let Some(rem) = remapper {
                match rem.remap(raw_cat) {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                raw_cat
            };

            for sp in &sampling.points {
                let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                let py = (chunk.row_offset as f64) + (row_idx as f64) + sp.dy;
                let (x, y) = gt.pixel_to_coord(px, py);
                if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                    if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                        if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat
                        {
                            continue;
                        }
                    }

                    if let Some((tile_idx, mosaic)) = overlap_ctx {
                        if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                            continue;
                        }
                    }

                    if let Ok(ll) = LatLng::new(lat, lon) {
                        for res_idx in 0..num_res {
                            let res = resolutions[res_idx];
                            let cell: u64 = ll.to_cell(res).into();
                            chunk_maps[res_idx]
                                .entry(cell)
                                .and_modify(|acc| acc.update_weighted(cat, sp.weight))
                                .or_insert_with(|| {
                                    let mut a = CategoricalAccumulator::default();
                                    a.update_weighted(cat, sp.weight);
                                    a
                                });
                        }
                    }
                }
            }
        }
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
    let is_multisample = samples_per_pixel > 1 && band > 1;
    let spp = samples_per_pixel.max(1) as usize;

    macro_rules! dispatch_categorical {
        ($dr:expr, $nodata:expr, |$slice:ident, $nd:ident| $body:expr) => {
            match $dr {
                DecodingResult::U8($slice) => {
                    let $nd = <u8 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::U16($slice) => {
                    let $nd = <u16 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::U32($slice) => {
                    let $nd = <u32 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::U64($slice) => {
                    let $nd = <u64 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::I8($slice) => {
                    let $nd = <i8 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::I16($slice) => {
                    let $nd = <i16 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::I32($slice) => {
                    let $nd = <i32 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::I64($slice) => {
                    let $nd = <i64 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::F32($slice) => {
                    let $nd = <f32 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
                DecodingResult::F64($slice) => {
                    let $nd = <f64 as NodataCast>::from_nodata_f64($nodata);
                    $body
                }
            }
        };
    }

    if !is_multisample {
        if is_decoding_result_all_nodata(decoding_result, nodata) {
            return false;
        }

        dispatch_categorical!(decoding_result, nodata, |slice, nd| {
            process_categorical_slice_into_maps(
                slice,
                chunk_bounds,
                nd,
                resolutions,
                crs_transformer,
                gt,
                sampling,
                bbox,
                chunk_stride,
                nodata,
                overlap_ctx,
                remapper,
                chunk_maps,
            );
        });
    } else {
        dispatch_categorical!(decoding_result, nodata, |slice, nd| {
            process_categorical_multisample_slice_into_maps(
                slice,
                chunk_bounds,
                nd,
                resolutions,
                crs_transformer,
                gt,
                sampling,
                bbox,
                chunk_stride,
                spp,
                band,
                overlap_ctx,
                remapper,
                chunk_maps,
            );
        });
    }
    true
}
