//! Multi-Resolution H3 to PMTiles v3 Tiling Engine
//!
//! Orchestrates the streaming aggregation of GeoTIFF rasters across multiple H3 resolutions
//! and packages the resulting vector hexagons directly into a single PMTiles v3 archive.

use rayon::prelude::*;
use rustc_hash::FxBuildHasher;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer, RecordStreamer,
};
use crate::pmtiles::mvt::{
    FeatureProperties, MercatorPoint, MvtFeature, PropertyFilter, PROP_CAT_COUNT,
    PROP_CAT_DISTINCT_CLASSES, PROP_CAT_ENTROPY, PROP_CAT_H3_HEX, PROP_CAT_H3_INDEX,
    PROP_CAT_MAJORITY, PROP_CAT_MAJORITY_FRACTION, PROP_CAT_RESOLUTION, PROP_COUNT, PROP_H3_HEX,
    PROP_H3_INDEX, PROP_MAX, PROP_MEAN, PROP_MIN, PROP_RESOLUTION, PROP_STDDEV, PROP_SUM,
};
use crate::pmtiles::overview::Overview;
use crate::pmtiles::writer::PmtilesWriter;
use crate::raster::geotiff::GeoTiffStreamReader;

// Re-export tile pyramid coordinate math and zoom calculations from pyramid module
pub use crate::pmtiles::pyramid::{
    cell_boundary_mercator, cell_boundary_mercator_for, cell_tile_range, cell_tile_range_mercator,
    for_each_cell_tile_mercator, grid_res_for_zoom, grid_res_to_zoom, h3_res_for_zoom,
    h3_res_to_zoom, lon_lat_to_tile_xy, max_cell_radius_deg, max_hex_radius_deg,
    mercator_to_tile_xy, tile_xy_to_bbox, zoom_to_h3_res, zooms_for_grid_res, zooms_for_h3_res,
    Wgs84BoundsAccumulator,
};

// Re-export feature definitions, metadata, accumulator, and export summaries from features module
pub use crate::pmtiles::features::{
    build_default_pmtiles_fields, build_pmtiles_metadata, H3Feature, PmtilesExportSummary,
    ResolutionAccumulatorStats, TilePyramidAccumulator,
};

/// Intermediate prepared tile operation generated across Rayon worker threads
struct PreparedTileOp {
    tile_key: (u8, u32, u32),
    feature: MvtFeature,
    is_parent: bool,
}

/// Precomputed cell ready for thread-safe insertion into tile buckets
struct PreparedHex<K: TileKind> {
    cell_u64: u64,
    resolution: u8,
    summary: K::Summary,
    accumulator: K::Acc,
    ops: Vec<PreparedTileOp>,
}

/// Batch of records transferred from the background scanline producer thread
struct StreamBatch<R> {
    records: Vec<R>,
    lat_horizon: f64,
}

/// Archive-level metadata that differs between the two pipelines.
struct ArchiveDescription {
    name: &'static str,
    description: &'static str,
    fields_json: serde_json::Value,
    extras: serde_json::Map<String, serde_json::Value>,
}

/// What differs between the continuous and categorical PMTiles pipelines.
trait TileKind: Sync {
    type Record: Send + 'static;
    type Acc: crate::aggregator::multi_horizon::spill::SpillAccumulator + Send;
    /// Per-cell values computed once on a worker and reused for the feature
    /// properties and the run statistics.
    type Summary: Send;
    type Stats: Default;
    fn parts(record: Self::Record) -> (u8, u64, Self::Acc); // (resolution, h3_index, accumulator)
    fn summarize(&self, acc: &Self::Acc) -> Self::Summary;
    fn properties(
        &self,
        h3_index: u64,
        resolution: u8,
        acc: &Self::Acc,
        summary: &Self::Summary,
    ) -> FeatureProperties;
    fn record(stats: &mut Self::Stats, resolution: u8, acc: &Self::Acc, summary: &Self::Summary);
    /// Name, description, `fields` JSON and extra metadata (including
    /// `h3_resolution_stats`) of the archive.
    fn describe(&self, stats: Self::Stats, filter: &PropertyFilter) -> ArchiveDescription;
}

struct ContinuousTiles {
    needs_stddev: bool,
}

impl TileKind for ContinuousTiles {
    type Record = MultiContinuousRecord;
    type Acc = H3Accumulator;
    type Summary = f64; // stddev
    type Stats = HashMap<u8, ResolutionAccumulatorStats, FxBuildHasher>;

    fn parts(record: Self::Record) -> (u8, u64, Self::Acc) {
        (record.resolution, record.h3_index, record.accumulator)
    }

    fn summarize(&self, acc: &Self::Acc) -> Self::Summary {
        if self.needs_stddev {
            acc.stddev()
        } else {
            0.0
        }
    }

    fn properties(
        &self,
        h3_index: u64,
        resolution: u8,
        acc: &Self::Acc,
        summary: &Self::Summary,
    ) -> FeatureProperties {
        FeatureProperties::Continuous {
            h3_index,
            resolution,
            mean: acc.mean(),
            sum: acc.sum,
            stddev: *summary,
            count: acc.count,
            min: acc.min,
            max: acc.max,
        }
    }

    fn record(stats: &mut Self::Stats, resolution: u8, acc: &Self::Acc, _summary: &Self::Summary) {
        stats.entry(resolution).or_default().record(acc);
    }

    fn describe(&self, stats: Self::Stats, filter: &PropertyFilter) -> ArchiveDescription {
        let grid = filter.grid;
        let mut sorted_res: Vec<u8> = stats.keys().copied().collect();
        sorted_res.sort_unstable();
        let min_res_val = sorted_res.first().copied().unwrap_or(0);

        let mut res_stats_json = serde_json::Map::new();
        for res in sorted_res {
            if let Some(st) = stats.get(&res) {
                let zooms = zooms_for_grid_res(grid, res, min_res_val);
                res_stats_json.insert(res.to_string(), st.to_json(&zooms));
            }
        }

        let index_col = grid.index_column_name();
        let hex_col = grid.hex_column_name();
        let fields_json = if filter.is_custom {
            let mut m = serde_json::Map::new();
            if filter.has_continuous(PROP_H3_INDEX) {
                m.insert(index_col.to_string(), json!("Number"));
            }
            if filter.has_continuous(PROP_H3_HEX) {
                m.insert(hex_col.to_string(), json!("String"));
            }
            if filter.has_continuous(PROP_RESOLUTION) {
                m.insert("resolution".to_string(), json!("Number"));
            }
            if filter.has_continuous(PROP_MEAN) {
                m.insert("mean".to_string(), json!("Number"));
            }
            if filter.has_continuous(PROP_SUM) {
                m.insert("sum".to_string(), json!("Number"));
            }
            if filter.has_continuous(PROP_STDDEV) {
                m.insert("stddev".to_string(), json!("Number"));
            }
            if filter.has_continuous(PROP_COUNT) {
                m.insert("count".to_string(), json!("Number"));
            }
            if filter.has_continuous(PROP_MIN) {
                m.insert("min".to_string(), json!("Number"));
            }
            if filter.has_continuous(PROP_MAX) {
                m.insert("max".to_string(), json!("Number"));
            }
            serde_json::Value::Object(m)
        } else {
            let mut m = serde_json::Map::new();
            m.insert(index_col.to_string(), json!("Number"));
            m.insert(hex_col.to_string(), json!("String"));
            m.insert("resolution".to_string(), json!("Number"));
            m.insert("mean".to_string(), json!("Number"));
            m.insert("sum".to_string(), json!("Number"));
            m.insert("stddev".to_string(), json!("Number"));
            m.insert("count".to_string(), json!("Number"));
            m.insert("min".to_string(), json!("Number"));
            m.insert("max".to_string(), json!("Number"));
            serde_json::Value::Object(m)
        };

        let mut extras = serde_json::Map::new();
        extras.insert("h3_resolution_stats".to_string(), json!(res_stats_json));
        ArchiveDescription {
            name: "raster_h3_pmtiles",
            description: "Multi-resolution H3 hexagonal vector tile pyramid generated by raster_h3",
            fields_json,
            extras,
        }
    }
}

struct CategoricalSummary {
    majority: i64,
    majority_fraction: f64,
    entropy: f64,
    distinct_classes: usize,
    pixel_count: f64,
}

#[derive(Default)]
struct CategoricalTileStats {
    cell_counts: HashMap<u8, usize, FxBuildHasher>,
    purity_sums: HashMap<u8, f64, FxBuildHasher>,
    class_counts: HashMap<u8, HashMap<i64, u64, FxBuildHasher>, FxBuildHasher>,
    entropy_sums: HashMap<u8, f64, FxBuildHasher>,
    distinct_sums: HashMap<u8, f64, FxBuildHasher>,
    pixel_sums: HashMap<u8, f64, FxBuildHasher>,
}

struct CategoricalTiles {
    needs_entropy: bool,
}

impl TileKind for CategoricalTiles {
    type Record = MultiCategoricalRecord;
    type Acc = CategoricalAccumulator;
    type Summary = CategoricalSummary;
    type Stats = CategoricalTileStats;

    fn parts(record: Self::Record) -> (u8, u64, Self::Acc) {
        (record.resolution, record.h3_index, record.accumulator)
    }

    fn summarize(&self, acc: &Self::Acc) -> Self::Summary {
        let (majority, _maj_count, majority_fraction) = acc.majority();
        CategoricalSummary {
            majority,
            majority_fraction,
            entropy: if self.needs_entropy {
                acc.shannon_entropy()
            } else {
                0.0
            },
            distinct_classes: acc.unique_classes(),
            pixel_count: acc.total_count,
        }
    }

    fn properties(
        &self,
        h3_index: u64,
        resolution: u8,
        _acc: &Self::Acc,
        summary: &Self::Summary,
    ) -> FeatureProperties {
        FeatureProperties::Categorical {
            h3_index,
            resolution,
            majority: summary.majority,
            majority_fraction: summary.majority_fraction,
            distinct_classes: summary.distinct_classes as u32,
            entropy: summary.entropy,
            count: summary.pixel_count,
        }
    }

    fn record(stats: &mut Self::Stats, resolution: u8, acc: &Self::Acc, summary: &Self::Summary) {
        *stats.cell_counts.entry(resolution).or_insert(0) += 1;
        *stats.purity_sums.entry(resolution).or_insert(0.0) += summary.majority_fraction;
        *stats.entropy_sums.entry(resolution).or_insert(0.0) += summary.entropy;
        *stats.distinct_sums.entry(resolution).or_insert(0.0) += summary.distinct_classes as f64;
        *stats.pixel_sums.entry(resolution).or_insert(0.0) += summary.pixel_count;
        let class_map = stats
            .class_counts
            .entry(resolution)
            .or_insert_with(|| HashMap::with_hasher(FxBuildHasher));
        acc.for_each_class(|cls, cnt| {
            *class_map.entry(cls).or_insert(0) += cnt as u64;
        });
    }

    fn describe(&self, stats: Self::Stats, filter: &PropertyFilter) -> ArchiveDescription {
        let grid = filter.grid;
        let mut sorted_res: Vec<u8> = stats.cell_counts.keys().copied().collect();
        sorted_res.sort_unstable();
        let min_res_val = sorted_res.first().copied().unwrap_or(0);

        let mut res_stats_json = serde_json::Map::new();
        for res in sorted_res {
            let zooms = zooms_for_grid_res(grid, res, min_res_val);
            let cell_count = stats.cell_counts.get(&res).copied().unwrap_or(0);
            let purity_sum = stats.purity_sums.get(&res).copied().unwrap_or(0.0);
            let entropy_sum = stats.entropy_sums.get(&res).copied().unwrap_or(0.0);
            let distinct_sum = stats.distinct_sums.get(&res).copied().unwrap_or(0.0);
            let pixel_sum = stats.pixel_sums.get(&res).copied().unwrap_or(0.0);

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
            if let Some(cmap) = stats.class_counts.get(&res) {
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

        let index_col = grid.index_column_name();
        let hex_col = grid.hex_column_name();
        let fields_json = if filter.is_custom {
            let mut m = serde_json::Map::new();
            if filter.has_categorical(PROP_CAT_H3_INDEX) {
                m.insert(index_col.to_string(), json!("Number"));
            }
            if filter.has_categorical(PROP_CAT_H3_HEX) {
                m.insert(hex_col.to_string(), json!("String"));
            }
            if filter.has_categorical(PROP_CAT_RESOLUTION) {
                m.insert("resolution".to_string(), json!("Number"));
            }
            if filter.has_categorical(PROP_CAT_MAJORITY) {
                m.insert("majority".to_string(), json!("Number"));
            }
            if filter.has_categorical(PROP_CAT_MAJORITY_FRACTION) {
                m.insert("majority_fraction".to_string(), json!("Number"));
            }
            if filter.has_categorical(PROP_CAT_DISTINCT_CLASSES) {
                m.insert("distinct_classes".to_string(), json!("Number"));
            }
            if filter.has_categorical(PROP_CAT_ENTROPY) {
                m.insert("entropy".to_string(), json!("Number"));
            }
            if filter.has_categorical(PROP_CAT_COUNT) {
                m.insert("count".to_string(), json!("Number"));
            }
            serde_json::Value::Object(m)
        } else {
            let mut m = serde_json::Map::new();
            m.insert(index_col.to_string(), json!("Number"));
            m.insert(hex_col.to_string(), json!("String"));
            m.insert("resolution".to_string(), json!("Number"));
            m.insert("majority".to_string(), json!("Number"));
            m.insert("majority_fraction".to_string(), json!("Number"));
            m.insert("distinct_classes".to_string(), json!("Number"));
            m.insert("entropy".to_string(), json!("Number"));
            m.insert("count".to_string(), json!("Number"));
            serde_json::Value::Object(m)
        };

        let mut extras = serde_json::Map::new();
        extras.insert("format".to_string(), json!("pbf"));
        extras.insert("type".to_string(), json!("overlay"));
        extras.insert("dataset_type".to_string(), json!("categorical"));
        extras.insert("h3_resolution_stats".to_string(), json!(res_stats_json));
        ArchiveDescription {
            name: "raster_h3_categorical_pmtiles",
            description:
                "Multi-resolution H3 categorical vector tile pyramid generated by raster_h3",
            fields_json,
            extras,
        }
    }
}

/// RAII channel wrapper ensuring the background producer thread is unconditionally joined upon scope exit.
struct ProducerChannel<R, T> {
    rx: Option<std::sync::mpsc::Receiver<R>>,
    handle: Option<std::thread::JoinHandle<T>>,
}

impl<R, T> ProducerChannel<R, T> {
    fn new(rx: std::sync::mpsc::Receiver<R>, handle: std::thread::JoinHandle<T>) -> Self {
        Self {
            rx: Some(rx),
            handle: Some(handle),
        }
    }

    fn recv(&self) -> Result<R, std::sync::mpsc::RecvError> {
        self.rx.as_ref().unwrap().recv()
    }

    fn join_producer(&mut self) -> std::thread::Result<T> {
        self.rx.take();
        self.handle.take().expect("producer already joined").join()
    }
}

impl<R, T> Drop for ProducerChannel<R, T> {
    fn drop(&mut self) {
        self.rx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// PMTiles tiling assumes every record is at a requested resolution: zooms and
/// the tile eviction margin are derived from those resolutions. A compacted
/// parent record is one level coarser and about 2.65 times wider, so it would
/// exceed the margin and appear only at coarser zooms, leaving holes at the
/// requested resolutions.
fn reject_compaction<S: RecordStreamer>(
    streamer: &S,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if streamer.compacts_children() {
        return Err(
            "PMTiles output does not support H3 child compaction; disable \
                    compact_h3_children for tiling"
                .into(),
        );
    }
    Ok(())
}

/// High-level builder to convert H3 data and GeoTIFF raster aggregations directly to PMTiles v3
pub struct H3PmtilesTiler;

impl H3PmtilesTiler {
    /// Export any collection of generic H3 features (with strict H3 validation) to a PMTiles v3 archive.
    ///
    /// Features may arrive in any order. Memory stays near
    /// [`DEFAULT_EXPORT_MEMORY_BUDGET`](crate::pmtiles::DEFAULT_EXPORT_MEMORY_BUDGET):
    /// larger inputs are sorted externally through temporary files (see
    /// [`ExternalFeatureExport`](crate::pmtiles::ExternalFeatureExport), which
    /// also allows a custom budget, spill directory or output writer).
    pub fn export_h3_features<P: AsRef<Path>, I: IntoIterator<Item = H3Feature>>(
        features: I,
        output_path: P,
    ) -> Result<PmtilesExportSummary, Box<dyn std::error::Error + Send + Sync>> {
        let mut exporter = crate::pmtiles::ExternalFeatureExport::new(
            crate::pmtiles::DEFAULT_EXPORT_MEMORY_BUDGET,
            None,
        );
        for feature in features {
            exporter.push(feature)?;
        }
        Ok(exporter.finish(output_path)?)
    }
    /// Stream continuous raster data from GeoTIFF across target resolutions and write PMTiles v3 archive
    pub fn generate_from_continuous_streamer<S, P: AsRef<Path>>(
        streamer: S,
        output_path: P,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
    where
        S: RecordStreamer<Record = MultiContinuousRecord> + Send + 'static,
    {
        Self::generate_from_continuous_streamer_with_properties(streamer, output_path, None)
    }

    /// Stream continuous raster data from GeoTIFF across target resolutions and write PMTiles v3 archive with selective property filtering
    pub fn generate_from_continuous_streamer_with_properties<S, P: AsRef<Path>>(
        streamer: S,
        output_path: P,
        properties: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
    where
        S: RecordStreamer<Record = MultiContinuousRecord> + Send + 'static,
    {
        let property_filter = properties
            .map(PropertyFilter::parse)
            .unwrap_or_else(PropertyFilter::all);
        let kind = ContinuousTiles {
            needs_stddev: property_filter.needs_stddev(),
        };
        Self::generate_tiles(streamer, output_path, property_filter, kind)
    }

    /// Stream categorical raster data from GeoTIFF across target resolutions and write PMTiles v3 archive
    pub fn generate_from_categorical_streamer<S, P: AsRef<Path>>(
        streamer: S,
        output_path: P,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
    where
        S: RecordStreamer<Record = MultiCategoricalRecord> + Send + 'static,
    {
        Self::generate_from_categorical_streamer_with_properties(streamer, output_path, None)
    }

    /// Stream categorical raster data from GeoTIFF across target resolutions and write PMTiles v3 archive with selective property filtering
    pub fn generate_from_categorical_streamer_with_properties<S, P: AsRef<Path>>(
        streamer: S,
        output_path: P,
        properties: Option<&str>,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
    where
        S: RecordStreamer<Record = MultiCategoricalRecord> + Send + 'static,
    {
        let property_filter = properties
            .map(PropertyFilter::parse)
            .unwrap_or_else(PropertyFilter::all);
        let kind = CategoricalTiles {
            needs_entropy: property_filter.needs_entropy(),
        };
        Self::generate_tiles(streamer, output_path, property_filter, kind)
    }

    fn generate_tiles<S, K, P: AsRef<Path>>(
        mut streamer: S,
        output_path: P,
        property_filter: PropertyFilter,
        kind: K,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
    where
        S: RecordStreamer<Record = K::Record> + Send + 'static,
        K: TileKind,
    {
        reject_compaction(&streamer)?;
        let grid = streamer.grid();
        let property_filter = property_filter.with_grid(grid);

        let resolutions = streamer.resolution_u8s().to_vec();
        let min_res = resolutions.iter().copied().min().unwrap_or(0);
        let mut min_zoom = 255u8;
        let mut max_zoom = 0u8;

        for &res in &resolutions {
            let zooms = zooms_for_grid_res(grid, res, min_res);
            for &z in &zooms {
                if z < min_zoom {
                    min_zoom = z;
                }
                if z > max_zoom {
                    max_zoom = z;
                }
            }
        }

        // Records arrive only at the requested resolutions; reject_compaction
        // rules out coarser parent records that would exceed this margin.
        let max_cell_radius = resolutions
            .iter()
            .map(|&r| max_cell_radius_deg(grid, r))
            .fold(0.0f64, f64::max);
        let safety_margin = 2.5 * max_cell_radius;

        let mut accumulator =
            TilePyramidAccumulator::new(safety_margin).with_filter(property_filter.clone());

        let mut total_hexagons = 0usize;
        let mut bounds_acc = Wgs84BoundsAccumulator::new();

        let mut stats = K::Stats::default();
        // Cells whose per-zoom tile range exceeded the enumeration cap
        // (polar-cap cells at fine zooms); surfaced in the archive metadata.
        let capped_cells = AtomicUsize::new(0);
        let min_res_zooms = zooms_for_grid_res(grid, min_res, min_res);
        let mut coarse_res_list: Vec<u8> = min_res_zooms
            .iter()
            .map(|&z| grid_res_for_zoom(grid, z))
            .filter(|&r| r < min_res)
            .collect();
        coarse_res_list.sort_unstable();
        coarse_res_list.dedup();
        let mut coarse_parents = Overview::<K::Acc>::new(grid, &coarse_res_list);

        let mut writer = PmtilesWriter::new(
            min_zoom,
            max_zoom,
            [-180.0, -90.0, 180.0, 90.0],
            String::new(),
        )?;

        let (tx, rx) = std::sync::mpsc::sync_channel::<StreamBatch<K::Record>>(4);
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
                    .send(StreamBatch {
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
        let mut producer_channel = ProducerChannel::new(rx, producer_handle);

        while let Ok(batch) = producer_channel.recv() {
            let prepared_batch: Vec<PreparedHex<K>> = batch
                .records
                .into_par_iter()
                .filter_map(|record| {
                    let (resolution, h3_index, accumulator) = K::parts(record);
                    let summary = kind.summarize(&accumulator);

                    let (c_lat, c_lon) = grid.center_lat_lng(h3_index)?;
                    let center_merc = MercatorPoint::from_lat_lng(c_lat, c_lon);
                    let (v_merc, v_count) = cell_boundary_mercator_for(grid, h3_index)?;
                    let vertices_merc = &v_merc[..v_count];

                    let properties = kind.properties(h3_index, resolution, &accumulator, &summary);

                    let zooms = zooms_for_grid_res(grid, resolution, min_res);
                    let num_zooms = zooms.len();
                    let mut ops = Vec::with_capacity(num_zooms);

                    for &zoom in &zooms {
                        let optimal_res = grid_res_for_zoom(grid, zoom);
                        if optimal_res < resolution {
                            // Coarse overview parents are aggregated across all
                            // contributing children using bounded spill buffers.
                            continue;
                        }

                        if for_each_cell_tile_mercator(
                            center_merc,
                            vertices_merc,
                            zoom,
                            |tx, ty| {
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
                            },
                        ) == 0
                        {
                            capped_cells.fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    Some(PreparedHex {
                        cell_u64: h3_index,
                        resolution,
                        summary,
                        accumulator,
                        ops,
                    })
                })
                .collect();

            for hex in prepared_batch {
                K::record(&mut stats, hex.resolution, &hex.accumulator, &hex.summary);

                if hex.resolution == min_res {
                    coarse_parents.push(hex.cell_u64, &hex.accumulator)?;
                }

                bounds_acc.record_cell_for(grid, hex.cell_u64);

                for op in hex.ops {
                    accumulator.add_op(op.tile_key, op.feature, op.is_parent);
                }

                total_hexagons += 1;
            }

            accumulator.evict_and_write_tiles(batch.lat_horizon, &mut writer)?;
        }

        match producer_channel.join_producer() {
            Ok(result) => result?,
            Err(e) => return Err(format!("Producer thread panicked: {:?}", e).into()),
        }

        let overview_capped = coarse_parents.write(
            &min_res_zooms,
            &property_filter,
            &mut writer,
            &mut bounds_acc,
            |id, res, acc| {
                let s = kind.summarize(acc);
                kind.properties(id, res, acc, &s)
            },
        )?;
        capped_cells.fetch_add(overview_capped, Ordering::Relaxed);

        accumulator.flush_all(&mut writer)?;

        if total_hexagons == 0 {
            min_zoom = 0;
            max_zoom = 0;
        }
        let bbox = bounds_acc.finish([-180.0, -90.0, 180.0, 90.0]);

        // Build vector layer JSON metadata for MapLibre / Web Vector Clients
        let desc = kind.describe(stats, &property_filter);
        let metadata = build_pmtiles_metadata(
            desc.name,
            desc.description,
            min_zoom,
            max_zoom,
            "h3_hexagons",
            "Aggregated H3 hexagonal grid cells",
            desc.fields_json,
            Some({
                let mut extras = desc.extras;
                let skipped = capped_cells.load(Ordering::Relaxed);
                if skipped > 0 {
                    extras.insert("h3_cells_over_tile_cap".to_string(), json!(skipped));
                }
                extras
            }),
        );

        writer.set_metadata(bbox, metadata.to_string());

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
            let mosaic = std::sync::Arc::new(
                crate::raster::mosaic::MosaicReader::open_with_remote_source_factory(
                    &resolved_paths,
                    config.bbox,
                    config.custom_crs.as_deref(),
                    config.overlap_rule,
                    config.remote_source_factory.as_ref(),
                )?,
            );
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
            let mosaic = std::sync::Arc::new(
                crate::raster::mosaic::MosaicReader::open_with_remote_source_factory(
                    &resolved_paths,
                    config.bbox,
                    config.custom_crs.as_deref(),
                    config.overlap_rule,
                    config.remote_source_factory.as_ref(),
                )?,
            );
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
        crate::transcode::process_parquet_to_pmtiles(parquet_path, pmtiles_path, h3_column_name)
    }
}
