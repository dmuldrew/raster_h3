use h3o::Resolution;
use nohash_hasher::IntMap;
use rayon::prelude::*;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::coherence::SpatialCoherenceCache;
use crate::aggregator::horizon_streamer::{is_chunk_all_nodata, AggregationConfig};
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

const WGS84_A: f64 = 6378137.0;
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

pub type H3HashMap = IntMap<u64, H3Accumulator>;

/// Helper to iterate through a typed native slice and aggregate into an H3 map using
/// Row-Constant Latitude Hoisting, Linear Longitude Stepping, and In-Register Run Accumulation
#[inline(always)]
fn aggregate_native_slice_hoisted<T, F, N>(
    slice: &[T],
    chunk: &RasterChunk,
    chunk_stride: u32,
    gt: &GeoTransform,
    transformer: &CrsTransformer,
    resolution: Resolution,
    nodata: Option<f64>,
    config_bbox: Option<[f64; 4]>,
    map: &mut H3HashMap,
    to_f64: F,
    native_nodata: Option<N>,
) where
    T: Copy + PartialEq,
    F: Fn(T) -> f64,
    N: Copy + PartialEq<T>,
{
    // 1. Fast NoData early-exit
    if is_chunk_all_nodata(slice, nodata, &to_f64) {
        return;
    }

    let is_wgs84 = matches!(transformer, CrsTransformer::Wgs84Identity);
    let is_web_mercator = matches!(transformer, CrsTransformer::WebMercatorFast);
    let d_lon_step = if is_wgs84 {
        gt.a
    } else if is_web_mercator {
        (gt.a / WGS84_A) * RAD_TO_DEG
    } else {
        0.0
    };

    // 2. Scanline processing with hoisted latitude and linear longitude stepping
    for r in 0..chunk.height {
        let row_idx = (chunk.row_offset + r) as usize;
        let slice_row_start = (r * chunk_stride) as usize;
        let mut row_cache = SpatialCoherenceCache::default();

        let mut run_cell: u64 = 0;
        let mut run_acc = H3Accumulator::default();

        // Hoist latitude and starting longitude for the entire row
        let (x_start, y_row) = gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);

        let (mut lon_curr, lat_row) = if is_wgs84 {
            (x_start, y_row)
        } else if is_web_mercator {
            let lat = (2.0 * (y_row / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG;
            let lon = (x_start / WGS84_A) * RAD_TO_DEG;
            (lon, lat)
        } else {
            match transformer.transform_point(x_start, y_row) {
                Ok(coords) => coords,
                Err(_) => (x_start, y_row),
            }
        };

        let mut c = 0;
        while c < chunk.width as usize {
            let (lon, lat) = if is_wgs84 || is_web_mercator {
                (lon_curr, lat_row)
            } else {
                let (x, y) = gt.pixel_center_to_coord((chunk.col_offset as usize) + c, row_idx);
                match transformer.transform_point(x, y) {
                    Ok(coords) => coords,
                    Err(_) => {
                        c += 1;
                        if is_wgs84 || is_web_mercator {
                            lon_curr += d_lon_step;
                        }
                        continue;
                    }
                }
            };

            // Filter point against bounding box if specified
            if let Some([b_min_lon, b_min_lat, b_max_lon, b_max_lat]) = config_bbox {
                if lon < b_min_lon || lon > b_max_lon || lat < b_min_lat || lat > b_max_lat {
                    c += 1;
                    if is_wgs84 || is_web_mercator {
                        lon_curr += d_lon_step;
                    }
                    continue;
                }
            }

            // Fast spatial coherence H3 lookup
            if let Some(cell_u64) = row_cache.get_or_compute(lat, lon, resolution) {
                if cell_u64 != run_cell {
                    if run_cell != 0 && run_acc.count > 0 {
                        map.entry(run_cell)
                            .and_modify(|acc| acc.merge(&run_acc))
                            .or_insert(run_acc);
                    }
                    run_cell = cell_u64;
                    run_acc = H3Accumulator::default();
                }

                // Compute safe span length guaranteed to remain in this cell
                let safe_span = if is_wgs84 || is_web_mercator {
                    row_cache.safe_span_length(lon_curr, d_lon_step)
                } else {
                    1
                };

                let span_end = (c + safe_span).min(chunk.width as usize);

                // Tight slice vector loop (auto-vectorizes with AVX2 / ARM NEON)
                for i in c..span_end {
                    let val_raw = slice[slice_row_start + i];

                    if let Some(nd_nat) = native_nodata {
                        if nd_nat == val_raw {
                            continue;
                        }
                    }

                    let val = to_f64(val_raw);
                    if !val.is_finite() {
                        continue;
                    }
                    if let Some(nd) = nodata {
                        if (val - nd).abs() < 1e-6 {
                            continue;
                        }
                    }

                    run_acc.update(val);
                }

                let num_stepped = span_end - c;
                if is_wgs84 || is_web_mercator {
                    lon_curr += (num_stepped as f64) * d_lon_step;
                }
                c = span_end;
            } else {
                c += 1;
                if is_wgs84 || is_web_mercator {
                    lon_curr += d_lon_step;
                }
            }
        }

        // Flush remaining run at end of row
        if run_cell != 0 && run_acc.count > 0 {
            map.entry(run_cell)
                .and_modify(|acc| acc.merge(&run_acc))
                .or_insert(run_acc);
        }
    }
}

/// Stream and aggregate raster pixels on-demand with spatial coherence caching, latitude hoisting, and Rayon work-stealing
pub fn aggregate_raster_stream(
    reader: &GeoTiffStreamReader,
    config: &AggregationConfig,
) -> Result<H3HashMap> {
    let resolution = Resolution::try_from(config.resolution)
        .map_err(|_| RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", config.resolution)))?;

    let crs_transformer = CrsTransformer::from_crs_or_epsg(
        reader.metadata.epsg,
        config.custom_crs.as_deref().or(reader.metadata.proj_string.as_deref()),
    )?;

    let nodata_val = config.custom_nodata.or(reader.metadata.nodata);
    let bbox = config.bbox;
    let gt = reader.metadata.geotransform;
    let total_chunks = reader.chunk_layout.total_chunks;
    let chunk_stride = reader.chunk_layout.chunk_width;

    let chunk_indices: Vec<u32> = (0..total_chunks)
        .filter(|&idx| {
            if let Some(ref b) = bbox {
                let chunk_bounds = reader.chunk_layout.get_chunk_bounds(
                    idx,
                    reader.metadata.width,
                    reader.metadata.height,
                );
                crate::aggregator::horizon_streamer::chunk_intersects_bbox(&chunk_bounds, &gt, &crs_transformer, b)
            } else {
                true
            }
        })
        .collect();

    let aggregated_map: H3HashMap = chunk_indices
        .into_par_iter()
        .map(|chunk_idx| {
            let mut local_map = H3HashMap::default();

            if let Ok((chunk_bounds, decoding_result)) = reader.read_chunk(chunk_idx) {
                match decoding_result {
                    DecodingResult::U8(slice) => {
                        let nd = nodata_val.and_then(|v| if (0.0..=255.0).contains(&v) { Some(v as u8) } else { None });
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::U16(slice) => {
                        let nd = nodata_val.and_then(|v| if (0.0..=65535.0).contains(&v) { Some(v as u16) } else { None });
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::U32(slice) => {
                        let nd = nodata_val.and_then(|v| if v >= 0.0 && v <= u32::MAX as f64 { Some(v as u32) } else { None });
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::U64(slice) => {
                        let nd = nodata_val.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None });
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::I8(slice) => {
                        let nd = nodata_val.and_then(|v| if (-128.0..=127.0).contains(&v) { Some(v as i8) } else { None });
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::I16(slice) => {
                        let nd = nodata_val.and_then(|v| if (-32768.0..=32767.0).contains(&v) { Some(v as i16) } else { None });
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::I32(slice) => {
                        let nd = nodata_val.and_then(|v| if v >= i32::MIN as f64 && v <= i32::MAX as f64 { Some(v as i32) } else { None });
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::I64(slice) => {
                        let nd = nodata_val.map(|v| v as i64);
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::F32(slice) => {
                        let nd = nodata_val.map(|v| v as f32);
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x as f64, nd);
                    }
                    DecodingResult::F64(slice) => {
                        let nd = nodata_val;
                        aggregate_native_slice_hoisted(&slice, &chunk_bounds, chunk_stride, &gt, &crs_transformer, resolution, nodata_val, bbox, &mut local_map, |x| x, nd);
                    }
                }
            }

            local_map
        })
        .reduce(H3HashMap::default, |mut map_a, map_b| {
            if map_a.len() < map_b.len() {
                let mut merged = map_b;
                for (k, v) in map_a {
                    merged
                        .entry(k)
                        .and_modify(|acc| acc.merge(&v))
                        .or_insert(v);
                }
                merged
            } else {
                for (k, v) in map_b {
                    map_a
                        .entry(k)
                        .and_modify(|acc| acc.merge(&v))
                        .or_insert(v);
                }
                map_a
            }
        });

    Ok(aggregated_map)
}
