//! Multi-Resolution H3 to PMTiles v3 Tiling Engine
//!
//! Orchestrates the streaming aggregation of GeoTIFF rasters across multiple H3 resolutions
//! and packages the resulting vector hexagons directly into a single PMTiles v3 archive.

use std::collections::HashMap;
use std::path::Path;
use fxhash::FxBuildHasher;
use h3o::{CellIndex, LatLng};
use serde_json::json;

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

            let (tx, ty) = lon_lat_to_tile_xy(c_lon, c_lat, zoom);
            let tile_key = (zoom, tx, ty);

            let layer = tile_buckets.entry(tile_key).or_insert_with(|| {
                MvtLayer::new("h3_hexagons")
            });

            let bbox = tile_xy_to_bbox(zoom, tx, ty);
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

            layer.add_hexagon(
                feat.h3_index,
                &boundary_vertices,
                bbox[0],
                bbox[2],
                bbox[1],
                bbox[3],
                properties,
            );
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
        for ((z, x, y), layer) in tile_buckets {
            let mvt_bytes = layer.encode();
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
                    let (tx, ty) = lon_lat_to_tile_xy(c_lon, c_lat, zoom);
                    let tile_key = (zoom, tx, ty);

                    let layer = tile_buckets.entry(tile_key).or_insert_with(|| {
                        MvtLayer::new("h3_hexagons")
                    });

                    let bbox = tile_xy_to_bbox(zoom, tx, ty);
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

                    layer.add_hexagon(
                        h3_index,
                        &boundary_vertices,
                        bbox[0],
                        bbox[2],
                        bbox[1],
                        bbox[3],
                        properties,
                    );

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

        for ((z, x, y), layer) in tile_buckets {
            let mvt_bytes = layer.encode();
            writer.add_tile(z, x, y, &mvt_bytes)?;
        }

        writer.finish(output_path)?;
        Ok(total_hexagons)
    }

    /// Convenience helper to run complete GeoTIFF-to-PMTiles pipeline from file path
    pub fn process_geotiff_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        tiff_path: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let reader = GeoTiffStreamReader::open(tiff_path)?;
        let streamer = MultiScanHorizonStreamer::new(reader, &config)?;
        Self::generate_from_continuous_streamer(streamer, pmtiles_path)
    }
}
