//! PMTiles Feature Definitions, Layer Metadata, and Export Summaries.

use fxhash::FxBuildHasher;
use rayon::prelude::*;
use serde_json::json;
use std::borrow::Cow;
use std::collections::{BinaryHeap, HashMap};

use crate::aggregator::accumulator::H3Accumulator;
use crate::pmtiles::mvt::{MercatorPoint, MvtFeature, MvtLayer, MvtValue, PropertyFilter};
use crate::pmtiles::pyramid::{cell_tile_range_mercator, tile_xy_to_bbox};
use crate::pmtiles::writer::PmtilesWriter;

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

    pub fn record(&mut self, acc: &H3Accumulator) {
        self.cell_count += 1;
        let mean = acc.mean();
        let sum = acc.sum;
        let max = acc.max;
        let min = acc.min;
        let count = acc.count;
        let stddev = acc.stddev();

        if !mean.is_nan() {
            if mean < self.min_mean {
                self.min_mean = mean;
            }
            if mean > self.max_mean {
                self.max_mean = mean;
            }
            self.total_mean += mean;
        }

        if !sum.is_nan() {
            if sum < self.min_sum {
                self.min_sum = sum;
            }
            if sum > self.max_sum {
                self.max_sum = sum;
            }
            self.total_sum += sum;
        }

        if !max.is_nan() {
            if max < self.min_max {
                self.min_max = max;
            }
            if max > self.max_max {
                self.max_max = max;
            }
        }

        if !min.is_nan() {
            if min < self.min_min {
                self.min_min = min;
            }
            if min > self.max_min {
                self.max_min = min;
            }
        }

        if !count.is_nan() {
            if count < self.min_count {
                self.min_count = count;
            }
            if count > self.max_count {
                self.max_count = count;
            }
            self.total_pixel_count += count;
        }

        if !stddev.is_nan() {
            if stddev < self.min_stddev {
                self.min_stddev = stddev;
            }
            if stddev > self.max_stddev {
                self.max_stddev = stddev;
            }
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

/// Priority queue entry for tile eviction ordered by southernmost latitude
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TileEvictionEntry {
    pub safe_evict_lat: f64,
    pub tile_key: (u8, u32, u32),
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

/// Unified accumulator and eviction manager for vector tile pyramids
pub struct TilePyramidAccumulator {
    pub tile_buckets: HashMap<(u8, u32, u32), MvtLayer, FxBuildHasher>,
    pub tile_eviction_queue: BinaryHeap<TileEvictionEntry>,
    pub safety_margin: f64,
    pub layer_name: Cow<'static, str>,
    pub property_filter: Option<PropertyFilter>,
}

impl TilePyramidAccumulator {
    /// Create a new tile pyramid accumulator with a safety margin (in degrees)
    pub fn new(safety_margin: f64) -> Self {
        Self::with_layer("h3_hexagons", safety_margin)
    }

    /// Create a new tile pyramid accumulator with a custom layer name and safety margin
    pub fn with_layer<S: Into<Cow<'static, str>>>(layer_name: S, safety_margin: f64) -> Self {
        Self {
            tile_buckets: HashMap::with_hasher(FxBuildHasher::default()),
            tile_eviction_queue: BinaryHeap::new(),
            safety_margin,
            layer_name: layer_name.into(),
            property_filter: None,
        }
    }

    /// Configure property filtering for all vector tile layers created by this accumulator
    pub fn with_filter(mut self, filter: PropertyFilter) -> Self {
        self.property_filter = Some(filter);
        self
    }

    /// Retrieve an existing layer or create a new one, registering its southernmost latitude in the eviction queue
    #[inline]
    pub fn get_or_create_layer(&mut self, tile_key: (u8, u32, u32)) -> &mut MvtLayer {
        if self.tile_buckets.contains_key(&tile_key) {
            self.tile_buckets.get_mut(&tile_key).unwrap()
        } else {
            let bbox = tile_xy_to_bbox(tile_key.0, tile_key.1, tile_key.2);
            let safe_evict_lat = bbox[1] - self.safety_margin;
            self.tile_eviction_queue.push(TileEvictionEntry {
                safe_evict_lat,
                tile_key,
            });
            let layer_name = self.layer_name.clone();
            let filter_opt = self.property_filter.clone();
            self.tile_buckets
                .entry(tile_key)
                .or_insert_with(|| match filter_opt {
                    Some(filter) => MvtLayer::with_filter(&layer_name, filter),
                    None => MvtLayer::new(&layer_name),
                })
        }
    }

    /// Add an MVT feature operation directly into a target tile layer, merging if parent
    #[inline]
    pub fn add_op(&mut self, tile_key: (u8, u32, u32), feature: MvtFeature, is_parent: bool) {
        let layer = self.get_or_create_layer(tile_key);
        if is_parent {
            layer.add_or_merge_feature(feature);
        } else {
            layer.add_feature(feature);
        }
    }

    /// Add an H3 hexagon feature with precomputed Mercator vertices across intersecting tiles
    pub fn add_hexagon_mercator(
        &mut self,
        h3_u64: u64,
        center_merc: MercatorPoint,
        vertices_merc: &[MercatorPoint],
        zoom: u8,
        mut properties: Vec<(Cow<'static, str>, MvtValue)>,
    ) {
        let (min_tx, max_tx, min_ty, max_ty) =
            cell_tile_range_mercator(center_merc, vertices_merc, zoom);
        if min_tx == max_tx && min_ty == max_ty {
            let layer = self.get_or_create_layer((zoom, min_tx, min_ty));
            layer.add_hexagon_mercator(h3_u64, vertices_merc, zoom, min_tx, min_ty, properties);
        } else {
            for tx in min_tx..=max_tx {
                for ty in min_ty..=max_ty {
                    let is_last = tx == max_tx && ty == max_ty;
                    let props = if is_last {
                        std::mem::take(&mut properties)
                    } else {
                        properties.clone()
                    };
                    let layer = self.get_or_create_layer((zoom, tx, ty));
                    layer.add_hexagon_mercator(h3_u64, vertices_merc, zoom, tx, ty, props);
                }
            }
        }
    }

    /// Add a prepared MVT feature operation directly into a target tile layer
    #[inline]
    pub fn add_prepared_op(&mut self, tile_key: (u8, u32, u32), feature: MvtFeature) {
        let layer = self.get_or_create_layer(tile_key);
        layer.add_feature(feature);
    }

    /// Evict all tiles whose southernmost reach is strictly north of lat_horizon,
    /// encode to MVT protobuf and Gzip compress across Rayon workers, and stream to PMTiles
    pub fn evict_and_write_tiles(
        &mut self,
        lat_horizon: f64,
        writer: &mut PmtilesWriter,
    ) -> std::io::Result<usize> {
        let mut ready_tiles = Vec::new();
        while let Some(top) = self.tile_eviction_queue.peek() {
            if top.safe_evict_lat > lat_horizon {
                let entry = self.tile_eviction_queue.pop().unwrap();
                if let Some(layer) = self.tile_buckets.remove(&entry.tile_key) {
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
            .collect::<std::io::Result<Vec<_>>>()?;

        for ((z, x, y), compressed_bytes) in compressed_batch {
            writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
        }

        Ok(count)
    }

    /// Flush all remaining active tiles at completion in parallel across Rayon workers
    pub fn flush_all(self, writer: &mut PmtilesWriter) -> std::io::Result<usize> {
        let remaining_tiles: Vec<((u8, u32, u32), MvtLayer)> =
            self.tile_buckets.into_iter().collect();
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
            .collect::<std::io::Result<Vec<_>>>()?;

        for ((z, x, y), compressed_bytes) in compressed_batch {
            writer.add_compressed_tile(z, x, y, &compressed_bytes)?;
        }

        Ok(count)
    }

    /// Returns the number of currently active tiles buffered in memory
    #[inline]
    pub fn active_tile_count(&self) -> usize {
        self.tile_buckets.len()
    }

    /// Returns the number of pending tile eviction entries in the priority queue
    #[inline]
    pub fn eviction_queue_len(&self) -> usize {
        self.tile_eviction_queue.len()
    }
}

/// Unified PMTiles v3 metadata / TileJSON builder
pub fn build_pmtiles_metadata(
    name: &str,
    description: &str,
    min_zoom: u8,
    max_zoom: u8,
    layer_id: &str,
    layer_description: &str,
    fields: serde_json::Value,
    extra_attributes: Option<serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    let mut meta = json!({
        "name": name,
        "description": description,
        "version": "3",
        "minzoom": min_zoom,
        "maxzoom": max_zoom,
        "vector_layers": [
            {
                "id": layer_id,
                "description": layer_description,
                "minzoom": min_zoom,
                "maxzoom": max_zoom,
                "fields": fields
            }
        ]
    });

    if let Some(extras) = extra_attributes {
        if let Some(obj) = meta.as_object_mut() {
            for (k, v) in extras {
                obj.insert(k, v);
            }
        }
    }

    meta
}

/// Standard fields map for H3 vector tiles containing h3_index, h3_hex, and resolution
pub fn build_default_pmtiles_fields() -> serde_json::Value {
    json!({
        "h3_index": "Number",
        "h3_hex": "String",
        "resolution": "Number"
    })
}
