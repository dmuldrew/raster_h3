//! Multi-Resolution H3 to PMTiles v3 Tiling Engine
//!
//! Orchestrates the streaming aggregation of GeoTIFF rasters across multiple H3 resolutions
//! and packages the resulting vector hexagons directly into a single PMTiles v3 archive.

use std::borrow::Cow;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;
use fxhash::FxBuildHasher;
use h3o::{CellIndex, LatLng, Resolution};
use rayon::prelude::*;
use serde_json::json;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::multi_horizon::{MultiContinuousRecord, MultiScanHorizonStreamer, MultiCategoricalHorizonStreamer, MultiResolutionConfig};
use crate::pmtiles::mvt::{
    FeatureProperties, MercatorPoint, MvtFeature, MvtLayer, MvtValue, PropertyFilter,
    PROP_CAT_COUNT, PROP_CAT_DISTINCT_CLASSES, PROP_CAT_ENTROPY, PROP_CAT_H3_HEX,
    PROP_CAT_H3_INDEX, PROP_CAT_MAJORITY, PROP_CAT_MAJORITY_FRACTION, PROP_CAT_RESOLUTION,
    PROP_COUNT, PROP_H3_HEX, PROP_H3_INDEX, PROP_MAX, PROP_MEAN, PROP_MIN, PROP_RESOLUTION,
    PROP_STDDEV, PROP_SUM,
};
use crate::pmtiles::writer::PmtilesWriter;
use crate::raster::geotiff::GeoTiffStreamReader;

/// Convert WGS84 (lon, lat) to Web Mercator tile coordinates (x, y) at zoom z
pub fn lon_lat_to_tile_xy(lon: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = (1u32 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor().max(0.0).min(n - 1.0) as u32;

    let lat_clamped = lat.max(-85.05112878).min(85.05112878);
    let lat_rad = lat_clamped.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor()
        .max(0.0)
        .min(n - 1.0) as u32;

    (x, y)
}

/// Convert a normalized MercatorPoint to tile coordinates (x, y) at zoom z using cheap bitshifts
#[inline(always)]
pub fn mercator_to_tile_xy(pt: MercatorPoint, z: u8) -> (u32, u32) {
    let n = (1u32 << z) as f64;
    let tx = (pt.x * n).floor().max(0.0).min(n - 1.0) as u32;
    let ty = (pt.y * n).floor().max(0.0).min(n - 1.0) as u32;
    (tx, ty)
}

/// Compute WGS84 bounding box [min_lon, min_lat, max_lon, max_lat] for tile (z, x, y)
pub fn tile_xy_to_bbox(z: u8, x: u32, y: u32) -> [f64; 4] {
    let n = (1u32 << z) as f64;
    let min_lon = (x as f64) / n * 360.0 - 180.0;
    let max_lon = ((x + 1) as f64) / n * 360.0 - 180.0;

    let lat_rad_max = ((std::f64::consts::PI * (1.0 - 2.0 * (y as f64) / n)).sinh()).atan();
    let lat_rad_min = ((std::f64::consts::PI * (1.0 - 2.0 * ((y + 1) as f64) / n)).sinh()).atan();

    let max_lat = lat_rad_max.to_degrees();
    let min_lat = lat_rad_min.to_degrees();

    [min_lon, min_lat, max_lon, max_lat]
}

/// Determine the range of tile coordinates (min_tx..=max_tx, min_ty..=max_ty)
/// intersected by an H3 cell's boundary vertices and center at zoom level z.
/// Ensures boundary-spanning hexagons are emitted into all overlapping tiles.
pub fn cell_tile_range(center: LatLng, vertices: &[LatLng], z: u8) -> (u32, u32, u32, u32) {
    let (mut min_tx, mut min_ty) = lon_lat_to_tile_xy(center.lng(), center.lat(), z);
    let mut max_tx = min_tx;
    let mut max_ty = min_ty;

    for v in vertices {
        let (tx, ty) = lon_lat_to_tile_xy(v.lng(), v.lat(), z);
        min_tx = min_tx.min(tx);
        max_tx = max_tx.max(tx);
        min_ty = min_ty.min(ty);
        max_ty = max_ty.max(ty);
    }

    (min_tx, max_tx, min_ty, max_ty)
}

/// Determine the tile coordinate range for an H3 cell with precalculated normalized Mercator points
#[inline(always)]
pub fn cell_tile_range_mercator(center: MercatorPoint, vertices: &[MercatorPoint], z: u8) -> (u32, u32, u32, u32) {
    let (mut min_tx, mut min_ty) = mercator_to_tile_xy(center, z);
    let mut max_tx = min_tx;
    let mut max_ty = min_ty;

    for &v in vertices {
        let (tx, ty) = mercator_to_tile_xy(v, z);
        min_tx = min_tx.min(tx);
        max_tx = max_tx.max(tx);
        min_ty = min_ty.min(ty);
        max_ty = max_ty.max(ty);
    }

    (min_tx, max_tx, min_ty, max_ty)
}

/// Compute boundary vertices for an H3 cell directly into a stack-allocated array (zero heap allocations)
#[inline(always)]
pub fn cell_boundary_mercator(cell: CellIndex) -> ([MercatorPoint; 8], usize) {
    let mut arr = [MercatorPoint { x: 0.0, y: 0.0 }; 8];
    let mut count = 0;
    for v in cell.boundary().iter() {
        if count < 8 {
            arr[count] = MercatorPoint::from_lat_lng(v.lat(), v.lng());
            count += 1;
        }
    }
    (arr, count)
}

/// Default mapping from H3 resolution to Web Mercator zoom level (scaling ratio ~1.4037)
pub fn h3_res_to_zoom(res: u8) -> u8 {
    match res {
        0 => 0,
        1 => 2,
        2 => 4,
        3 => 5,
        4 => 7,
        5 => 8,
        6 => 10,
        7 => 11,
        8 => 13,
        9 => 14,
        10 => 16,
        11 => 17,
        12 => 19,
        13 => 20,
        14 => 21,
        15 => 23,
        _ => 24,
    }
}

/// Determine the optimal H3 resolution for a given Web Mercator zoom level
pub fn h3_res_for_zoom(zoom: u8) -> u8 {
    match zoom {
        0 | 1 => 0,
        2 | 3 => 1,
        4 => 2,
        5 => 3,
        6 | 7 => 4,
        8 | 9 => 5,
        10 => 6,
        11 | 12 => 7,
        13 => 8,
        14 => 9,
        15 | 16 => 10,
        17 => 11,
        18 | 19 => 12,
        20 => 13,
        21 | 22 => 14,
        _ => 15,
    }
}

/// Map an H3 resolution to the continuous range of Web Mercator zoom levels it covers
pub fn zooms_for_h3_res(res: u8, min_res: u8) -> Vec<u8> {
    let standard_zooms: Vec<u8> = match res {
        0 => vec![0, 1],
        1 => vec![2, 3],
        2 => vec![4],
        3 => vec![5],
        4 => vec![6, 7],
        5 => vec![8, 9],
        6 => vec![10],
        7 => vec![11, 12],
        8 => vec![13],
        9 => vec![14],
        10 => vec![15, 16],
        11 => vec![17],
        12 => vec![18, 19],
        13 => vec![20],
        14 => vec![21, 22],
        _ => vec![23, 24],
    };

    if res == min_res {
        let max_z = standard_zooms.iter().copied().max().unwrap_or(0);
        (0..=max_z).collect()
    } else {
        standard_zooms
    }
}

/// Estimate maximum radius (in WGS84 degrees) of an H3 hexagon at resolution res
pub fn max_hex_radius_deg(res: u8) -> f64 {
    match res {
        0 => 12.0,
        1 => 4.5,
        2 => 1.7,
        3 => 0.65,
        4 => 0.25,
        5 => 0.10,
        6 => 0.04,
        7 => 0.015,
        8 => 0.006,
        9 => 0.0025,
        10 => 0.001,
        11 => 0.0004,
        12 => 0.00015,
        13 => 0.00006,
        14 => 0.000025,
        _ => 0.00001,
    }
}

/// Priority queue entry for tile eviction ordered by southernmost latitude
#[derive(Clone, Copy, PartialEq)]
struct TileEvictionEntry {
    safe_evict_lat: f64,
    tile_key: (u8, u32, u32),
}

impl Eq for TileEvictionEntry {}

impl Ord for TileEvictionEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.safe_evict_lat
            .partial_cmp(&other.safe_evict_lat)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for TileEvictionEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Intermediate prepared tile operation generated across Rayon worker threads
struct PreparedTileOp {
    tile_key: (u8, u32, u32),
    feature: MvtFeature,
    is_parent: bool,
}

/// Precomputed continuous hexagon ready for thread-safe insertion into tile buckets
struct PreparedContinuousHex {
    resolution: u8,
    accumulator: H3Accumulator,
    c_lat: f64,
    c_lon: f64,
    ops: Vec<PreparedTileOp>,
}

/// Precomputed categorical hexagon ready for thread-safe insertion into tile buckets
struct PreparedCategoricalHex {
    resolution: u8,
    majority_fraction: f64,
    entropy: f64,
    distinct_classes: usize,
    pixel_count: f64,
    accumulator: CategoricalAccumulator,
    c_lat: f64,
    c_lon: f64,
    ops: Vec<PreparedTileOp>,
}

/// Per-resolution statistics accumulator for multi-resolution pyramids
#[derive(Debug, Clone)]
pub struct ResolutionAccumulatorStats {
    pub cell_count: usize,
    pub min_mean: f64,
    pub max_mean: f64,
    pub total_mean: f64,
    pub min_sum: f64,
    pub max_sum: f64,
    pub total_sum: f64,
    pub min_max: f64,
    pub max_max: f64,
    pub min_min: f64,
    pub max_min: f64,
    pub min_count: f64,
    pub max_count: f64,
    pub total_pixel_count: f64,
    pub min_stddev: f64,
    pub max_stddev: f64,
    pub total_stddev: f64,
}

impl Default for ResolutionAccumulatorStats {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolutionAccumulatorStats {
    pub fn new() -> Self {
        Self {
            cell_count: 0,
            min_mean: f64::INFINITY,
            max_mean: f64::NEG_INFINITY,
            total_mean: 0.0,
            min_sum: f64::INFINITY,
            max_sum: f64::NEG_INFINITY,
            total_sum: 0.0,
            min_max: f64::INFINITY,
            max_max: f64::NEG_INFINITY,
            min_min: f64::INFINITY,
            max_min: f64::NEG_INFINITY,
            min_count: f64::INFINITY,
            max_count: f64::NEG_INFINITY,
            total_pixel_count: 0.0,
            min_stddev: f64::INFINITY,
            max_stddev: f64::NEG_INFINITY,
            total_stddev: 0.0,
        }
    }

    pub fn record(&mut self, acc: &crate::aggregator::accumulator::H3Accumulator) {
        self.cell_count += 1;
        let mean = acc.mean();
        let sum = acc.sum;
        let max = acc.max;
        let min = acc.min;
        let count = acc.count;
        let stddev = acc.stddev();

        if !mean.is_nan() {
            if mean < self.min_mean { self.min_mean = mean; }
            if mean > self.max_mean { self.max_mean = mean; }
            self.total_mean += mean;
        }

        if !sum.is_nan() {
            if sum < self.min_sum { self.min_sum = sum; }
            if sum > self.max_sum { self.max_sum = sum; }
            self.total_sum += sum;
        }

        if !max.is_nan() {
            if max < self.min_max { self.min_max = max; }
            if max > self.max_max { self.max_max = max; }
        }

        if !min.is_nan() {
            if min < self.min_min { self.min_min = min; }
            if min > self.max_min { self.max_min = min; }
        }

        if !count.is_nan() {
            if count < self.min_count { self.min_count = count; }
            if count > self.max_count { self.max_count = count; }
            self.total_pixel_count += count;
        }

        if !stddev.is_nan() {
            if stddev < self.min_stddev { self.min_stddev = stddev; }
            if stddev > self.max_stddev { self.max_stddev = stddev; }
            self.total_stddev += stddev;
        }
    }

    pub fn to_json(&self, zooms: &[u8]) -> serde_json::Value {
        let n = self.cell_count.max(1) as f64;
        json!({
            "cell_count": self.cell_count,
            "zooms": zooms,
            "mean": {
                "min": if self.min_mean.is_infinite() { 0.0 } else { self.min_mean },
                "max": if self.max_mean.is_infinite() { 0.0 } else { self.max_mean },
                "avg": self.total_mean / n
            },
            "sum": {
                "min": if self.min_sum.is_infinite() { 0.0 } else { self.min_sum },
                "max": if self.max_sum.is_infinite() { 0.0 } else { self.max_sum },
                "avg": self.total_sum / n
            },
            "max": {
                "min": if self.min_max.is_infinite() { 0.0 } else { self.min_max },
                "max": if self.max_max.is_infinite() { 0.0 } else { self.max_max }
            },
            "min": {
                "min": if self.min_min.is_infinite() { 0.0 } else { self.min_min },
                "max": if self.max_min.is_infinite() { 0.0 } else { self.max_min }
            },
            "count": {
                "min": if self.min_count.is_infinite() { 0.0 } else { self.min_count },
                "max": if self.max_count.is_infinite() { 0.0 } else { self.max_count },
                "avg": self.total_pixel_count / n
            },
            "stddev": {
                "min": if self.min_stddev.is_infinite() { 0.0 } else { self.min_stddev },
                "max": if self.max_stddev.is_infinite() { 0.0 } else { self.max_stddev },
                "avg": self.total_stddev / n
            }
        })
    }
}

/// A generic H3 feature record with arbitrary properties for PMTiles export
#[derive(Debug, Clone)]
pub struct H3Feature {
    pub h3_index: u64,
    pub properties: Vec<(Cow<'static, str>, MvtValue)>,
}

impl H3Feature {
    pub fn new<K: Into<Cow<'static, str>>>(h3_index: u64, properties: Vec<(K, MvtValue)>) -> Self {
        Self {
            h3_index,
            properties: properties.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }
}

/// Result summary of PMTiles export
#[derive(Debug, Clone)]
pub struct PmtilesExportSummary {
    pub total_features: usize,
    pub valid_features: usize,
    pub invalid_features_dropped: usize,
    pub total_tiles: usize,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

/// High-level builder to convert H3 data and GeoTIFF raster aggregations directly to PMTiles v3
pub struct H3PmtilesTiler;

impl H3PmtilesTiler {
    /// Export any collection of generic H3 features (with strict H3 validation) to a PMTiles v3 archive
    pub fn export_h3_features<P: AsRef<Path>, I: IntoIterator<Item = H3Feature>>(
        features: I,
        output_path: P,
    ) -> Result<PmtilesExportSummary, Box<dyn std::error::Error + Send + Sync>> {
        let mut tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());

        let mut total_features = 0usize;
        let mut valid_features = 0usize;
        let mut invalid_dropped = 0usize;

        let mut global_min_lon = 180.0f64;
        let mut global_min_lat = 90.0f64;
        let mut global_max_lon = -180.0f64;
        let mut global_max_lat = -90.0f64;

        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;

        for feat in features {
            total_features += 1;

            // Strict H3 Mathematical Validation: mode, base cell range, resolution bounds, directional digits
            let cell = match CellIndex::try_from(feat.h3_index) {
                Ok(c) => c,
                Err(_) => {
                    invalid_dropped += 1;
                    continue;
                }
            };

            valid_features += 1;
            let center: LatLng = cell.into();
            let c_lat = center.lat();
            let c_lon = center.lng();

            if c_lon < global_min_lon { global_min_lon = c_lon; }
            if c_lon > global_max_lon { global_max_lon = c_lon; }
            if c_lat < global_min_lat { global_min_lat = c_lat; }
            if c_lat > global_max_lat { global_max_lat = c_lat; }

            let center_merc = MercatorPoint::from_lat_lng(c_lat, c_lon);
            let (v_merc, v_count) = cell_boundary_mercator(cell);
            let vertices_merc = &v_merc[..v_count];

            let res_u8: u8 = cell.resolution().into();
            let zoom = h3_res_to_zoom(res_u8);
            if zoom < min_zoom { min_zoom = zoom; }
            if zoom > max_zoom { max_zoom = zoom; }

            let mut properties = feat.properties;
            if !properties.iter().any(|(k, _)| k == "h3_index") {
                properties.push((Cow::Borrowed("h3_index"), MvtValue::UInt(feat.h3_index)));
            }
            if !properties.iter().any(|(k, _)| k == "h3_hex") {
                properties.push((Cow::Borrowed("h3_hex"), MvtValue::from_hex_u64(feat.h3_index)));
            }
            if !properties.iter().any(|(k, _)| k == "resolution") {
                properties.push((Cow::Borrowed("resolution"), MvtValue::UInt(res_u8 as u64)));
            }

            let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(center_merc, vertices_merc, zoom);
            if min_tx == max_tx && min_ty == max_ty {
                let tile_key = (zoom, min_tx, min_ty);
                let layer = tile_buckets.entry(tile_key).or_insert_with(|| {
                    MvtLayer::new("h3_hexagons")
                });
                layer.add_hexagon_mercator(
                    feat.h3_index,
                    vertices_merc,
                    zoom,
                    min_tx,
                    min_ty,
                    properties,
                );
            } else {
                for tx in min_tx..=max_tx {
                    for ty in min_ty..=max_ty {
                        let is_last = tx == max_tx && ty == max_ty;
                        let tile_key = (zoom, tx, ty);
                        let layer = tile_buckets.entry(tile_key).or_insert_with(|| {
                            MvtLayer::new("h3_hexagons")
                        });

                        let props = if is_last {
                            std::mem::take(&mut properties)
                        } else {
                            properties.clone()
                        };

                        layer.add_hexagon_mercator(
                            feat.h3_index,
                            vertices_merc,
                            zoom,
                            tx,
                            ty,
                            props,
                        );
                    }
                }
            }
        }

        if valid_features == 0 {
            global_min_lon = -180.0;
            global_min_lat = -85.0;
            global_max_lon = 180.0;
            global_max_lat = 85.0;
            min_zoom = 0;
            max_zoom = 0;
        }

        let metadata = json!({
            "name": "h3_pmtiles_export",
            "description": "H3 vector hexagon tile pyramid exported by raster_h3",
            "version": "3",
            "minzoom": min_zoom,
            "maxzoom": max_zoom,
            "vector_layers": [
                {
                    "id": "h3_hexagons",
                    "description": "H3 hexagonal vector polygons with attributes",
                    "minzoom": min_zoom,
                    "maxzoom": max_zoom,
                    "fields": {
                        "h3_index": "Number",
                        "h3_hex": "String",
                        "resolution": "Number"
                    }
                }
            ]
        });

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [global_min_lon, global_min_lat, global_max_lon, global_max_lat],
            metadata.to_string(),
        )?;

        let total_tiles = tile_buckets.len();
        let encoded_tiles: Vec<((u8, u32, u32), Vec<u8>)> = tile_buckets
            .into_par_iter()
            .map(|((z, x, y), layer)| {
                let pbf_bytes = layer.encode();
                let compressed = crate::pmtiles::writer::gzip_compress(&pbf_bytes).unwrap();
                ((z, x, y), compressed)
            })
            .collect();

        for ((z, x, y), compressed_bytes) in encoded_tiles {
            writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
        }

        writer.finish(output_path)?;

        Ok(PmtilesExportSummary {
            total_features,
            valid_features,
            invalid_features_dropped: invalid_dropped,
            total_tiles,
            min_zoom,
            max_zoom,
        })
    }
    /// Stream continuous raster data from GeoTIFF across target resolutions and write PMTiles v3 archive
    pub fn generate_from_continuous_streamer<P: AsRef<Path>>(
        streamer: MultiScanHorizonStreamer,
        output_path: P,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Self::generate_from_continuous_streamer_with_properties(streamer, output_path, None)
    }

    /// Stream continuous raster data from GeoTIFF across target resolutions and write PMTiles v3 archive with selective property filtering
    pub fn generate_from_continuous_streamer_with_properties<P: AsRef<Path>>(
        mut streamer: MultiScanHorizonStreamer,
        output_path: P,
        properties: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let property_filter = properties.map(PropertyFilter::parse).unwrap_or_else(PropertyFilter::all);
        let needs_stddev = property_filter.needs_stddev();

        let resolutions = streamer.resolution_u8s().to_vec();
        let min_res = resolutions.iter().copied().min().unwrap_or(0);
        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;

        for &res in &resolutions {
            let zooms = zooms_for_h3_res(res, min_res);
            for &z in &zooms {
                if z < min_zoom { min_zoom = z; }
                if z > max_zoom { max_zoom = z; }
            }
        }

        let max_cell_radius = resolutions.iter().map(|&r| max_hex_radius_deg(r)).fold(0.0f64, f64::max);
        let safety_margin = 2.5 * max_cell_radius;

        let mut tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut tile_eviction_queue: BinaryHeap<TileEvictionEntry> = BinaryHeap::new();

        let mut total_hexagons = 0usize;
        let mut global_min_lon = 180.0f64;
        let mut global_min_lat = 90.0f64;
        let mut global_max_lon = -180.0f64;
        let mut global_max_lat = -90.0f64;

        let mut res_stats: HashMap<u8, ResolutionAccumulatorStats, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [-180.0, -90.0, 180.0, 90.0],
            String::new(),
        )?;

        let mut batch = Vec::with_capacity(4096);

        loop {
            batch.clear();
            streamer.drain_completed_into(4096, |_i, record| {
                batch.push(record);
            });
            if batch.is_empty() {
                break;
            }

            let prepared_batch: Vec<PreparedContinuousHex> = batch
                .par_iter()
                .filter_map(|record| {
                    let MultiContinuousRecord {
                        resolution,
                        h3_index,
                        accumulator,
                    } = *record;

                    let cell = CellIndex::try_from(h3_index).ok()?;
                    let center: LatLng = cell.into();
                    let c_lat = center.lat();
                    let c_lon = center.lng();

                    let center_merc = MercatorPoint::from_lat_lng(c_lat, c_lon);
                    let (v_merc, v_count) = cell_boundary_mercator(cell);
                    let vertices_merc = &v_merc[..v_count];

                    let stddev = if needs_stddev { accumulator.stddev() } else { 0.0 };

                    let properties = FeatureProperties::Continuous {
                        h3_index,
                        resolution: resolution as u8,
                        mean: accumulator.mean(),
                        sum: accumulator.sum,
                        stddev,
                        count: accumulator.count,
                        min: accumulator.min,
                        max: accumulator.max,
                    };

                    let zooms = zooms_for_h3_res(resolution, min_res);
                    let num_zooms = zooms.len();
                    let mut ops = Vec::with_capacity(num_zooms);

                    for (z_idx, &zoom) in zooms.iter().enumerate() {
                        let is_last_zoom = z_idx == num_zooms - 1;
                        let optimal_res = h3_res_for_zoom(zoom);
                        if optimal_res < resolution {
                            if let Ok(res_enum) = Resolution::try_from(optimal_res) {
                                if let Some(parent_cell) = cell.parent(res_enum) {
                                    let parent_h3: u64 = parent_cell.into();
                                    let p_center: LatLng = parent_cell.into();
                                    let p_center_merc = MercatorPoint::from_lat_lng(p_center.lat(), p_center.lng());
                                    let (p_v_merc, p_v_count) = cell_boundary_mercator(parent_cell);
                                    let p_vertices_merc = &p_v_merc[..p_v_count];

                                    let parent_stddev = if needs_stddev { accumulator.stddev() } else { 0.0 };
                                    let parent_properties = FeatureProperties::Continuous {
                                        h3_index: parent_h3,
                                        resolution: optimal_res,
                                        mean: accumulator.mean(),
                                        sum: accumulator.sum,
                                        stddev: parent_stddev,
                                        count: accumulator.count,
                                        min: accumulator.min,
                                        max: accumulator.max,
                                    };

                                    let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(p_center_merc, p_vertices_merc, zoom);
                                    if min_tx == max_tx && min_ty == max_ty {
                                        let feature = MvtFeature::from_mercator(
                                            parent_h3,
                                            p_vertices_merc,
                                            zoom,
                                            min_tx,
                                            min_ty,
                                            4096,
                                            parent_properties,
                                        );
                                        ops.push(PreparedTileOp {
                                            tile_key: (zoom, min_tx, min_ty),
                                            feature,
                                            is_parent: true,
                                        });
                                    } else {
                                        for tx in min_tx..=max_tx {
                                            for ty in min_ty..=max_ty {
                                                let feature = MvtFeature::from_mercator(
                                                    parent_h3,
                                                    p_vertices_merc,
                                                    zoom,
                                                    tx,
                                                    ty,
                                                    4096,
                                                    parent_properties.clone(),
                                                );
                                                ops.push(PreparedTileOp {
                                                    tile_key: (zoom, tx, ty),
                                                    feature,
                                                    is_parent: true,
                                                });
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }
                        }

                        let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(center_merc, vertices_merc, zoom);
                        if min_tx == max_tx && min_ty == max_ty {
                            let feature = MvtFeature::from_mercator(
                                h3_index,
                                vertices_merc,
                                zoom,
                                min_tx,
                                min_ty,
                                4096,
                                if is_last_zoom { properties.clone() } else { properties.clone() },
                            );
                            ops.push(PreparedTileOp {
                                tile_key: (zoom, min_tx, min_ty),
                                feature,
                                is_parent: false,
                            });
                        } else {
                            for tx in min_tx..=max_tx {
                                for ty in min_ty..=max_ty {
                                    let feature = MvtFeature::from_mercator(
                                        h3_index,
                                        vertices_merc,
                                        zoom,
                                        tx,
                                        ty,
                                        4096,
                                        properties.clone(),
                                    );
                                    ops.push(PreparedTileOp {
                                        tile_key: (zoom, tx, ty),
                                        feature,
                                        is_parent: false,
                                    });
                                }
                            }
                        }
                    }

                    Some(PreparedContinuousHex {
                        resolution,
                        accumulator,
                        c_lat,
                        c_lon,
                        ops,
                    })
                })
                .collect();

            for hex in prepared_batch {
                res_stats
                    .entry(hex.resolution)
                    .or_insert_with(ResolutionAccumulatorStats::new)
                    .record(&hex.accumulator);

                if hex.c_lon < global_min_lon { global_min_lon = hex.c_lon; }
                if hex.c_lon > global_max_lon { global_max_lon = hex.c_lon; }
                if hex.c_lat < global_min_lat { global_min_lat = hex.c_lat; }
                if hex.c_lat > global_max_lat { global_max_lat = hex.c_lat; }

                for op in hex.ops {
                    let tile_key = op.tile_key;
                    let layer = if let Some(l) = tile_buckets.get_mut(&tile_key) {
                        l
                    } else {
                        let bbox = tile_xy_to_bbox(tile_key.0, tile_key.1, tile_key.2);
                        let safe_evict_lat = bbox[1] - safety_margin;
                        tile_eviction_queue.push(TileEvictionEntry {
                            safe_evict_lat,
                            tile_key,
                        });
                        tile_buckets.entry(tile_key).or_insert_with(|| {
                            MvtLayer::with_filter("h3_hexagons", property_filter.clone())
                        })
                    };

                    if op.is_parent {
                        layer.add_or_merge_feature(op.feature);
                    } else {
                        layer.features.push(op.feature);
                    }
                }

                total_hexagons += 1;
            }

            // Evict completed tiles whose southernmost reach is north of current scanline horizon
            let lat_horizon = streamer.current_lat_horizon();
            let mut ready_tiles = Vec::new();
            while let Some(top) = tile_eviction_queue.peek() {
                if top.safe_evict_lat > lat_horizon {
                    let entry = tile_eviction_queue.pop().unwrap();
                    if let Some(layer) = tile_buckets.remove(&entry.tile_key) {
                        ready_tiles.push((entry.tile_key, layer));
                    }
                } else {
                    break;
                }
            }

            if !ready_tiles.is_empty() {
                let compressed_batch: Vec<((u8, u32, u32), Vec<u8>)> = ready_tiles
                    .into_par_iter()
                    .map(|(key, layer)| {
                        let pbf_bytes = layer.encode();
                        let compressed = crate::pmtiles::writer::gzip_compress(&pbf_bytes).unwrap();
                        (key, compressed)
                    })
                    .collect();

                for ((z, x, y), compressed_bytes) in compressed_batch {
                    writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
                }
            }
        }

        // Flush any remaining active tiles at raster completion in parallel
        let remaining_tiles: Vec<((u8, u32, u32), MvtLayer)> = tile_buckets.into_iter().collect();
        let compressed_batch: Vec<((u8, u32, u32), Vec<u8>)> = remaining_tiles
            .into_par_iter()
            .map(|(key, layer)| {
                let pbf_bytes = layer.encode();
                let compressed = crate::pmtiles::writer::gzip_compress(&pbf_bytes).unwrap();
                (key, compressed)
            })
            .collect();

        for ((z, x, y), compressed_bytes) in compressed_batch {
            writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
        }

        if total_hexagons == 0 {
            global_min_lon = -180.0;
            global_min_lat = -90.0;
            global_max_lon = 180.0;
            global_max_lat = 90.0;
            min_zoom = 0;
            max_zoom = 0;
        }

        let mut sorted_res: Vec<u8> = res_stats.keys().copied().collect();
        sorted_res.sort_unstable();
        let min_res_val = sorted_res.first().copied().unwrap_or(0);

        let mut res_stats_json = serde_json::Map::new();
        for res in sorted_res {
            if let Some(st) = res_stats.get(&res) {
                let zooms = zooms_for_h3_res(res, min_res_val);
                res_stats_json.insert(res.to_string(), st.to_json(&zooms));
            }
        }

        let fields_json = if property_filter.is_custom {
            let mut m = serde_json::Map::new();
            if property_filter.has_continuous(PROP_H3_INDEX) { m.insert("h3_index".to_string(), json!("Number")); }
            if property_filter.has_continuous(PROP_H3_HEX) { m.insert("h3_hex".to_string(), json!("String")); }
            if property_filter.has_continuous(PROP_RESOLUTION) { m.insert("resolution".to_string(), json!("Number")); }
            if property_filter.has_continuous(PROP_MEAN) { m.insert("mean".to_string(), json!("Number")); }
            if property_filter.has_continuous(PROP_SUM) { m.insert("sum".to_string(), json!("Number")); }
            if property_filter.has_continuous(PROP_STDDEV) { m.insert("stddev".to_string(), json!("Number")); }
            if property_filter.has_continuous(PROP_COUNT) { m.insert("count".to_string(), json!("Number")); }
            if property_filter.has_continuous(PROP_MIN) { m.insert("min".to_string(), json!("Number")); }
            if property_filter.has_continuous(PROP_MAX) { m.insert("max".to_string(), json!("Number")); }
            serde_json::Value::Object(m)
        } else {
            json!({
                "h3_index": "Number",
                "h3_hex": "String",
                "resolution": "Number",
                "mean": "Number",
                "sum": "Number",
                "stddev": "Number",
                "count": "Number",
                "min": "Number",
                "max": "Number"
            })
        };

        let metadata = json!({
            "name": "raster_h3_pmtiles",
            "description": "Multi-resolution H3 hexagonal vector tile pyramid generated by raster_h3",
            "version": "3",
            "minzoom": min_zoom,
            "maxzoom": max_zoom,
            "h3_resolution_stats": res_stats_json,
            "vector_layers": [
                {
                    "id": "h3_hexagons",
                    "description": "Aggregated H3 hexagonal grid cells",
                    "minzoom": min_zoom,
                    "maxzoom": max_zoom,
                    "fields": fields_json
                }
            ]
        });

        writer.set_metadata(
            [global_min_lon, global_min_lat, global_max_lon, global_max_lat],
            metadata.to_string(),
        );

        writer.finish(output_path)?;
        Ok(total_hexagons)
    }

    /// Stream categorical raster data from GeoTIFF across target resolutions and write PMTiles v3 archive
    pub fn generate_from_categorical_streamer<P: AsRef<Path>>(
        streamer: MultiCategoricalHorizonStreamer,
        output_path: P,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Self::generate_from_categorical_streamer_with_properties(streamer, output_path, None)
    }

    /// Stream categorical raster data from GeoTIFF across target resolutions and write PMTiles v3 archive with selective property filtering
    pub fn generate_from_categorical_streamer_with_properties<P: AsRef<Path>>(
        mut streamer: MultiCategoricalHorizonStreamer,
        output_path: P,
        properties: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let property_filter = properties.map(PropertyFilter::parse).unwrap_or_else(PropertyFilter::all);
        let needs_entropy = property_filter.needs_entropy();

        let resolutions = streamer.resolution_u8s().to_vec();
        let min_res = resolutions.iter().copied().min().unwrap_or(0);

        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;

        for &res in &resolutions {
            let zooms = zooms_for_h3_res(res, min_res);
            for &z in &zooms {
                if z < min_zoom { min_zoom = z; }
                if z > max_zoom { max_zoom = z; }
            }
        }

        let max_cell_radius = resolutions.iter().map(|&r| max_hex_radius_deg(r)).fold(0.0f64, f64::max);
        let safety_margin = 2.5 * max_cell_radius;

        let mut tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut tile_eviction_queue: BinaryHeap<TileEvictionEntry> = BinaryHeap::new();

        let mut total_hexagons = 0usize;
        let mut global_min_lon = 180.0f64;
        let mut global_min_lat = 90.0f64;
        let mut global_max_lon = -180.0f64;
        let mut global_max_lat = -90.0f64;

        let mut res_cell_counts: HashMap<u8, usize, FxBuildHasher> = HashMap::with_hasher(FxBuildHasher::default());
        let mut res_purity_sums: HashMap<u8, f64, FxBuildHasher> = HashMap::with_hasher(FxBuildHasher::default());
        let mut res_class_counts: HashMap<u8, HashMap<i64, u64, FxBuildHasher>, FxBuildHasher> = HashMap::with_hasher(FxBuildHasher::default());
        let mut res_entropy_sums: HashMap<u8, f64, FxBuildHasher> = HashMap::with_hasher(FxBuildHasher::default());
        let mut res_distinct_sums: HashMap<u8, f64, FxBuildHasher> = HashMap::with_hasher(FxBuildHasher::default());
        let mut res_pixel_sums: HashMap<u8, f64, FxBuildHasher> = HashMap::with_hasher(FxBuildHasher::default());

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [-180.0, -90.0, 180.0, 90.0],
            String::new(),
        )?;

        let mut batch = Vec::with_capacity(4096);

        loop {
            batch.clear();
            streamer.drain_completed_into(4096, |_i, record| {
                batch.push(record);
            });
            if batch.is_empty() {
                break;
            }
            let prepared_batch: Vec<PreparedCategoricalHex> = batch
                .par_iter()
                .filter_map(|record| {
                    let resolution = record.resolution;
                    let h3_index = record.h3_index;
                    let accumulator = &record.accumulator;

                    let (majority_class, _maj_count, majority_fraction) = accumulator.majority();
                    let entropy = if needs_entropy { accumulator.shannon_entropy() } else { 0.0 };
                    let distinct_classes = accumulator.unique_classes();
                    let pixel_count = accumulator.total_count;

                    let cell = CellIndex::try_from(h3_index).ok()?;
                    let center: LatLng = cell.into();
                    let c_lat = center.lat();
                    let c_lon = center.lng();

                    let center_merc = MercatorPoint::from_lat_lng(c_lat, c_lon);
                    let (v_merc, v_count) = cell_boundary_mercator(cell);
                    let vertices_merc = &v_merc[..v_count];

                    let properties = FeatureProperties::Categorical {
                        h3_index,
                        resolution: resolution as u8,
                        majority: majority_class,
                        majority_fraction,
                        distinct_classes: distinct_classes as u32,
                        entropy,
                        count: pixel_count,
                    };

                    let zooms = zooms_for_h3_res(resolution, min_res);
                    let num_zooms = zooms.len();
                    let mut ops = Vec::with_capacity(num_zooms);

                    for (z_idx, &zoom) in zooms.iter().enumerate() {
                        let is_last_zoom = z_idx == num_zooms - 1;
                        let optimal_res = h3_res_for_zoom(zoom);
                        if optimal_res < resolution {
                            if let Ok(res_enum) = Resolution::try_from(optimal_res) {
                                if let Some(parent_cell) = cell.parent(res_enum) {
                                    let parent_h3: u64 = parent_cell.into();
                                    let p_center: LatLng = parent_cell.into();
                                    let p_center_merc = MercatorPoint::from_lat_lng(p_center.lat(), p_center.lng());
                                    let (p_v_merc, p_v_count) = cell_boundary_mercator(parent_cell);
                                    let p_vertices_merc = &p_v_merc[..p_v_count];

                                    let parent_properties = FeatureProperties::Categorical {
                                        h3_index: parent_h3,
                                        resolution: optimal_res,
                                        majority: majority_class,
                                        majority_fraction,
                                        distinct_classes: distinct_classes as u32,
                                        entropy,
                                        count: pixel_count,
                                    };

                                    let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(p_center_merc, p_vertices_merc, zoom);
                                    if min_tx == max_tx && min_ty == max_ty {
                                        let feature = MvtFeature::from_mercator(
                                            parent_h3,
                                            p_vertices_merc,
                                            zoom,
                                            min_tx,
                                            min_ty,
                                            4096,
                                            parent_properties,
                                        );
                                        ops.push(PreparedTileOp {
                                            tile_key: (zoom, min_tx, min_ty),
                                            feature,
                                            is_parent: true,
                                        });
                                    } else {
                                        for tx in min_tx..=max_tx {
                                            for ty in min_ty..=max_ty {
                                                let feature = MvtFeature::from_mercator(
                                                    parent_h3,
                                                    p_vertices_merc,
                                                    zoom,
                                                    tx,
                                                    ty,
                                                    4096,
                                                    parent_properties.clone(),
                                                );
                                                ops.push(PreparedTileOp {
                                                    tile_key: (zoom, tx, ty),
                                                    feature,
                                                    is_parent: true,
                                                });
                                            }
                                        }
                                    }
                                    continue;
                                }
                            }
                        }

                        let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(center_merc, vertices_merc, zoom);
                        if min_tx == max_tx && min_ty == max_ty {
                            let feature = MvtFeature::from_mercator(
                                h3_index,
                                vertices_merc,
                                zoom,
                                min_tx,
                                min_ty,
                                4096,
                                if is_last_zoom { properties.clone() } else { properties.clone() },
                            );
                            ops.push(PreparedTileOp {
                                tile_key: (zoom, min_tx, min_ty),
                                feature,
                                is_parent: false,
                            });
                        } else {
                            for tx in min_tx..=max_tx {
                                for ty in min_ty..=max_ty {
                                    let feature = MvtFeature::from_mercator(
                                        h3_index,
                                        vertices_merc,
                                        zoom,
                                        tx,
                                        ty,
                                        4096,
                                        properties.clone(),
                                    );
                                    ops.push(PreparedTileOp {
                                        tile_key: (zoom, tx, ty),
                                        feature,
                                        is_parent: false,
                                    });
                                }
                            }
                        }
                    }

                    Some(PreparedCategoricalHex {
                        resolution,
                        majority_fraction,
                        entropy,
                        distinct_classes,
                        pixel_count,
                        accumulator: accumulator.clone(),
                        c_lat,
                        c_lon,
                        ops,
                    })
                })
                .collect();

            for hex in prepared_batch {
                *res_cell_counts.entry(hex.resolution).or_insert(0) += 1;
                *res_purity_sums.entry(hex.resolution).or_insert(0.0) += hex.majority_fraction;
                *res_entropy_sums.entry(hex.resolution).or_insert(0.0) += hex.entropy;
                *res_distinct_sums.entry(hex.resolution).or_insert(0.0) += hex.distinct_classes as f64;
                *res_pixel_sums.entry(hex.resolution).or_insert(0.0) += hex.pixel_count;

                let class_map = res_class_counts.entry(hex.resolution).or_insert_with(|| HashMap::with_hasher(FxBuildHasher::default()));
                hex.accumulator.for_each_class(|cls, cnt| {
                    *class_map.entry(cls).or_insert(0) += cnt as u64;
                });

                if hex.c_lon < global_min_lon { global_min_lon = hex.c_lon; }
                if hex.c_lon > global_max_lon { global_max_lon = hex.c_lon; }
                if hex.c_lat < global_min_lat { global_min_lat = hex.c_lat; }
                if hex.c_lat > global_max_lat { global_max_lat = hex.c_lat; }

                for op in hex.ops {
                    let tile_key = op.tile_key;
                    let layer = if let Some(l) = tile_buckets.get_mut(&tile_key) {
                        l
                    } else {
                        let bbox = tile_xy_to_bbox(tile_key.0, tile_key.1, tile_key.2);
                        let safe_evict_lat = bbox[1] - safety_margin;
                        tile_eviction_queue.push(TileEvictionEntry {
                            safe_evict_lat,
                            tile_key,
                        });
                        tile_buckets.entry(tile_key).or_insert_with(|| {
                            MvtLayer::with_filter("h3_hexagons", property_filter.clone())
                        })
                    };

                    if op.is_parent {
                        layer.add_or_merge_feature(op.feature);
                    } else {
                        layer.features.push(op.feature);
                    }
                }

                total_hexagons += 1;
            }

            // Evict completed tiles whose southernmost reach is north of current scanline horizon
            let lat_horizon = streamer.current_lat_horizon();
            let mut ready_tiles = Vec::new();
            while let Some(top) = tile_eviction_queue.peek() {
                if top.safe_evict_lat > lat_horizon {
                    let entry = tile_eviction_queue.pop().unwrap();
                    if let Some(layer) = tile_buckets.remove(&entry.tile_key) {
                        ready_tiles.push((entry.tile_key, layer));
                    }
                } else {
                    break;
                }
            }

            if !ready_tiles.is_empty() {
                let compressed_batch: Vec<((u8, u32, u32), Vec<u8>)> = ready_tiles
                    .into_par_iter()
                    .map(|(key, layer)| {
                        let pbf_bytes = layer.encode();
                        let compressed = crate::pmtiles::writer::gzip_compress(&pbf_bytes).unwrap();
                        (key, compressed)
                    })
                    .collect();

                for ((z, x, y), compressed_bytes) in compressed_batch {
                    writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
                }
            }
        }

        // Flush any remaining active tiles at raster completion in parallel
        let remaining_tiles: Vec<((u8, u32, u32), MvtLayer)> = tile_buckets.into_iter().collect();
        let compressed_batch: Vec<((u8, u32, u32), Vec<u8>)> = remaining_tiles
            .into_par_iter()
            .map(|(key, layer)| {
                let pbf_bytes = layer.encode();
                let compressed = crate::pmtiles::writer::gzip_compress(&pbf_bytes).unwrap();
                (key, compressed)
            })
            .collect();

        for ((z, x, y), compressed_bytes) in compressed_batch {
            writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
        }

        if total_hexagons == 0 {
            global_min_lon = -180.0;
            global_min_lat = -90.0;
            global_max_lon = 180.0;
            global_max_lat = 90.0;
            min_zoom = 0;
            max_zoom = 0;
        }

        let mut sorted_res: Vec<u8> = res_cell_counts.keys().copied().collect();
        sorted_res.sort_unstable();
        let min_res_val = sorted_res.first().copied().unwrap_or(0);

        let mut res_stats_json = serde_json::Map::new();
        for res in sorted_res {
            let zooms = zooms_for_h3_res(res, min_res_val);
            let cell_count = res_cell_counts.get(&res).copied().unwrap_or(0);
            let purity_sum = res_purity_sums.get(&res).copied().unwrap_or(0.0);
            let entropy_sum = res_entropy_sums.get(&res).copied().unwrap_or(0.0);
            let distinct_sum = res_distinct_sums.get(&res).copied().unwrap_or(0.0);
            let pixel_sum = res_pixel_sums.get(&res).copied().unwrap_or(0.0);

            let avg_purity = if cell_count > 0 { purity_sum / cell_count as f64 } else { 0.0 };
            let avg_entropy = if cell_count > 0 { entropy_sum / cell_count as f64 } else { 0.0 };
            let avg_distinct = if cell_count > 0 { distinct_sum / cell_count as f64 } else { 0.0 };
            let avg_pixels = if cell_count > 0 { pixel_sum / cell_count as f64 } else { 0.0 };

            let mut class_freq_json = serde_json::Map::new();
            if let Some(cmap) = res_class_counts.get(&res) {
                let mut sorted_classes: Vec<(&i64, &u64)> = cmap.iter().collect();
                sorted_classes.sort_by(|a, b| b.1.cmp(a.1));
                for (cls, cnt) in sorted_classes.into_iter().take(256) {
                    class_freq_json.insert(cls.to_string(), json!(cnt));
                }
            }

            let stat_entry = json!({
                "zooms": zooms,
                "cell_count": cell_count,
                "avg_purity": avg_purity,
                "entropy": { "avg": avg_entropy },
                "distinct_classes": { "avg": avg_distinct },
                "count": { "avg": avg_pixels },
                "class_frequencies": class_freq_json
            });
            res_stats_json.insert(res.to_string(), stat_entry);
        }

        let fields_json = if property_filter.is_custom {
            let mut m = serde_json::Map::new();
            if property_filter.has_categorical(PROP_CAT_H3_INDEX) { m.insert("h3_index".to_string(), json!("Number")); }
            if property_filter.has_categorical(PROP_CAT_H3_HEX) { m.insert("h3_hex".to_string(), json!("String")); }
            if property_filter.has_categorical(PROP_CAT_RESOLUTION) { m.insert("resolution".to_string(), json!("Number")); }
            if property_filter.has_categorical(PROP_CAT_MAJORITY) { m.insert("majority".to_string(), json!("Number")); }
            if property_filter.has_categorical(PROP_CAT_MAJORITY_FRACTION) { m.insert("majority_fraction".to_string(), json!("Number")); }
            if property_filter.has_categorical(PROP_CAT_DISTINCT_CLASSES) { m.insert("distinct_classes".to_string(), json!("Number")); }
            if property_filter.has_categorical(PROP_CAT_ENTROPY) { m.insert("entropy".to_string(), json!("Number")); }
            if property_filter.has_categorical(PROP_CAT_COUNT) { m.insert("count".to_string(), json!("Number")); }
            serde_json::Value::Object(m)
        } else {
            json!({
                "h3_index": "Number",
                "h3_hex": "String",
                "resolution": "Number",
                "majority": "Number",
                "majority_fraction": "Number",
                "distinct_classes": "Number",
                "entropy": "Number",
                "count": "Number"
            })
        };

        // Build vector layer JSON metadata for MapLibre / Web Vector Clients
        let metadata = json!({
            "name": "raster_h3_categorical_pmtiles",
            "format": "pbf",
            "type": "overlay",
            "description": "Multi-resolution H3 categorical vector tile pyramid generated by raster_h3",
            "version": "3",
            "dataset_type": "categorical",
            "minzoom": min_zoom,
            "maxzoom": max_zoom,
            "h3_resolution_stats": res_stats_json,
            "vector_layers": [
                {
                    "id": "h3_hexagons",
                    "description": "Aggregated H3 hexagonal grid cells",
                    "minzoom": min_zoom,
                    "maxzoom": max_zoom,
                    "fields": fields_json
                }
            ]
        });

        writer.set_metadata(
            [global_min_lon, global_min_lat, global_max_lon, global_max_lat],
            metadata.to_string(),
        );

        writer.finish(output_path)?;
        Ok(total_hexagons)
    }

    /// Convenience helper to run categorical GeoTIFF-to-PMTiles pipeline in single-pass streaming mode
    pub fn process_categorical_geotiff_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        tiff_path: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let props = config.properties.clone();
        let reader = GeoTiffStreamReader::open(tiff_path)?;
        let streamer = MultiCategoricalHorizonStreamer::new(reader, &config)?;
        Self::generate_from_categorical_streamer_with_properties(streamer, pmtiles_path, props.as_deref())
    }

    /// Convenience helper to run continuous GeoTIFF-to-PMTiles pipeline in single-pass streaming mode
    pub fn process_geotiff_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        tiff_path: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let props = config.properties.clone();
        let reader = GeoTiffStreamReader::open(tiff_path)?;
        let streamer = MultiScanHorizonStreamer::new(reader, &config)?;
        Self::generate_from_continuous_streamer_with_properties(streamer, pmtiles_path, props.as_deref())
    }

    /// Convert any H3-indexed Parquet file directly into a PMTiles v3 archive
    pub fn process_parquet_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        parquet_path: P1,
        pmtiles_path: P2,
        h3_column_name: Option<&str>,
    ) -> Result<PmtilesExportSummary, Box<dyn std::error::Error + Send + Sync>> {
        use std::fs::File;
        use parquet::file::reader::{FileReader, SerializedFileReader};
        use crate::functions::fast_hex::parse_hex_u64;

        let file = File::open(parquet_path)?;
        let reader = SerializedFileReader::new(file)?;
        let schema = reader.metadata().file_metadata().schema_descr();

        // Identify H3 index column
        let mut h3_col_idx = None;
        if let Some(target) = h3_column_name {
            for (idx, field) in schema.columns().iter().enumerate() {
                if field.name().eq_ignore_ascii_case(target) {
                    h3_col_idx = Some(idx);
                    break;
                }
            }
        }

        if h3_col_idx.is_none() {
            // Auto-detect common H3 column names: h3_index, h3_hex, h3, cell, hex, or column 0
            for (idx, field) in schema.columns().iter().enumerate() {
                let name = field.name().to_ascii_lowercase();
                if name == "h3_index" || name == "h3_hex" || name == "h3" || name == "cell" || name == "hex" {
                    h3_col_idx = Some(idx);
                    break;
                }
            }
        }

        let h3_idx = h3_col_idx.unwrap_or(0);

        let mut features = Vec::new();
        for row_result in reader.into_iter() {
            let row = row_result?;
            let mut h3_val_u64 = None;

            if let Some((_, field_val)) = row.get_column_iter().nth(h3_idx) {
                match field_val {
                    parquet::record::Field::ULong(u) => h3_val_u64 = Some(*u),
                    parquet::record::Field::Long(i) => h3_val_u64 = Some(*i as u64),
                    parquet::record::Field::Str(s) => {
                        h3_val_u64 = parse_hex_u64(s);
                    }
                    parquet::record::Field::Bytes(b) => {
                        if let Ok(s) = std::str::from_utf8(b.data()) {
                            h3_val_u64 = parse_hex_u64(s);
                        }
                    }
                    _ => {}
                }
            }

            if let Some(h3_u64) = h3_val_u64 {
                let mut properties = Vec::new();
                for (col_i, (name, field_val)) in row.get_column_iter().enumerate() {
                    if col_i == h3_idx {
                        continue;
                    }
                    match field_val {
                        parquet::record::Field::Double(d) => properties.push((name.to_string(), MvtValue::Double(*d))),
                        parquet::record::Field::Float(f) => properties.push((name.to_string(), MvtValue::Float(*f))),
                        parquet::record::Field::Long(i) => properties.push((name.to_string(), MvtValue::Int(*i))),
                        parquet::record::Field::ULong(u) => properties.push((name.to_string(), MvtValue::UInt(*u))),
                        parquet::record::Field::Int(i) => properties.push((name.to_string(), MvtValue::Int(*i as i64))),
                        parquet::record::Field::UInt(u) => properties.push((name.to_string(), MvtValue::UInt(*u as u64))),
                        parquet::record::Field::Short(s) => properties.push((name.to_string(), MvtValue::Int(*s as i64))),
                        parquet::record::Field::UShort(u) => properties.push((name.to_string(), MvtValue::UInt(*u as u64))),
                        parquet::record::Field::Byte(b) => properties.push((name.to_string(), MvtValue::Int(*b as i64))),
                        parquet::record::Field::UByte(u) => properties.push((name.to_string(), MvtValue::UInt(*u as u64))),
                        parquet::record::Field::Str(s) => properties.push((name.to_string(), MvtValue::String(s.clone()))),
                        parquet::record::Field::Bool(b) => properties.push((name.to_string(), MvtValue::Bool(*b))),
                        _ => {}
                    }
                }
                features.push(H3Feature::new(h3_u64, properties));
            }
        }

        Self::export_h3_features(features, pmtiles_path)
    }
}
