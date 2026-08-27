//! Multi-Resolution H3 to PMTiles v3 Tiling Engine
//!
//! Orchestrates the streaming aggregation of GeoTIFF rasters across multiple H3 resolutions
//! and packages the resulting vector hexagons directly into a single PMTiles v3 archive.

use std::collections::HashMap;
use std::path::Path;
use fxhash::FxBuildHasher;
use h3o::{CellIndex, LatLng};
use rayon::prelude::*;
use serde_json::json;

use crate::aggregator::h3_map::{H3HashMap, aggregate_raster_stream};
use crate::aggregator::horizon_streamer::AggregationConfig;
use crate::aggregator::multi_horizon::{MultiContinuousRecord, MultiScanHorizonStreamer, MultiResolutionConfig};
use crate::functions::fast_hex::fast_hex_u64;
use crate::pmtiles::mvt::{MvtLayer, MvtValue};
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

/// Map an H3 resolution to the continuous range of Web Mercator zoom levels it covers
pub fn zooms_for_h3_res(res: u8, min_res: u8) -> Vec<u8> {
    match res {
        0 => vec![0, 1],
        1 => vec![2, 3],
        2 => vec![4],
        3 => {
            if min_res >= 3 {
                (0..=5).collect()
            } else {
                vec![5]
            }
        }
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
    }
}

/// A generic H3 feature record with arbitrary properties for PMTiles export
#[derive(Debug, Clone)]
pub struct H3Feature {
    pub h3_index: u64,
    pub properties: Vec<(String, MvtValue)>,
}

impl H3Feature {
    pub fn new(h3_index: u64, properties: Vec<(String, MvtValue)>) -> Self {
        Self { h3_index, properties }
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

            let res_u8: u8 = cell.resolution().into();
            let zoom = h3_res_to_zoom(res_u8);
            if zoom < min_zoom { min_zoom = zoom; }
            if zoom > max_zoom { max_zoom = zoom; }

            let boundary_vertices: Vec<LatLng> = cell.boundary().iter().copied().collect();

            let mut properties = feat.properties;
            let mut hex_buf = [0u8; 16];
            let hex_bytes = fast_hex_u64(feat.h3_index, &mut hex_buf);
            let hex_str = std::str::from_utf8(hex_bytes).unwrap_or("").to_string();

            if !properties.iter().any(|(k, _)| k == "h3_index") {
                properties.push(("h3_index".to_string(), MvtValue::UInt(feat.h3_index)));
            }
            if !properties.iter().any(|(k, _)| k == "h3_hex") {
                properties.push(("h3_hex".to_string(), MvtValue::String(hex_str)));
            }
            if !properties.iter().any(|(k, _)| k == "resolution") {
                properties.push(("resolution".to_string(), MvtValue::UInt(res_u8 as u64)));
            }

            let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range(center, &boundary_vertices, zoom);
            for tx in min_tx..=max_tx {
                for ty in min_ty..=max_ty {
                    let tile_key = (zoom, tx, ty);
                    let layer = tile_buckets.entry(tile_key).or_insert_with(|| {
                        MvtLayer::new("h3_hexagons")
                    });
                    let bbox = tile_xy_to_bbox(zoom, tx, ty);

                    layer.add_hexagon(
                        feat.h3_index,
                        &boundary_vertices,
                        bbox[0],
                        bbox[2],
                        bbox[1],
                        bbox[3],
                        properties.clone(),
                    );
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
        );

        let total_tiles = tile_buckets.len();
        let encoded_tiles: Vec<((u8, u32, u32), Vec<u8>)> = tile_buckets
            .into_par_iter()
            .map(|((z, x, y), layer)| ((z, x, y), layer.encode()))
            .collect();

        for ((z, x, y), mvt_bytes) in encoded_tiles {
            writer.add_tile(z, x, y, &mvt_bytes)?;
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
        mut streamer: MultiScanHorizonStreamer,
        output_path: P,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let resolutions = streamer.resolution_u8s().to_vec();
        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;

        for &res in &resolutions {
            let z = h3_res_to_zoom(res);
            if z < min_zoom {
                min_zoom = z;
            }
            if z > max_zoom {
                max_zoom = z;
            }
        }

        let mut tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());

        let mut total_hexagons = 0usize;
        let mut global_min_lon = 180.0f64;
        let mut global_min_lat = 90.0f64;
        let mut global_max_lon = -180.0f64;
        let mut global_max_lat = -90.0f64;

        // Stream continuous records directly from the scan horizon engine
        loop {
            let batch = streamer.fetch_next_batch(4096);
            if batch.is_empty() {
                break;
            }

            for record in batch {
                let MultiContinuousRecord {
                    resolution,
                    h3_index,
                    accumulator,
                } = record;

                if let Ok(cell) = CellIndex::try_from(h3_index) {
                    let center: LatLng = cell.into();
                    let c_lat = center.lat();
                    let c_lon = center.lng();

                    if c_lon < global_min_lon { global_min_lon = c_lon; }
                    if c_lon > global_max_lon { global_max_lon = c_lon; }
                    if c_lat < global_min_lat { global_min_lat = c_lat; }
                    if c_lat > global_max_lat { global_max_lat = c_lat; }

                    let zoom = h3_res_to_zoom(resolution);
                    let boundary_vertices: Vec<LatLng> = cell.boundary().iter().copied().collect();

                    let mut hex_buf = [0u8; 16];
                    let hex_bytes = fast_hex_u64(h3_index, &mut hex_buf);
                    let hex_str = std::str::from_utf8(hex_bytes).unwrap_or("").to_string();

                    let properties = vec![
                        ("h3_index".to_string(), MvtValue::UInt(h3_index)),
                        ("h3_hex".to_string(), MvtValue::String(hex_str)),
                        ("resolution".to_string(), MvtValue::UInt(resolution as u64)),
                        ("mean".to_string(), MvtValue::Double(accumulator.mean())),
                        ("stddev".to_string(), MvtValue::Double(accumulator.stddev())),
                        ("count".to_string(), MvtValue::Double(accumulator.count)),
                        ("min".to_string(), MvtValue::Double(accumulator.min)),
                        ("max".to_string(), MvtValue::Double(accumulator.max)),
                    ];

                    let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range(center, &boundary_vertices, zoom);
                    for tx in min_tx..=max_tx {
                        for ty in min_ty..=max_ty {
                            let tile_key = (zoom, tx, ty);
                            let layer = tile_buckets.entry(tile_key).or_insert_with(|| {
                                MvtLayer::new("h3_hexagons")
                            });
                            let bbox = tile_xy_to_bbox(zoom, tx, ty);

                            layer.add_hexagon(
                                h3_index,
                                &boundary_vertices,
                                bbox[0],
                                bbox[2],
                                bbox[1],
                                bbox[3],
                                properties.clone(),
                            );
                        }
                    }

                    total_hexagons += 1;
                }
            }
        }

        if total_hexagons == 0 {
            global_min_lon = -180.0;
            global_min_lat = -85.0;
            global_max_lon = 180.0;
            global_max_lat = 85.0;
            min_zoom = 0;
            max_zoom = 0;
        }

        // Build vector layer JSON metadata for MapLibre / Kepler.gl
        let metadata = json!({
            "name": "raster_h3_pmtiles",
            "description": "Multi-resolution H3 hexagonal vector tile pyramid generated by raster_h3",
            "version": "3",
            "minzoom": min_zoom,
            "maxzoom": max_zoom,
            "vector_layers": [
                {
                    "id": "h3_hexagons",
                    "description": "Aggregated H3 hexagonal grid cells",
                    "minzoom": min_zoom,
                    "maxzoom": max_zoom,
                    "fields": {
                        "h3_index": "Number",
                        "h3_hex": "String",
                        "resolution": "Number",
                        "mean": "Number",
                        "stddev": "Number",
                        "count": "Number",
                        "min": "Number",
                        "max": "Number"
                    }
                }
            ]
        });

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [global_min_lon, global_min_lat, global_max_lon, global_max_lat],
            metadata.to_string(),
        );

        let encoded_tiles: Vec<((u8, u32, u32), Vec<u8>)> = tile_buckets
            .into_par_iter()
            .map(|((z, x, y), layer)| ((z, x, y), layer.encode()))
            .collect();

        for ((z, x, y), mvt_bytes) in encoded_tiles {
            writer.add_tile(z, x, y, &mvt_bytes)?;
        }

        writer.finish(output_path)?;
        Ok(total_hexagons)
    }

    /// Convenience helper to run complete GeoTIFF-to-PMTiles pipeline in parallel across all CPU cores
    pub fn process_geotiff_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        tiff_path: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let reader = GeoTiffStreamReader::open(tiff_path)?;
        let resolutions = config.resolutions.clone();

        // 1. Parallel multi-resolution chunk aggregation across all CPU cores
        let agg_configs: Vec<(u8, AggregationConfig)> = resolutions
            .iter()
            .map(|&res| {
                let single_cfg = AggregationConfig {
                    resolution: res,
                    custom_crs: config.custom_crs.clone(),
                    custom_nodata: config.custom_nodata,
                    bbox: config.bbox,
                    sampling: config.sampling.clone(),
                };
                (res, single_cfg)
            })
            .collect();

        let resolution_maps: Vec<(u8, H3HashMap)> = agg_configs
            .into_par_iter()
            .map(|(res, cfg)| {
                let map = aggregate_raster_stream(&reader, &cfg).unwrap_or_default();
                (res, map)
            })
            .collect();

        // 2. Build multi-resolution continuous tile pyramid
        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;
        let min_res = resolutions.iter().copied().min().unwrap_or(0);

        for &(res, _) in &resolution_maps {
            let zooms = zooms_for_h3_res(res, min_res);
            for &z in &zooms {
                if z < min_zoom { min_zoom = z; }
                if z > max_zoom { max_zoom = z; }
            }
        }

        let mut tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());

        let mut total_hexagons = 0usize;
        let mut global_min_lon = 180.0f64;
        let mut global_min_lat = 90.0f64;
        let mut global_max_lon = -180.0f64;
        let mut global_max_lat = -90.0f64;

        for (res, map) in resolution_maps {
            let zooms = zooms_for_h3_res(res, min_res);
            for (h3_index, accumulator) in map {
                if let Ok(cell) = CellIndex::try_from(h3_index) {
                    let center: LatLng = cell.into();
                    let c_lat = center.lat();
                    let c_lon = center.lng();

                    if c_lon < global_min_lon { global_min_lon = c_lon; }
                    if c_lon > global_max_lon { global_max_lon = c_lon; }
                    if c_lat < global_min_lat { global_min_lat = c_lat; }
                    if c_lat > global_max_lat { global_max_lat = c_lat; }

                    let boundary_vertices: Vec<LatLng> = cell.boundary().iter().copied().collect();

                    let mut hex_buf = [0u8; 16];
                    let hex_bytes = fast_hex_u64(h3_index, &mut hex_buf);
                    let hex_str = std::str::from_utf8(hex_bytes).unwrap_or("").to_string();

                    for &zoom in &zooms {
                        let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range(center, &boundary_vertices, zoom);
                        for tx in min_tx..=max_tx {
                            for ty in min_ty..=max_ty {
                                let tile_key = (zoom, tx, ty);
                                let layer = tile_buckets.entry(tile_key).or_insert_with(|| {
                                    MvtLayer::new("h3_hexagons")
                                });

                                let bbox = tile_xy_to_bbox(zoom, tx, ty);

                                let properties = vec![
                                    ("h3_index".to_string(), MvtValue::UInt(h3_index)),
                                    ("h3_hex".to_string(), MvtValue::String(hex_str.clone())),
                                    ("resolution".to_string(), MvtValue::UInt(res as u64)),
                                    ("mean".to_string(), MvtValue::Double(accumulator.mean())),
                                    ("stddev".to_string(), MvtValue::Double(accumulator.stddev())),
                                    ("count".to_string(), MvtValue::Double(accumulator.count)),
                                    ("min".to_string(), MvtValue::Double(accumulator.min)),
                                    ("max".to_string(), MvtValue::Double(accumulator.max)),
                                ];

                                layer.add_hexagon(
                                    h3_index,
                                    &boundary_vertices,
                                    bbox[0],
                                    bbox[2],
                                    bbox[1],
                                    bbox[3],
                                    properties,
                                );
                            }
                        }
                    }

                    total_hexagons += 1;
                }
            }
        }

        if total_hexagons == 0 {
            global_min_lon = -180.0;
            global_min_lat = -90.0;
            global_max_lon = 180.0;
            global_max_lat = 90.0;
            if min_zoom == 255 {
                min_zoom = 0;
                max_zoom = 0;
            }
        }

        let metadata = json!({
            "name": "raster_h3_pmtiles",
            "format": "pbf",
            "type": "overlay",
            "description": "Multi-resolution H3 hexagonal vector tile pyramid generated by raster_h3",
            "version": "2",
            "minzoom": min_zoom,
            "maxzoom": max_zoom,
            "vector_layers": [
                {
                    "id": "h3_hexagons",
                    "description": "Aggregated H3 hexagonal grid cells",
                    "minzoom": min_zoom,
                    "maxzoom": max_zoom,
                    "fields": {
                        "h3_index": "Number",
                        "h3_hex": "String",
                        "resolution": "Number",
                        "mean": "Number",
                        "stddev": "Number",
                        "count": "Number",
                        "min": "Number",
                        "max": "Number"
                    }
                }
            ],
            "tilestats": {
                "layerCount": 1,
                "layers": [
                    {
                        "layer": "h3_hexagons",
                        "count": total_hexagons,
                        "geometry": "Polygon",
                        "attributeCount": 8,
                        "attributes": [
                            { "attribute": "mean", "type": "number" },
                            { "attribute": "max", "type": "number" },
                            { "attribute": "min", "type": "number" },
                            { "attribute": "count", "type": "number" },
                            { "attribute": "stddev", "type": "number" },
                            { "attribute": "resolution", "type": "number" },
                            { "attribute": "h3_hex", "type": "string" },
                            { "attribute": "h3_index", "type": "number" }
                        ]
                    }
                ]
            }
        });

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [global_min_lon, global_min_lat, global_max_lon, global_max_lat],
            metadata.to_string(),
        );

        // 3. Parallel Rayon MVT Protobuf compression across all CPU cores
        let encoded_tiles: Vec<((u8, u32, u32), Vec<u8>)> = tile_buckets
            .into_par_iter()
            .map(|((z, x, y), layer)| ((z, x, y), layer.encode()))
            .collect();

        for ((z, x, y), mvt_bytes) in encoded_tiles {
            writer.add_tile(z, x, y, &mvt_bytes)?;
        }

        writer.finish(pmtiles_path)?;
        Ok(total_hexagons)
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
