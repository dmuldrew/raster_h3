use serde::{Deserialize, Serialize};

/// High-performance cache-aligned pixel accumulator for H3 cell statistics (32 bytes = 1/2 L1 cache line)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct H3Accumulator {
    pub sum: f64,
    pub count: f64,
    pub min: f64,
    pub max: f64,
}

impl Default for H3Accumulator {
    #[inline(always)]
    fn default() -> Self {
        Self {
            sum: 0.0,
            count: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
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
        }
    }

    /// Update running statistics with a full pixel (weight = 1.0)
    #[inline(always)]
    pub fn update(&mut self, val: f64) {
        self.update_weighted(val, 1.0);
    }

    /// Branchless update of running statistics with a sub-pixel weighted value
    #[inline(always)]
    pub fn update_weighted(&mut self, val: f64, weight: f64) {
        self.sum += val * weight;
        self.count += weight;
        self.min = self.min.min(val);
        self.max = self.max.max(val);
    }

    /// Branchless merge of another accumulator into this one
    #[inline(always)]
    pub fn merge(&mut self, other: &Self) {
        if other.count == 0.0 {
            return;
        }
        if self.count == 0.0 {
            *self = *other;
            return;
        }
        self.sum += other.sum;
        self.count += other.count;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
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
}
