//! Multi-Resolution Direct Ground-Truth Horizon Streaming
//!
//! Provides single-pass streaming aggregation across multiple H3 resolution levels simultaneously
//! while preserving 100% true pixel-in-polygon containment at each resolution.
//!
//! Employs multi-core chunk-row parallelism via Rayon to process chunks across all CPU cores
//! lock-free while strictly bounding RAM to the active scanline horizon.

use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::Arc;
use fxhash::FxBuildHasher;
use h3o::{CellIndex, LatLng, Resolution};
use rayon::prelude::*;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::h3_scanline::H3ScanlineLookahead;
use crate::aggregator::horizon_streamer::{
    compute_cell_south_lat, is_chunk_all_nodata, HexEvictionEntry,
};
use crate::aggregator::remap::CategoryRemapper;
use crate::aggregator::sampling::SamplingPattern;
use crate::aggregator::simd::SimdSpanAccumulate;
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::{MosaicReader, OverlapRule};
use crate::raster::prefetch::PrefetchedMosaicReader;
use crate::raster::RasterChunk;

const WGS84_A: f64 = 6378137.0;
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Supported on-the-fly spectral index formulas
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpectralFormula {
    Ndvi { nir_band: usize, red_band: usize },
    Ndwi { green_band: usize, nir_band: usize },
    Nbr { nir_band: usize, swir_band: usize },
    Evi { nir_band: usize, red_band: usize, blue_band: usize },
}

impl SpectralFormula {
    pub fn parse(name: &str, nir: usize, red: usize, green: usize, blue: usize, swir: usize) -> Option<Self> {
        match name.to_lowercase().trim() {
            "ndvi" => Some(Self::Ndvi { nir_band: nir, red_band: red }),
            "ndwi" => Some(Self::Ndwi { green_band: green, nir_band: nir }),
            "nbr" => Some(Self::Nbr { nir_band: nir, swir_band: swir }),
            "evi" => Some(Self::Evi { nir_band: nir, red_band: red, blue_band: blue }),
            _ => None,
        }
    }
}

/// Quantile target specification (percentile in [0.0, 1.0] or interquartile range)
#[derive(Debug, Clone, PartialEq)]
pub enum QuantileTarget {
    Percentile(f64, String),
    Iqr(String),
}

impl QuantileTarget {
    pub fn column_name(&self) -> &str {
        match self {
            Self::Percentile(_, name) => name,
            Self::Iqr(name) => name,
        }
    }

    /// Parse a comma- or space-separated list of quantile targets or presets.
    ///
    /// Presets:
    /// - `"true"` / `"all"` / `"default"` -> p50, p90, p95, p99, iqr
    /// - `"box"` -> p25, p50, p75, iqr
    /// - `"tails"` -> p01, p05, p10, p90, p95, p99
    /// - `"deciles"` -> p10, p20, p30, p40, p50, p60, p70, p80, p90
    /// - `"false"` / `"none"` / `"off"` -> none
    ///
    /// Individual items:
    /// - Percentiles: `p50`, `p95`, `p01`, `p5`, `p99.5`, `p99_5`
    /// - Decimals: `0.5`, `0.05`, `0.95`, `0.99`
    /// - Aliases: `median`, `q1`, `q2`, `q3`, `iqr`
    pub fn parse_list(s: &str) -> Result<Vec<Self>> {
        let mut targets = Vec::new();
        for raw_token in s.split(|c: char| c == ',' || c.is_whitespace()) {
            let token = raw_token.trim().to_lowercase();
            if token.is_empty() {
                continue;
            }
            match token.as_str() {
                "false" | "none" | "off" => {}
                "true" | "all" | "default" => {
                    targets.push(Self::Percentile(0.50, "p50".to_string()));
                    targets.push(Self::Percentile(0.90, "p90".to_string()));
                    targets.push(Self::Percentile(0.95, "p95".to_string()));
                    targets.push(Self::Percentile(0.99, "p99".to_string()));
                    targets.push(Self::Iqr("iqr".to_string()));
                }
                "box" => {
                    targets.push(Self::Percentile(0.25, "p25".to_string()));
                    targets.push(Self::Percentile(0.50, "p50".to_string()));
                    targets.push(Self::Percentile(0.75, "p75".to_string()));
                    targets.push(Self::Iqr("iqr".to_string()));
                }
                "tails" => {
                    targets.push(Self::Percentile(0.01, "p01".to_string()));
                    targets.push(Self::Percentile(0.05, "p05".to_string()));
                    targets.push(Self::Percentile(0.10, "p10".to_string()));
                    targets.push(Self::Percentile(0.90, "p90".to_string()));
                    targets.push(Self::Percentile(0.95, "p95".to_string()));
                    targets.push(Self::Percentile(0.99, "p99".to_string()));
                }
                "deciles" => {
                    for d in 1..=9 {
                        let pct = d * 10;
                        let q = pct as f64 / 100.0;
                        let name = format!("p{}", pct);
                        targets.push(Self::Percentile(q, name));
                    }
                }
                "iqr" => {
                    targets.push(Self::Iqr("iqr".to_string()));
                }
                "median" => {
                    targets.push(Self::Percentile(0.50, "median".to_string()));
                }
                "q1" => {
                    targets.push(Self::Percentile(0.25, "q1".to_string()));
                }
                "q2" => {
                    targets.push(Self::Percentile(0.50, "q2".to_string()));
                }
                "q3" => {
                    targets.push(Self::Percentile(0.75, "q3".to_string()));
                }
                _ => {
                    if let Some(stripped) = token.strip_prefix('p') {
                        let num_str = stripped.replace('_', ".");
                        let val: f64 = num_str.parse().map_err(|_| {
                            RasterH3Error::InvalidParameter(format!(
                                "Invalid percentile specification '{}'",
                                raw_token
                            ))
                        })?;
                        if val <= 0.0 || val >= 100.0 {
                            return Err(RasterH3Error::InvalidParameter(format!(
                                "Percentile '{}' must be strictly between 0 and 100",
                                raw_token
                            )));
                        }
                        let q = val / 100.0;
                        let name = if (val.round() - val).abs() < 1e-6 {
                            format!("p{:02}", val.round() as u32)
                        } else {
                            format!("p{}", val).replace('.', "_")
                        };
                        targets.push(Self::Percentile(q, name));
                    } else if let Ok(val) = token.parse::<f64>() {
                        if val <= 0.0 || val >= 1.0 {
                            return Err(RasterH3Error::InvalidParameter(format!(
                                "Decimal quantile '{}' must be strictly between 0 and 1",
                                raw_token
                            )));
                        }
                        let pct = val * 100.0;
                        let name = if (pct.round() - pct).abs() < 1e-6 {
                            format!("p{:02}", pct.round() as u32)
                        } else {
                            format!("p{}", pct).replace('.', "_")
                        };
                        targets.push(Self::Percentile(val, name));
                    } else {
                        return Err(RasterH3Error::InvalidParameter(format!(
                            "Unrecognized quantile/percentile target '{}'",
                            raw_token
                        )));
                    }
                }
            }
        }

        // Deduplicate while preserving order
        let mut seen = std::collections::HashSet::new();
        let mut deduped = Vec::new();
        for t in targets {
            if seen.insert(t.column_name().to_string()) {
                deduped.push(t);
            }
        }
        Ok(deduped)
    }
}

/// Configuration for multi-resolution aggregation
#[derive(Debug, Clone)]
pub struct MultiResolutionConfig {
    pub resolutions: Vec<u8>,
    pub band: usize,
    pub custom_nodata: Option<f64>,
    pub bbox: Option<[f64; 4]>,
    pub sampling: SamplingPattern,
    pub custom_crs: Option<String>,
    pub properties: Option<String>,
    pub spectral_formula: Option<SpectralFormula>,
    pub min_count: Option<f64>,
    pub min_mean: Option<f64>,
    pub max_mean: Option<f64>,
    pub min_majority_fraction: Option<f64>,
    pub compact: bool,
    pub overlap_rule: OverlapRule,
    pub quantiles: Vec<QuantileTarget>,
    pub remapper: Option<Arc<CategoryRemapper>>,
}

impl MultiResolutionConfig {
    /// Create a new multi-resolution configuration
    pub fn new(resolutions: Vec<u8>) -> Self {
        Self {
            resolutions,
            band: 1,
            custom_nodata: None,
            bbox: None,
            sampling: SamplingPattern::center(),
            custom_crs: None,
            properties: None,
            spectral_formula: None,
            min_count: None,
            min_mean: None,
            max_mean: None,
            min_majority_fraction: None,
            compact: false,
            overlap_rule: OverlapRule::default(),
            quantiles: Vec::new(),
            remapper: None,
        }
    }

    /// Check whether streaming quantile calculations are enabled
    #[inline(always)]
    pub fn track_quantiles(&self) -> bool {
        !self.quantiles.is_empty()
    }
}

impl Default for MultiResolutionConfig {
    fn default() -> Self {
        Self::new(vec![8])
    }
}

/// Continuous record yielded by the multi-resolution streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiContinuousRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: H3Accumulator,
}



/// Fast check if an entire row slice consists purely of NoData values
#[inline(always)]
fn is_slice_all_native_nodata<T, N>(slice: &[T], native_nodata: Option<N>) -> bool
where
    T: Copy + PartialEq,
    N: Copy + PartialEq<T>,
{
    if let Some(nd_nat) = native_nodata {
        if slice.is_empty() {
            return true;
        }
        let len = slice.len();
        if nd_nat != slice[0] || nd_nat != slice[len / 2] || nd_nat != slice[len - 1] {
            return false;
        }
        slice.iter().all(|&val| nd_nat == val)
    } else {
        false
    }
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
    let stride = if chunk_stride > 0 && slice.len() >= chunk_stride as usize {
        chunk_stride as usize
    } else {
        (chunk.width as usize).max(1)
    };
    let actual_rows = (slice.len() / stride).min(chunk.height as usize);
    let num_res = resolutions.len();

    for r in 0..actual_rows {
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        for c in 0..row_width {
            let val = slice[slice_row_start + c];
            if !val.is_valid(native_nodata) {
                continue;
            }
            let float_val = val.to_f64_val();

            if sampling.is_single_point() {
                let (x, y) = gt.pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                let (lon, lat) = match crs_transformer.transform_point(x, y) {
                    Ok(coords) => coords,
                    Err(_) => continue,
                };

                if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                    if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                        continue;
                    }
                }

                if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                    continue;
                }

                if let Ok(ll) = LatLng::new(lat, lon) {
                    for res_idx in 0..num_res {
                        let res = resolutions[res_idx];
                        let cell_u64: u64 = ll.to_cell(res).into();
                        chunk_maps[res_idx]
                            .entry(cell_u64)
                            .and_modify(|acc| acc.update(float_val))
                            .or_insert_with(|| {
                                let mut acc = if track_quantiles {
                                    H3Accumulator::with_quantiles()
                                } else {
                                    H3Accumulator::default()
                                };
                                acc.update(float_val);
                                acc
                            });
                    }
                }
            } else {
                for sp in &sampling.points {
                    let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                    let py = (chunk.row_offset as f64) + (r as f64) + sp.dy;
                    let (x, y) = gt.pixel_to_coord(px, py);
                    if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                continue;
                            }
                        }

                        if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                            continue;
                        }

                        if let Ok(ll) = LatLng::new(lat, lon) {
                            for res_idx in 0..num_res {
                                let res = resolutions[res_idx];
                                let cell_u64: u64 = ll.to_cell(res).into();
                                chunk_maps[res_idx]
                                    .entry(cell_u64)
                                    .and_modify(|acc| acc.update_weighted(float_val, sp.weight))
                                    .or_insert_with(|| {
                                        let mut acc = if track_quantiles {
                                            H3Accumulator::with_quantiles()
                                        } else {
                                            H3Accumulator::default()
                                        };
                                        acc.update_weighted(float_val, sp.weight);
                                        acc
                                    });
                            }
                        }
                    }
                }
            }
        }
    }
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

    let is_wgs84 = matches!(crs_transformer, CrsTransformer::Wgs84Identity);
    let is_web_mercator = matches!(crs_transformer, CrsTransformer::WebMercatorFast);
    let d_lon_step = if is_wgs84 {
        gt.a
    } else if is_web_mercator {
        (gt.a / WGS84_A) * RAD_TO_DEG
    } else {
        0.0
    };

    let stride = if chunk_stride > 0 && slice.len() >= chunk_stride as usize {
        chunk_stride as usize
    } else {
        (chunk.width as usize).max(1)
    };
    let actual_rows = (slice.len() / stride).min(chunk.height as usize);
    let num_res = resolutions.len();
    let is_north_up = gt.b == 0.0 && gt.d == 0.0;
    let dx_step = gt.a;

    let mut row_caches: Vec<H3ScanlineLookahead> = resolutions
        .iter()
        .map(|&res| H3ScanlineLookahead::for_resolution(res))
        .collect();

    for r in 0..actual_rows {
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        let slice_row = &slice[slice_row_start..slice_row_start + row_width];
        if is_slice_all_native_nodata(slice_row, native_nodata) {
            continue;
        }

        if sampling.is_single_point() {
            let (x_start, y_row) = gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);
            let (lon_start, lat_row) = if is_wgs84 {
                (x_start, y_row)
            } else if is_web_mercator {
                let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2)
                    * RAD_TO_DEG;
                let lon = (x_start / WGS84_A) * RAD_TO_DEG;
                (lon, lat)
            } else {
                match crs_transformer.transform_point(x_start, y_row) {
                    Ok(coords) => coords,
                    Err(_) => (x_start, y_row),
                }
            };

            for res_idx in 0..num_res {
                let res = resolutions[res_idx];
                let active_map = &mut chunk_maps[res_idx];
                let row_cache = &mut row_caches[res_idx];
                row_cache.reset_row();
                let mut run_cell: u64 = 0;
                let mut run_acc = if track_quantiles {
                    H3Accumulator::with_quantiles()
                } else {
                    H3Accumulator::default()
                };

                let mut lon_curr = lon_start;
                let mut x_curr = x_start;
                let mut c = 0;
                let mut known_next_cell: Option<u64> = None;

                while c < row_width {
                    let cell_opt = if let Some(known) = known_next_cell.take() {
                        Some(known)
                    } else {
                        let (lon, lat) = if is_wgs84 || is_web_mercator {
                            (lon_curr, lat_row)
                        } else if is_north_up {
                            match crs_transformer.transform_point(x_curr, y_row) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    c += 1;
                                    x_curr += dx_step;
                                    continue;
                                }
                            }
                        } else {
                            let (x, y) = gt
                                .pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                            match crs_transformer.transform_point(x, y) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    c += 1;
                                    continue;
                                }
                            }
                        };

                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                c += 1;
                                if is_wgs84 || is_web_mercator {
                                    lon_curr += d_lon_step;
                                } else if is_north_up {
                                    x_curr += dx_step;
                                }
                                continue;
                            }
                        }

                        row_cache.get_or_compute_cell(lat, lon, res)
                    };

                    if let Some(cell_u64) = cell_opt {
                        if cell_u64 != run_cell {
                            if run_cell != 0 && run_acc.count > 0.0 {
                                active_map
                                    .entry(run_cell)
                                    .and_modify(|acc| acc.merge(&run_acc))
                                    .or_insert_with(|| run_acc.clone());
                            }
                            run_cell = cell_u64;
                            run_acc.clear();
                            row_cache.on_cell_changed();
                        }

                        let (span_end, next_cell) = if is_wgs84 || is_web_mercator {
                            row_cache.find_span_end(c, row_width, lon_curr, lat_row, d_lon_step, res, run_cell)
                        } else if is_north_up {
                            row_cache.find_span_end_projected(
                                c,
                                row_width,
                                x_start,
                                y_row,
                                dx_step,
                                |x, y| match crs_transformer.transform_point(x, y) {
                                    Ok((p_lon, p_lat)) => {
                                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                            if p_lon < b_min_lon || p_lon > b_max_lon || p_lat < b_min_lat || p_lat > b_max_lat {
                                                return None;
                                            }
                                        }
                                        LatLng::new(p_lat, p_lon).ok().map(|ll| ll.to_cell(res).into())
                                    }
                                    Err(_) => None,
                                },
                                run_cell,
                            )
                        } else {
                            (c + 1, None)
                        };

                        let span_slice = &slice[slice_row_start + c..slice_row_start + span_end];
                        let span_acc = T::accumulate_span(span_slice, native_nodata);
                        if span_acc.count > 0.0 {
                            run_acc.merge(&span_acc);
                            if track_quantiles {
                                if let Some(ref mut q) = run_acc.quantiles {
                                    for &v in span_slice {
                                        if v.is_valid(native_nodata) {
                                            q.update(v.to_f64_val(), 1.0);
                                        }
                                    }
                                }
                            }
                        }

                        let num_stepped = span_end - c;
                        row_cache.advance_span(num_stepped);
                        if is_wgs84 || is_web_mercator {
                            lon_curr += (num_stepped as f64) * d_lon_step;
                        } else if is_north_up {
                            x_curr += (num_stepped as f64) * dx_step;
                        }
                        c = span_end;
                        known_next_cell = next_cell;
                    } else {
                        c += 1;
                        if is_wgs84 || is_web_mercator {
                            lon_curr += d_lon_step;
                        } else if is_north_up {
                            x_curr += dx_step;
                        }
                    }
                }

                if run_cell != 0 && run_acc.count > 0.0 {
                    active_map
                        .entry(run_cell)
                        .and_modify(|acc| acc.merge(&run_acc))
                        .or_insert_with(|| run_acc.clone());
                }
            }
        } else {
            for c in 0..row_width {
                let val_raw = slice[slice_row_start + c];
                if !val_raw.is_valid(native_nodata) {
                    continue;
                }
                let val = val_raw.to_f64_val();

                for sp in &sampling.points {
                    let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                    let py = (row_idx as f64) + sp.dy;
                    let (x, y) = gt.pixel_to_coord(px, py);
                    if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                continue;
                            }
                        }

                        if let Ok(ll) = LatLng::new(lat, lon) {
                            for res_idx in 0..num_res {
                                let res = resolutions[res_idx];
                                let active_map = &mut chunk_maps[res_idx];
                                let cell: u64 = ll.to_cell(res).into();
                                active_map
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
                Some(SpectralFormula::Ndwi { green_band, nir_band }) => {
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
                Some(SpectralFormula::Nbr { nir_band, swir_band }) => {
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
                Some(SpectralFormula::Evi { nir_band, red_band, blue_band }) => {
                    let nir_idx = (nir_band.saturating_sub(1)).min(spp - 1);
                    let red_idx = (red_band.saturating_sub(1)).min(spp - 1);
                    let blue_idx = (blue_band.saturating_sub(1)).min(spp - 1);
                    let nir_raw = slice[pixel_base + nir_idx];
                    let red_raw = slice[pixel_base + red_idx];
                    let blue_raw = slice[pixel_base + blue_idx];
                    if nir_raw.is_valid(native_nodata) && red_raw.is_valid(native_nodata) && blue_raw.is_valid(native_nodata) {
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
                        if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
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
fn process_continuous_chunk_payload_into(
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

    if !is_multisample {
        let is_all_nodata = match decoding_result {
            DecodingResult::U8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::F32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::F64(slice) => is_chunk_all_nodata(slice, nodata, |x| x),
        };

        if is_all_nodata {
            return false;
        }

        match decoding_result {
            DecodingResult::U8(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_continuous_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, overlap_ctx, track_quantiles, chunk_maps);
            }
        }
    } else {
        match decoding_result {
            DecodingResult::U8(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_continuous_multisample_slice_into_maps(slice, chunk_bounds, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, spectral_formula, overlap_ctx, track_quantiles, chunk_maps);
            }
        }
    }
    true
}

/// Single-pass streaming aggregator across multiple H3 resolutions (Continuous Data)
pub struct MultiScanHorizonStreamer {
    prefetcher: Option<PrefetchedMosaicReader>,
    pub mosaic: Arc<MosaicReader>,
    resolutions: Vec<Resolution>,
    resolution_u8s: Vec<u8>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    active_maps: Vec<HashMap<u64, H3Accumulator, FxBuildHasher>>,
    eviction_queues: Vec<BinaryHeap<HexEvictionEntry>>,
    completed_buffer: VecDeque<MultiContinuousRecord>,
    is_finished: bool,
    current_lat_horizon: f64,
    pub profile_stats: [u64; 4],
    processed_chunk_count: usize,
    band: usize,
    spectral_formula: Option<SpectralFormula>,
    min_count: Option<f64>,
    min_mean: Option<f64>,
    max_mean: Option<f64>,
    compact: bool,
    pending_compact: HashMap<u64, (H3Accumulator, Vec<(u64, H3Accumulator)>), FxBuildHasher>,
    pub track_quantiles: bool,
}

impl MultiScanHorizonStreamer {
    /// Initialize a new MultiScanHorizonStreamer from a single GeoTIFF reader
    pub fn new(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> Result<Self> {
        let mosaic = Arc::new(MosaicReader::from_single_reader(
            reader,
            config.bbox,
            config.custom_crs.as_deref(),
        )?);
        Self::new_mosaic(mosaic, config)
    }

    /// Initialize a new MultiScanHorizonStreamer from a multi-file MosaicReader
    pub fn new_mosaic(mosaic: Arc<MosaicReader>, config: &MultiResolutionConfig) -> Result<Self> {
        if config.resolutions.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "Resolutions list cannot be empty".to_string(),
            ));
        }

        let mut resolutions = Vec::with_capacity(config.resolutions.len());
        let mut resolution_u8s = Vec::with_capacity(config.resolutions.len());
        for &res_u8 in &config.resolutions {
            let res = Resolution::try_from(res_u8).map_err(|_| {
                RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", res_u8))
            })?;
            resolutions.push(res);
            resolution_u8s.push(res_u8);
        }

        let prefetcher = PrefetchedMosaicReader::spawn(Arc::clone(&mosaic), 256);
        let num_res = resolutions.len();

        let mut active_maps = Vec::with_capacity(num_res);
        let mut eviction_queues = Vec::with_capacity(num_res);
        for _ in 0..num_res {
            active_maps.push(HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default()));
            eviction_queues.push(BinaryHeap::with_capacity(1024));
        }

        let band = config.band;
        let spectral_formula = config.spectral_formula;
        let min_count = config.min_count;
        let min_mean = config.min_mean;
        let max_mean = config.max_mean;
        let compact = config.compact;
        let pending_compact = HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default());

        Ok(Self {
            prefetcher: Some(prefetcher),
            mosaic,
            resolutions,
            resolution_u8s,
            nodata: config.custom_nodata,
            bbox: config.bbox,
            sampling: config.sampling.clone(),
            active_maps,
            eviction_queues,
            completed_buffer: VecDeque::with_capacity(2048),
            is_finished: false,
            current_lat_horizon: f64::INFINITY,
            profile_stats: [0; 4],
            processed_chunk_count: 0,
            band,
            spectral_formula,
            min_count,
            min_mean,
            max_mean,
            compact,
            pending_compact,
            track_quantiles: config.track_quantiles(),
        })
    }

    /// Return current southernmost latitude reached by scanline horizon
    pub fn current_lat_horizon(&self) -> f64 {
        self.current_lat_horizon
    }

    /// Target H3 resolutions
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Target H3 resolution integer levels
    pub fn resolution_u8s(&self) -> &[u8] {
        &self.resolution_u8s
    }

    fn push_continuous_record(&mut self, res_u8: u8, cell_u64: u64, acc: H3Accumulator) {
        if let Some(min_c) = self.min_count {
            if acc.count < min_c {
                return;
            }
        }
        if let Some(min_m) = self.min_mean {
            if acc.mean() < min_m {
                return;
            }
        }
        if let Some(max_m) = self.max_mean {
            if acc.mean() > max_m {
                return;
            }
        }

        if self.compact {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                if let Some(parent_res) = cell.resolution().pred() {
                    if let Some(parent) = cell.parent(parent_res) {
                        let parent_u64: u64 = parent.into();
                        let entry = self.pending_compact.entry(parent_u64).or_insert_with(|| {
                            (
                                if self.track_quantiles {
                                    H3Accumulator::with_quantiles()
                                } else {
                                    H3Accumulator::default()
                                },
                                Vec::with_capacity(7),
                            )
                        });
                        entry.0.merge(&acc);
                        entry.1.push((cell_u64, acc));

                        if entry.1.len() == 7 {
                            let (parent_acc, _) = self.pending_compact.remove(&parent_u64).unwrap();
                            let p_res_u8: u8 = parent_res.into();
                            self.completed_buffer.push_back(MultiContinuousRecord {
                                resolution: p_res_u8,
                                h3_index: parent_u64,
                                accumulator: parent_acc,
                            });
                            return;
                        }
                        return;
                    }
                }
            }
        }

        self.completed_buffer.push_back(MultiContinuousRecord {
            resolution: res_u8,
            h3_index: cell_u64,
            accumulator: acc,
        });
    }

    fn flush_pending_compact(&mut self) {
        for (_, (_, children)) in self.pending_compact.drain() {
            for (cell_u64, acc) in children {
                let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                    cell.resolution().into()
                } else {
                    8
                };
                self.completed_buffer.push_back(MultiContinuousRecord {
                    resolution: res_u8,
                    h3_index: cell_u64,
                    accumulator: acc,
                });
            }
        }
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            let res_u8 = self.resolution_u8s[res_idx];
            while let Some(top) = self.eviction_queues[res_idx].peek() {
                if top.south_lat > lat_horizon {
                    let entry = self.eviction_queues[res_idx].pop().unwrap();
                    if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                        self.push_continuous_record(res_u8, entry.cell_u64, acc);
                    }
                } else {
                    break;
                }
            }
        }

        if self.compact && !self.pending_compact.is_empty() {
            let mut to_flush = Vec::new();
            for (&parent_u64, _) in self.pending_compact.iter() {
                let parent_south = compute_cell_south_lat(parent_u64);
                if parent_south > lat_horizon {
                    to_flush.push(parent_u64);
                }
            }
            for p in to_flush {
                if let Some((_, children)) = self.pending_compact.remove(&p) {
                    for (cell_u64, acc) in children {
                        let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                            cell.resolution().into()
                        } else {
                            8
                        };
                        self.completed_buffer.push_back(MultiContinuousRecord {
                            resolution: res_u8,
                            h3_index: cell_u64,
                            accumulator: acc,
                        });
                    }
                }
            }
        }
    }

    /// Advance scanline horizon until at least `min_rows` completed records are available or finished
    pub fn advance_until_completed(&mut self, min_rows: usize) {
        let batch_size = (rayon::current_num_threads() * 4).max(32);
        let min_batch = rayon::current_num_threads().clamp(4, 16);
        let mut chunk_items = Vec::with_capacity(batch_size);

        while self.completed_buffer.len() < min_rows && !self.is_finished {
            chunk_items.clear();
            let t0 = std::time::Instant::now();
            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut chunk_items, min_batch, batch_size);
            }
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;

            if chunk_items.is_empty() {
                self.is_finished = true;
                self.current_lat_horizon = f64::NEG_INFINITY;
                let num_res = self.resolutions.len();
                for res_idx in 0..num_res {
                    let res_u8 = self.resolution_u8s[res_idx];
                    while let Some(entry) = self.eviction_queues[res_idx].pop() {
                        if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                            self.push_continuous_record(res_u8, entry.cell_u64, acc);
                        }
                    }
                    let remaining: Vec<(u64, H3Accumulator)> = self.active_maps[res_idx].drain().collect();
                    for (cell_u64, acc) in remaining {
                        self.push_continuous_record(res_u8, cell_u64, acc);
                    }
                }
                self.flush_pending_compact();
                break;
            }

            let resolutions = &self.resolutions;
            let sampling = &self.sampling;
            let bbox = self.bbox;
            let band = self.band;
            let spectral_formula = self.spectral_formula;
            let mosaic = Arc::clone(&self.mosaic);
            let user_nodata = self.nodata;
            let track_quantiles = self.track_quantiles;

            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(Vec<Vec<(u64, H3Accumulator)>>, DecodingResult)> = chunk_items
                .par_iter_mut()
                .map_init(
                    || {
                        let mut maps = Vec::with_capacity(resolutions.len());
                        for _ in 0..resolutions.len() {
                            maps.push(HashMap::with_capacity_and_hasher(128, FxBuildHasher::default()));
                        }
                        maps
                    },
                    |local_maps, item| {
                        match item {
                            Ok((tile_idx, _chunk_idx, chunk_bounds, decoding_result, has_overlap)) => {
                                for m in local_maps.iter_mut() {
                                    m.clear();
                                }
                                let tile = &mosaic.tiles[*tile_idx];
                                let crs_transformer = &tile.crs_transformer;
                                let gt = &tile.reader.metadata.geotransform;
                                let chunk_stride = tile.reader.chunk_layout.chunk_width;
                                let nodata = user_nodata.or(tile.reader.metadata.nodata);
                                let samples_per_pixel = tile.reader.metadata.samples_per_pixel;

                                let overlap_ctx = if *has_overlap {
                                    Some((*tile_idx, &*mosaic))
                                } else {
                                    None
                                };

                                let has_data = process_continuous_chunk_payload_into(
                                    chunk_bounds,
                                    decoding_result,
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
                                    local_maps,
                                );
                                let mut chunk_entries = Vec::with_capacity(if has_data { local_maps.len() } else { 0 });
                                if has_data {
                                    for m in local_maps.iter_mut() {
                                        let entries: Vec<(u64, H3Accumulator)> = m.drain().collect();
                                        chunk_entries.push(entries);
                                    }
                                }
                                let dec = std::mem::replace(decoding_result, DecodingResult::U8(Vec::new()));
                                Some((chunk_entries, dec))
                            }
                            Err(_) => None,
                        }
                    },
                )
                .filter_map(|x| x)
                .collect();
            self.profile_stats[1] += t1.elapsed().as_nanos() as u64;

            let mut recycled_buffers = Vec::with_capacity(parallel_results.len());

            let t2 = std::time::Instant::now();
            self.processed_chunk_count += chunk_items.len();

            for (chunk_entries, decoding_result) in parallel_results {
                if !chunk_entries.is_empty() {
                    for (res_idx, entries) in chunk_entries.into_iter().enumerate() {
                        let active_map = &mut self.active_maps[res_idx];
                        let eviction_queue = &mut self.eviction_queues[res_idx];

                        for (cell_u64, acc) in entries {
                            active_map
                                .entry(cell_u64)
                                .and_modify(|existing| existing.merge(&acc))
                                .or_insert_with(|| {
                                    let south_lat = compute_cell_south_lat(cell_u64);
                                    eviction_queue.push(HexEvictionEntry {
                                        south_lat,
                                        cell_u64,
                                    });
                                    acc
                                });
                        }
                    }
                }

                recycled_buffers.push(decoding_result);
            }

            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.recycle_batch(recycled_buffers);
            }
            self.profile_stats[2] += t2.elapsed().as_nanos() as u64;

            if self.processed_chunk_count < self.mosaic.chunk_refs.len() {
                let next_chunk = &self.mosaic.chunk_refs[self.processed_chunk_count];
                let safe_lat = next_chunk.north_lat;
                if safe_lat < self.current_lat_horizon {
                    let t3 = std::time::Instant::now();
                    self.current_lat_horizon = safe_lat;
                    self.evict_completed(safe_lat);
                    self.profile_stats[3] += t3.elapsed().as_nanos() as u64;
                }
            }
        }
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiContinuousRecord> {
        self.advance_until_completed(max_rows);
        let num_to_take = max_rows.min(self.completed_buffer.len());
        let mut batch = Vec::with_capacity(num_to_take);
        for _ in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                batch.push(record);
            }
        }
        batch
    }

    /// Drain up to `max_rows` completed records directly into a closure with zero heap allocation
    pub fn drain_completed_into<F>(&mut self, max_rows: usize, mut consumer: F) -> usize
    where
        F: FnMut(usize, MultiContinuousRecord),
    {
        self.advance_until_completed(max_rows);
        let num_to_take = max_rows.min(self.completed_buffer.len());
        for i in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                consumer(i, record);
            }
        }
        num_to_take
    }

    /// Return total active in-flight cells across all resolutions
    pub fn active_cell_count(&self) -> usize {
        self.active_maps.iter().map(|m| m.len()).sum()
    }
}

/// Categorical record yielded by the multi-resolution categorical streamer
#[derive(Debug, Clone, PartialEq)]
pub struct MultiCategoricalRecord {
    pub resolution: u8,
    pub h3_index: u64,
    pub accumulator: CategoricalAccumulator,
}

/// Direct pixel-by-pixel categorical slice aggregation with strict tile ownership resolution
fn process_categorical_overlap_slice_into_maps<T, F, N>(
    slice: &[T],
    chunk: &RasterChunk,
    to_i64: F,
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
    T: Copy + PartialEq,
    F: Fn(T) -> Option<i64>,
    N: Copy + PartialEq<T>,
{
    let stride = if chunk_stride > 0 && slice.len() >= chunk_stride as usize {
        chunk_stride as usize
    } else {
        (chunk.width as usize).max(1)
    };
    let actual_rows = (slice.len() / stride).min(chunk.height as usize);
    let num_res = resolutions.len();

    for r in 0..actual_rows {
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        for c in 0..row_width {
            let val = slice[slice_row_start + c];
            if let Some(nd) = native_nodata {
                if nd == val {
                    continue;
                }
            }
            let raw_cat = match to_i64(val) {
                Some(k) => k,
                None => continue,
            };
            if let Some(nd_f64) = nodata {
                if (raw_cat as f64 - nd_f64).abs() < 1e-6 {
                    continue;
                }
            }
            let cat = if let Some(rem) = remapper {
                match rem.remap(raw_cat) {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                raw_cat
            };

            if sampling.is_single_point() {
                let (x, y) = gt.pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                let (lon, lat) = match crs_transformer.transform_point(x, y) {
                    Ok(coords) => coords,
                    Err(_) => continue,
                };

                if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                    if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                        continue;
                    }
                }

                if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                    continue;
                }

                if let Ok(ll) = LatLng::new(lat, lon) {
                    for res_idx in 0..num_res {
                        let res = resolutions[res_idx];
                        let cell_u64: u64 = ll.to_cell(res).into();
                        chunk_maps[res_idx]
                            .entry(cell_u64)
                            .and_modify(|acc| acc.update(cat))
                            .or_insert_with(|| {
                                let mut acc = CategoricalAccumulator::default();
                                acc.update(cat);
                                acc
                            });
                    }
                }
            } else {
                for sp in &sampling.points {
                    let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                    let py = (chunk.row_offset as f64) + (r as f64) + sp.dy;
                    let (x, y) = gt.pixel_to_coord(px, py);
                    if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                continue;
                            }
                        }

                        if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                            continue;
                        }

                        if let Ok(ll) = LatLng::new(lat, lon) {
                            for res_idx in 0..num_res {
                                let res = resolutions[res_idx];
                                let cell_u64: u64 = ll.to_cell(res).into();
                                chunk_maps[res_idx]
                                    .entry(cell_u64)
                                    .and_modify(|acc| acc.update_weighted(cat, sp.weight))
                                    .or_insert_with(|| {
                                        let mut acc = CategoricalAccumulator::default();
                                        acc.update_weighted(cat, sp.weight);
                                        acc
                                    });
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Process a single typed chunk slice for categorical landcover aggregation across resolutions
fn process_categorical_slice_into_maps<T, F, N>(
    slice: &[T],
    chunk: &RasterChunk,
    to_i64: F,
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
    T: Copy + PartialEq,
    F: Fn(T) -> Option<i64>,
    N: Copy + PartialEq<T>,
{
    if slice.is_empty() {
        return;
    }

    if let Some((tile_idx, mosaic)) = overlap_ctx {
        process_categorical_overlap_slice_into_maps(
            slice,
            chunk,
            to_i64,
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

    let is_wgs84 = matches!(crs_transformer, CrsTransformer::Wgs84Identity);
    let is_web_mercator = matches!(crs_transformer, CrsTransformer::WebMercatorFast);
    let d_lon_step = if is_wgs84 {
        gt.a
    } else if is_web_mercator {
        (gt.a / WGS84_A) * RAD_TO_DEG
    } else {
        0.0
    };

    let stride = if chunk_stride > 0 && slice.len() >= chunk_stride as usize {
        chunk_stride as usize
    } else {
        (chunk.width as usize).max(1)
    };
    let actual_rows = (slice.len() / stride).min(chunk.height as usize);
    let num_res = resolutions.len();
    let is_north_up = gt.b == 0.0 && gt.d == 0.0;
    let dx_step = gt.a;

    let mut row_caches: Vec<H3ScanlineLookahead> = resolutions
        .iter()
        .map(|&res| H3ScanlineLookahead::for_resolution(res))
        .collect();

    for r in 0..actual_rows {
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        let slice_row = &slice[slice_row_start..slice_row_start + row_width];
        if is_slice_all_native_nodata(slice_row, native_nodata) {
            continue;
        }

        if sampling.is_single_point() {
            let (x_start, y_row) = gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);
            let (lon_start, lat_row) = if is_wgs84 {
                (x_start, y_row)
            } else if is_web_mercator {
                let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2)
                    * RAD_TO_DEG;
                let lon = (x_start / WGS84_A) * RAD_TO_DEG;
                (lon, lat)
            } else {
                match crs_transformer.transform_point(x_start, y_row) {
                    Ok(coords) => coords,
                    Err(_) => (x_start, y_row),
                }
            };

            for res_idx in 0..num_res {
                let res = resolutions[res_idx];
                let active_map = &mut chunk_maps[res_idx];
                let row_cache = &mut row_caches[res_idx];
                row_cache.reset_row();
                let mut run_cell: u64 = 0;
                let mut run_acc = CategoricalAccumulator::default();

                let mut lon_curr = lon_start;
                let mut x_curr = x_start;
                let mut c = 0;
                let mut known_next_cell: Option<u64> = None;

                while c < row_width {
                    let cell_opt = if let Some(known) = known_next_cell.take() {
                        Some(known)
                    } else {
                        let (lon, lat) = if is_wgs84 || is_web_mercator {
                            (lon_curr, lat_row)
                        } else if is_north_up {
                            match crs_transformer.transform_point(x_curr, y_row) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    c += 1;
                                    x_curr += dx_step;
                                    continue;
                                }
                            }
                        } else {
                            let (x, y) = gt
                                .pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                            match crs_transformer.transform_point(x, y) {
                                Ok(coords) => coords,
                                Err(_) => {
                                    c += 1;
                                    continue;
                                }
                            }
                        };

                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                c += 1;
                                if is_wgs84 || is_web_mercator {
                                    lon_curr += d_lon_step;
                                } else if is_north_up {
                                    x_curr += dx_step;
                                }
                                continue;
                            }
                        }

                        row_cache.get_or_compute_cell(lat, lon, res)
                    };

                    if let Some(cell_u64) = cell_opt {
                        if cell_u64 != run_cell {
                            if run_cell != 0 && run_acc.total_count > 0.0 {
                                active_map
                                    .entry(run_cell)
                                    .and_modify(|acc| acc.merge(&run_acc))
                                    .or_insert_with(|| run_acc);
                            }
                            run_cell = cell_u64;
                            run_acc = CategoricalAccumulator::default();
                            row_cache.on_cell_changed();
                        }

                        let (span_end, next_cell) = if is_wgs84 || is_web_mercator {
                            row_cache.find_span_end(c, row_width, lon_curr, lat_row, d_lon_step, res, run_cell)
                        } else if is_north_up {
                            row_cache.find_span_end_projected(
                                c,
                                row_width,
                                x_start,
                                y_row,
                                dx_step,
                                |x, y| match crs_transformer.transform_point(x, y) {
                                    Ok((p_lon, p_lat)) => {
                                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                            if p_lon < b_min_lon || p_lon > b_max_lon || p_lat < b_min_lat || p_lat > b_max_lat {
                                                return None;
                                            }
                                        }
                                        LatLng::new(p_lat, p_lon).ok().map(|ll| ll.to_cell(res).into())
                                    }
                                    Err(_) => None,
                                },
                                run_cell,
                            )
                        } else {
                            (c + 1, None)
                        };

                        let span_slice = &slice[slice_row_start + c..slice_row_start + span_end];
                        let first_val = span_slice[0];
                        let is_uniform = span_slice.iter().all(|&v| v == first_val);

                        if is_uniform {
                            let mut is_nd = false;
                            if let Some(nd_nat) = native_nodata {
                                if nd_nat == first_val {
                                    is_nd = true;
                                }
                            }
                            if !is_nd {
                                if let Some(raw_cat) = to_i64(first_val) {
                                    let mut is_nd_float = false;
                                    if native_nodata.is_none() {
                                        if let Some(nd) = nodata {
                                            if (raw_cat as f64 - nd).abs() < 1e-6 {
                                                is_nd_float = true;
                                            }
                                        }
                                    }
                                    if !is_nd_float {
                                        let cat_opt = if let Some(rem) = remapper {
                                            rem.remap(raw_cat)
                                        } else {
                                            Some(raw_cat)
                                        };
                                        if let Some(cat) = cat_opt {
                                            run_acc.update_weighted(cat, span_slice.len() as f64);
                                        }
                                    }
                                }
                            }
                        } else {
                            let mut curr_cat: Option<i64> = None;
                            let mut curr_cat_count: f64 = 0.0;

                            for &val_raw in span_slice {
                                if let Some(nd_nat) = native_nodata {
                                    if nd_nat == val_raw {
                                        continue;
                                    }
                                }

                                if let Some(raw_cat) = to_i64(val_raw) {
                                    if native_nodata.is_none() {
                                        if let Some(nd) = nodata {
                                            if (raw_cat as f64 - nd).abs() < 1e-6 {
                                                continue;
                                            }
                                        }
                                    }
                                    let cat = if let Some(rem) = remapper {
                                        match rem.remap(raw_cat) {
                                            Some(c) => c,
                                            None => continue,
                                        }
                                    } else {
                                        raw_cat
                                    };
                                    if Some(cat) == curr_cat {
                                        curr_cat_count += 1.0;
                                    } else {
                                        if let Some(prev) = curr_cat {
                                            run_acc.update_weighted(prev, curr_cat_count);
                                        }
                                        curr_cat = Some(cat);
                                        curr_cat_count = 1.0;
                                    }
                                }
                            }

                            if let Some(prev) = curr_cat {
                                run_acc.update_weighted(prev, curr_cat_count);
                            }
                        }

                        let num_stepped = span_end - c;
                        row_cache.advance_span(num_stepped);
                        if is_wgs84 || is_web_mercator {
                            lon_curr += (num_stepped as f64) * d_lon_step;
                        } else if is_north_up {
                            x_curr += (num_stepped as f64) * dx_step;
                        }
                        c = span_end;
                        known_next_cell = next_cell;
                    } else {
                        c += 1;
                        if is_wgs84 || is_web_mercator {
                            lon_curr += d_lon_step;
                        } else if is_north_up {
                            x_curr += dx_step;
                        }
                    }
                }

                if run_cell != 0 && run_acc.total_count > 0.0 {
                    active_map
                        .entry(run_cell)
                        .and_modify(|acc| acc.merge(&run_acc))
                        .or_insert_with(|| run_acc);
                }
            }
        } else {
            for c in 0..row_width {
                let val_raw = slice[slice_row_start + c];
                if let Some(nd_nat) = native_nodata {
                    if nd_nat == val_raw {
                        continue;
                    }
                }

                if let Some(raw_cat) = to_i64(val_raw) {
                    if native_nodata.is_none() {
                        if let Some(nd) = nodata {
                            if (raw_cat as f64 - nd).abs() < 1e-6 {
                                continue;
                            }
                        }
                    }
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
                        let py = (row_idx as f64) + sp.dy;
                        let (x, y) = gt.pixel_to_coord(px, py);
                        if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                            if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                                if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                                    continue;
                                }
                            }

                            if let Ok(ll) = LatLng::new(lat, lon) {
                                for res_idx in 0..num_res {
                                    let res = resolutions[res_idx];
                                    let active_map = &mut chunk_maps[res_idx];
                                    let cell: u64 = ll.to_cell(res).into();
                                    active_map
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
    }
}

/// Process a multi-sample categorical slice into thread-local hash maps for a specific band
fn process_categorical_multisample_slice_into_maps<T, F>(
    slice: &[T],
    chunk: &RasterChunk,
    to_class_fn: F,
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
)
where
    T: Copy + PartialEq,
    F: Fn(T) -> Option<i64>,
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

            let raw_cat = match to_class_fn(raw) {
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
                        if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
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
fn process_categorical_chunk_payload_into(
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

    if !is_multisample {
        let is_all_nodata = match decoding_result {
            DecodingResult::U8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::U64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::I64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::F32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
            DecodingResult::F64(slice) => is_chunk_all_nodata(slice, nodata, |x| x),
        };

        if is_all_nodata {
            return false;
        }

        match decoding_result {
            DecodingResult::U8(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x <= i64::MAX as u64 { Some(x as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| Some(x), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_categorical_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, nodata, overlap_ctx, remapper, chunk_maps);
            }
        }
    } else {
        match decoding_result {
            DecodingResult::U8(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U16(slice) => {
                let nd = nodata.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U32(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::U64(slice) => {
                let nd = nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| if x <= i64::MAX as u64 { Some(x as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I8(slice) => {
                let nd = nodata.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I16(slice) => {
                let nd = nodata.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I32(slice) => {
                let nd = nodata.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x as i64), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::I64(slice) => {
                let nd = nodata.map(|v| v as i64);
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| Some(x), nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F32(slice) => {
                let nd = nodata.map(|v| v as f32);
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
            DecodingResult::F64(slice) => {
                let nd = nodata;
                process_categorical_multisample_slice_into_maps(slice, chunk_bounds, |x| if x.is_finite() { Some(x.round() as i64) } else { None }, nd, resolutions, crs_transformer, gt, sampling, bbox, chunk_stride, spp, band, overlap_ctx, remapper, chunk_maps);
            }
        }
    }
    true
}

/// Single-pass streaming aggregator across multiple H3 resolutions (Categorical Data)
pub struct MultiCategoricalHorizonStreamer {
    prefetcher: Option<PrefetchedMosaicReader>,
    pub mosaic: Arc<MosaicReader>,
    resolutions: Vec<Resolution>,
    resolution_u8s: Vec<u8>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    sampling: SamplingPattern,
    active_maps: Vec<HashMap<u64, CategoricalAccumulator, FxBuildHasher>>,
    eviction_queues: Vec<BinaryHeap<HexEvictionEntry>>,
    completed_buffer: VecDeque<MultiCategoricalRecord>,
    is_finished: bool,
    current_lat_horizon: f64,
    pub profile_stats: [u64; 4],
    processed_chunk_count: usize,
    band: usize,
    min_count: Option<f64>,
    min_majority_fraction: Option<f64>,
    compact: bool,
    pub remapper: Option<Arc<CategoryRemapper>>,
    pending_compact: HashMap<u64, (CategoricalAccumulator, Vec<(u64, CategoricalAccumulator)>), FxBuildHasher>,
}

impl MultiCategoricalHorizonStreamer {
    /// Initialize a new MultiCategoricalHorizonStreamer from a single GeoTIFF reader
    pub fn new(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> Result<Self> {
        let mosaic = Arc::new(MosaicReader::from_single_reader(
            reader,
            config.bbox,
            config.custom_crs.as_deref(),
        )?);
        Self::new_mosaic(mosaic, config)
    }

    /// Initialize a new MultiCategoricalHorizonStreamer from a multi-file MosaicReader
    pub fn new_mosaic(mosaic: Arc<MosaicReader>, config: &MultiResolutionConfig) -> Result<Self> {
        if config.resolutions.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "Resolutions list cannot be empty".to_string(),
            ));
        }

        let mut resolutions = Vec::with_capacity(config.resolutions.len());
        let mut resolution_u8s = Vec::with_capacity(config.resolutions.len());
        for &res_u8 in &config.resolutions {
            let res = Resolution::try_from(res_u8).map_err(|_| {
                RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", res_u8))
            })?;
            resolutions.push(res);
            resolution_u8s.push(res_u8);
        }

        let prefetcher = PrefetchedMosaicReader::spawn(Arc::clone(&mosaic), 256);
        let num_res = resolutions.len();

        let mut active_maps = Vec::with_capacity(num_res);
        let mut eviction_queues = Vec::with_capacity(num_res);
        for _ in 0..num_res {
            active_maps.push(HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default()));
            eviction_queues.push(BinaryHeap::with_capacity(1024));
        }

        let band = config.band;
        let min_count = config.min_count;
        let min_majority_fraction = config.min_majority_fraction;
        let compact = config.compact;
        let remapper = config.remapper.clone();
        let pending_compact = HashMap::with_capacity_and_hasher(1024, FxBuildHasher::default());

        Ok(Self {
            prefetcher: Some(prefetcher),
            mosaic,
            resolutions,
            resolution_u8s,
            nodata: config.custom_nodata,
            bbox: config.bbox,
            sampling: config.sampling.clone(),
            active_maps,
            eviction_queues,
            completed_buffer: VecDeque::with_capacity(2048),
            is_finished: false,
            current_lat_horizon: f64::INFINITY,
            profile_stats: [0; 4],
            processed_chunk_count: 0,
            band,
            min_count,
            min_majority_fraction,
            compact,
            remapper,
            pending_compact,
        })
    }

    /// Return current southernmost latitude reached by scanline horizon
    pub fn current_lat_horizon(&self) -> f64 {
        self.current_lat_horizon
    }

    /// Target H3 resolutions
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Target H3 resolution integer levels
    pub fn resolution_u8s(&self) -> &[u8] {
        &self.resolution_u8s
    }

    fn push_categorical_record(&mut self, res_u8: u8, cell_u64: u64, acc: CategoricalAccumulator) {
        if let Some(min_c) = self.min_count {
            if acc.total_count < min_c {
                return;
            }
        }
        if let Some(min_frac) = self.min_majority_fraction {
            let (_, _, frac) = acc.majority();
            if frac < min_frac {
                return;
            }
        }

        if self.compact {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                if let Some(parent_res) = cell.resolution().pred() {
                    if let Some(parent) = cell.parent(parent_res) {
                        let parent_u64: u64 = parent.into();
                        let entry = self.pending_compact.entry(parent_u64).or_insert_with(|| {
                            (CategoricalAccumulator::default(), Vec::with_capacity(7))
                        });
                        entry.0.merge(&acc);
                        entry.1.push((cell_u64, acc));

                        if entry.1.len() == 7 {
                            let (parent_acc, _) = self.pending_compact.remove(&parent_u64).unwrap();
                            let p_res_u8: u8 = parent_res.into();
                            self.completed_buffer.push_back(MultiCategoricalRecord {
                                resolution: p_res_u8,
                                h3_index: parent_u64,
                                accumulator: parent_acc,
                            });
                            return;
                        }
                        return;
                    }
                }
            }
        }

        self.completed_buffer.push_back(MultiCategoricalRecord {
            resolution: res_u8,
            h3_index: cell_u64,
            accumulator: acc,
        });
    }

    fn flush_pending_compact(&mut self) {
        for (_, (_, children)) in self.pending_compact.drain() {
            for (cell_u64, acc) in children {
                let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                    cell.resolution().into()
                } else {
                    8
                };
                self.completed_buffer.push_back(MultiCategoricalRecord {
                    resolution: res_u8,
                    h3_index: cell_u64,
                    accumulator: acc,
                });
            }
        }
    }

    /// Evict completed cells across all resolutions that lie north of the given latitude horizon
    fn evict_completed(&mut self, lat_horizon: f64) {
        let num_res = self.resolutions.len();
        for res_idx in 0..num_res {
            let res_u8 = self.resolution_u8s[res_idx];
            while let Some(top) = self.eviction_queues[res_idx].peek() {
                if top.south_lat > lat_horizon {
                    let entry = self.eviction_queues[res_idx].pop().unwrap();
                    if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                        self.push_categorical_record(res_u8, entry.cell_u64, acc);
                    }
                } else {
                    break;
                }
            }
        }

        if self.compact && !self.pending_compact.is_empty() {
            let mut to_flush = Vec::new();
            for (&parent_u64, _) in self.pending_compact.iter() {
                let parent_south = compute_cell_south_lat(parent_u64);
                if parent_south > lat_horizon {
                    to_flush.push(parent_u64);
                }
            }
            for p in to_flush {
                if let Some((_, children)) = self.pending_compact.remove(&p) {
                    for (cell_u64, acc) in children {
                        let res_u8 = if let Ok(cell) = CellIndex::try_from(cell_u64) {
                            cell.resolution().into()
                        } else {
                            8
                        };
                        self.completed_buffer.push_back(MultiCategoricalRecord {
                            resolution: res_u8,
                            h3_index: cell_u64,
                            accumulator: acc,
                        });
                    }
                }
            }
        }
    }

    /// Advance scanline horizon until at least `min_rows` completed records are available or finished
    pub fn advance_until_completed(&mut self, min_rows: usize) {
        let batch_size = (rayon::current_num_threads() * 4).max(32);
        let min_batch = rayon::current_num_threads().clamp(4, 16);
        let mut chunk_items = Vec::with_capacity(batch_size);

        while self.completed_buffer.len() < min_rows && !self.is_finished {
            chunk_items.clear();
            let t0 = std::time::Instant::now();
            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.drain_chunk_batch_into(&mut chunk_items, min_batch, batch_size);
            }
            self.profile_stats[0] += t0.elapsed().as_nanos() as u64;

            if chunk_items.is_empty() {
                self.is_finished = true;
                self.current_lat_horizon = f64::NEG_INFINITY;
                let num_res = self.resolutions.len();
                for res_idx in 0..num_res {
                    let res_u8 = self.resolution_u8s[res_idx];
                    while let Some(entry) = self.eviction_queues[res_idx].pop() {
                        if let Some(acc) = self.active_maps[res_idx].remove(&entry.cell_u64) {
                            self.push_categorical_record(res_u8, entry.cell_u64, acc);
                        }
                    }
                    let remaining: Vec<(u64, CategoricalAccumulator)> = self.active_maps[res_idx].drain().collect();
                    for (cell_u64, acc) in remaining {
                        self.push_categorical_record(res_u8, cell_u64, acc);
                    }
                }
                self.flush_pending_compact();
                break;
            }

            let resolutions = &self.resolutions;
            let sampling = &self.sampling;
            let bbox = self.bbox;
            let band = self.band;
            let mosaic = Arc::clone(&self.mosaic);
            let user_nodata = self.nodata;
            let remapper = self.remapper.as_deref();

            // Parallel process all chunks using thread-local reusable HashMaps to eliminate allocation churn
            let t1 = std::time::Instant::now();
            let parallel_results: Vec<(Vec<Vec<(u64, CategoricalAccumulator)>>, DecodingResult)> = chunk_items
                .par_iter_mut()
                .map_init(
                    || {
                        let mut maps = Vec::with_capacity(resolutions.len());
                        for _ in 0..resolutions.len() {
                            maps.push(HashMap::with_capacity_and_hasher(128, FxBuildHasher::default()));
                        }
                        maps
                    },
                    |local_maps, item| {
                        match item {
                            Ok((tile_idx, _chunk_idx, chunk_bounds, decoding_result, has_overlap)) => {
                                for m in local_maps.iter_mut() {
                                    m.clear();
                                }
                                let tile = &mosaic.tiles[*tile_idx];
                                let crs_transformer = &tile.crs_transformer;
                                let gt = &tile.reader.metadata.geotransform;
                                let chunk_stride = tile.reader.chunk_layout.chunk_width;
                                let nodata = user_nodata.or(tile.reader.metadata.nodata);
                                let samples_per_pixel = tile.reader.metadata.samples_per_pixel;

                                let overlap_ctx = if *has_overlap {
                                    Some((*tile_idx, &*mosaic))
                                } else {
                                    None
                                };

                                let has_data = process_categorical_chunk_payload_into(
                                    chunk_bounds,
                                    decoding_result,
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
                                    local_maps,
                                );
                                let mut chunk_entries = Vec::with_capacity(if has_data { local_maps.len() } else { 0 });
                                if has_data {
                                    for m in local_maps.iter_mut() {
                                        let entries: Vec<(u64, CategoricalAccumulator)> = m.drain().collect();
                                        chunk_entries.push(entries);
                                    }
                                }
                                let dec = std::mem::replace(decoding_result, DecodingResult::U8(Vec::new()));
                                Some((chunk_entries, dec))
                            }
                            Err(_) => None,
                        }
                    },
                )
                .filter_map(|x| x)
                .collect();
            self.profile_stats[1] += t1.elapsed().as_nanos() as u64;

            let mut recycled_buffers = Vec::with_capacity(parallel_results.len());

            let t2 = std::time::Instant::now();
            self.processed_chunk_count += chunk_items.len();

            for (chunk_entries, decoding_result) in parallel_results {
                if !chunk_entries.is_empty() {
                    for (res_idx, entries) in chunk_entries.into_iter().enumerate() {
                        let active_map = &mut self.active_maps[res_idx];
                        let eviction_queue = &mut self.eviction_queues[res_idx];

                        for (cell_u64, acc) in entries {
                            active_map
                                .entry(cell_u64)
                                .and_modify(|existing| existing.merge(&acc))
                                .or_insert_with(|| {
                                    let south_lat = compute_cell_south_lat(cell_u64);
                                    eviction_queue.push(HexEvictionEntry {
                                        south_lat,
                                        cell_u64,
                                    });
                                    acc
                                });
                        }
                    }
                }

                recycled_buffers.push(decoding_result);
            }

            if let Some(ref prefetcher) = self.prefetcher {
                prefetcher.recycle_batch(recycled_buffers);
            }
            self.profile_stats[2] += t2.elapsed().as_nanos() as u64;

            if self.processed_chunk_count < self.mosaic.chunk_refs.len() {
                let next_chunk = &self.mosaic.chunk_refs[self.processed_chunk_count];
                let safe_lat = next_chunk.north_lat;
                if safe_lat < self.current_lat_horizon {
                    let t3 = std::time::Instant::now();
                    self.current_lat_horizon = safe_lat;
                    self.evict_completed(safe_lat);
                    self.profile_stats[3] += t3.elapsed().as_nanos() as u64;
                }
            }
        }
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiCategoricalRecord> {
        self.advance_until_completed(max_rows);
        let num_to_take = max_rows.min(self.completed_buffer.len());
        let mut batch = Vec::with_capacity(num_to_take);
        for _ in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                batch.push(record);
            }
        }
        batch
    }

    /// Drain up to `max_rows` completed records directly into a closure with zero heap allocation
    pub fn drain_completed_into<F>(&mut self, max_rows: usize, mut consumer: F) -> usize
    where
        F: FnMut(usize, MultiCategoricalRecord),
    {
        self.advance_until_completed(max_rows);
        let num_to_take = max_rows.min(self.completed_buffer.len());
        for i in 0..num_to_take {
            if let Some(record) = self.completed_buffer.pop_front() {
                consumer(i, record);
            }
        }
        num_to_take
    }

    /// Return total active in-flight cells across all resolutions
    pub fn active_cell_count(&self) -> usize {
        self.active_maps.iter().map(|m| m.len()).sum()
    }
}
