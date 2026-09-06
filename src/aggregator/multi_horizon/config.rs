use std::sync::Arc;

use crate::aggregator::remap::CategoryRemapper;
use crate::aggregator::sampling::SamplingPattern;
use crate::error::{RasterH3Error, Result};
use crate::raster::mosaic::OverlapRule;

/// Supported on-the-fly spectral index formulas
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpectralFormula {
    Ndvi { nir_band: usize, red_band: usize },
    Ndwi { green_band: usize, nir_band: usize },
    Nbr { nir_band: usize, swir_band: usize },
    Evi { nir_band: usize, red_band: usize, blue_band: usize },
}

impl SpectralFormula {
    pub fn parse(name: &str, nir: usize, red: usize, green: usize, blue: usize, swir: usize) -> Option<Self> {
        match name.to_lowercase().trim() {
            "ndvi" => Some(Self::Ndvi { nir_band: nir, red_band: red }),
            "ndwi" => Some(Self::Ndwi { green_band: green, nir_band: nir }),
            "nbr" => Some(Self::Nbr { nir_band: nir, swir_band: swir }),
            "evi" => Some(Self::Evi { nir_band: nir, red_band: red, blue_band: blue }),
            _ => None,
        }
    }
}

/// Quantile target specification (percentile in [0.0, 1.0] or interquartile range)
#[derive(Debug, Clone, PartialEq)]
pub enum QuantileTarget {
    Percentile(f64, String),
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
    pub resolutions: Vec<u8>,
    pub band: usize,
    pub custom_nodata: Option<f64>,
    pub bbox: Option<[f64; 4]>,
    pub sampling: SamplingPattern,
    pub custom_crs: Option<String>,
    pub properties: Option<String>,
    pub spectral_formula: Option<SpectralFormula>,
    pub min_count: Option<f64>,
    pub min_mean: Option<f64>,
    pub max_mean: Option<f64>,
    pub min_majority_fraction: Option<f64>,
    pub compact: bool,
    pub overlap_rule: OverlapRule,
    pub quantiles: Vec<QuantileTarget>,
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
            compact: false,
            overlap_rule: OverlapRule::default(),
            quantiles: Vec::new(),
            remapper: None,
        }
    }

    /// Check whether streaming quantile calculations are enabled
    #[inline(always)]
    pub fn track_quantiles(&self) -> bool {
        !self.quantiles.is_empty()
    }
}

impl Default for MultiResolutionConfig {
    fn default() -> Self {
        Self::new(vec![8])
    }
}
