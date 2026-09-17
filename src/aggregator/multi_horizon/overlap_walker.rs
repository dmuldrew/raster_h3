//! Pixel-by-pixel walker for mosaic overlap resolution.
//!
//! Evaluates per-pixel mosaic tile ownership when chunks overlap multiple tiles.

use h3o::{LatLng, Resolution};

use super::coordinates::{is_point_in_bbox, CoordinateTransformer};
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

/// Generic pixel-by-pixel walker for mosaic overlap resolution using CoordinateTransformer.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
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
    let coord_tx = CoordinateTransformer::new(gt, crs_transformer);

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
                let (lon, lat) = match coord_tx
                    .pixel_center_to_wgs84((chunk.col_offset as usize) + c, row_idx)
                {
                    Ok(coords) => coords,
                    Err(_) => continue,
                };

                if !is_point_in_bbox(lon, lat, bbox) {
                    continue;
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
                    let (lon, lat) = match coord_tx.subpixel_to_wgs84(
                        (chunk.col_offset as usize) + c,
                        row_idx,
                        *sp,
                    ) {
                        Ok(coords) => coords,
                        Err(_) => continue,
                    };

                    if !is_point_in_bbox(lon, lat, bbox) {
                        continue;
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
