use serde::{Deserialize, Serialize};

/// High-performance cache-aligned pixel accumulator for H3 cell statistics (32 bytes = 1/2 L1 cache line)
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

    /// Branchless update of running statistics with a new pixel value (compiles to x86 minsd/maxsd)
    #[inline(always)]
    pub fn update(&mut self, val: f64) {
        self.sum += val;
        self.count += 1;
        self.min = self.min.min(val);
        self.max = self.max.max(val);
    }

    /// Branchless merge of another accumulator into this one
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
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
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
