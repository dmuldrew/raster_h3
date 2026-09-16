//! Multi-Resolution H3 to PMTiles v3 Tiling Engine
//!
//! Orchestrates the streaming aggregation of GeoTIFF rasters across multiple H3 resolutions
//! and packages the resulting vector hexagons directly into a single PMTiles v3 archive.

use fxhash::FxBuildHasher;
use h3o::{CellIndex, LatLng, Resolution};
use rayon::prelude::*;
use serde_json::json;
use std::borrow::Cow;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::path::Path;

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
    h3_res_to_zoom, lon_lat_to_tile_xy, max_hex_radius_deg, mercator_to_tile_xy, tile_xy_to_bbox,
    zoom_to_h3_res, zooms_for_h3_res,
};

// Re-export feature definitions, metadata, accumulator, and export summaries from features module
pub use crate::pmtiles::features::{
    build_default_pmtiles_fields, build_pmtiles_metadata, H3Feature, PmtilesExportSummary,
    ResolutionAccumulatorStats, TileEvictionEntry, TilePyramidAccumulator,
};

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

/// Evict all tiles whose southernmost reach is strictly north of lat_horizon,
/// encode to MVT protobuf and Gzip compress across Rayon workers, and stream to PMTiles
#[allow(dead_code)]
pub fn evict_and_write_tiles(
    tile_buckets: &mut HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher>,
    tile_eviction_queue: &mut BinaryHeap<TileEvictionEntry>,
    lat_horizon: f64,
    writer: &mut PmtilesWriter,
) -> io::Result<usize> {
    let mut acc = TilePyramidAccumulator {
        tile_buckets: std::mem::take(tile_buckets),
        tile_eviction_queue: std::mem::take(tile_eviction_queue),
        safety_margin: 0.0,
        layer_name: std::borrow::Cow::Borrowed("h3_hexagons"),
        property_filter: None,
    };
    let count = acc.evict_and_write_tiles(lat_horizon, writer)?;
    *tile_buckets = acc.tile_buckets;
    *tile_eviction_queue = acc.tile_eviction_queue;
    Ok(count)
}

/// Flush all remaining active tiles at raster/stream completion in parallel
#[allow(dead_code)]
pub fn flush_all_tiles(
    tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher>,
    writer: &mut PmtilesWriter,
) -> io::Result<usize> {
    let acc = TilePyramidAccumulator {
        tile_buckets,
        tile_eviction_queue: BinaryHeap::new(),
        safety_margin: 0.0,
        layer_name: std::borrow::Cow::Borrowed("h3_hexagons"),
        property_filter: None,
    };
    acc.flush_all(writer)
}

/// High-level builder to convert H3 data and GeoTIFF raster aggregations directly to PMTiles v3
pub struct H3PmtilesTiler;

impl H3PmtilesTiler {
    /// Export any collection of generic H3 features (with strict H3 validation) to a PMTiles v3 archive
    pub fn export_h3_features<P: AsRef<Path>, I: IntoIterator<Item = H3Feature>>(
        features: I,
        output_path: P,
    ) -> Result<PmtilesExportSummary, Box<dyn std::error::Error + Send + Sync>> {
        let mut accumulator = TilePyramidAccumulator::new(0.0);

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

            if c_lon < global_min_lon {
                global_min_lon = c_lon;
            }
            if c_lon > global_max_lon {
                global_max_lon = c_lon;
            }
            if c_lat < global_min_lat {
                global_min_lat = c_lat;
            }
            if c_lat > global_max_lat {
                global_max_lat = c_lat;
            }

            let center_merc = MercatorPoint::from_lat_lng(c_lat, c_lon);
            let (v_merc, v_count) = cell_boundary_mercator(cell);
            let vertices_merc = &v_merc[..v_count];

            let res_u8: u8 = cell.resolution().into();
            let zoom = h3_res_to_zoom(res_u8);
            if zoom < min_zoom {
                min_zoom = zoom;
            }
            if zoom > max_zoom {
                max_zoom = zoom;
            }

            let mut properties = feat.properties;
            if !properties.iter().any(|(k, _)| k == "h3_index") {
                properties.push((Cow::Borrowed("h3_index"), MvtValue::UInt(feat.h3_index)));
            }
            if !properties.iter().any(|(k, _)| k == "h3_hex") {
                properties.push((
                    Cow::Borrowed("h3_hex"),
                    MvtValue::from_hex_u64(feat.h3_index),
                ));
            }
            if !properties.iter().any(|(k, _)| k == "resolution") {
                properties.push((Cow::Borrowed("resolution"), MvtValue::UInt(res_u8 as u64)));
            }

            accumulator.add_hexagon_mercator(
                feat.h3_index,
                center_merc,
                vertices_merc,
                zoom,
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

        let metadata = build_pmtiles_metadata(
            "h3_pmtiles_export",
            "H3 vector hexagon tile pyramid exported by raster_h3",
            min_zoom,
            max_zoom,
            "h3_hexagons",
            "H3 hexagonal vector polygons with attributes",
            build_default_pmtiles_fields(),
            None,
        );

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [
                global_min_lon,
                global_min_lat,
                global_max_lon,
                global_max_lat,
            ],
            metadata.to_string(),
        )?;

        let total_tiles = accumulator.active_tile_count();
        accumulator.flush_all(&mut writer)?;

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
        let property_filter = properties
            .map(PropertyFilter::parse)
            .unwrap_or_else(PropertyFilter::all);
        let needs_stddev = property_filter.needs_stddev();

        let resolutions = streamer.resolution_u8s().to_vec();
        let min_res = resolutions.iter().copied().min().unwrap_or(0);
        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;

        for &res in &resolutions {
            let zooms = zooms_for_h3_res(res, min_res);
            for &z in &zooms {
                if z < min_zoom {
                    min_zoom = z;
                }
                if z > max_zoom {
                    max_zoom = z;
                }
            }
        }

        let max_cell_radius = resolutions
            .iter()
            .map(|&r| max_hex_radius_deg(r))
            .fold(0.0f64, f64::max);
        let safety_margin = 2.5 * max_cell_radius;

        let mut accumulator =
            TilePyramidAccumulator::new(safety_margin).with_filter(property_filter.clone());

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
        let producer_handle = std::thread::spawn(move || -> crate::error::Result<()> {
            loop {
                let mut records = Vec::with_capacity(8192);
                streamer.drain_completed_into(8192, |_i, record| {
                    records.push(record);
                })?;
                if records.is_empty() {
                    break;
                }
                let lat_horizon = streamer.current_lat_horizon();
                if tx
                    .send(ContinuousStreamBatch {
                        records,
                        lat_horizon,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(())
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

                    let stddev = if needs_stddev {
                        accumulator.stddev()
                    } else {
                        0.0
                    };

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
                                    let p_center_merc =
                                        MercatorPoint::from_lat_lng(p_center.lat(), p_center.lng());
                                    let (p_v_merc, p_v_count) = cell_boundary_mercator(parent_cell);
                                    let p_vertices_merc = &p_v_merc[..p_v_count];

                                    let parent_stddev = if needs_stddev {
                                        accumulator.stddev()
                                    } else {
                                        0.0
                                    };
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

                                    let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(
                                        p_center_merc,
                                        p_vertices_merc,
                                        zoom,
                                    );
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

                        let (min_tx, max_tx, min_ty, max_ty) =
                            cell_tile_range_mercator(center_merc, vertices_merc, zoom);
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

                if hex.c_lon < global_min_lon {
                    global_min_lon = hex.c_lon;
                }
                if hex.c_lon > global_max_lon {
                    global_max_lon = hex.c_lon;
                }
                if hex.c_lat < global_min_lat {
                    global_min_lat = hex.c_lat;
                }
                if hex.c_lat > global_max_lat {
                    global_max_lat = hex.c_lat;
                }

                for op in hex.ops {
                    accumulator.add_op(op.tile_key, op.feature, op.is_parent);
                }

                total_hexagons += 1;
            }

            accumulator.evict_and_write_tiles(batch.lat_horizon, &mut writer)?;
        }

        match producer_handle.join() {
            Ok(result) => result?,
            Err(e) => return Err(format!("Producer thread panicked: {:?}", e).into()),
        }

        accumulator.flush_all(&mut writer)?;

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
            if property_filter.has_continuous(PROP_H3_INDEX) {
                m.insert("h3_index".to_string(), json!("Number"));
            }
            if property_filter.has_continuous(PROP_H3_HEX) {
                m.insert("h3_hex".to_string(), json!("String"));
            }
            if property_filter.has_continuous(PROP_RESOLUTION) {
                m.insert("resolution".to_string(), json!("Number"));
            }
            if property_filter.has_continuous(PROP_MEAN) {
                m.insert("mean".to_string(), json!("Number"));
            }
            if property_filter.has_continuous(PROP_SUM) {
                m.insert("sum".to_string(), json!("Number"));
            }
            if property_filter.has_continuous(PROP_STDDEV) {
                m.insert("stddev".to_string(), json!("Number"));
            }
            if property_filter.has_continuous(PROP_COUNT) {
                m.insert("count".to_string(), json!("Number"));
            }
            if property_filter.has_continuous(PROP_MIN) {
                m.insert("min".to_string(), json!("Number"));
            }
            if property_filter.has_continuous(PROP_MAX) {
                m.insert("max".to_string(), json!("Number"));
            }
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

        let metadata = build_pmtiles_metadata(
            "raster_h3_pmtiles",
            "Multi-resolution H3 hexagonal vector tile pyramid generated by raster_h3",
            min_zoom,
            max_zoom,
            "h3_hexagons",
            "Aggregated H3 hexagonal grid cells",
            fields_json,
            Some({
                let mut extras = serde_json::Map::new();
                extras.insert("h3_resolution_stats".to_string(), json!(res_stats_json));
                extras
            }),
        );

        writer.set_metadata(
            [
                global_min_lon,
                global_min_lat,
                global_max_lon,
                global_max_lat,
            ],
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
        let property_filter = properties
            .map(PropertyFilter::parse)
            .unwrap_or_else(PropertyFilter::all);
        let needs_entropy = property_filter.needs_entropy();

        let resolutions = streamer.resolution_u8s().to_vec();
        let min_res = resolutions.iter().copied().min().unwrap_or(0);

        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;

        for &res in &resolutions {
            let zooms = zooms_for_h3_res(res, min_res);
            for &z in &zooms {
                if z < min_zoom {
                    min_zoom = z;
                }
                if z > max_zoom {
                    max_zoom = z;
                }
            }
        }

        let max_cell_radius = resolutions
            .iter()
            .map(|&r| max_hex_radius_deg(r))
            .fold(0.0f64, f64::max);
        let safety_margin = 2.5 * max_cell_radius;

        let mut accumulator =
            TilePyramidAccumulator::new(safety_margin).with_filter(property_filter.clone());

        let mut total_hexagons = 0usize;
        let mut global_min_lon = 180.0f64;
        let mut global_min_lat = 90.0f64;
        let mut global_max_lon = -180.0f64;
        let mut global_max_lat = -90.0f64;

        let mut res_cell_counts: HashMap<u8, usize, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut res_purity_sums: HashMap<u8, f64, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut res_class_counts: HashMap<u8, HashMap<i64, u64, FxBuildHasher>, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut res_entropy_sums: HashMap<u8, f64, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut res_distinct_sums: HashMap<u8, f64, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());
        let mut res_pixel_sums: HashMap<u8, f64, FxBuildHasher> =
            HashMap::with_hasher(FxBuildHasher::default());

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [-180.0, -90.0, 180.0, 90.0],
            String::new(),
        )?;

        let (tx, rx) = std::sync::mpsc::sync_channel::<CategoricalStreamBatch>(4);
        let producer_handle = std::thread::spawn(move || -> crate::error::Result<()> {
            loop {
                let mut records = Vec::with_capacity(8192);
                streamer.drain_completed_into(8192, |_i, record| {
                    records.push(record);
                })?;
                if records.is_empty() {
                    break;
                }
                let lat_horizon = streamer.current_lat_horizon();
                if tx
                    .send(CategoricalStreamBatch {
                        records,
                        lat_horizon,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(())
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
                    let entropy = if needs_entropy {
                        accumulator.shannon_entropy()
                    } else {
                        0.0
                    };
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
                                    let p_center_merc =
                                        MercatorPoint::from_lat_lng(p_center.lat(), p_center.lng());
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

                                    let (min_tx, max_tx, min_ty, max_ty) = cell_tile_range_mercator(
                                        p_center_merc,
                                        p_vertices_merc,
                                        zoom,
                                    );
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

                        let (min_tx, max_tx, min_ty, max_ty) =
                            cell_tile_range_mercator(center_merc, vertices_merc, zoom);
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
                *res_distinct_sums.entry(hex.resolution).or_insert(0.0) +=
                    hex.distinct_classes as f64;
                *res_pixel_sums.entry(hex.resolution).or_insert(0.0) += hex.pixel_count;

                let class_map = res_class_counts
                    .entry(hex.resolution)
                    .or_insert_with(|| HashMap::with_hasher(FxBuildHasher::default()));
                hex.accumulator.for_each_class(|cls, cnt| {
                    *class_map.entry(cls).or_insert(0) += cnt as u64;
                });

                if hex.c_lon < global_min_lon {
                    global_min_lon = hex.c_lon;
                }
                if hex.c_lon > global_max_lon {
                    global_max_lon = hex.c_lon;
                }
                if hex.c_lat < global_min_lat {
                    global_min_lat = hex.c_lat;
                }
                if hex.c_lat > global_max_lat {
                    global_max_lat = hex.c_lat;
                }

                for op in hex.ops {
                    accumulator.add_op(op.tile_key, op.feature, op.is_parent);
                }

                total_hexagons += 1;
            }

            accumulator.evict_and_write_tiles(batch.lat_horizon, &mut writer)?;
        }

        match producer_handle.join() {
            Ok(result) => result?,
            Err(e) => return Err(format!("Producer thread panicked: {:?}", e).into()),
        }

        accumulator.flush_all(&mut writer)?;

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

            let avg_purity = if cell_count > 0 {
                purity_sum / cell_count as f64
            } else {
                0.0
            };
            let avg_entropy = if cell_count > 0 {
                entropy_sum / cell_count as f64
            } else {
                0.0
            };
            let avg_distinct = if cell_count > 0 {
                distinct_sum / cell_count as f64
            } else {
                0.0
            };
            let avg_pixels = if cell_count > 0 {
                pixel_sum / cell_count as f64
            } else {
                0.0
            };

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
            if property_filter.has_categorical(PROP_CAT_H3_INDEX) {
                m.insert("h3_index".to_string(), json!("Number"));
            }
            if property_filter.has_categorical(PROP_CAT_H3_HEX) {
                m.insert("h3_hex".to_string(), json!("String"));
            }
            if property_filter.has_categorical(PROP_CAT_RESOLUTION) {
                m.insert("resolution".to_string(), json!("Number"));
            }
            if property_filter.has_categorical(PROP_CAT_MAJORITY) {
                m.insert("majority".to_string(), json!("Number"));
            }
            if property_filter.has_categorical(PROP_CAT_MAJORITY_FRACTION) {
                m.insert("majority_fraction".to_string(), json!("Number"));
            }
            if property_filter.has_categorical(PROP_CAT_DISTINCT_CLASSES) {
                m.insert("distinct_classes".to_string(), json!("Number"));
            }
            if property_filter.has_categorical(PROP_CAT_ENTROPY) {
                m.insert("entropy".to_string(), json!("Number"));
            }
            if property_filter.has_categorical(PROP_CAT_COUNT) {
                m.insert("count".to_string(), json!("Number"));
            }
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
        let metadata = build_pmtiles_metadata(
            "raster_h3_categorical_pmtiles",
            "Multi-resolution H3 categorical vector tile pyramid generated by raster_h3",
            min_zoom,
            max_zoom,
            "h3_hexagons",
            "Aggregated H3 hexagonal grid cells",
            fields_json,
            Some({
                let mut extras = serde_json::Map::new();
                extras.insert("format".to_string(), json!("pbf"));
                extras.insert("type".to_string(), json!("overlay"));
                extras.insert("dataset_type".to_string(), json!("categorical"));
                extras.insert("h3_resolution_stats".to_string(), json!(res_stats_json));
                extras
            }),
        );

        writer.set_metadata(
            [
                global_min_lon,
                global_min_lat,
                global_max_lon,
                global_max_lat,
            ],
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

        if resolved_paths.len() == 1
            && !crate::raster::http_range::is_remote_url(resolved_paths[0].to_str().unwrap_or(""))
        {
            let reader = GeoTiffStreamReader::open(&resolved_paths[0])?;
            let streamer = MultiCategoricalHorizonStreamer::new(reader, &config)?;
            Self::generate_from_categorical_streamer_with_properties(
                streamer,
                pmtiles_path,
                props.as_deref(),
            )
        } else {
            let mosaic = std::sync::Arc::new(crate::raster::mosaic::MosaicReader::open(
                &resolved_paths,
                config.bbox,
                config.custom_crs.as_deref(),
                config.overlap_rule,
            )?);
            let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)?;
            Self::generate_from_categorical_streamer_with_properties(
                streamer,
                pmtiles_path,
                props.as_deref(),
            )
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

        if resolved_paths.len() == 1
            && !crate::raster::http_range::is_remote_url(resolved_paths[0].to_str().unwrap_or(""))
        {
            let reader = GeoTiffStreamReader::open(&resolved_paths[0])?;
            let streamer = MultiScanHorizonStreamer::new(reader, &config)?;
            Self::generate_from_continuous_streamer_with_properties(
                streamer,
                pmtiles_path,
                props.as_deref(),
            )
        } else {
            let mosaic = std::sync::Arc::new(crate::raster::mosaic::MosaicReader::open(
                &resolved_paths,
                config.bbox,
                config.custom_crs.as_deref(),
                config.overlap_rule,
            )?);
            let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?;
            Self::generate_from_continuous_streamer_with_properties(
                streamer,
                pmtiles_path,
                props.as_deref(),
            )
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
    /// Convert any H3-indexed Parquet file directly into a PMTiles v3 archive with streaming horizon eviction
    #[inline]
    pub fn process_parquet_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
        parquet_path: P1,
        pmtiles_path: P2,
        h3_column_name: Option<&str>,
    ) -> Result<PmtilesExportSummary, Box<dyn std::error::Error + Send + Sync>> {
        crate::pmtiles::parquet_tiler::process_parquet_to_pmtiles(
            parquet_path,
            pmtiles_path,
            h3_column_name,
        )
    }
}
