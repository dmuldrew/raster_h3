use std::collections::HashMap;
use fxhash::FxBuildHasher;
use serde::{Deserialize, Serialize};

use crate::aggregator::horizon_streamer::AggregationConfig;
use crate::aggregator::multi_horizon::{MultiCategoricalHorizonStreamer, MultiResolutionConfig};
use crate::error::Result;
use crate::raster::geotiff::GeoTiffStreamReader;

/// Maximum number of distinct categories tracked inline without heap allocation.
pub const INLINE_CAPACITY: usize = 16;

/// High-performance accumulator for categorical class frequencies per H3 cell.
/// Uses an inline 16-slot array for zero-heap allocation in >99.99% of cells,
/// with an optional boxed hash map for complex multi-class boundaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoricalAccumulator {
    pub inline_entries: [(i64, f64); INLINE_CAPACITY],
    pub inline_len: u8,
    pub heap_counts: Option<Box<HashMap<i64, f64, FxBuildHasher>>>,
    pub total_count: f64,
}

impl Default for CategoricalAccumulator {
    #[inline(always)]
    fn default() -> Self {
        Self {
            inline_entries: [(0, 0.0); INLINE_CAPACITY],
            inline_len: 0,
            heap_counts: None,
            total_count: 0.0,
        }
    }
}

impl CategoricalAccumulator {
    /// Initialize a new empty categorical accumulator
    pub fn new() -> Self {
        Self::default()
    }

    /// Iterate through all category-count pairs
    pub fn for_each_class<F: FnMut(i64, f64)>(&self, mut f: F) {
        if let Some(ref heap) = self.heap_counts {
            for (&cat, &cnt) in heap.iter() {
                f(cat, cnt);
            }
        } else {
            let len = self.inline_len as usize;
            for i in 0..len {
                f(self.inline_entries[i].0, self.inline_entries[i].1);
            }
        }
    }

    /// Get count for a specific category
    pub fn get_class_count(&self, category: i64) -> f64 {
        if let Some(ref heap) = self.heap_counts {
            heap.get(&category).copied().unwrap_or(0.0)
        } else {
            let len = self.inline_len as usize;
            for i in 0..len {
                if self.inline_entries[i].0 == category {
                    return self.inline_entries[i].1;
                }
            }
            0.0
        }
    }

    /// Update with a single unweighted category
    #[inline(always)]
    pub fn update(&mut self, category: i64) {
        self.update_weighted(category, 1.0);
    }

    /// Update with a weighted category (e.g. from sub-pixel super-sampling)
    #[inline(always)]
    pub fn update_weighted(&mut self, category: i64, weight: f64) {
        if weight <= 0.0 {
            return;
        }
        self.total_count += weight;

        if let Some(ref mut heap) = self.heap_counts {
            *heap.entry(category).or_insert(0.0) += weight;
            return;
        }

        let len = self.inline_len as usize;
        for i in 0..len {
            if self.inline_entries[i].0 == category {
                self.inline_entries[i].1 += weight;
                return;
            }
        }

        if len < INLINE_CAPACITY {
            self.inline_entries[len] = (category, weight);
            self.inline_len += 1;
        } else {
            // Spill to heap
            let mut map: HashMap<i64, f64, FxBuildHasher> = HashMap::with_capacity_and_hasher(32, FxBuildHasher::default());
            for i in 0..INLINE_CAPACITY {
                map.insert(self.inline_entries[i].0, self.inline_entries[i].1);
            }
            map.insert(category, weight);
            self.heap_counts = Some(Box::new(map));
        }
    }

    /// Merge another categorical accumulator
    pub fn merge(&mut self, other: &Self) {
        if other.total_count == 0.0 {
            return;
        }
        other.for_each_class(|cat, cnt| {
            self.update_weighted(cat, cnt);
        });
    }

    /// Return majority (mode) category, its count, and its fraction of total
    pub fn majority(&self) -> (i64, f64, f64) {
        if self.total_count == 0.0 {
            return (0, 0.0, 0.0);
        }
        let mut max_cat = 0;
        let mut max_count = -1.0;
        self.for_each_class(|cat, cnt| {
            if cnt > max_count {
                max_count = cnt;
                max_cat = cat;
            }
        });
        let frac = if self.total_count > 0.0 && max_count > 0.0 {
            max_count / self.total_count
        } else {
            0.0
        };
        (max_cat, max_count, frac)
    }

    /// Return number of unique categories present (richness)
    #[inline(always)]
    pub fn unique_classes(&self) -> usize {
        if let Some(ref heap) = self.heap_counts {
            heap.len()
        } else {
            self.inline_len as usize
        }
    }

    /// Serialize histogram directly into a reusable string buffer without intermediate allocations
    pub fn histogram_json_into(&self, s: &mut String) {
        s.clear();
        let count = self.unique_classes();
        if count == 0 {
            s.push_str("{}");
            return;
        }

        let mut stack_entries = [(0i64, 0.0f64); 16];
        let use_stack = count <= 16;
        let mut heap_entries;

        let entries_slice: &mut [(i64, f64)] = if use_stack {
            let mut idx = 0;
            self.for_each_class(|cat, cnt| {
                if idx < 16 {
                    stack_entries[idx] = (cat, cnt);
                    idx += 1;
                }
            });
            &mut stack_entries[..count]
        } else {
            heap_entries = Vec::with_capacity(count);
            self.for_each_class(|cat, cnt| {
                heap_entries.push((cat, cnt));
            });
            heap_entries.as_mut_slice()
        };

        entries_slice.sort_unstable_by_key(|&(k, _)| k);

        let required_cap = 32 + count * 20;
        if s.capacity() < required_cap {
            s.reserve(required_cap - s.capacity());
        }

        s.push('{');
        for (i, &(k, cnt)) in entries_slice.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            let frac = if self.total_count > 0.0 {
                (cnt / self.total_count).max(0.0).min(1.0)
            } else {
                0.0
            };
            let frac_i = (frac * 10000.0 + 0.5) as u32;
            let int_part = frac_i / 10000;
            let dec_part = frac_i % 10000;
            use std::fmt::Write;
            let _ = write!(s, "\"{}\": {}.{:04}", k, int_part, dec_part);
        }
        s.push('}');
    }

    /// Serialize histogram to a JSON string representation
    pub fn histogram_json(&self) -> String {
        let count = self.unique_classes();
        let mut s = String::with_capacity(32 + count * 20);
        self.histogram_json_into(&mut s);
        s
    }

    /// Return Shannon entropy of the class distribution (measure of diversity / ambiguity)
    pub fn shannon_entropy(&self) -> f64 {
        if self.total_count <= 0.0 || self.unique_classes() <= 1 {
            return 0.0;
        }
        let mut entropy = 0.0f64;
        self.for_each_class(|_cat, cnt| {
            if cnt > 0.0 {
                let p = cnt / self.total_count;
                entropy -= p * p.ln();
            }
        });
        entropy
    }
}

/// Streaming categorical aggregator using Southernmost Scan-Line Horizon Eviction.
/// Delegates to the optimized multi-resolution categorical streaming engine.
pub struct CategoricalHorizonStreamer {
    inner: MultiCategoricalHorizonStreamer,
}

impl CategoricalHorizonStreamer {
    /// Initialize a new CategoricalHorizonStreamer with background async prefetching and bbox pruning
    pub fn new(reader: GeoTiffStreamReader, config: &AggregationConfig) -> Result<Self> {
        let multi_config = MultiResolutionConfig::from(config);
        let inner = MultiCategoricalHorizonStreamer::new(reader, &multi_config)?;
        Ok(Self { inner })
    }

    /// Pull up to `max_rows` completed categorical records from the stream
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<(u64, CategoricalAccumulator)> {
        self.inner
            .fetch_next_batch(max_rows)
            .into_iter()
            .map(|record| (record.h3_index, record.accumulator))
            .collect()
    }

    /// Return current number of active cells in memory
    pub fn active_cell_count(&self) -> usize {
        self.inner.active_cell_count()
    }
}

/// Trait for numeric raster pixel types that support high-throughput SIMD / branchless span uniformity detection.
pub trait CategoricalUniformity: Copy + PartialEq + Send + Sync + 'static {
    /// Return true if all values in the slice are identical to slice[0], or if slice is empty.
    fn is_uniform(slice: &[Self]) -> bool;
}

impl CategoricalUniformity for u8 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        if slice.len() <= 1 {
            return true;
        }
        let first = slice[0];
        let rest = &slice[1..];
        let chunks = rest.chunks_exact(32);
        let rem = chunks.remainder();
        for chunk in chunks {
            let mut diff = 0u8;
            for &v in chunk {
                diff |= v ^ first;
            }
            if diff != 0 {
                return false;
            }
        }
        for &v in rem {
            if v != first {
                return false;
            }
        }
        true
    }
}

impl CategoricalUniformity for i8 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u8_slice: &[u8] = unsafe {
            std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len())
        };
        u8::is_uniform(u8_slice)
    }
}

impl CategoricalUniformity for u16 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        if slice.len() <= 1 {
            return true;
        }
        let first = slice[0];
        let rest = &slice[1..];
        let chunks = rest.chunks_exact(16);
        let rem = chunks.remainder();
        for chunk in chunks {
            let mut diff = 0u16;
            for &v in chunk {
                diff |= v ^ first;
            }
            if diff != 0 {
                return false;
            }
        }
        for &v in rem {
            if v != first {
                return false;
            }
        }
        true
    }
}

impl CategoricalUniformity for i16 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u16_slice: &[u16] = unsafe {
            std::slice::from_raw_parts(slice.as_ptr() as *const u16, slice.len())
        };
        u16::is_uniform(u16_slice)
    }
}

impl CategoricalUniformity for u32 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        if slice.len() <= 1 {
            return true;
        }
        let first = slice[0];
        let rest = &slice[1..];
        let chunks = rest.chunks_exact(8);
        let rem = chunks.remainder();
        for chunk in chunks {
            let mut diff = 0u32;
            for &v in chunk {
                diff |= v ^ first;
            }
            if diff != 0 {
                return false;
            }
        }
        for &v in rem {
            if v != first {
                return false;
            }
        }
        true
    }
}

impl CategoricalUniformity for i32 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u32_slice: &[u32] = unsafe {
            std::slice::from_raw_parts(slice.as_ptr() as *const u32, slice.len())
        };
        u32::is_uniform(u32_slice)
    }
}

impl CategoricalUniformity for u64 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        if slice.len() <= 1 {
            return true;
        }
        let first = slice[0];
        let rest = &slice[1..];
        let chunks = rest.chunks_exact(8);
        let rem = chunks.remainder();
        for chunk in chunks {
            let mut diff = 0u64;
            for &v in chunk {
                diff |= v ^ first;
            }
            if diff != 0 {
                return false;
            }
        }
        for &v in rem {
            if v != first {
                return false;
            }
        }
        true
    }
}

impl CategoricalUniformity for i64 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u64_slice: &[u64] = unsafe {
            std::slice::from_raw_parts(slice.as_ptr() as *const u64, slice.len())
        };
        u64::is_uniform(u64_slice)
    }
}

impl CategoricalUniformity for f32 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        if slice.len() <= 1 {
            return true;
        }
        let first = slice[0];
        let first_bits = first.to_bits();
        let rest = &slice[1..];
        let chunks = rest.chunks_exact(8);
        let rem = chunks.remainder();
        for chunk in chunks {
            let mut diff = 0u32;
            for &v in chunk {
                diff |= v.to_bits() ^ first_bits;
            }
            if diff != 0 {
                for &v in chunk {
                    if v != first {
                        return false;
                    }
                }
            }
        }
        for &v in rem {
            if v != first {
                return false;
            }
        }
        true
    }
}

impl CategoricalUniformity for f64 {
    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        if slice.len() <= 1 {
            return true;
        }
        let first = slice[0];
        let first_bits = first.to_bits();
        let rest = &slice[1..];
        let chunks = rest.chunks_exact(8);
        let rem = chunks.remainder();
        for chunk in chunks {
            let mut diff = 0u64;
            for &v in chunk {
                diff |= v.to_bits() ^ first_bits;
            }
            if diff != 0 {
                for &v in chunk {
                    if v != first {
                        return false;
                    }
                }
            }
        }
        for &v in rem {
            if v != first {
                return false;
            }
        }
        true
    }
}
