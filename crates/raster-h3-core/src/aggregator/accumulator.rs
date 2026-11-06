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
    /// Sum of squared observation weights, `sum(w_i^2)`. Equals `count` when every
    /// observation has weight one. Used for the reliability-weighted sample variance.
    pub sum_w2: f64,
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
            sum_w2: 0.0,
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
            sum_w2: 1.0,
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
            sum_w2: weight * weight,
            quantiles: None,
        }
    }

    /// Initialize with raw stats of `count` unit-weight observations, without quantiles
    #[inline(always)]
    pub fn from_stats(sum: f64, count: f64, min: f64, max: f64, m2: f64) -> Self {
        Self::from_weighted_stats(sum, count, count, min, max, m2)
    }

    /// Initialize with raw stats of weighted observations (`sum_w2` is `sum(w_i^2)`),
    /// without quantiles
    #[inline(always)]
    pub fn from_weighted_stats(
        sum: f64,
        count: f64,
        sum_w2: f64,
        min: f64,
        max: f64,
        m2: f64,
    ) -> Self {
        Self {
            sum,
            count,
            min,
            max,
            m2,
            sum_w2,
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
            sum_w2: 0.0,
            quantiles: Some(Box::new(QuantileSketch::new())),
        }
    }

    /// Map every observation `v` to `v * scale + offset`, exactly as if the
    /// transformed values had been accumulated. The quantile sketch is not
    /// transformed; use this on accumulators built without one.
    #[inline]
    pub fn apply_affine(&mut self, scale: f64, offset: f64) {
        debug_assert!(self.quantiles.is_none());
        if self.count == 0.0 {
            return;
        }
        self.sum = self.sum * scale + offset * self.count;
        self.m2 *= scale * scale;
        let (lo, hi) = (self.min * scale + offset, self.max * scale + offset);
        (self.min, self.max) = if scale < 0.0 { (hi, lo) } else { (lo, hi) };
    }

    /// Reset statistics and clear quantile sketch while retaining capacity
    #[inline]
    pub fn clear(&mut self) {
        self.sum = 0.0;
        self.count = 0.0;
        self.min = f64::INFINITY;
        self.max = f64::NEG_INFINITY;
        self.m2 = 0.0;
        self.sum_w2 = 0.0;
        if let Some(ref mut q) = self.quantiles {
            q.clear();
        }
    }

    /// Update running statistics with a full pixel (weight = 1.0)
    #[inline(always)]
    pub fn update(&mut self, val: f64) {
        // Non-finite samples never enter the accumulator, matching
        // update_slice and the SIMD span kernels.
        if !val.is_finite() {
            return;
        }
        if let Some(ref mut q) = self.quantiles {
            q.update(val, 1.0);
        }

        if self.count == 0.0 {
            self.sum = val;
            self.count = 1.0;
            self.min = val;
            self.max = val;
            self.m2 = 0.0;
            self.sum_w2 = 1.0;
            return;
        }

        let delta = val - (self.sum / self.count);
        self.sum += val;
        self.count += 1.0;
        self.sum_w2 += 1.0;
        let delta2 = val - (self.sum / self.count);
        self.m2 += delta * delta2;
        self.min = self.min.min(val);
        self.max = self.max.max(val);
    }

    /// Branchless update of running statistics with single-pass Welford online variance
    #[inline(always)]
    pub fn update_weighted(&mut self, val: f64, weight: f64) {
        // NaN slips past `weight <= 0.0`; require a positive finite weight and
        // a finite value so neither can poison the running statistics.
        if !(weight.is_finite() && weight > 0.0) || !val.is_finite() {
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
            self.sum_w2 = weight * weight;
            return;
        }

        let old_mean = self.sum / self.count;
        self.sum += val * weight;
        self.count += weight;
        self.sum_w2 += weight * weight;
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
        let chunk_acc = Self::from_stats(sum, count, min, max, m2);
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
            self.sum_w2 = other.sum_w2;
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
        self.sum_w2 += other.sum_w2;
        self.m2 += other.m2 + delta * delta * (n1 * n2 / (n1 + n2));
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);

        if let (Some(ref mut q_self), Some(ref q_other)) = (&mut self.quantiles, &other.quantiles) {
            q_self.merge(q_other);
        }
        // A sketch-less `self` must not adopt `other`'s sketch: it would
        // describe only the `other` samples while `count` covers both sides.
        // The reverse direction (sketch on self, none on other) is kept —
        // span kernels feed `self.quantiles` separately from the stats merge.
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

    /// Reliability-weighted sample variance, `M2 / (W - sum(w_i^2) / W)` with
    /// `W = sum(w_i)`. With unit weights this is the usual `M2 / (n - 1)`. The
    /// estimator is invariant to rescaling all weights, so fractional weights from
    /// sub-pixel sampling still measure the spread of distinct pixel values.
    /// Returns `0.0` for a single observation (or when every observation carries
    /// the same value) and `NaN` when empty.
    #[inline(always)]
    pub fn variance(&self) -> f64 {
        if self.count <= 0.0 {
            return f64::NAN;
        }
        let denom = self.count - self.sum_w2 / self.count;
        // denom is 0 for a single observation; guard rounding near that limit.
        if denom > self.count * 1e-12 && self.m2 > 0.0 {
            self.m2 / denom
        } else {
            0.0
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
            let q = sketch.quantiles_batch(&[0.25, 0.75], self.min, self.max);
            (q[1] - q[0]).max(0.0)
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
        // Each run element is one unit-weight pixel.
        H3Accumulator {
            sum: self.sum,
            count: self.count,
            min: self.min,
            max: self.max,
            m2: m2.max(0.0),
            sum_w2: self.count,
            quantiles: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_rejects_non_finite_values() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut acc = H3Accumulator::default();
            acc.update(bad);
            assert_eq!(acc.count, 0.0, "update({bad}) must not count a sample");
        }
        // A NaN after valid samples must not poison them either.
        let mut acc = H3Accumulator::default();
        acc.update(1.0);
        acc.update(f64::NAN);
        acc.update(f64::INFINITY);
        assert_eq!((acc.count, acc.sum, acc.mean()), (1.0, 1.0, 1.0));
    }

    #[test]
    fn update_weighted_rejects_bad_weights_and_values() {
        // NaN slips past `weight <= 0.0`, infinity inflates count forever.
        for bad_w in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let mut acc = H3Accumulator::default();
            acc.update_weighted(5.0, bad_w);
            assert_eq!(acc.count, 0.0, "weight {bad_w} must be rejected");
        }
        let mut acc = H3Accumulator::default();
        acc.update_weighted(f64::NAN, 1.0);
        acc.update_weighted(f64::INFINITY, 1.0);
        assert_eq!(acc.count, 0.0);
    }

    #[test]
    fn merge_does_not_adopt_partial_sketch() {
        // Self has unsketched samples; other's sketch covers only its own
        // samples. Adopting it would report quantiles for a subset.
        let mut a = H3Accumulator::default();
        a.update(1.0);
        a.update(2.0);
        let mut b = H3Accumulator::with_quantiles();
        b.update(3.0);
        b.update(4.0);
        a.merge(&b);
        assert_eq!(a.count, 4.0);
        assert!(a.quantiles.is_none());
        assert!(a.quantile(0.5).is_nan());
    }

    #[test]
    fn merge_keeps_self_sketch_for_stats_only_other() {
        // Span kernels feed self.quantiles separately from the stats merge,
        // so a stats-only merge must leave an existing sketch intact.
        let mut a = H3Accumulator::with_quantiles();
        a.update(1.0);
        let mut b = H3Accumulator::default();
        b.update(2.0);
        a.merge(&b);
        assert!(a.quantiles.is_some());
        assert!(a.quantile(0.5).is_finite());
    }

    #[test]
    fn merge_adopts_sketch_when_self_was_empty() {
        let mut a = H3Accumulator::with_quantiles();
        let mut b = H3Accumulator::with_quantiles();
        b.update(3.0);
        a.merge(&b);
        assert!(a.quantiles.is_some());
        assert_eq!(a.quantile(0.5), 3.0);
    }
}
