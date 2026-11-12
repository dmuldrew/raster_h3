//! Scan configuration shared by the Rust scanner and the C-ABI entry points.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiResolutionConfig, QuantileTarget, ScanPartition, SpectralFormula,
};
use raster_h3_core::aggregator::{CategoryRemapper, SamplingPattern};
use raster_h3_core::columnar::CategoricalFormat;
use raster_h3_core::raster::mosaic::OverlapRule;

/// Configuration for a Python / Polars H3 raster scan.
#[derive(Debug, Clone)]
pub struct PyScanConfig {
    pub path: String,
    pub resolutions: Vec<u8>,
    pub categorical: bool,
    pub format: CategoricalFormat,
    pub crs: Option<String>,
    pub nodata: Option<f64>,
    pub bbox: Option<[f64; 4]>,
    pub columns: Option<Vec<String>>,
    pub batch_size: usize,
    pub limit: Option<usize>,
    /// Fail the scan when a valid sample point cannot be transformed to WGS84.
    pub strict_transforms: bool,
    /// Receives the number of sample points dropped by failed transforms.
    pub transform_failure_counter: Option<Arc<AtomicU64>>,
    /// Scan only this partition of the mosaic (see `plan_partitions`). The output gains a
    /// nullable `partial_state` column; rows where it is set must be merged with
    /// `merge_partials`.
    pub partition: Option<ScanPartition>,
    /// 1-indexed raster band to aggregate.
    pub band: usize,
    /// Sub-pixel sampling pattern (default: one sample at each pixel centre).
    pub sampling: SamplingPattern,
    /// How overlapping mosaic tiles are resolved (default `Cutline`).
    pub overlap_rule: OverlapRule,
    /// Categorical only: drop cells whose majority class covers less than this fraction.
    pub min_majority_fraction: Option<f64>,
    /// Continuous only: aggregate a spectral index computed from several bands.
    pub spectral_formula: Option<SpectralFormula>,
    /// Continuous only: streaming quantile columns appended to the output.
    pub quantiles: Vec<QuantileTarget>,
    /// Categorical only: class remapping applied to every sample.
    pub remapper: Option<Arc<CategoryRemapper>>,
    /// Drop cells with fewer samples (`count`, or `total_count` for categorical scans).
    pub min_count: Option<f64>,
    /// Continuous only: drop cells whose mean is below this value.
    pub min_mean: Option<f64>,
    /// Continuous only: drop cells whose mean is above this value.
    pub max_mean: Option<f64>,
}

impl PyScanConfig {
    /// Copy the options that shape aggregated cells (filters and quantiles) onto a core
    /// config. Scans and `merge_partials` both use it, so merged boundary cells get the same
    /// filters and quantile columns as the scans' interior cells.
    pub(crate) fn apply_cell_options(&self, mr_cfg: &mut MultiResolutionConfig) {
        mr_cfg.min_majority_fraction = self.min_majority_fraction;
        mr_cfg.quantiles = self.quantiles.clone();
        mr_cfg.min_count = self.min_count;
        mr_cfg.min_mean = self.min_mean;
        mr_cfg.max_mean = self.max_mean;
    }
}

impl Default for PyScanConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            resolutions: vec![8],
            categorical: false,
            format: CategoricalFormat::Wide,
            crs: None,
            nodata: None,
            bbox: None,
            columns: None,
            batch_size: 2048,
            limit: None,
            strict_transforms: false,
            transform_failure_counter: None,
            partition: None,
            band: 1,
            sampling: SamplingPattern::default(),
            overlap_rule: OverlapRule::Cutline,
            min_majority_fraction: None,
            spectral_formula: None,
            quantiles: Vec::new(),
            remapper: None,
            min_count: None,
            min_mean: None,
            max_mean: None,
        }
    }
}
