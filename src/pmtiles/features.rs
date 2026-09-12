//! PMTiles Feature Definitions, Layer Metadata, and Export Summaries.

use std::borrow::Cow;
use serde_json::json;

use crate::aggregator::accumulator::H3Accumulator;
use crate::pmtiles::mvt::MvtValue;

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
