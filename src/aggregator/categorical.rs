use fxhash::FxBuildHasher;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
            let mut map: HashMap<i64, f64, FxBuildHasher> =
                HashMap::with_capacity_and_hasher(32, FxBuildHasher::default());
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

/// Trait for numeric raster pixel types that support high-throughput SIMD / branchless span uniformity detection.
pub trait CategoricalUniformity: Copy + PartialEq + Send + Sync + 'static {
    /// Return true if all values in the slice are identical to `slice[0]`, or if slice is empty.
    fn is_uniform(slice: &[Self]) -> bool;

    /// Convert native pixel value to an i64 category ID if in valid range
    fn to_category(self) -> Option<i64>;
}

impl CategoricalUniformity for u8 {
    #[inline(always)]
    fn to_category(self) -> Option<i64> {
        Some(self as i64)
    }

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
    fn to_category(self) -> Option<i64> {
        Some(self as i64)
    }

    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u8_slice: &[u8] =
            unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len()) };
        u8::is_uniform(u8_slice)
    }
}

impl CategoricalUniformity for u16 {
    #[inline(always)]
    fn to_category(self) -> Option<i64> {
        Some(self as i64)
    }

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
    fn to_category(self) -> Option<i64> {
        Some(self as i64)
    }

    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u16_slice: &[u16] =
            unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u16, slice.len()) };
        u16::is_uniform(u16_slice)
    }
}

impl CategoricalUniformity for u32 {
    #[inline(always)]
    fn to_category(self) -> Option<i64> {
        Some(self as i64)
    }

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
    fn to_category(self) -> Option<i64> {
        Some(self as i64)
    }

    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u32_slice: &[u32] =
            unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u32, slice.len()) };
        u32::is_uniform(u32_slice)
    }
}

impl CategoricalUniformity for u64 {
    #[inline(always)]
    fn to_category(self) -> Option<i64> {
        if self <= i64::MAX as u64 {
            Some(self as i64)
        } else {
            None
        }
    }

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
    fn to_category(self) -> Option<i64> {
        Some(self)
    }

    #[inline(always)]
    fn is_uniform(slice: &[Self]) -> bool {
        let u64_slice: &[u64] =
            unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u64, slice.len()) };
        u64::is_uniform(u64_slice)
    }
}

impl CategoricalUniformity for f32 {
    #[inline(always)]
    fn to_category(self) -> Option<i64> {
        if self.is_finite() {
            Some(self.round() as i64)
        } else {
            None
        }
    }

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
    fn to_category(self) -> Option<i64> {
        if self.is_finite() {
            Some(self.round() as i64)
        } else {
            None
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_categorical_inline_to_heap_spillover_boundary() {
        let mut acc = CategoricalAccumulator::new();
        assert_eq!(acc.unique_classes(), 0);
        assert!(acc.heap_counts.is_none());

        // Fill up to exactly 16 unique classes (INLINE_CAPACITY)
        for c in 0..16 {
            acc.update_weighted(c, 1.0);
            assert_eq!(acc.unique_classes(), (c + 1) as usize);
            assert!(
                acc.heap_counts.is_none(),
                "Must remain in inline storage for <= 16 classes"
            );
        }
        assert_eq!(acc.inline_len, 16);
        assert_eq!(acc.total_count, 16.0);

        // Update existing class 5: must NOT trigger heap spillover
        acc.update_weighted(5, 4.0);
        assert!(
            acc.heap_counts.is_none(),
            "Updating existing class must not allocate heap"
        );
        assert_eq!(acc.unique_classes(), 16);
        assert_eq!(acc.get_class_count(5), 5.0);
        assert_eq!(acc.total_count, 20.0);

        // Add 17th class: triggers spillover to heap
        acc.update_weighted(100, 10.0);
        assert!(
            acc.heap_counts.is_some(),
            "Adding 17th class must spill to heap HashMap"
        );
        assert_eq!(acc.unique_classes(), 17);
        assert_eq!(acc.total_count, 30.0);

        // Verify all 16 previous classes are preserved in heap map
        for c in 0..16 {
            let expected = if c == 5 { 5.0 } else { 1.0 };
            assert_eq!(
                acc.get_class_count(c),
                expected,
                "Preserved class count mismatch for class {}",
                c
            );
        }
        assert_eq!(acc.get_class_count(100), 10.0);

        // Add more classes to heap
        for c in 200..233 {
            acc.update(c);
        }
        assert_eq!(acc.unique_classes(), 17 + 33); // 50 unique classes

        // Test merge of another accumulator that has 16 inline classes
        let mut acc2 = CategoricalAccumulator::new();
        for c in 0..16 {
            acc2.update_weighted(c, 2.0);
        }
        acc.merge(&acc2);

        // Class 5 should now have 5.0 + 2.0 = 7.0
        assert_eq!(acc.get_class_count(5), 7.0);
        // Class 0 should have 1.0 + 2.0 = 3.0
        assert_eq!(acc.get_class_count(0), 3.0);
    }

    #[test]
    fn test_categorical_shannon_entropy_theoretical_bounds() {
        // 1. Empty accumulator: entropy must be 0.0
        let empty = CategoricalAccumulator::new();
        assert_eq!(empty.shannon_entropy(), 0.0);

        // 2. Single class: entropy must be 0.0 regardless of sample count
        let mut single = CategoricalAccumulator::new();
        single.update_weighted(42, 50000.0);
        assert_eq!(single.shannon_entropy(), 0.0);

        // 3. K equiprobable classes: entropy must theoretically equal ln(K)
        for &k in &[2, 4, 8, 16, 32, 64] {
            let mut acc = CategoricalAccumulator::new();
            for i in 0..k {
                acc.update_weighted(i, 10.0); // uniform weight
            }
            let entropy = acc.shannon_entropy();
            let theoretical = (k as f64).ln();
            assert!(
                (entropy - theoretical).abs() < 1e-12,
                "Entropy for K={} must be ln({}): {} vs {}",
                k,
                k,
                entropy,
                theoretical
            );
        }

        // 4. Heavily skewed distribution: entropy must be strictly > 0 and < ln(2)
        let mut skewed = CategoricalAccumulator::new();
        skewed.update_weighted(1, 1_000_000.0);
        skewed.update_weighted(2, 1.0);
        let s_entropy = skewed.shannon_entropy();
        assert!(s_entropy > 0.0);
        assert!(s_entropy < 2.0f64.ln());
        assert!(
            s_entropy < 1e-4,
            "Skewed distribution must have near-zero entropy: got {}",
            s_entropy
        );
    }

    #[test]
    fn test_categorical_tied_majority_determinism() {
        let mut acc = CategoricalAccumulator::new();
        // Empty accumulator majority
        assert_eq!(acc.majority(), (0, 0.0, 0.0));

        // Two perfectly tied classes: 10 and 20 each with 50.0
        acc.update_weighted(10, 50.0);
        acc.update_weighted(20, 50.0);
        let (maj_cat, maj_cnt, maj_frac) = acc.majority();
        assert_eq!(maj_cnt, 50.0);
        assert!((maj_frac - 0.5).abs() < 1e-9);
        assert!(maj_cat == 10 || maj_cat == 20);

        // Adding 0.001 to class 20 cleanly breaks tie
        acc.update_weighted(20, 0.001);
        let (maj_cat2, maj_cnt2, _) = acc.majority();
        assert_eq!(maj_cat2, 20);
        assert_eq!(maj_cnt2, 50.001);
    }

    #[test]
    fn test_categorical_histogram_json_formatting() {
        let mut acc = CategoricalAccumulator::new();
        acc.update_weighted(3, 25.0);
        acc.update_weighted(1, 75.0);

        let json = acc.histogram_json();
        // Keys must be ordered numerically: "1" before "3"
        assert_eq!(json, r#"{"1": 0.7500, "3": 0.2500}"#);

        // Test with > 16 classes (heap path)
        for i in 4..25 {
            acc.update_weighted(i, 1.0);
        }
        let json_heap = acc.histogram_json();
        assert!(json_heap.starts_with(r#"{"1":"#));
        assert!(json_heap.ends_with('}'));
    }
}
