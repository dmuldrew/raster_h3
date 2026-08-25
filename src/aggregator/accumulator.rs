use serde::{Deserialize, Serialize};

/// High-performance cache-aligned pixel accumulator for H3 cell statistics
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct H3Accumulator {
    pub sum: f64,
    pub count: u64,
    pub min: f64,
    pub max: f64,
}

impl Default for H3Accumulator {
    #[inline(always)]
    fn default() -> Self {
        Self {
            sum: 0.0,
            count: 0,
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
            count: 1,
            min: val,
            max: val,
        }
    }

    /// Update running statistics with a new pixel value
    #[inline(always)]
    pub fn update(&mut self, val: f64) {
        self.sum += val;
        self.count += 1;
        if val < self.min {
            self.min = val;
        }
        if val > self.max {
            self.max = val;
        }
    }

    /// Merge another accumulator into this one (for parallel tree reduction)
    #[inline(always)]
    pub fn merge(&mut self, other: &Self) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            *self = *other;
            return;
        }
        self.sum += other.sum;
        self.count += other.count;
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }
    }

    /// Calculate arithmetic mean
    #[inline(always)]
    pub fn mean(&self) -> f64 {
        if self.count > 0 {
            self.sum / (self.count as f64)
        } else {
            f64::NAN
        }
    }
}
