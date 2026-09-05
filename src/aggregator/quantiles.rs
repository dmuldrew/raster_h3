//! High-Performance Streaming Non-Parametric Quantile Sketch (DDSketch)
//!
//! Provides fast, mergeable, constant-memory estimation of arbitrary percentiles
//! (p01, p05, p10, p25, p50, p75, p90, p95, p99, IQR) with bounded relative error (alpha <= 0.01).
//!
//! Fully commutative and associative across multi-core Rayon threads and multi-tile chunks.

use fxhash::FxHashMap;

/// Bounded relative error alpha (1% relative error guarantee)
pub const QUANTILE_ALPHA: f64 = 0.01;

/// Threshold below which float values are counted in the exact zero bucket
pub const ZERO_THRESHOLD: f64 = 1e-7;

/// Logarithmic binning constant: gamma = (1 + alpha) / (1 - alpha)
const GAMMA: f64 = (1.0 + QUANTILE_ALPHA) / (1.0 - QUANTILE_ALPHA);

/// Precomputed 1.0 / ln(gamma) for fast bucket indexing
const LOG_GAMMA_INV: f64 = 1.0 / 0.02000066671111664; // ln(1.01 / 0.99)

/// Streaming DDSketch quantile sketch
#[derive(Debug, Clone, PartialEq)]
pub struct QuantileSketch {
    pub(crate) zero_count: f64,
    pub(crate) pos_bins: FxHashMap<i32, f64>,
    pub(crate) neg_bins: FxHashMap<i32, f64>,
    pub(crate) total_count: f64,
}

impl Default for QuantileSketch {
    fn default() -> Self {
        Self::new()
    }
}

impl QuantileSketch {
    /// Create a new empty quantile sketch
    #[inline]
    pub fn new() -> Self {
        Self {
            zero_count: 0.0,
            pos_bins: FxHashMap::default(),
            neg_bins: FxHashMap::default(),
            total_count: 0.0,
        }
    }

    /// Reset state while retaining allocated hash table capacities
    #[inline]
    pub fn clear(&mut self) {
        self.zero_count = 0.0;
        self.pos_bins.clear();
        self.neg_bins.clear();
        self.total_count = 0.0;
    }

    /// Number of observations in the sketch
    #[inline(always)]
    pub fn count(&self) -> f64 {
        self.total_count
    }

    /// Map a positive value to its logarithmic bucket key
    #[inline(always)]
    fn key_for_positive(val: f64) -> i32 {
        (val.ln() * LOG_GAMMA_INV).floor() as i32
    }

    /// Compute lower bound gamma^k
    #[inline(always)]
    fn lower_bound(key: i32) -> f64 {
        ((key as f64) * 0.02000066671111664).exp()
    }

    /// Insert a single value with weight 1.0
    #[inline]
    pub fn insert(&mut self, val: f64) {
        self.update(val, 1.0);
    }

    /// Insert a single value with custom weight
    #[inline]
    pub fn insert_weighted(&mut self, val: f64, weight: f64) {
        self.update(val, weight);
    }

    /// Update the sketch with a new value and weight
    #[inline]
    pub fn update(&mut self, val: f64, weight: f64) {
        if weight <= 0.0 || !val.is_finite() {
            return;
        }

        self.total_count += weight;

        if val.abs() <= ZERO_THRESHOLD {
            self.zero_count += weight;
        } else if val > 0.0 {
            let key = Self::key_for_positive(val);
            *self.pos_bins.entry(key).or_insert(0.0) += weight;
        } else {
            let key = Self::key_for_positive(-val);
            *self.neg_bins.entry(key).or_insert(0.0) += weight;
        }
    }

    /// Associative and commutative merge of another sketch
    #[inline]
    pub fn merge(&mut self, other: &Self) {
        if other.total_count == 0.0 {
            return;
        }
        if self.total_count == 0.0 {
            self.zero_count = other.zero_count;
            self.pos_bins = other.pos_bins.clone();
            self.neg_bins = other.neg_bins.clone();
            self.total_count = other.total_count;
            return;
        }

        self.zero_count += other.zero_count;
        self.total_count += other.total_count;

        for (&k, &w) in &other.pos_bins {
            *self.pos_bins.entry(k).or_insert(0.0) += w;
        }
        for (&k, &w) in &other.neg_bins {
            *self.neg_bins.entry(k).or_insert(0.0) += w;
        }
    }

    /// Query an estimated quantile q in [0.0, 1.0], clamped by known exact [min, max]
    pub fn quantile(&self, q: f64, min: f64, max: f64) -> f64 {
        if self.total_count <= 0.0 || q.is_nan() {
            return f64::NAN;
        }
        if q <= 0.0 {
            return min;
        }
        if q >= 1.0 {
            return max;
        }
        if (min - max).abs() < 1e-12 {
            return min;
        }

        let target_rank = q * self.total_count;
        let mut cum = 0.0;

        // 1. Traverse negative bins in descending key order (largest key = largest magnitude = most negative)
        if !self.neg_bins.is_empty() {
            let mut neg_keys: Vec<i32> = self.neg_bins.keys().copied().collect();
            neg_keys.sort_unstable_by(|a, b| b.cmp(a)); // Descending order

            for key in neg_keys {
                let count = self.neg_bins[&key];
                let prior = cum;
                cum += count;
                if cum >= target_rank {
                    let f = if count > 0.0 {
                        ((target_rank - prior) / count).clamp(0.0, 1.0)
                    } else {
                        0.5
                    };
                    let l = Self::lower_bound(key);
                    let val = -l * (GAMMA - f * (GAMMA - 1.0));
                    return val.clamp(min, max);
                }
            }
        }

        // 2. Traverse exact zero bucket
        if self.zero_count > 0.0 {
            cum += self.zero_count;
            if cum >= target_rank {
                return 0.0f64.clamp(min, max);
            }
        }

        // 3. Traverse positive bins in ascending key order (smallest key = smallest magnitude)
        if !self.pos_bins.is_empty() {
            let mut pos_keys: Vec<i32> = self.pos_bins.keys().copied().collect();
            pos_keys.sort_unstable();

            for key in pos_keys {
                let count = self.pos_bins[&key];
                let prior = cum;
                cum += count;
                if cum >= target_rank {
                    let f = if count > 0.0 {
                        ((target_rank - prior) / count).clamp(0.0, 1.0)
                    } else {
                        0.5
                    };
                    let l = Self::lower_bound(key);
                    let val = l * (1.0 + f * (GAMMA - 1.0));
                    return val.clamp(min, max);
                }
            }
        }

        max
    }

    /// Query multiple quantiles in a single sorted sweep over all bins
    pub fn quantiles_batch(&self, targets: &[f64], min: f64, max: f64) -> Vec<f64> {
        if targets.is_empty() {
            return Vec::new();
        }
        if self.total_count <= 0.0 {
            return vec![f64::NAN; targets.len()];
        }
        if (min - max).abs() < 1e-12 {
            return vec![min; targets.len()];
        }

        let mut sorted_targets: Vec<(usize, f64)> = targets
            .iter()
            .enumerate()
            .map(|(i, &q)| (i, q))
            .collect();
        sorted_targets.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut results = vec![0.0f64; targets.len()];
        let mut target_idx = 0;

        while target_idx < sorted_targets.len() && sorted_targets[target_idx].1 <= 0.0 {
            let (orig_idx, _) = sorted_targets[target_idx];
            results[orig_idx] = min;
            target_idx += 1;
        }

        if target_idx >= sorted_targets.len() {
            return results;
        }

        let mut cum = 0.0;

        // 1. Negative bins
        if !self.neg_bins.is_empty() {
            let mut neg_keys: Vec<i32> = self.neg_bins.keys().copied().collect();
            neg_keys.sort_unstable_by(|a, b| b.cmp(a)); // Descending

            for key in neg_keys {
                let count = self.neg_bins[&key];
                let prior = cum;
                cum += count;

                while target_idx < sorted_targets.len() {
                    let (orig_idx, q) = sorted_targets[target_idx];
                    if q >= 1.0 {
                        break;
                    }
                    let rank = q * self.total_count;
                    if rank <= cum {
                        let f = if count > 0.0 {
                            ((rank - prior) / count).clamp(0.0, 1.0)
                        } else {
                            0.5
                        };
                        let l = Self::lower_bound(key);
                        let val = -l * (GAMMA - f * (GAMMA - 1.0));
                        results[orig_idx] = val.clamp(min, max);
                        target_idx += 1;
                    } else {
                        break;
                    }
                }

                if target_idx >= sorted_targets.len() {
                    return results;
                }
            }
        }

        // 2. Zero count
        if self.zero_count > 0.0 {
            cum += self.zero_count;

            while target_idx < sorted_targets.len() {
                let (orig_idx, q) = sorted_targets[target_idx];
                if q >= 1.0 {
                    break;
                }
                let rank = q * self.total_count;
                if rank <= cum {
                    results[orig_idx] = 0.0f64.clamp(min, max);
                    target_idx += 1;
                } else {
                    break;
                }
            }

            if target_idx >= sorted_targets.len() {
                return results;
            }
        }

        // 3. Positive bins
        if !self.pos_bins.is_empty() {
            let mut pos_keys: Vec<i32> = self.pos_bins.keys().copied().collect();
            pos_keys.sort_unstable();

            for key in pos_keys {
                let count = self.pos_bins[&key];
                let prior = cum;
                cum += count;

                while target_idx < sorted_targets.len() {
                    let (orig_idx, q) = sorted_targets[target_idx];
                    if q >= 1.0 {
                        break;
                    }
                    let rank = q * self.total_count;
                    if rank <= cum {
                        let f = if count > 0.0 {
                            ((rank - prior) / count).clamp(0.0, 1.0)
                        } else {
                            0.5
                        };
                        let l = Self::lower_bound(key);
                        let val = l * (1.0 + f * (GAMMA - 1.0));
                        results[orig_idx] = val.clamp(min, max);
                        target_idx += 1;
                    } else {
                        break;
                    }
                }

                if target_idx >= sorted_targets.len() {
                    return results;
                }
            }
        }

        while target_idx < sorted_targets.len() {
            let (orig_idx, _) = sorted_targets[target_idx];
            results[orig_idx] = max;
            target_idx += 1;
        }

        results
    }
}
