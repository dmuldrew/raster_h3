//! Multi-Resolution H3 to PMTiles v3 Tiling Engine
//!
//! Orchestrates the streaming aggregation of GeoTIFF rasters across multiple H3 resolutions
//! and packages the resulting vector hexagons directly into a single PMTiles v3 archive.

use std::borrow::Cow;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::path::Path;
use fxhash::FxBuildHasher;
use h3o::{CellIndex, LatLng, Resolution};
use rayon::prelude::*;
use serde_json::json;

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer,
};
use crate::pmtiles::mvt::{
    FeatureProperties, MercatorPoint, MvtFeature, MvtLayer, MvtValue, PropertyFilter,
    PROP_CAT_COUNT, PROP_CAT_DISTINCT_CLASSES, PROP_CAT_ENTROPY, PROP_CAT_H3_HEX,
    PROP_CAT_H3_INDEX, PROP_CAT_MAJORITY, PROP_CAT_MAJORITY_FRACTION, PROP_CAT_RESOLUTION,
    PROP_COUNT, PROP_H3_HEX, PROP_H3_INDEX, PROP_MAX, PROP_MEAN, PROP_MIN, PROP_RESOLUTION,
    PROP_STDDEV, PROP_SUM,
};
use crate::pmtiles::writer::PmtilesWriter;
use crate::raster::geotiff::GeoTiffStreamReader;

// Re-export tile pyramid coordinate math and zoom calculations from pyramid module
pub use crate::pmtiles::pyramid::{
    cell_boundary_mercator, cell_tile_range, cell_tile_range_mercator, h3_res_for_zoom,
    h3_res_to_zoom, lon_lat_to_tile_xy, max_hex_radius_deg, mercator_to_tile_xy,
    tile_xy_to_bbox, zoom_to_h3_res, zooms_for_h3_res,
};

/// Priority queue entry for tile eviction ordered by southernmost latitude
#[derive(Clone, Copy, PartialEq)]
struct TileEvictionEntry {
    safe_evict_lat: f64,
    tile_key: (u8, u32, u32),
}

impl Eq for TileEvictionEntry {}

impl Ord for TileEvictionEntry {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.safe_evict_lat.total_cmp(&other.safe_evict_lat)
    }
}

impl PartialOrd for TileEvictionEntry {
    #[inline(always)]
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

/// Batch of continuous records transferred from the background scanline producer thread
struct ContinuousStreamBatch {
    records: Vec<MultiContinuousRecord>,
    lat_horizon: f64,
}

/// Batch of categorical records transferred from the background scanline producer thread
struct CategoricalStreamBatch {
    records: Vec<MultiCategoricalRecord>,
    lat_horizon: f64,
}

/// Extent and statistics for a Parquet row group discovered during pre-scan
#[derive(Debug, Clone, Copy)]
struct RowGroupExtent {
    rg_idx: usize,
    min_lat: f64,
    max_lat: f64,
    min_lon: f64,
    max_lon: f64,
    min_zoom: u8,
    max_zoom: u8,
    min_res: u8,
}

/// Evict all tiles whose southernmost reach is strictly north of lat_horizon,
/// encode to MVT protobuf and Gzip compress across Rayon workers, and stream to PMTiles
fn evict_and_write_tiles(
    tile_buckets: &mut HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher>,
    tile_eviction_queue: &mut BinaryHeap<TileEvictionEntry>,
    lat_horizon: f64,
    writer: &mut PmtilesWriter,
) -> io::Result<usize> {
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

    if ready_tiles.is_empty() {
        return Ok(0);
    }

    let count = ready_tiles.len();
    let compressed_batch: Vec<((u8, u32, u32), Vec<u8>)> = ready_tiles
        .into_par_iter()
        .map(|(key, layer)| {
            let pbf_bytes = layer.encode();
            let compressed = crate::pmtiles::writer::gzip_compress(&pbf_bytes)?;
            Ok((key, compressed))
        })
        .collect::<io::Result<Vec<_>>>()?;

    for ((z, x, y), compressed_bytes) in compressed_batch {
        writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
    }

    Ok(count)
}

/// Flush all remaining active tiles at raster/stream completion in parallel
fn flush_all_tiles(
    tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher>,
    writer: &mut PmtilesWriter,
) -> io::Result<usize> {
    let remaining_tiles: Vec<((u8, u32, u32), MvtLayer)> = tile_buckets.into_iter().collect();
    if remaining_tiles.is_empty() {
        return Ok(0);
    }
    let count = remaining_tiles.len();
    let compressed_batch: Vec<((u8, u32, u32), Vec<u8>)> = remaining_tiles
        .into_par_iter()
        .map(|(key, layer)| {
            let pbf_bytes = layer.encode();
            let compressed = crate::pmtiles::writer::gzip_compress(&pbf_bytes)?;
            Ok((key, compressed))
        })
        .collect::<io::Result<Vec<_>>>()?;

    for ((z, x, y), compressed_bytes) in compressed_batch {
        writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
    }

    Ok(count)
}

// Re-export feature definitions, metadata, and export summaries from features module
pub use crate::pmtiles::features::{H3Feature, PmtilesExportSummary, ResolutionAccumulatorStats};

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
        flush_all_tiles(tile_buckets, &mut writer)?;

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

        let (tx, rx) = std::sync::mpsc::sync_channel::<ContinuousStreamBatch>(4);
        let producer_handle = std::thread::spawn(move || {
            loop {
                let mut records = Vec::with_capacity(8192);
                streamer.drain_completed_into(8192, |_i, record| {
                    records.push(record);
                });
                if records.is_empty() {
                    break;
                }
                let lat_horizon = streamer.current_lat_horizon();
                if tx.send(ContinuousStreamBatch { records, lat_horizon }).is_err() {
                    break;
                }
            }
        });

        while let Ok(batch) = rx.recv() {
            let prepared_batch: Vec<PreparedContinuousHex> = batch
                .records
                .into_par_iter()
                .filter_map(|record| {
                    let resolution = record.resolution;
                    let h3_index = record.h3_index;
                    let accumulator = record.accumulator;

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

                    for &zoom in &zooms {
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
                                properties.clone(),
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
                        layer.add_feature(op.feature);
                    }
                }

                total_hexagons += 1;
            }

            evict_and_write_tiles(&mut tile_buckets, &mut tile_eviction_queue, batch.lat_horizon, &mut writer)?;
        }

        if let Err(e) = producer_handle.join() {
            return Err(format!("Producer thread panicked: {:?}", e).into());
        }

        flush_all_tiles(tile_buckets, &mut writer)?;

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

        let (tx, rx) = std::sync::mpsc::sync_channel::<CategoricalStreamBatch>(4);
        let producer_handle = std::thread::spawn(move || {
            loop {
                let mut records = Vec::with_capacity(8192);
                streamer.drain_completed_into(8192, |_i, record| {
                    records.push(record);
                });
                if records.is_empty() {
                    break;
                }
                let lat_horizon = streamer.current_lat_horizon();
                if tx.send(CategoricalStreamBatch { records, lat_horizon }).is_err() {
                    break;
                }
            }
        });

        while let Ok(batch) = rx.recv() {
            let prepared_batch: Vec<PreparedCategoricalHex> = batch
                .records
                .into_par_iter()
                .filter_map(|record| {
                    let resolution = record.resolution;
                    let h3_index = record.h3_index;
                    let accumulator = record.accumulator;

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

                    for &zoom in &zooms {
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
                                properties.clone(),
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
                        accumulator,
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
                        layer.add_feature(op.feature);
                    }
                }

                total_hexagons += 1;
            }

            evict_and_write_tiles(&mut tile_buckets, &mut tile_eviction_queue, batch.lat_horizon, &mut writer)?;
        }

        if let Err(e) = producer_handle.join() {
            return Err(format!("Producer thread panicked: {:?}", e).into());
        }

        flush_all_tiles(tile_buckets, &mut writer)?;

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

    /// Convenience helper to run categorical GeoTIFF or multi-file mosaic pipeline to PMTiles v3
    pub fn process_categorical_source_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        source: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let source_str = source.as_ref().to_string_lossy();
        let resolved_paths = crate::raster::mosaic::resolve_raster_sources(&source_str)?;
        let props = config.properties.clone();

        if resolved_paths.len() == 1 && !crate::raster::http_range::is_remote_url(resolved_paths[0].to_str().unwrap_or("")) {
            let reader = GeoTiffStreamReader::open(&resolved_paths[0])?;
            let streamer = MultiCategoricalHorizonStreamer::new(reader, &config)?;
            Self::generate_from_categorical_streamer_with_properties(streamer, pmtiles_path, props.as_deref())
        } else {
            let mosaic = std::sync::Arc::new(crate::raster::mosaic::MosaicReader::open(
                &resolved_paths,
                config.bbox,
                config.custom_crs.as_deref(),
                config.overlap_rule,
            )?);
            let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)?;
            Self::generate_from_categorical_streamer_with_properties(streamer, pmtiles_path, props.as_deref())
        }
    }

    /// Convenience helper to run categorical GeoTIFF-to-PMTiles pipeline in single-pass streaming mode
    pub fn process_categorical_geotiff_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        tiff_path: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Self::process_categorical_source_to_pmtiles(tiff_path, pmtiles_path, config)
    }

    /// Convenience helper to run continuous GeoTIFF or multi-file mosaic pipeline to PMTiles v3
    pub fn process_raster_source_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        source: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let source_str = source.as_ref().to_string_lossy();
        let resolved_paths = crate::raster::mosaic::resolve_raster_sources(&source_str)?;
        let props = config.properties.clone();

        if resolved_paths.len() == 1 && !crate::raster::http_range::is_remote_url(resolved_paths[0].to_str().unwrap_or("")) {
            let reader = GeoTiffStreamReader::open(&resolved_paths[0])?;
            let streamer = MultiScanHorizonStreamer::new(reader, &config)?;
            Self::generate_from_continuous_streamer_with_properties(streamer, pmtiles_path, props.as_deref())
        } else {
            let mosaic = std::sync::Arc::new(crate::raster::mosaic::MosaicReader::open(
                &resolved_paths,
                config.bbox,
                config.custom_crs.as_deref(),
                config.overlap_rule,
            )?);
            let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?;
            Self::generate_from_continuous_streamer_with_properties(streamer, pmtiles_path, props.as_deref())
        }
    }

    /// Convenience helper to run continuous GeoTIFF-to-PMTiles pipeline in single-pass streaming mode
    pub fn process_geotiff_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        tiff_path: P1,
        pmtiles_path: P2,
        config: MultiResolutionConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        Self::process_raster_source_to_pmtiles(tiff_path, pmtiles_path, config)
    }

    /// Pre-scanned geographical and resolution extent of a Parquet row group
    fn scan_row_group_h3_extent(
        rg: &dyn parquet::file::reader::RowGroupReader,
        h3_idx: usize,
        rg_idx: usize,
    ) -> Result<Option<RowGroupExtent>, Box<dyn std::error::Error + Send + Sync>> {
        use crate::functions::fast_hex::parse_hex_u64;

        let col_reader = match rg.get_column_reader(h3_idx) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };

        let mut min_lat = 90.0f64;
        let mut max_lat = -90.0f64;
        let mut min_lon = 180.0f64;
        let mut max_lon = -180.0f64;
        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;
        let mut min_res = 255u8;
        let mut count = 0usize;

        let mut process_h3 = |h3: u64| {
            if let Ok(cell) = CellIndex::try_from(h3) {
                let center: LatLng = cell.into();
                let lat = center.lat();
                let lon = center.lng();
                if lat < min_lat { min_lat = lat; }
                if lat > max_lat { max_lat = lat; }
                if lon < min_lon { min_lon = lon; }
                if lon > max_lon { max_lon = lon; }
                let res: u8 = cell.resolution().into();
                if res < min_res { min_res = res; }
                let zoom = h3_res_to_zoom(res);
                if zoom < min_zoom { min_zoom = zoom; }
                if zoom > max_zoom { max_zoom = zoom; }
                count += 1;
            }
        };

        match col_reader {
            parquet::column::reader::ColumnReader::Int64ColumnReader(mut r) => {
                let mut vals = Vec::with_capacity(8192);
                loop {
                    vals.clear();
                    let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                    if read == 0 { break; }
                    for &v in &vals {
                        process_h3(v as u64);
                    }
                }
            }
            parquet::column::reader::ColumnReader::Int32ColumnReader(mut r) => {
                let mut vals = Vec::with_capacity(8192);
                loop {
                    vals.clear();
                    let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                    if read == 0 { break; }
                    for &v in &vals {
                        process_h3(v as u64);
                    }
                }
            }
            parquet::column::reader::ColumnReader::ByteArrayColumnReader(mut r) => {
                let mut vals = Vec::with_capacity(8192);
                loop {
                    vals.clear();
                    let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                    if read == 0 { break; }
                    for v in &vals {
                        if let Ok(s) = std::str::from_utf8(v.data()) {
                            if let Some(h3) = parse_hex_u64(s) {
                                process_h3(h3);
                            }
                        }
                    }
                }
            }
            parquet::column::reader::ColumnReader::FixedLenByteArrayColumnReader(mut r) => {
                let mut vals = Vec::with_capacity(8192);
                loop {
                    vals.clear();
                    let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                    if read == 0 { break; }
                    for v in &vals {
                        if let Ok(s) = std::str::from_utf8(v.data()) {
                            if let Some(h3) = parse_hex_u64(s) {
                                process_h3(h3);
                            }
                        }
                    }
                }
            }
            _ => return Ok(None),
        }

        if count == 0 {
            Ok(None)
        } else {
            Ok(Some(RowGroupExtent {
                rg_idx,
                min_lat,
                max_lat,
                min_lon,
                max_lon,
                min_zoom,
                max_zoom,
                min_res,
            }))
        }
    }

    /// Convert any H3-indexed Parquet file directly into a PMTiles v3 archive with streaming horizon eviction
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
        let num_rgs = reader.num_row_groups();
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

        // Pre-scan row group extents to compute global bounds and row group horizons
        let mut rg_extents = Vec::with_capacity(num_rgs);
        for rg_i in 0..num_rgs {
            if let Ok(rg) = reader.get_row_group(rg_i) {
                if let Ok(Some(ext)) = Self::scan_row_group_h3_extent(&*rg, h3_idx, rg_i) {
                    rg_extents.push(ext);
                }
            }
        }

        // If no extents found or empty file, fallback to empty summary
        if rg_extents.is_empty() {
            let metadata = json!({
                "name": "h3_pmtiles_export",
                "description": "H3 vector hexagon tile pyramid exported by raster_h3",
                "version": "3",
                "minzoom": 0,
                "maxzoom": 0,
                "vector_layers": [
                    {
                        "id": "h3_hexagons",
                        "description": "H3 hexagonal vector polygons with attributes",
                        "minzoom": 0,
                        "maxzoom": 0,
                        "fields": {}
                    }
                ]
            });
            let writer = PmtilesWriter::new(
                0,
                0,
                [-180.0, -85.0, 180.0, 85.0],
                metadata.to_string(),
            )?;
            writer.finish(pmtiles_path)?;
            return Ok(PmtilesExportSummary {
                total_features: 0,
                valid_features: 0,
                invalid_features_dropped: 0,
                total_tiles: 0,
                min_zoom: 0,
                max_zoom: 0,
            });
        }

        let global_min_lon = rg_extents.iter().map(|e| e.min_lon).fold(180.0f64, f64::min);
        let global_min_lat = rg_extents.iter().map(|e| e.min_lat).fold(90.0f64, f64::min);
        let global_max_lon = rg_extents.iter().map(|e| e.max_lon).fold(-180.0f64, f64::max);
        let global_max_lat = rg_extents.iter().map(|e| e.max_lat).fold(-90.0f64, f64::max);
        let min_zoom = rg_extents.iter().map(|e| e.min_zoom).min().unwrap_or(0);
        let max_zoom = rg_extents.iter().map(|e| e.max_zoom).max().unwrap_or(0);
        let min_res = rg_extents.iter().map(|e| e.min_res).min().unwrap_or(0);
        let max_cell_radius = max_hex_radius_deg(min_res);
        let safety_margin = 2.5 * max_cell_radius;

        // Schedule row groups North-to-South (descending max_lat)
        let mut scheduled_rgs: Vec<RowGroupExtent> = rg_extents;
        scheduled_rgs.sort_by(|a, b| b.max_lat.total_cmp(&a.max_lat));

        // Compute future horizons
        let n_rgs = scheduled_rgs.len();
        let mut future_horizons = vec![-90.0f64; n_rgs];
        let mut max_future = -90.0f64;
        for k in (0..n_rgs).rev() {
            future_horizons[k] = max_future;
            max_future = max_future.max(scheduled_rgs[k].max_lat);
        }

        // Build dynamic fields metadata for vector layer
        let mut fields_map = serde_json::Map::new();
        fields_map.insert("h3_index".to_string(), json!("Number"));
        fields_map.insert("h3_hex".to_string(), json!("String"));
        fields_map.insert("resolution".to_string(), json!("Number"));
        for (idx, col) in schema.columns().iter().enumerate() {
            if idx == h3_idx { continue; }
            let type_str = match col.physical_type() {
                parquet::basic::Type::INT32 | parquet::basic::Type::INT64 | parquet::basic::Type::INT96 => "Number",
                parquet::basic::Type::FLOAT | parquet::basic::Type::DOUBLE => "Number",
                parquet::basic::Type::BYTE_ARRAY | parquet::basic::Type::FIXED_LEN_BYTE_ARRAY => "String",
                parquet::basic::Type::BOOLEAN => "Boolean",
            };
            fields_map.insert(col.name().to_string(), json!(type_str));
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
                    "fields": fields_map
                }
            ]
        });

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [global_min_lon, global_min_lat, global_max_lon, global_max_lat],
            metadata.to_string(),
        )?;

        let mut tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut tile_eviction_queue: BinaryHeap<TileEvictionEntry> = BinaryHeap::new();

        let mut total_features = 0usize;
        let mut valid_features = 0usize;
        let mut invalid_dropped = 0usize;

        // Pre-extract column names once so we don't allocate String per field per row
        let col_names: Vec<Cow<'static, str>> = schema
            .columns()
            .iter()
            .map(|f| Cow::Owned(f.name().to_string()))
            .collect();

        for (k, rg_ext) in scheduled_rgs.iter().enumerate() {
            let rg = reader.get_row_group(rg_ext.rg_idx)?;
            let row_iter = rg.get_row_iter(None)?;

            for row_result in row_iter {
                total_features += 1;
                let row = match row_result {
                    Ok(r) => r,
                    Err(_) => {
                        invalid_dropped += 1;
                        continue;
                    }
                };

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

                let h3_u64 = match h3_val_u64 {
                    Some(h) => h,
                    None => {
                        invalid_dropped += 1;
                        continue;
                    }
                };

                let cell = match CellIndex::try_from(h3_u64) {
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

                let center_merc = MercatorPoint::from_lat_lng(c_lat, c_lon);
                let (v_merc, v_count) = cell_boundary_mercator(cell);
                let vertices_merc = &v_merc[..v_count];

                let res_u8: u8 = cell.resolution().into();
                let zoom = h3_res_to_zoom(res_u8);

                let mut properties = Vec::with_capacity(col_names.len().saturating_sub(1) + 3);
                for (col_i, (_, field_val)) in row.get_column_iter().enumerate() {
                    if col_i == h3_idx {
                        continue;
                    }
                    let mvt_val = match field_val {
                        parquet::record::Field::Double(d) => MvtValue::Double(*d),
                        parquet::record::Field::Float(f) => MvtValue::Float(*f),
                        parquet::record::Field::Long(i) => MvtValue::Int(*i),
                        parquet::record::Field::ULong(u) => MvtValue::UInt(*u),
                        parquet::record::Field::Int(i) => MvtValue::Int(*i as i64),
                        parquet::record::Field::UInt(u) => MvtValue::UInt(*u as u64),
                        parquet::record::Field::Short(s) => MvtValue::Int(*s as i64),
                        parquet::record::Field::UShort(u) => MvtValue::UInt(*u as u64),
                        parquet::record::Field::Byte(b) => MvtValue::Int(*b as i64),
                        parquet::record::Field::UByte(u) => MvtValue::UInt(*u as u64),
                        parquet::record::Field::Str(s) => MvtValue::String(s.clone()),
                        parquet::record::Field::Bool(b) => MvtValue::Bool(*b),
                        _ => continue,
                    };
                    if let Some(col_name) = col_names.get(col_i) {
                        properties.push((col_name.clone(), mvt_val));
                    }
                }

                if !properties.iter().any(|(k, _)| k == "h3_index") {
                    properties.push((Cow::Borrowed("h3_index"), MvtValue::UInt(h3_u64)));
                }
                if !properties.iter().any(|(k, _)| k == "h3_hex") {
                    properties.push((Cow::Borrowed("h3_hex"), MvtValue::from_hex_u64(h3_u64)));
                }
                if !properties.iter().any(|(k, _)| k == "resolution") {
                    properties.push((Cow::Borrowed("resolution"), MvtValue::UInt(res_u8 as u64)));
                }

                let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(center_merc, vertices_merc, zoom);
                if min_tx == max_tx && min_ty == max_ty {
                    let tile_key = (zoom, min_tx, min_ty);
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
                            MvtLayer::new("h3_hexagons")
                        })
                    };
                    layer.add_hexagon_mercator(
                        h3_u64,
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
                                    MvtLayer::new("h3_hexagons")
                                })
                            };

                            let props = if is_last {
                                std::mem::take(&mut properties)
                            } else {
                                properties.clone()
                            };

                            layer.add_hexagon_mercator(
                                h3_u64,
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

            // Streaming Horizon Eviction: Evict and compress all tiles completed prior to the remaining horizon
            let lat_horizon = future_horizons[k];
            evict_and_write_tiles(&mut tile_buckets, &mut tile_eviction_queue, lat_horizon, &mut writer)?;
        }

        let total_tiles = writer.tile_count() + tile_buckets.len();
        flush_all_tiles(tile_buckets, &mut writer)?;
        writer.finish(pmtiles_path)?;

        Ok(PmtilesExportSummary {
            total_features,
            valid_features,
            invalid_features_dropped: invalid_dropped,
            total_tiles,
            min_zoom,
            max_zoom,
        })
    }
}
