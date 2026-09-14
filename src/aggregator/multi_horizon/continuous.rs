use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use std::collections::HashMap;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::horizon_streamer::{is_decoding_result_all_nodata, NodataCast};
use crate::aggregator::sampling::SamplingPattern;
use crate::aggregator::simd::SimdSpanAccumulate;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::config::SpectralFormula;
use super::walker::{
    is_slice_all_native_nodata, scanline_walk, walk_overlap_pixel_cells, ScanlineEngine,
};

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
}

/// Direct pixel-by-pixel continuous slice aggregation with strict tile ownership resolution
fn process_continuous_overlap_slice_into_maps<T>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<T>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    tile_idx: usize,
    mosaic: &MosaicReader,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) where
    T: SimdSpanAccumulate,
{
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
        |val| val.is_valid(native_nodata),
        |res_idx, cell_u64, weight, val| {
            let float_val = val.to_f64_val();
            chunk_maps[res_idx]
                .entry(cell_u64)
                .and_modify(|acc| {
                    if weight == 1.0 {
                        acc.update(float_val);
                    } else {
                        acc.update_weighted(float_val, weight);
                    }
                })
                .or_insert_with(|| {
                    let mut acc = if track_quantiles {
                        H3Accumulator::with_quantiles()
                    } else {
                        H3Accumulator::default()
                    };
                    if weight == 1.0 {
                        acc.update(float_val);
                    } else {
                        acc.update_weighted(float_val, weight);
                    }
                    acc
                });
        },
    );
}

/// Process a single typed chunk slice for continuous numeric aggregation across resolutions
fn process_continuous_slice_into_maps<T>(
    slice: &[T],
    chunk: &RasterChunk,
    native_nodata: Option<T>,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) where
    T: SimdSpanAccumulate,
{
    if slice.is_empty() {
        return;
    }

    if let Some((tile_idx, mosaic)) = overlap_ctx {
        process_continuous_overlap_slice_into_maps(
            slice,
            chunk,
            native_nodata,
            resolutions,
            crs_transformer,
            gt,
            sampling,
            bbox,
            chunk_stride,
            tile_idx,
            mosaic,
            track_quantiles,
            chunk_maps,
        );
        return;
    }

    let engine = ContinuousEngine {
        native_nodata,
        track_quantiles,
    };
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

struct ContinuousEngine<T> {
    native_nodata: Option<T>,
    track_quantiles: bool,
}

impl<T: SimdSpanAccumulate> ScanlineEngine<T, H3Accumulator> for ContinuousEngine<T> {
    type Sample = f64;

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
        if self.track_quantiles {
            if let Some(ref mut q) = acc.quantiles {
                q.update(sample, weight);
            }
        }
    }
}

/// Process a multi-sample or spectral formula continuous slice into thread-local hash maps
fn process_continuous_multisample_slice_into_maps<T: SimdSpanAccumulate>(
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
    spectral_formula: Option<SpectralFormula>,
    overlap_ctx: Option<(usize, &MosaicReader)>,
    track_quantiles: bool,
    chunk_maps: &mut [HashMap<u64, H3Accumulator, FxBuildHasher>],
) {
    let row_width = chunk.width as usize;
    let spp = samples_per_pixel.max(1);
    let num_res = resolutions.len();

    for row_idx in 0..chunk.height as usize {
        let slice_row_start = (row_idx * (chunk_stride as usize)) * spp;

        for c in 0..row_width {
            let pixel_base = slice_row_start + c * spp;

            let val_opt = match spectral_formula {
                Some(SpectralFormula::Ndvi { nir_band, red_band }) => {
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let red_idx = (red_band.saturating_sub(1)).min(spp - 1);
                    let nir_raw = slice[pixel_base + nir_idx];
                    let red_raw = slice[pixel_base + red_idx];
                    if nir_raw.is_valid(native_nodata) && red_raw.is_valid(native_nodata) {
                        let nir = nir_raw.to_f64_val();
                        let red = red_raw.to_f64_val();
                        let denom = nir + red;
                        if denom.abs() > 1e-12 {
                            Some((nir - red) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Some(SpectralFormula::Ndwi {
                    green_band,
                    nir_band,
                }) => {
                    let green_idx = (green_band.saturating_sub(1)).min(spp - 1);
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let green_raw = slice[pixel_base + green_idx];
                    let nir_raw = slice[pixel_base + nir_idx];
                    if green_raw.is_valid(native_nodata) && nir_raw.is_valid(native_nodata) {
                        let green = green_raw.to_f64_val();
                        let nir = nir_raw.to_f64_val();
                        let denom = green + nir;
                        if denom.abs() > 1e-12 {
                            Some((green - nir) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Some(SpectralFormula::Nbr {
                    nir_band,
                    swir_band,
                }) => {
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let swir_idx = (swir_band.saturating_sub(1)).min(spp - 1);
                    let nir_raw = slice[pixel_base + nir_idx];
                    let swir_raw = slice[pixel_base + swir_idx];
                    if nir_raw.is_valid(native_nodata) && swir_raw.is_valid(native_nodata) {
                        let nir = nir_raw.to_f64_val();
                        let swir = swir_raw.to_f64_val();
                        let denom = nir + swir;
                        if denom.abs() > 1e-12 {
                            Some((nir - swir) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Some(SpectralFormula::Evi {
                    nir_band,
                    red_band,
                    blue_band,
                }) => {
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let red_idx = (red_band.saturating_sub(1)).min(spp - 1);
                    let blue_idx = (blue_band.saturating_sub(1)).min(spp - 1);
                    let nir_raw = slice[pixel_base + nir_idx];
                    let red_raw = slice[pixel_base + red_idx];
                    let blue_raw = slice[pixel_base + blue_idx];
                    if nir_raw.is_valid(native_nodata)
                        && red_raw.is_valid(native_nodata)
                        && blue_raw.is_valid(native_nodata)
                    {
                        let nir = nir_raw.to_f64_val();
                        let red = red_raw.to_f64_val();
                        let blue = blue_raw.to_f64_val();
                        let denom = nir + 6.0 * red - 7.5 * blue + 1.0;
                        if denom.abs() > 1e-12 {
                            Some(2.5 * (nir - red) / denom)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                None => {
                    let b_idx = (band.saturating_sub(1)).min(spp - 1);
                    let raw = slice[pixel_base + b_idx];
                    if raw.is_valid(native_nodata) {
                        Some(raw.to_f64_val())
                    } else {
                        None
                    }
                }
            };

            let val = match val_opt {
                Some(v) => v,
                None => continue,
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
                                .and_modify(|acc| acc.update_weighted(val, sp.weight))
                                .or_insert_with(|| {
                                    let mut a = if track_quantiles {
                                        H3Accumulator::with_quantiles()
                                    } else {
                                        H3Accumulator::default()
                                    };
                                    a.update_weighted(val, sp.weight);
                                    a
                                });
                        }
                    }
                }
            }
        }
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
    let is_multisample = samples_per_pixel > 1 || spectral_formula.is_some() || band > 1;
    let spp = samples_per_pixel.max(1) as usize;

    macro_rules! dispatch_continuous {
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

        dispatch_continuous!(decoding_result, nodata, |slice, nd| {
            process_continuous_slice_into_maps(
                slice,
                chunk_bounds,
                nd,
                resolutions,
                crs_transformer,
                gt,
                sampling,
                bbox,
                chunk_stride,
                overlap_ctx,
                track_quantiles,
                chunk_maps,
            );
        });
    } else {
        dispatch_continuous!(decoding_result, nodata, |slice, nd| {
            process_continuous_multisample_slice_into_maps(
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
                spectral_formula,
                overlap_ctx,
                track_quantiles,
                chunk_maps,
            );
        });
    }
    true
}
