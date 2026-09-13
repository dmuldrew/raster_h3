//! Unified Geometric Pixel Walker & Spatial Math for Multi-Horizon Aggregators
//!
//! Centralizes raster coordinate projection, scanline derivatives, bounding box pruning,
//! and mosaic overlap resolution across both continuous (Welford) and categorical engines.

use h3o::{LatLng, Resolution};

use crate::aggregator::h3_scanline::{H3NeighborDiskCache, H3ScanlineLookahead};
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
    pub cos_lat: f64,
    pub cos_lat_sq: f64,
    pub px_diag_m: f64,
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
            let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
            let lon = (x_start / WGS84_A) * RAD_TO_DEG;
            (lon, lat)
        } else {
            match crs_transformer.transform_point(x_start, y_row) {
                Ok(coords) => coords,
                Err(_) => (x_start, y_row),
            }
        };

        if ctx.is_wgs84 || ctx.is_web_mercator {
            if let Some([_, b_min_lat, _, b_max_lat]) = bbox {
                if lat_row < b_min_lat || lat_row > b_max_lat {
                    return None;
                }
            }
        }

        let is_single_point = sampling.is_single_point();
        let (d_lon_dx, d_lat_dx, d_lon_dy, d_lat_dy) = if !is_single_point {
            if ctx.is_wgs84 {
                (gt.a, gt.d, gt.b, gt.e)
            } else if ctx.is_web_mercator {
                let lon_dx = ((x_start + gt.a) / WGS84_A) * RAD_TO_DEG;
                let lat_dy = (2.0 * ((y_row + gt.e) / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
                (lon_dx - lon_start, 0.0, 0.0, lat_dy - lat_row)
            } else {
                let (lon_x, lat_x) = match crs_transformer.transform_point(x_start + ctx.dx_step, y_row) {
                    Ok(coords) => coords,
                    Err(_) => (lon_start, lat_row),
                };
                let (lon_y, lat_y) = match crs_transformer.transform_point(x_start, y_row + gt.e) {
                    Ok(coords) => coords,
                    Err(_) => (lon_start, lat_row),
                };
                (lon_x - lon_start, lat_x - lat_row, lon_y - lon_start, lat_y - lat_row)
            }
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };

        let (row_c_start, row_c_end) = if (ctx.is_wgs84 || ctx.is_web_mercator) && bbox.is_some() {
            let [b_min_lon, _, b_max_lon, _] = bbox.unwrap();
            if ctx.d_lon_step > 0.0 {
                let c_s = if lon_start < b_min_lon {
                    ((b_min_lon - lon_start) / ctx.d_lon_step).ceil().max(0.0) as usize
                } else {
                    0
                };
                let c_e = if lon_start < b_max_lon {
                    (((b_max_lon - lon_start) / ctx.d_lon_step).floor().max(0.0) as usize + 1).min(row_width)
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
                    (((b_min_lon - lon_start) / ctx.d_lon_step).floor().max(0.0) as usize + 1).min(row_width)
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

        let cos_lat = lat_row.to_radians().cos();
        let cos_lat_sq = cos_lat * cos_lat;

        let px_diag_m = if !is_single_point {
            if ctx.is_wgs84 {
                let dx_m = ctx.d_lon_step.abs() * 111_320.0 * cos_lat;
                let dy_m = gt.e.abs() * 110_540.0;
                dx_m.hypot(dy_m)
            } else if ctx.is_web_mercator {
                let dx_m = ctx.d_lon_step.abs() * 111_320.0 * cos_lat;
                let dy_m = d_lat_dy.abs() * 110_540.0;
                dx_m.hypot(dy_m)
            } else {
                let dx_m = (d_lon_dx * cos_lat).hypot(d_lat_dx) * 111_320.0;
                let dy_m = (d_lon_dy * cos_lat).hypot(d_lat_dy) * 110_540.0;
                dx_m.hypot(dy_m)
            }
        } else {
            0.0
        };

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
            cos_lat,
            cos_lat_sq,
            px_diag_m,
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
        if ctx.is_wgs84 || ctx.is_web_mercator {
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
        if ctx.is_wgs84 || ctx.is_web_mercator {
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
                            LatLng::new(p_lat, p_lon).ok().map(|ll| ll.to_cell(res).into())
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
        row_cache.find_core_span(c, span_end, dx_bounds, dy_bounds, |px, py| {
            let (lon, lat) = if ctx.is_wgs84 {
                (self.lon_start + (px - 0.5) * ctx.d_lon_step, self.lat_row + (py - 0.5) * gt.e)
            } else if ctx.is_web_mercator {
                (self.lon_start + (px - 0.5) * ctx.d_lon_step, self.lat_row + (py - 0.5) * self.d_lat_dy)
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
        let (k_lon, k_lat) = if ctx.is_wgs84 || ctx.is_web_mercator {
            (self.lon_start + (k as f64) * ctx.d_lon_step, self.lat_row)
        } else if ctx.is_north_up {
            let x_k = self.x_start + (k as f64) * ctx.dx_step;
            match crs_transformer.transform_point(x_k, self.y_row) {
                Ok(coords) => coords,
                Err(_) => (self.lon_start + (k as f64) * self.d_lon_dx, self.lat_row + (k as f64) * self.d_lat_dx),
            }
        } else {
            let (x_k, y_k) = gt.pixel_center_to_coord(col_offset + k, self.row_idx);
            match crs_transformer.transform_point(x_k, y_k) {
                Ok(coords) => coords,
                Err(_) => (self.lon_start + (k as f64) * self.d_lon_dx, self.lat_row + (k as f64) * self.d_lat_dx),
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

/// Resolve subpixel cell taking fast-paths into account (e.g. center sample or neighbor disk cache)
#[inline(always)]
pub fn resolve_subpixel_cell(
    run_cell: u64,
    lat: f64,
    lon: f64,
    d_x: f64,
    d_y: f64,
    res: Resolution,
    use_neighbor_cache: bool,
    disk_cache: &mut H3NeighborDiskCache,
) -> Option<u64> {
    if d_x.abs() < 1e-9 && d_y.abs() < 1e-9 {
        Some(run_cell)
    } else if use_neighbor_cache {
        Some(disk_cache.resolve_point(lat, lon))
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
                                on_cell(res_idx, cell_u64, sp.weight, val);
                            }
                        }
                    }
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
        let mut disk_cache = H3NeighborDiskCache::default();
        let res = Resolution::try_from(8).unwrap();
        let cell = resolve_subpixel_cell(
            run_cell, 37.75, -122.25, 0.0, 0.0, res, false, &mut disk_cache,
        );
        assert_eq!(cell, Some(run_cell));
    }
}
