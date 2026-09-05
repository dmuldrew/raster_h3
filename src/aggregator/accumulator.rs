use serde::{Deserialize, Serialize};

use crate::aggregator::quantiles::QuantileSketch;

/// High-performance pixel accumulator for H3 cell statistics with single-pass Welford online variance
/// and optional streaming non-parametric quantile estimation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct H3Accumulator {
    pub sum: f64,
    pub count: f64,
    pub min: f64,
    pub max: f64,
    pub m2: f64, // Sum of squared deviations from mean (Welford's algorithm)
    #[serde(skip)]
    pub quantiles: Option<Box<QuantileSketch>>,
}

impl Default for H3Accumulator {
    #[inline(always)]
    fn default() -> Self {
        Self {
            sum: 0.0,
            count: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            m2: 0.0,
            quantiles: None,
        }
    }
}

impl H3Accumulator {
    /// Initialize with a single pixel value
    #[inline(always)]
    pub fn new(val: f64) -> Self {
        Self {
            sum: val,
            count: 1.0,
            min: val,
            max: val,
            m2: 0.0,
            quantiles: None,
        }
    }

    /// Initialize with a weighted pixel value
    #[inline(always)]
    pub fn new_weighted(val: f64, weight: f64) -> Self {
        Self {
            sum: val * weight,
            count: weight,
            min: val,
            max: val,
            m2: 0.0,
            quantiles: None,
        }
    }

    /// Initialize with raw stats without quantiles
    #[inline(always)]
    pub fn from_stats(sum: f64, count: f64, min: f64, max: f64, m2: f64) -> Self {
        Self {
            sum,
            count,
            min,
            max,
            m2,
            quantiles: None,
        }
    }

    /// Initialize with streaming quantile tracking enabled
    #[inline]
    pub fn with_quantiles() -> Self {
        Self {
            sum: 0.0,
            count: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            m2: 0.0,
            quantiles: Some(Box::new(QuantileSketch::new())),
        }
    }

    /// Reset statistics and clear quantile sketch while retaining capacity
    #[inline]
    pub fn clear(&mut self) {
        self.sum = 0.0;
        self.count = 0.0;
        self.min = f64::INFINITY;
        self.max = f64::NEG_INFINITY;
        self.m2 = 0.0;
        if let Some(ref mut q) = self.quantiles {
            q.clear();
        }
    }

    /// Update running statistics with a full pixel (weight = 1.0)
    #[inline(always)]
    pub fn update(&mut self, val: f64) {
        if let Some(ref mut q) = self.quantiles {
            q.update(val, 1.0);
        }

        if self.count == 0.0 {
            self.sum = val;
            self.count = 1.0;
            self.min = val;
            self.max = val;
            self.m2 = 0.0;
            return;
        }

        let delta = val - (self.sum / self.count);
        self.sum += val;
        self.count += 1.0;
        let delta2 = val - (self.sum / self.count);
        self.m2 += delta * delta2;
        self.min = self.min.min(val);
        self.max = self.max.max(val);
    }

    /// Branchless update of running statistics with single-pass Welford online variance
    #[inline(always)]
    pub fn update_weighted(&mut self, val: f64, weight: f64) {
        if weight <= 0.0 {
            return;
        }
        if let Some(ref mut q) = self.quantiles {
            q.update(val, weight);
        }

        if self.count == 0.0 {
            self.sum = val * weight;
            self.count = weight;
            self.min = val;
            self.max = val;
            self.m2 = 0.0;
            return;
        }

        let old_mean = self.sum / self.count;
        self.sum += val * weight;
        self.count += weight;
        let new_mean = self.sum / self.count;
        self.m2 += weight * (val - old_mean) * (val - new_mean);
        self.min = self.min.min(val);
        self.max = self.max.max(val);
    }

    /// Two-pass SIMD-vectorizable accumulation over a contiguous slice of values
    #[inline]
    pub fn update_slice(&mut self, vals: &[f64]) {
        if vals.is_empty() {
            return;
        }
        if let Some(ref mut q) = self.quantiles {
            for &v in vals {
                if v.is_finite() {
                    q.update(v, 1.0);
                }
            }
        }
        let mut sum = 0.0;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut count = 0.0;
        for &v in vals {
            if v.is_finite() {
                sum += v;
                min = min.min(v);
                max = max.max(v);
                count += 1.0;
            }
        }
        if count == 0.0 {
            return;
        }
        let mean = sum / count;
        let mut m2 = 0.0;
        for &v in vals {
            if v.is_finite() {
                let d = v - mean;
                m2 += d * d;
            }
        }
        let chunk_acc = Self { sum, count, min, max, m2, quantiles: None };
        self.merge(&chunk_acc);
    }

    /// Branchless merge of another accumulator using parallel Chan-Golub-LeVeque combine formula
    #[inline(always)]
    pub fn merge(&mut self, other: &Self) {
        if other.count == 0.0 {
            return;
        }
        if self.count == 0.0 {
            self.sum = other.sum;
            self.count = other.count;
            self.min = other.min;
            self.max = other.max;
            self.m2 = other.m2;
            if let Some(ref q_other) = other.quantiles {
                if let Some(ref mut q_self) = self.quantiles {
                    q_self.merge(q_other);
                } else {
                    self.quantiles = Some(q_other.clone());
                }
            }
            return;
        }

        let n1 = self.count;
        let n2 = other.count;
        let mean1 = self.sum / n1;
        let mean2 = other.sum / n2;
        let delta = mean2 - mean1;

        self.sum += other.sum;
        self.count += other.count;
        self.m2 += other.m2 + delta * delta * (n1 * n2 / (n1 + n2));
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);

        if let (Some(ref mut q_self), Some(ref q_other)) = (&mut self.quantiles, &other.quantiles) {
            q_self.merge(q_other);
        } else if self.quantiles.is_none() && other.quantiles.is_some() {
            self.quantiles = other.quantiles.clone();
        }
    }

    /// Calculate arithmetic mean
    #[inline(always)]
    pub fn mean(&self) -> f64 {
        if self.count > 0.0 {
            self.sum / self.count
        } else {
            f64::NAN
        }
    }

    /// Calculate sample variance
    #[inline(always)]
    pub fn variance(&self) -> f64 {
        if self.count > 1.0 {
            self.m2 / (self.count - 1.0)
        } else if self.count > 0.0 {
            0.0
        } else {
            f64::NAN
        }
    }

    /// Calculate sample standard deviation
    #[inline(always)]
    pub fn stddev(&self) -> f64 {
        let var = self.variance();
        if var.is_nan() {
            f64::NAN
        } else {
            var.max(0.0).sqrt()
        }
    }

    /// Calculate estimated quantile q in [0.0, 1.0]
    #[inline]
    pub fn quantile(&self, q: f64) -> f64 {
        if let Some(ref sketch) = self.quantiles {
            sketch.quantile(q, self.min, self.max)
        } else {
            f64::NAN
        }
    }

    /// Calculate interquartile range (p75 - p25)
    #[inline]
    pub fn iqr(&self) -> f64 {
        if let Some(ref sketch) = self.quantiles {
            let p75 = sketch.quantile(0.75, self.min, self.max);
            let p25 = sketch.quantile(0.25, self.min, self.max);
            (p75 - p25).max(0.0)
        } else {
            f64::NAN
        }
    }
}

/// A highly optimized accumulator for tracking short, contiguous runs of pixels
/// without the overhead of Welford's algorithm divisions per pixel.
/// Automatically vectorizes nicely for SIMD architectures.
#[derive(Debug, Clone, PartialEq)]
pub struct FastRunAccumulator {
    pub sum: f64,
    pub sum_sq: f64,
    pub count: f64,
    pub min: f64,
    pub max: f64,
}

impl Default for FastRunAccumulator {
    #[inline(always)]
    fn default() -> Self {
        Self {
            sum: 0.0,
            sum_sq: 0.0,
            count: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}

impl FastRunAccumulator {
    /// Convert the fast run statistics into a numerically stable Welford accumulator
    #[inline(always)]
    pub fn into_h3(self) -> H3Accumulator {
        let m2 = if self.count > 1.0 {
            self.sum_sq - (self.sum * self.sum) / self.count
        } else {
            0.0
        };
        H3Accumulator {
            sum: self.sum,
            count: self.count,
            min: self.min,
            max: self.max,
            m2: m2.max(0.0),
            quantiles: None,
        }
    }
}
