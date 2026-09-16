//! Unified Geometric Pixel Walker & Spatial Math for Multi-Horizon Aggregators
//!
//! Centralizes raster coordinate projection, scanline derivatives, bounding box pruning,
//! and mosaic overlap resolution across both continuous (Welford) and categorical engines.

use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use std::collections::HashMap;

use crate::aggregator::h3_scanline::H3ScanlineLookahead;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

pub const WGS84_A: f64 = 6378137.0;
pub const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Fast check if an entire row slice consists purely of NoData values
#[inline(always)]
pub fn is_slice_all_native_nodata<T, N>(slice: &[T], native_nodata: Option<N>) -> bool
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

/// Precomputed chunk geometry context shared across all scanlines in a chunk
#[derive(Debug, Clone)]
pub struct RowGeometryContext {
    pub is_wgs84: bool,
    pub is_web_mercator: bool,
    pub is_north_up: bool,
    pub d_lon_step: f64,
    pub dx_step: f64,
    pub stride: usize,
    pub actual_rows: usize,
}

impl RowGeometryContext {
    pub fn new(
        chunk: &RasterChunk,
        slice_len: usize,
        chunk_stride: u32,
        crs_transformer: &CrsTransformer,
        gt: &GeoTransform,
    ) -> Self {
        let is_wgs84 = matches!(crs_transformer, CrsTransformer::Wgs84Identity);
        let is_web_mercator = matches!(crs_transformer, CrsTransformer::WebMercatorFast);
        let d_lon_step = if is_wgs84 {
            gt.a
        } else if is_web_mercator {
            (gt.a / WGS84_A) * RAD_TO_DEG
        } else {
            0.0
        };

        let stride = if chunk_stride > 0 && slice_len >= chunk_stride as usize {
            chunk_stride as usize
        } else {
            (chunk.width as usize).max(1)
        };
        let actual_rows = (slice_len / stride).min(chunk.height as usize);
        let is_north_up = gt.b == 0.0 && gt.d == 0.0;
        let dx_step = gt.a;

        Self {
            is_wgs84,
            is_web_mercator,
            is_north_up,
            d_lon_step,
            dx_step,
            stride,
            actual_rows,
        }
    }
}

/// Per-row spatial coordinates, bounds, derivatives, and lookahead parameters
#[derive(Debug, Clone, Copy)]
pub struct RowCoordinates {
    pub row_idx: usize,
    pub x_start: f64,
    pub y_row: f64,
    pub lon_start: f64,
    pub lat_row: f64,
    pub d_lon_dx: f64,
    pub d_lat_dx: f64,
    pub d_lon_dy: f64,
    pub d_lat_dy: f64,
    pub row_c_start: usize,
    pub row_c_end: usize,
}

impl RowCoordinates {
    pub fn compute(
        r: usize,
        chunk: &RasterChunk,
        row_width: usize,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        sampling: &SamplingPattern,
        bbox: Option<[f64; 4]>,
    ) -> Option<Self> {
        let row_idx = (chunk.row_offset + r as u32) as usize;
        let (x_start, y_row) = gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);
        let (lon_start, lat_row) = if ctx.is_wgs84 {
            (x_start, y_row)
        } else if ctx.is_web_mercator {
            let lat =
                (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
            let lon = (x_start / WGS84_A) * RAD_TO_DEG;
            (lon, lat)
        } else {
            match crs_transformer.transform_point(x_start, y_row) {
                Ok(coords) => coords,
                Err(_) => (x_start, y_row),
            }
        };

        if ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) {
            if let Some([_, b_min_lat, _, b_max_lat]) = bbox {
                if lat_row < b_min_lat || lat_row > b_max_lat {
                    return None;
                }
            }
        }

        let is_single_point = sampling.is_single_point();
        let (d_lon_dx, d_lat_dx, d_lon_dy, d_lat_dy) = if !is_single_point && ctx.is_north_up {
            if ctx.is_wgs84 {
                (gt.a, gt.d, gt.b, gt.e)
            } else if ctx.is_web_mercator {
                let lon_dx = ((x_start + gt.a) / WGS84_A) * RAD_TO_DEG;
                let lat_dy = (2.0 * ((y_row + gt.e) / WGS84_A).exp().atan()
                    - std::f64::consts::FRAC_PI_2)
                    * RAD_TO_DEG;
                (lon_dx - lon_start, 0.0, 0.0, lat_dy - lat_row)
            } else {
                let (lon_x, lat_x) =
                    match crs_transformer.transform_point(x_start + ctx.dx_step, y_row) {
                        Ok(coords) => coords,
                        Err(_) => (lon_start, lat_row),
                    };
                let (lon_y, lat_y) = match crs_transformer.transform_point(x_start, y_row + gt.e) {
                    Ok(coords) => coords,
                    Err(_) => (lon_start, lat_row),
                };
                (
                    lon_x - lon_start,
                    lat_x - lat_row,
                    lon_y - lon_start,
                    lat_y - lat_row,
                )
            }
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };

        let (row_c_start, row_c_end) =
            if ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) && bbox.is_some() {
                let [b_min_lon, _, b_max_lon, _] = bbox.unwrap();
                if ctx.d_lon_step > 0.0 {
                    let c_s = if lon_start < b_min_lon {
                        ((b_min_lon - lon_start) / ctx.d_lon_step).ceil().max(0.0) as usize
                    } else {
                        0
                    };
                    let c_e = if lon_start < b_max_lon {
                        (((b_max_lon - lon_start) / ctx.d_lon_step).floor().max(0.0) as usize + 1)
                            .min(row_width)
                    } else {
                        0
                    };
                    (c_s, c_e)
                } else if ctx.d_lon_step < 0.0 {
                    let c_s = if lon_start > b_max_lon {
                        ((b_max_lon - lon_start) / ctx.d_lon_step).ceil().max(0.0) as usize
                    } else {
                        0
                    };
                    let c_e = if lon_start > b_min_lon {
                        (((b_min_lon - lon_start) / ctx.d_lon_step).floor().max(0.0) as usize + 1)
                            .min(row_width)
                    } else {
                        0
                    };
                    (c_s, c_e)
                } else {
                    (0, row_width)
                }
            } else {
                (0, row_width)
            };

        if row_c_start >= row_c_end || row_c_start >= row_width {
            return None;
        }

        Some(Self {
            row_idx,
            x_start,
            y_row,
            lon_start,
            lat_row,
            d_lon_dx,
            d_lat_dx,
            d_lon_dy,
            d_lat_dy,
            row_c_start,
            row_c_end,
        })
    }

    /// Compute the projected longitude and latitude for the pixel center at column `c`
    #[inline(always)]
    pub fn pixel_center_lon_lat(
        &self,
        c: usize,
        x_curr: f64,
        lon_curr: f64,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        col_offset: usize,
    ) -> Option<(f64, f64)> {
        if ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) {
            Some((lon_curr, self.lat_row))
        } else if ctx.is_north_up {
            crs_transformer.transform_point(x_curr, self.y_row).ok()
        } else {
            let (x, y) = gt.pixel_center_to_coord(col_offset + c, self.row_idx);
            crs_transformer.transform_point(x, y).ok()
        }
    }

    /// Delegate span end search to scanline lookahead cache across coordinate reference systems
    #[inline(always)]
    pub fn find_span_end(
        &self,
        row_cache: &mut H3ScanlineLookahead,
        c: usize,
        lon_curr: f64,
        ctx: &RowGeometryContext,
        crs_transformer: &CrsTransformer,
        res: Resolution,
        run_cell: u64,
        bbox: Option<[f64; 4]>,
    ) -> (usize, Option<u64>) {
        if ctx.is_north_up && (ctx.is_wgs84 || ctx.is_web_mercator) {
            row_cache.find_span_end(
                c,
                self.row_c_end,
                lon_curr,
                self.lat_row,
                ctx.d_lon_step,
                res,
                run_cell,
            )
        } else if ctx.is_north_up {
            row_cache.find_span_end_projected(
                c,
                self.row_c_end,
                self.x_start,
                self.y_row,
                ctx.dx_step,
                |x, y| match crs_transformer.transform_point(x, y) {
                    Ok((p_lon, p_lat)) => {
                        if is_point_in_bbox(p_lon, p_lat, bbox) {
                            LatLng::new(p_lat, p_lon)
                                .ok()
                                .map(|ll| ll.to_cell(res).into())
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                },
                run_cell,
            )
        } else {
            (c + 1, None)
        }
    }

    /// Identify the inner core column range `(core_start, core_end)` where all subpixel sample points land inside `run_cell`
    #[inline(always)]
    pub fn find_core_span<FCheck>(
        &self,
        row_cache: &H3ScanlineLookahead,
        c: usize,
        span_end: usize,
        dx_bounds: (f64, f64),
        dy_bounds: (f64, f64),
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        bbox: Option<[f64; 4]>,
        mut is_in_cell: FCheck,
    ) -> (usize, usize)
    where
        FCheck: FnMut(f64, f64) -> bool,
    {
        // A rotated pixel needs the full affine transform for each sample.
        if !ctx.is_north_up {
            return (c, c);
        }
        row_cache.find_core_span(c, span_end, dx_bounds, dy_bounds, |px, py| {
            let (lon, lat) = if ctx.is_wgs84 {
                (
                    self.lon_start + (px - 0.5) * ctx.d_lon_step,
                    self.lat_row + (py - 0.5) * gt.e,
                )
            } else if ctx.is_web_mercator {
                (
                    self.lon_start + (px - 0.5) * ctx.d_lon_step,
                    self.lat_row + (py - 0.5) * self.d_lat_dy,
                )
            } else {
                let d_col = px - 0.5;
                let d_row = py - 0.5;
                (
                    self.lon_start + d_col * self.d_lon_dx + d_row * self.d_lon_dy,
                    self.lat_row + d_col * self.d_lat_dx + d_row * self.d_lat_dy,
                )
            };
            if !is_point_in_bbox(lon, lat, bbox) {
                return false;
            }
            is_in_cell(lat, lon)
        })
    }

    /// Iterate over all subpixel sampling points for pixel at column `k`, invoking `f(lon, lat, d_x, d_y, weight)`
    #[inline(always)]
    pub fn for_each_subpixel<F>(
        &self,
        k: usize,
        ctx: &RowGeometryContext,
        gt: &GeoTransform,
        crs_transformer: &CrsTransformer,
        col_offset: usize,
        sampling: &SamplingPattern,
        bbox: Option<[f64; 4]>,
        mut f: F,
    ) where
        F: FnMut(f64, f64, f64, f64, f64),
    {
        if !ctx.is_north_up {
            for sp in &sampling.points {
                let (x, y) =
                    gt.pixel_to_coord((col_offset + k) as f64 + sp.dx, self.row_idx as f64 + sp.dy);
                if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                    if is_point_in_bbox(lon, lat, bbox) {
                        f(lon, lat, sp.dx - 0.5, sp.dy - 0.5, sp.weight);
                    }
                }
            }
            return;
        }
        let (k_lon, k_lat) = if ctx.is_wgs84 || ctx.is_web_mercator {
            (self.lon_start + (k as f64) * ctx.d_lon_step, self.lat_row)
        } else {
            let x_k = self.x_start + (k as f64) * ctx.dx_step;
            match crs_transformer.transform_point(x_k, self.y_row) {
                Ok(coords) => coords,
                Err(_) => (
                    self.lon_start + (k as f64) * self.d_lon_dx,
                    self.lat_row + (k as f64) * self.d_lat_dx,
                ),
            }
        };

        for sp in &sampling.points {
            let d_x = sp.dx - 0.5;
            let d_y = sp.dy - 0.5;
            let lon = k_lon + d_x * self.d_lon_dx + d_y * self.d_lon_dy;
            let lat = k_lat + d_x * self.d_lat_dx + d_y * self.d_lat_dy;

            if !is_point_in_bbox(lon, lat, bbox) {
                continue;
            }

            f(lon, lat, d_x, d_y, sp.weight);
        }
    }
}

/// Check if a WGS84 point `(lon, lat)` falls within an optional bounding box `[min_lon, min_lat, max_lon, max_lat]`
#[inline(always)]
pub fn is_point_in_bbox(lon: f64, lat: f64, bbox: Option<[f64; 4]>) -> bool {
    if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
        lon >= b_min_lon && lon <= b_max_lon && lat >= b_min_lat && lat <= b_max_lat
    } else {
        true
    }
}

/// Resolve boundary samples with exact H3 indexing; reuse only the exact center sample.
#[inline(always)]
pub fn resolve_subpixel_cell(
    run_cell: u64,
    lat: f64,
    lon: f64,
    d_x: f64,
    d_y: f64,
    res: Resolution,
) -> Option<u64> {
    if d_x == 0.0 && d_y == 0.0 {
        Some(run_cell)
    } else {
        LatLng::new(lat, lon).ok().map(|ll| ll.to_cell(res).into())
    }
}

/// Generic pixel-by-pixel walker for mosaic overlap resolution
pub fn walk_overlap_pixel_cells<T, FVal, FAccum>(
    slice: &[T],
    chunk: &RasterChunk,
    chunk_stride: u32,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    tile_idx: usize,
    mosaic: &MosaicReader,
    mut is_valid: FVal,
    mut on_cell: FAccum,
) where
    T: Copy,
    FVal: FnMut(T) -> bool,
    FAccum: FnMut(usize, u64, f64, T),
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
            if !is_valid(val) {
                continue;
            }

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
                        on_cell(res_idx, cell_u64, 1.0, val);
                    }
                }
            } else {
                for sp in &sampling.points {
                    let px = (chunk.col_offset as f64) + (c as f64) + sp.dx;
                    let py = (chunk.row_offset as f64) + (r as f64) + sp.dy;
                    let (x, y) = gt.pixel_to_coord(px, py);
                    if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                        if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = bbox {
                            if lon < b_min_lon
                                || lon > b_max_lon
                                || lat < b_min_lat
                                || lat > b_max_lat
                            {
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
                                on_cell(res_idx, cell_u64, sp.weight, val);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Trait implemented by accumulator engines (continuous and categorical) to drive the generic scanline walker
pub trait ScanlineEngine<T, Acc> {
    type Sample: Copy;

    /// Allocate a fresh empty accumulator (or with quantiles if configured)
    fn new_acc(&self) -> Acc;

    /// Reset an accumulator to its zero/empty state
    fn clear_acc(&self, acc: &mut Acc);

    /// Check if accumulator contains valid observations
    fn has_samples(&self, acc: &Acc) -> bool;

    /// Merge the source accumulator into the destination
    fn merge_acc(&self, dest: &mut Acc, src: &Acc);

    /// Accumulate a span of pixels into a single accumulator
    fn accumulate_span(&self, acc: &mut Acc, slice: &[T]);

    /// Accumulate a span of pixels across multiple active resolution accumulators
    fn accumulate_span_multi(&self, run_accs: &mut [Acc], run_cells: &[u64], slice: &[T]);

    /// Extract a valid sample from a raw pixel, or None if nodata
    fn get_sample(&self, pixel: T) -> Option<Self::Sample>;

    /// Update an accumulator with a sample and weight
    fn update_sample(&self, acc: &mut Acc, sample: Self::Sample, weight: f64);
}

/// Unified generic scanline walker across all resolutions and sampling patterns
pub fn scanline_walk<T, Acc, E, FNoData>(
    slice: &[T],
    chunk: &RasterChunk,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    is_row_all_nodata: FNoData,
    engine: &E,
    chunk_maps: &mut [HashMap<u64, Acc, FxBuildHasher>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
    FNoData: Fn(&[T]) -> bool,
{
    let geom_ctx = RowGeometryContext::new(chunk, slice.len(), chunk_stride, crs_transformer, gt);
    let RowGeometryContext {
        is_wgs84,
        is_web_mercator,
        is_north_up,
        d_lon_step,
        dx_step,
        stride,
        actual_rows,
    } = geom_ctx;
    let num_res = resolutions.len();

    let mut row_caches: Vec<H3ScanlineLookahead> = resolutions
        .iter()
        .map(|&res| H3ScanlineLookahead::for_resolution(res))
        .collect();

    let is_single_point = sampling.is_single_point();
    let dx_bounds = sampling.dx_bounds();
    let dy_bounds = sampling.dy_bounds();

    let mut span_ends = if num_res > 1 {
        vec![0usize; num_res]
    } else {
        Vec::new()
    };
    let mut run_cells = if num_res > 1 {
        vec![0u64; num_res]
    } else {
        Vec::new()
    };
    let mut run_accs: Vec<Acc> = if num_res > 1 {
        (0..num_res).map(|_| engine.new_acc()).collect()
    } else {
        Vec::new()
    };
    let mut known_next_cells: Vec<Option<u64>> = if num_res > 1 {
        vec![None; num_res]
    } else {
        Vec::new()
    };
    let mut core_starts = if num_res > 1 {
        vec![0usize; num_res]
    } else {
        Vec::new()
    };
    let mut core_ends = if num_res > 1 {
        vec![0usize; num_res]
    } else {
        Vec::new()
    };

    for r in 0..actual_rows {
        let slice_row_start = r * stride;
        let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
        if row_width == 0 {
            continue;
        }

        let slice_row = &slice[slice_row_start..slice_row_start + row_width];
        if is_row_all_nodata(slice_row) {
            continue;
        }

        let coords = match RowCoordinates::compute(
            r,
            chunk,
            row_width,
            &geom_ctx,
            gt,
            crs_transformer,
            sampling,
            bbox,
        ) {
            Some(c) => c,
            None => continue,
        };

        let RowCoordinates {
            x_start,
            lon_start,
            row_c_start,
            row_c_end,
            ..
        } = coords;

        if num_res == 1 {
            let res_idx = 0;
            let res = resolutions[res_idx];
            let active_map = &mut chunk_maps[res_idx];
            let row_cache = &mut row_caches[res_idx];
            row_cache.reset_row();
            let mut run_cell: u64 = 0;
            let mut run_acc = engine.new_acc();

            let mut lon_curr = lon_start + (row_c_start as f64) * d_lon_step;
            let mut x_curr = x_start + (row_c_start as f64) * dx_step;
            let mut c = row_c_start;
            let mut known_next_cell: Option<u64> = None;

            while c < row_c_end {
                let (lon, lat) = match coords.pixel_center_lon_lat(
                    c,
                    x_curr,
                    lon_curr,
                    &geom_ctx,
                    gt,
                    crs_transformer,
                    chunk.col_offset as usize,
                ) {
                    Some(ll) => ll,
                    None => {
                        known_next_cell = None;
                        c += 1;
                        if is_north_up {
                            x_curr += dx_step;
                        }
                        continue;
                    }
                };

                if (is_north_up && !is_wgs84 && !is_web_mercator)
                    || (!is_north_up && is_single_point)
                {
                    if !is_point_in_bbox(lon, lat, bbox) {
                        known_next_cell = None;
                        c += 1;
                        if is_north_up {
                            x_curr += dx_step;
                        }
                        continue;
                    }
                }

                let cell_opt = known_next_cell
                    .take()
                    .or_else(|| row_cache.get_or_compute_cell(lat, lon, res));

                if let Some(cell_u64) = cell_opt {
                    if cell_u64 != run_cell {
                        if run_cell != 0 && engine.has_samples(&run_acc) {
                            active_map
                                .entry(run_cell)
                                .and_modify(|acc| engine.merge_acc(acc, &run_acc))
                                .or_insert_with(|| run_acc.clone());
                        }
                        run_cell = cell_u64;
                        engine.clear_acc(&mut run_acc);
                        row_cache.on_cell_changed();
                    }

                    let (span_end, next_cell) = coords.find_span_end(
                        row_cache,
                        c,
                        lon_curr,
                        &geom_ctx,
                        crs_transformer,
                        res,
                        run_cell,
                        bbox,
                    );

                    if is_single_point {
                        let span_slice = &slice[slice_row_start + c..slice_row_start + span_end];
                        engine.accumulate_span(&mut run_acc, span_slice);
                    } else {
                        let (core_start, core_end) = coords.find_core_span(
                            row_cache,
                            c,
                            span_end,
                            dx_bounds,
                            dy_bounds,
                            &geom_ctx,
                            gt,
                            bbox,
                            |lat, lon| {
                                LatLng::new(lat, lon).ok().map(|ll| ll.to_cell(res).into())
                                    == Some(run_cell)
                            },
                        );

                        let evaluate_boundary = |k: usize,
                                                 run_acc: &mut Acc,
                                                 active_map: &mut HashMap<
                            u64,
                            Acc,
                            FxBuildHasher,
                        >| {
                            let val_raw = slice[slice_row_start + k];
                            if let Some(sample) = engine.get_sample(val_raw) {
                                coords.for_each_subpixel(
                                    k,
                                    &geom_ctx,
                                    gt,
                                    crs_transformer,
                                    chunk.col_offset as usize,
                                    sampling,
                                    bbox,
                                    |lon, lat, d_x, d_y, weight| {
                                        let cell = match resolve_subpixel_cell(
                                            run_cell, lat, lon, d_x, d_y, res,
                                        ) {
                                            Some(c) => c,
                                            None => return,
                                        };

                                        if cell == run_cell {
                                            engine.update_sample(run_acc, sample, weight);
                                        } else {
                                            active_map
                                                .entry(cell)
                                                .and_modify(|acc| {
                                                    engine.update_sample(acc, sample, weight)
                                                })
                                                .or_insert_with(|| {
                                                    let mut acc = engine.new_acc();
                                                    engine.update_sample(&mut acc, sample, weight);
                                                    acc
                                                });
                                        }
                                    },
                                );
                            }
                        };

                        for k in c..core_start {
                            evaluate_boundary(k, &mut run_acc, active_map);
                        }

                        if core_end > core_start {
                            let core_slice =
                                &slice[slice_row_start + core_start..slice_row_start + core_end];
                            engine.accumulate_span(&mut run_acc, core_slice);
                        }

                        for k in core_end..span_end {
                            evaluate_boundary(k, &mut run_acc, active_map);
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

            if run_cell != 0 && engine.has_samples(&run_acc) {
                active_map
                    .entry(run_cell)
                    .and_modify(|acc| engine.merge_acc(acc, &run_acc))
                    .or_insert_with(|| run_acc.clone());
            }
        } else {
            for i in 0..num_res {
                row_caches[i].reset_row();
                run_cells[i] = 0;
                engine.clear_acc(&mut run_accs[i]);
                known_next_cells[i] = None;
                span_ends[i] = row_c_start;
                core_starts[i] = row_c_start;
                core_ends[i] = row_c_start;
            }

            let mut lon_curr = lon_start + (row_c_start as f64) * d_lon_step;
            let mut x_curr = x_start + (row_c_start as f64) * dx_step;
            let mut c = row_c_start;

            while c < row_c_end {
                let (lon, lat) = match coords.pixel_center_lon_lat(
                    c,
                    x_curr,
                    lon_curr,
                    &geom_ctx,
                    gt,
                    crs_transformer,
                    chunk.col_offset as usize,
                ) {
                    Some(ll) => ll,
                    None => {
                        for i in 0..num_res {
                            if run_cells[i] != 0 && engine.has_samples(&run_accs[i]) {
                                chunk_maps[i]
                                    .entry(run_cells[i])
                                    .and_modify(|acc| engine.merge_acc(acc, &run_accs[i]))
                                    .or_insert_with(|| run_accs[i].clone());
                                engine.clear_acc(&mut run_accs[i]);
                            }
                            run_cells[i] = 0;
                            known_next_cells[i] = None;
                            span_ends[i] = c + 1;
                        }
                        c += 1;
                        if is_north_up {
                            x_curr += dx_step;
                        }
                        continue;
                    }
                };

                if (is_north_up && !is_wgs84 && !is_web_mercator)
                    || (!is_north_up && is_single_point)
                {
                    if !is_point_in_bbox(lon, lat, bbox) {
                        for i in 0..num_res {
                            if run_cells[i] != 0 && engine.has_samples(&run_accs[i]) {
                                chunk_maps[i]
                                    .entry(run_cells[i])
                                    .and_modify(|acc| engine.merge_acc(acc, &run_accs[i]))
                                    .or_insert_with(|| run_accs[i].clone());
                                engine.clear_acc(&mut run_accs[i]);
                            }
                            run_cells[i] = 0;
                            known_next_cells[i] = None;
                            span_ends[i] = c + 1;
                        }
                        c += 1;
                        if is_north_up {
                            x_curr += dx_step;
                        }
                        continue;
                    }
                }

                for i in 0..num_res {
                    if c >= span_ends[i] {
                        let res = resolutions[i];
                        let cell_opt = known_next_cells[i]
                            .take()
                            .or_else(|| row_caches[i].get_or_compute_cell(lat, lon, res));

                        if let Some(cell_u64) = cell_opt {
                            if cell_u64 != run_cells[i] {
                                if run_cells[i] != 0 && engine.has_samples(&run_accs[i]) {
                                    chunk_maps[i]
                                        .entry(run_cells[i])
                                        .and_modify(|acc| engine.merge_acc(acc, &run_accs[i]))
                                        .or_insert_with(|| run_accs[i].clone());
                                    engine.clear_acc(&mut run_accs[i]);
                                }
                                run_cells[i] = cell_u64;
                                row_caches[i].on_cell_changed();
                            }

                            let (span_end, next_cell) = coords.find_span_end(
                                &mut row_caches[i],
                                c,
                                lon_curr,
                                &geom_ctx,
                                crs_transformer,
                                res,
                                run_cells[i],
                                bbox,
                            );

                            span_ends[i] = span_end;
                            known_next_cells[i] = next_cell;

                            if !is_single_point {
                                let (c_start, c_end) = coords.find_core_span(
                                    &row_caches[i],
                                    c,
                                    span_end,
                                    dx_bounds,
                                    dy_bounds,
                                    &geom_ctx,
                                    gt,
                                    bbox,
                                    |test_lat, test_lon| {
                                        LatLng::new(test_lat, test_lon)
                                            .ok()
                                            .map(|ll| ll.to_cell(res).into())
                                            == Some(run_cells[i])
                                    },
                                );
                                core_starts[i] = c_start;
                                core_ends[i] = c_end;
                            }
                        } else {
                            if run_cells[i] != 0 && engine.has_samples(&run_accs[i]) {
                                chunk_maps[i]
                                    .entry(run_cells[i])
                                    .and_modify(|acc| engine.merge_acc(acc, &run_accs[i]))
                                    .or_insert_with(|| run_accs[i].clone());
                                engine.clear_acc(&mut run_accs[i]);
                            }
                            run_cells[i] = 0;
                            span_ends[i] = c + 1;
                            known_next_cells[i] = None;
                            core_starts[i] = c + 1;
                            core_ends[i] = c + 1;
                        }
                    }
                }

                let mut step_end = row_c_end;
                for i in 0..num_res {
                    step_end = step_end.min(span_ends[i]);
                }
                let step_end = step_end.max(c + 1).min(row_c_end);

                if is_single_point {
                    let span_slice = &slice[slice_row_start + c..slice_row_start + step_end];
                    engine.accumulate_span_multi(&mut run_accs, &run_cells, span_slice);
                } else {
                    let mut sub_core_start = c;
                    let mut sub_core_end = step_end;
                    for i in 0..num_res {
                        if run_cells[i] != 0 {
                            sub_core_start = sub_core_start.max(core_starts[i]);
                            sub_core_end = sub_core_end.min(core_ends[i]);
                        }
                    }

                    let evaluate_boundary_multi = |k: usize,
                                                   run_accs: &mut [Acc],
                                                   chunk_maps: &mut [HashMap<
                        u64,
                        Acc,
                        FxBuildHasher,
                    >]| {
                        let val_raw = slice[slice_row_start + k];
                        if let Some(sample) = engine.get_sample(val_raw) {
                            for i in 0..num_res {
                                if run_cells[i] == 0 {
                                    continue;
                                }
                                if k >= core_starts[i] && k < core_ends[i] {
                                    engine.update_sample(&mut run_accs[i], sample, 1.0);
                                }
                            }

                            let any_subpixel = (0..num_res).any(|i| {
                                run_cells[i] != 0 && (k < core_starts[i] || k >= core_ends[i])
                            });
                            if any_subpixel {
                                coords.for_each_subpixel(
                                    k,
                                    &geom_ctx,
                                    gt,
                                    crs_transformer,
                                    chunk.col_offset as usize,
                                    sampling,
                                    bbox,
                                    |lon, lat, d_x, d_y, weight| {
                                        for i in 0..num_res {
                                            if run_cells[i] == 0
                                                || (k >= core_starts[i] && k < core_ends[i])
                                            {
                                                continue;
                                            }
                                            let res = resolutions[i];
                                            let cell = match resolve_subpixel_cell(
                                                run_cells[i],
                                                lat,
                                                lon,
                                                d_x,
                                                d_y,
                                                res,
                                            ) {
                                                Some(c) => c,
                                                None => continue,
                                            };

                                            if cell == run_cells[i] {
                                                engine.update_sample(
                                                    &mut run_accs[i],
                                                    sample,
                                                    weight,
                                                );
                                            } else {
                                                chunk_maps[i]
                                                    .entry(cell)
                                                    .and_modify(|acc| {
                                                        engine.update_sample(acc, sample, weight)
                                                    })
                                                    .or_insert_with(|| {
                                                        let mut a = engine.new_acc();
                                                        engine
                                                            .update_sample(&mut a, sample, weight);
                                                        a
                                                    });
                                            }
                                        }
                                    },
                                );
                            }
                        }
                    };

                    if sub_core_start < sub_core_end {
                        for k in c..sub_core_start {
                            evaluate_boundary_multi(k, &mut run_accs, chunk_maps);
                        }

                        let core_slice = &slice
                            [slice_row_start + sub_core_start..slice_row_start + sub_core_end];
                        engine.accumulate_span_multi(&mut run_accs, &run_cells, core_slice);

                        for k in sub_core_end..step_end {
                            evaluate_boundary_multi(k, &mut run_accs, chunk_maps);
                        }
                    } else {
                        for k in c..step_end {
                            evaluate_boundary_multi(k, &mut run_accs, chunk_maps);
                        }
                    }
                }

                let num_stepped = step_end - c;
                for i in 0..num_res {
                    row_caches[i].advance_span(num_stepped);
                }

                if is_wgs84 || is_web_mercator {
                    lon_curr += (num_stepped as f64) * d_lon_step;
                } else if is_north_up {
                    x_curr += (num_stepped as f64) * dx_step;
                }
                c = step_end;
            }

            for i in 0..num_res {
                if run_cells[i] != 0 && engine.has_samples(&run_accs[i]) {
                    chunk_maps[i]
                        .entry(run_cells[i])
                        .and_modify(|acc| engine.merge_acc(acc, &run_accs[i]))
                        .or_insert_with(|| run_accs[i].clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_point_in_bbox() {
        let bbox = Some([-122.5, 37.5, -122.0, 38.0]);
        assert!(is_point_in_bbox(-122.25, 37.75, bbox));
        assert!(is_point_in_bbox(-122.5, 37.5, bbox));
        assert!(is_point_in_bbox(-122.0, 38.0, bbox));
        assert!(!is_point_in_bbox(-122.6, 37.75, bbox));
        assert!(!is_point_in_bbox(-122.25, 38.1, bbox));
        assert!(is_point_in_bbox(0.0, 0.0, None));
    }

    #[test]
    fn test_resolve_subpixel_cell_center_fastpath() {
        let run_cell = 0x8828308281ffffff;
        let res = Resolution::try_from(8).unwrap();
        let cell = resolve_subpixel_cell(run_cell, 37.75, -122.25, 0.0, 0.0, res);
        assert_eq!(cell, Some(run_cell));
    }
}
