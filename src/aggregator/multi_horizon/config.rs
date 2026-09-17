//! Configuration for multi-resolution H3 aggregation.
//!
//! Defines [`MultiResolutionConfig`] and supporting types for multi-resolution H3
//! raster processing, including resolution ranges, spectral indices ([`SpectralFormula`]),
//! streaming quantile and percentile targets ([`QuantileTarget`]), and category remapping.

use std::sync::Arc;

use crate::aggregator::remap::CategoryRemapper;
use crate::aggregator::sampling::SamplingPattern;
use crate::error::{RasterH3Error, Result};
use crate::raster::mosaic::OverlapRule;

pub use super::spectral::SpectralFormula;

/// Quantile target specification (percentile in [0.0, 1.0] or interquartile range)
#[derive(Debug, Clone, PartialEq)]
pub enum QuantileTarget {
    /// A specific percentile (e.g. 0.50 for P50), along with its output column name.
    Percentile(f64, String),
    /// Interquartile range (IQR = P75 - P25), along with its output column name.
    Iqr(String),
}

impl QuantileTarget {
    pub fn column_name(&self) -> &str {
        match self {
            Self::Percentile(_, name) => name,
            Self::Iqr(name) => name,
        }
    }

    /// Parse a comma- or space-separated list of quantile targets or presets.
    ///
    /// Presets:
    /// - `"true"` / `"all"` / `"default"` -> p50, p90, p95, p99, iqr
    /// - `"box"` -> p25, p50, p75, iqr
    /// - `"tails"` -> p01, p05, p10, p90, p95, p99
    /// - `"deciles"` -> p10, p20, p30, p40, p50, p60, p70, p80, p90
    /// - `"false"` / `"none"` / `"off"` -> none
    ///
    /// Individual items:
    /// - Percentiles: `p50`, `p95`, `p01`, `p5`, `p99.5`, `p99_5`
    /// - Decimals: `0.5`, `0.05`, `0.95`, `0.99`
    /// - Aliases: `median`, `q1`, `q2`, `q3`, `iqr`
    pub fn parse_list(s: &str) -> Result<Vec<Self>> {
        let mut targets = Vec::new();
        for raw_token in s.split(|c: char| c == ',' || c.is_whitespace()) {
            let token = raw_token.trim().to_lowercase();
            if token.is_empty() {
                continue;
            }
            match token.as_str() {
                "false" | "none" | "off" => {}
                "true" | "all" | "default" => {
                    targets.push(Self::Percentile(0.50, "p50".to_string()));
                    targets.push(Self::Percentile(0.90, "p90".to_string()));
                    targets.push(Self::Percentile(0.95, "p95".to_string()));
                    targets.push(Self::Percentile(0.99, "p99".to_string()));
                    targets.push(Self::Iqr("iqr".to_string()));
                }
                "box" => {
                    targets.push(Self::Percentile(0.25, "p25".to_string()));
                    targets.push(Self::Percentile(0.50, "p50".to_string()));
                    targets.push(Self::Percentile(0.75, "p75".to_string()));
                    targets.push(Self::Iqr("iqr".to_string()));
                }
                "tails" => {
                    targets.push(Self::Percentile(0.01, "p01".to_string()));
                    targets.push(Self::Percentile(0.05, "p05".to_string()));
                    targets.push(Self::Percentile(0.10, "p10".to_string()));
                    targets.push(Self::Percentile(0.90, "p90".to_string()));
                    targets.push(Self::Percentile(0.95, "p95".to_string()));
                    targets.push(Self::Percentile(0.99, "p99".to_string()));
                }
                "deciles" => {
                    for d in 1..=9 {
                        let pct = d * 10;
                        let q = pct as f64 / 100.0;
                        let name = format!("p{}", pct);
                        targets.push(Self::Percentile(q, name));
                    }
                }
                "iqr" => {
                    targets.push(Self::Iqr("iqr".to_string()));
                }
                "median" => {
                    targets.push(Self::Percentile(0.50, "median".to_string()));
                }
                "q1" => {
                    targets.push(Self::Percentile(0.25, "q1".to_string()));
                }
                "q2" => {
                    targets.push(Self::Percentile(0.50, "q2".to_string()));
                }
                "q3" => {
                    targets.push(Self::Percentile(0.75, "q3".to_string()));
                }
                _ => {
                    if let Some(stripped) = token.strip_prefix('p') {
                        let num_str = stripped.replace('_', ".");
                        let val: f64 = num_str.parse().map_err(|_| {
                            RasterH3Error::InvalidParameter(format!(
                                "Invalid percentile specification '{}'",
                                raw_token
                            ))
                        })?;
                        if val <= 0.0 || val >= 100.0 {
                            return Err(RasterH3Error::InvalidParameter(format!(
                                "Percentile '{}' must be strictly between 0 and 100",
                                raw_token
                            )));
                        }
                        let q = val / 100.0;
                        let name = if (val.round() - val).abs() < 1e-6 {
                            format!("p{:02}", val.round() as u32)
                        } else {
                            format!("p{}", val).replace('.', "_")
                        };
                        targets.push(Self::Percentile(q, name));
                    } else if let Ok(val) = token.parse::<f64>() {
                        if val <= 0.0 || val >= 1.0 {
                            return Err(RasterH3Error::InvalidParameter(format!(
                                "Decimal quantile '{}' must be strictly between 0 and 1",
                                raw_token
                            )));
                        }
                        let pct = val * 100.0;
                        let name = if (pct.round() - pct).abs() < 1e-6 {
                            format!("p{:02}", pct.round() as u32)
                        } else {
                            format!("p{}", pct).replace('.', "_")
                        };
                        targets.push(Self::Percentile(val, name));
                    } else {
                        return Err(RasterH3Error::InvalidParameter(format!(
                            "Unrecognized quantile/percentile target '{}'",
                            raw_token
                        )));
                    }
                }
            }
        }

        // Deduplicate while preserving order
        let mut seen = std::collections::HashSet::new();
        let mut deduped = Vec::new();
        for t in targets {
            if seen.insert(t.column_name().to_string()) {
                deduped.push(t);
            }
        }
        Ok(deduped)
    }
}

/// Configuration for multi-resolution aggregation
#[derive(Debug, Clone)]
pub struct MultiResolutionConfig {
    /// Sorted list of target H3 resolution levels.
    pub resolutions: Vec<u8>,
    /// 1-indexed raster band to extract.
    pub band: usize,
    /// Optional user-specified NoData override.
    pub custom_nodata: Option<f64>,
    /// Optional spatial bounding box for chunk-level pruning.
    pub bbox: Option<[f64; 4]>,
    /// Sub-pixel super-sampling pattern.
    pub sampling: SamplingPattern,
    /// Optional CRS override string (e.g. `"EPSG:4326"`).
    pub custom_crs: Option<String>,
    /// Optional property whitelist for selective output.
    pub properties: Option<String>,
    /// Optional spectral index formula for multi-band computation.
    pub spectral_formula: Option<SpectralFormula>,
    /// Minimum pixel count threshold for output.
    pub min_count: Option<f64>,
    /// Minimum mean value filter.
    pub min_mean: Option<f64>,
    /// Maximum mean value filter.
    pub max_mean: Option<f64>,
    /// Minimum majority class fraction for categorical filtering.
    pub min_majority_fraction: Option<f64>,
    /// Whether to compact 7 child H3 cells into parent cells hierarchically.
    pub compact_h3_children: bool,
    /// Legacy adapter for `compact_h3_children`.
    pub compact: bool,
    /// Mosaic overlap resolution strategy.
    pub overlap_rule: OverlapRule,
    /// List of quantile/percentile targets to compute.
    pub quantiles: Vec<QuantileTarget>,
    /// Optional category remapping configuration.
    pub remapper: Option<Arc<CategoryRemapper>>,
}

impl MultiResolutionConfig {
    /// Create a new multi-resolution configuration
    pub fn new(resolutions: Vec<u8>) -> Self {
        Self {
            resolutions,
            band: 1,
            custom_nodata: None,
            bbox: None,
            sampling: SamplingPattern::center(),
            custom_crs: None,
            properties: None,
            spectral_formula: None,
            min_count: None,
            min_mean: None,
            max_mean: None,
            min_majority_fraction: None,
            compact_h3_children: false,
            compact: false,
            overlap_rule: OverlapRule::default(),
            quantiles: Vec::new(),
            remapper: None,
        }
    }

    /// Create a single-resolution configuration (convenience constructor)
    pub fn single(resolution: u8) -> Self {
        Self::new(vec![resolution])
    }

    /// Whether hierarchical 7-cell compaction is enabled (checking both new and legacy fields)
    #[inline(always)]
    pub fn should_compact_h3_children(&self) -> bool {
        self.compact_h3_children || self.compact
    }

    /// Check whether streaming quantile calculations are enabled
    #[inline(always)]
    pub fn track_quantiles(&self) -> bool {
        !self.quantiles.is_empty()
    }

    /// Validate configuration parameters and invariants.
    ///
    /// Hierarchical compaction (`compact_h3_children`) merges 7 fine-resolution child cells
    /// into their coarser parent cell. When adjacent resolutions (e.g. [7, 8]) are both requested
    /// and compaction is active, the child resolution (8) would emit parent cells (7) that collide
    /// and duplicate records from the requested parent resolution (7). Therefore, adjacent
    /// resolutions cannot be combined with hierarchical compaction.
    pub fn validate(&self) -> Result<()> {
        if self.resolutions.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "Resolutions list cannot be empty".to_string(),
            ));
        }
        for &r in &self.resolutions {
            if r > 15 {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Invalid H3 resolution: {}",
                    r
                )));
            }
        }
        if self.should_compact_h3_children() {
            let requested: std::collections::HashSet<u8> =
                self.resolutions.iter().copied().collect();
            if let Some(&child) = self
                .resolutions
                .iter()
                .find(|&&res| res > 0 && requested.contains(&(res - 1)))
            {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "compact output cannot combine H3 resolutions {} and {}: compaction would emit duplicate parent cells",
                    child - 1,
                    child
                )));
            }
        }
        Ok(())
    }
}

impl Default for MultiResolutionConfig {
    fn default() -> Self {
        Self::new(vec![8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let cfg = MultiResolutionConfig::default();
        assert_eq!(cfg.resolutions, vec![8]);
        assert_eq!(cfg.quantiles.len(), 0);
        assert!(cfg.spectral_formula.is_none());
    }

    #[test]
    fn test_quantile_target() {
        let qt = QuantileTarget::Percentile(0.9, "p90".into());
        assert_eq!(qt.column_name(), "p90");
        let iqr = QuantileTarget::Iqr("iqr".into());
        assert_eq!(iqr.column_name(), "iqr");
    }
}
