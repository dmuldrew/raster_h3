use crate::aggregator::accumulator::H3Accumulator;

/// Trait for types that support high-throughput SIMD / multi-lane scanline span accumulation.
pub trait SimdSpanAccumulate: Copy + PartialEq + Send + Sync + 'static {
    /// Vectorized accumulation of a contiguous horizontal span into an H3Accumulator.
    fn accumulate_span(slice: &[Self], nodata: Option<Self>) -> H3Accumulator;

    /// Scalar conversion of a single value to f64 (for sub-pixel sampling or fallback points).
    fn to_f64_val(self) -> f64;

    /// Fast test if value is valid (finite and not NoData).
    fn is_valid(self, nodata: Option<Self>) -> bool;
}

// =========================================================================
// Float32 Specialization (f32) - Dominant Continuous Format (DEM, Climate)
// =========================================================================

impl SimdSpanAccumulate for f32 {
    #[inline(always)]
    fn to_f64_val(self) -> f64 {
        self as f64
    }

    #[inline(always)]
    fn is_valid(self, nodata: Option<Self>) -> bool {
        if !self.is_finite() {
            return false;
        }
        if let Some(nd) = nodata {
            if self == nd || (self - nd).abs() < 1e-6 {
                return false;
            }
        }
        true
    }

    fn accumulate_span(slice: &[Self], nodata: Option<Self>) -> H3Accumulator {
        if slice.is_empty() {
            return H3Accumulator::default();
        }

        match nodata {
            None => accumulate_span_f32_no_nodata(slice),
            Some(nd) => accumulate_span_f32_with_nodata(slice, nd),
        }
    }
}

#[inline]
fn accumulate_span_f32_no_nodata(slice: &[f32]) -> H3Accumulator {
    let mut sum0 = 0.0f64;
    let mut sum1 = 0.0f64;
    let mut sum2 = 0.0f64;
    let mut sum3 = 0.0f64;

    let mut min0 = f32::INFINITY;
    let mut min1 = f32::INFINITY;
    let mut min2 = f32::INFINITY;
    let mut min3 = f32::INFINITY;

    let mut max0 = f32::NEG_INFINITY;
    let mut max1 = f32::NEG_INFINITY;
    let mut max2 = f32::NEG_INFINITY;
    let mut max3 = f32::NEG_INFINITY;

    let mut count0 = 0usize;
    let mut count1 = 0usize;
    let mut count2 = 0usize;
    let mut count3 = 0usize;

    let chunks = slice.chunks_exact(4);
    let remainder = chunks.remainder();

    for chunk in chunks {
        let v0 = chunk[0];
        let v1 = chunk[1];
        let v2 = chunk[2];
        let v3 = chunk[3];

        if v0.is_finite() {
            sum0 += v0 as f64;
            min0 = min0.min(v0);
            max0 = max0.max(v0);
            count0 += 1;
        }
        if v1.is_finite() {
            sum1 += v1 as f64;
            min1 = min1.min(v1);
            max1 = max1.max(v1);
            count1 += 1;
        }
        if v2.is_finite() {
            sum2 += v2 as f64;
            min2 = min2.min(v2);
            max2 = max2.max(v2);
            count2 += 1;
        }
        if v3.is_finite() {
            sum3 += v3 as f64;
            min3 = min3.min(v3);
            max3 = max3.max(v3);
            count3 += 1;
        }
    }

    for &v in remainder {
        if v.is_finite() {
            sum0 += v as f64;
            min0 = min0.min(v);
            max0 = max0.max(v);
            count0 += 1;
        }
    }

    let total_count = (count0 + count1 + count2 + count3) as f64;
    if total_count == 0.0 {
        return H3Accumulator::default();
    }

    let total_sum = (sum0 + sum1) + (sum2 + sum3);
    let total_min = min0.min(min1).min(min2.min(min3)) as f64;
    let total_max = max0.max(max1).max(max2.max(max3)) as f64;

    // Fast-path: uniform span (e.g. flat water or uniform elevation) has zero variance
    if total_min == total_max {
        return H3Accumulator::from_stats(total_sum, total_count, total_min, total_max, 0.0);
    }

    // Pass 2: Vectorized M2 variance computation
    let mean = total_sum / total_count;
    let mut m2_0 = 0.0f64;
    let mut m2_1 = 0.0f64;
    let mut m2_2 = 0.0f64;
    let mut m2_3 = 0.0f64;

    let chunks2 = slice.chunks_exact(4);
    let remainder2 = chunks2.remainder();

    if (total_count as usize) == slice.len() {
        // All values are finite: branchless second pass
        for chunk in chunks2 {
            let d0 = (chunk[0] as f64) - mean;
            let d1 = (chunk[1] as f64) - mean;
            let d2 = (chunk[2] as f64) - mean;
            let d3 = (chunk[3] as f64) - mean;
            m2_0 += d0 * d0;
            m2_1 += d1 * d1;
            m2_2 += d2 * d2;
            m2_3 += d3 * d3;
        }
        for &v in remainder2 {
            let d = (v as f64) - mean;
            m2_0 += d * d;
        }
    } else {
        for chunk in chunks2 {
            let v0 = chunk[0];
            let v1 = chunk[1];
            let v2 = chunk[2];
            let v3 = chunk[3];

            if v0.is_finite() {
                let d0 = (v0 as f64) - mean;
                m2_0 += d0 * d0;
            }
            if v1.is_finite() {
                let d1 = (v1 as f64) - mean;
                m2_1 += d1 * d1;
            }
            if v2.is_finite() {
                let d2 = (v2 as f64) - mean;
                m2_2 += d2 * d2;
            }
            if v3.is_finite() {
                let d3 = (v3 as f64) - mean;
                m2_3 += d3 * d3;
            }
        }
        for &v in remainder2 {
            if v.is_finite() {
                let d = (v as f64) - mean;
                m2_0 += d * d;
            }
        }
    }

    let total_m2 = (m2_0 + m2_1) + (m2_2 + m2_3);

    H3Accumulator::from_stats(total_sum, total_count, total_min, total_max, total_m2)
}

#[inline]
fn accumulate_span_f32_with_nodata(slice: &[f32], nd: f32) -> H3Accumulator {
    let mut sum0 = 0.0f64;
    let mut sum1 = 0.0f64;
    let mut sum2 = 0.0f64;
    let mut sum3 = 0.0f64;

    let mut min0 = f32::INFINITY;
    let mut min1 = f32::INFINITY;
    let mut min2 = f32::INFINITY;
    let mut min3 = f32::INFINITY;

    let mut max0 = f32::NEG_INFINITY;
    let mut max1 = f32::NEG_INFINITY;
    let mut max2 = f32::NEG_INFINITY;
    let mut max3 = f32::NEG_INFINITY;

    let mut count0 = 0usize;
    let mut count1 = 0usize;
    let mut count2 = 0usize;
    let mut count3 = 0usize;

    let chunks = slice.chunks_exact(4);
    let remainder = chunks.remainder();

    for chunk in chunks {
        let v0 = chunk[0];
        let v1 = chunk[1];
        let v2 = chunk[2];
        let v3 = chunk[3];

        if v0.is_finite() && v0 != nd && (v0 - nd).abs() >= 1e-6 {
            sum0 += v0 as f64;
            min0 = min0.min(v0);
            max0 = max0.max(v0);
            count0 += 1;
        }
        if v1.is_finite() && v1 != nd && (v1 - nd).abs() >= 1e-6 {
            sum1 += v1 as f64;
            min1 = min1.min(v1);
            max1 = max1.max(v1);
            count1 += 1;
        }
        if v2.is_finite() && v2 != nd && (v2 - nd).abs() >= 1e-6 {
            sum2 += v2 as f64;
            min2 = min2.min(v2);
            max2 = max2.max(v2);
            count2 += 1;
        }
        if v3.is_finite() && v3 != nd && (v3 - nd).abs() >= 1e-6 {
            sum3 += v3 as f64;
            min3 = min3.min(v3);
            max3 = max3.max(v3);
            count3 += 1;
        }
    }

    for &v in remainder {
        if v.is_finite() && v != nd && (v - nd).abs() >= 1e-6 {
            sum0 += v as f64;
            min0 = min0.min(v);
            max0 = max0.max(v);
            count0 += 1;
        }
    }

    let total_count = (count0 + count1 + count2 + count3) as f64;
    if total_count == 0.0 {
        return H3Accumulator::default();
    }

    let total_sum = (sum0 + sum1) + (sum2 + sum3);
    let total_min = min0.min(min1).min(min2.min(min3)) as f64;
    let total_max = max0.max(max1).max(max2.max(max3)) as f64;

    if total_min == total_max {
        return H3Accumulator::from_stats(total_sum, total_count, total_min, total_max, 0.0);
    }

    let mean = total_sum / total_count;
    let mut m2_0 = 0.0f64;
    let mut m2_1 = 0.0f64;
    let mut m2_2 = 0.0f64;
    let mut m2_3 = 0.0f64;

    let chunks2 = slice.chunks_exact(4);
    let remainder2 = chunks2.remainder();

    for chunk in chunks2 {
        let v0 = chunk[0];
        let v1 = chunk[1];
        let v2 = chunk[2];
        let v3 = chunk[3];

        if v0.is_finite() && v0 != nd && (v0 - nd).abs() >= 1e-6 {
            let d0 = (v0 as f64) - mean;
            m2_0 += d0 * d0;
        }
        if v1.is_finite() && v1 != nd && (v1 - nd).abs() >= 1e-6 {
            let d1 = (v1 as f64) - mean;
            m2_1 += d1 * d1;
        }
        if v2.is_finite() && v2 != nd && (v2 - nd).abs() >= 1e-6 {
            let d2 = (v2 as f64) - mean;
            m2_2 += d2 * d2;
        }
        if v3.is_finite() && v3 != nd && (v3 - nd).abs() >= 1e-6 {
            let d3 = (v3 as f64) - mean;
            m2_3 += d3 * d3;
        }
    }

    for &v in remainder2 {
        if v.is_finite() && v != nd && (v - nd).abs() >= 1e-6 {
            let d = (v as f64) - mean;
            m2_0 += d * d;
        }
    }

    let total_m2 = (m2_0 + m2_1) + (m2_2 + m2_3);

    H3Accumulator::from_stats(total_sum, total_count, total_min, total_max, total_m2)
}

// =========================================================================
// Float64 Specialization (f64)
// =========================================================================

impl SimdSpanAccumulate for f64 {
    #[inline(always)]
    fn to_f64_val(self) -> f64 {
        self
    }

    #[inline(always)]
    fn is_valid(self, nodata: Option<Self>) -> bool {
        if !self.is_finite() {
            return false;
        }
        if let Some(nd) = nodata {
            if self == nd || (self - nd).abs() < 1e-6 {
                return false;
            }
        }
        true
    }

    fn accumulate_span(slice: &[Self], nodata: Option<Self>) -> H3Accumulator {
        if slice.is_empty() {
            return H3Accumulator::default();
        }

        let mut sum0 = 0.0f64;
        let mut sum1 = 0.0f64;
        let mut min0 = f64::INFINITY;
        let mut min1 = f64::INFINITY;
        let mut max0 = f64::NEG_INFINITY;
        let mut max1 = f64::NEG_INFINITY;
        let mut count0 = 0usize;
        let mut count1 = 0usize;

        let is_valid_fn = |v: f64| -> bool {
            if !v.is_finite() {
                return false;
            }
            if let Some(nd) = nodata {
                if v == nd || (v - nd).abs() < 1e-6 {
                    return false;
                }
            }
            true
        };

        let chunks = slice.chunks_exact(2);
        let remainder = chunks.remainder();

        for chunk in chunks {
            let v0 = chunk[0];
            let v1 = chunk[1];
            if is_valid_fn(v0) {
                sum0 += v0;
                min0 = min0.min(v0);
                max0 = max0.max(v0);
                count0 += 1;
            }
            if is_valid_fn(v1) {
                sum1 += v1;
                min1 = min1.min(v1);
                max1 = max1.max(v1);
                count1 += 1;
            }
        }
        for &v in remainder {
            if is_valid_fn(v) {
                sum0 += v;
                min0 = min0.min(v);
                max0 = max0.max(v);
                count0 += 1;
            }
        }

        let total_count = (count0 + count1) as f64;
        if total_count == 0.0 {
            return H3Accumulator::default();
        }
        let total_sum = sum0 + sum1;
        let total_min = min0.min(min1);
        let total_max = max0.max(max1);

        if total_min == total_max {
            return H3Accumulator::from_stats(total_sum, total_count, total_min, total_max, 0.0);
        }

        let mean = total_sum / total_count;
        let mut m2_0 = 0.0f64;
        let mut m2_1 = 0.0f64;

        let chunks2 = slice.chunks_exact(2);
        let remainder2 = chunks2.remainder();

        for chunk in chunks2 {
            let v0 = chunk[0];
            let v1 = chunk[1];
            if is_valid_fn(v0) {
                let d = v0 - mean;
                m2_0 += d * d;
            }
            if is_valid_fn(v1) {
                let d = v1 - mean;
                m2_1 += d * d;
            }
        }
        for &v in remainder2 {
            if is_valid_fn(v) {
                let d = v - mean;
                m2_0 += d * d;
            }
        }

        H3Accumulator::from_stats(
            total_sum,
            total_count,
            total_min,
            total_max,
            m2_0 + m2_1,
        )
    }
}

// =========================================================================
// Generic Integer Macro for High-Throughput Integer Accumulators
// =========================================================================

macro_rules! impl_simd_span_integer {
    ($type:ty) => {
        impl SimdSpanAccumulate for $type {
            #[inline(always)]
            fn to_f64_val(self) -> f64 {
                self as f64
            }

            #[inline(always)]
            fn is_valid(self, nodata: Option<Self>) -> bool {
                match nodata {
                    Some(nd) => self != nd,
                    None => true,
                }
            }

            fn accumulate_span(slice: &[Self], nodata: Option<Self>) -> H3Accumulator {
                if slice.is_empty() {
                    return H3Accumulator::default();
                }

                let mut sum0 = 0.0f64;
                let mut sum1 = 0.0f64;
                let mut min0 = <$type>::MAX;
                let mut min1 = <$type>::MAX;
                let mut max0 = <$type>::MIN;
                let mut max1 = <$type>::MIN;
                let mut count0 = 0usize;
                let mut count1 = 0usize;

                let chunks = slice.chunks_exact(2);
                let remainder = chunks.remainder();

                match nodata {
                    None => {
                        for chunk in chunks {
                            let v0 = chunk[0];
                            let v1 = chunk[1];
                            sum0 += v0 as f64;
                            min0 = min0.min(v0);
                            max0 = max0.max(v0);
                            count0 += 1;

                            sum1 += v1 as f64;
                            min1 = min1.min(v1);
                            max1 = max1.max(v1);
                            count1 += 1;
                        }
                        for &v in remainder {
                            sum0 += v as f64;
                            min0 = min0.min(v);
                            max0 = max0.max(v);
                            count0 += 1;
                        }
                    }
                    Some(nd) => {
                        for chunk in chunks {
                            let v0 = chunk[0];
                            let v1 = chunk[1];
                            if v0 != nd {
                                sum0 += v0 as f64;
                                min0 = min0.min(v0);
                                max0 = max0.max(v0);
                                count0 += 1;
                            }
                            if v1 != nd {
                                sum1 += v1 as f64;
                                min1 = min1.min(v1);
                                max1 = max1.max(v1);
                                count1 += 1;
                            }
                        }
                        for &v in remainder {
                            if v != nd {
                                sum0 += v as f64;
                                min0 = min0.min(v);
                                max0 = max0.max(v);
                                count0 += 1;
                            }
                        }
                    }
                }

                let total_count = (count0 + count1) as f64;
                if total_count == 0.0 {
                    return H3Accumulator::default();
                }

                let total_sum = sum0 + sum1;
                let total_min = min0.min(min1) as f64;
                let total_max = max0.max(max1) as f64;

                if total_min == total_max {
                    return H3Accumulator::from_stats(total_sum, total_count, total_min, total_max, 0.0);
                }

                let mean = total_sum / total_count;
                let mut m2_0 = 0.0f64;
                let mut m2_1 = 0.0f64;

                let chunks2 = slice.chunks_exact(2);
                let remainder2 = chunks2.remainder();

                match nodata {
                    None => {
                        for chunk in chunks2 {
                            let d0 = (chunk[0] as f64) - mean;
                            let d1 = (chunk[1] as f64) - mean;
                            m2_0 += d0 * d0;
                            m2_1 += d1 * d1;
                        }
                        for &v in remainder2 {
                            let d = (v as f64) - mean;
                            m2_0 += d * d;
                        }
                    }
                    Some(nd) => {
                        for chunk in chunks2 {
                            let v0 = chunk[0];
                            let v1 = chunk[1];
                            if v0 != nd {
                                let d0 = (v0 as f64) - mean;
                                m2_0 += d0 * d0;
                            }
                            if v1 != nd {
                                let d1 = (v1 as f64) - mean;
                                m2_1 += d1 * d1;
                            }
                        }
                        for &v in remainder2 {
                            if v != nd {
                                let d = (v as f64) - mean;
                                m2_0 += d * d;
                            }
                        }
                    }
                }

                H3Accumulator::from_stats(
                    total_sum,
                    total_count,
                    total_min,
                    total_max,
                    m2_0 + m2_1,
                )
            }
        }
    };
}

impl_simd_span_integer!(u8);
impl_simd_span_integer!(u16);
impl_simd_span_integer!(u32);
impl_simd_span_integer!(u64);
impl_simd_span_integer!(i8);
impl_simd_span_integer!(i16);
impl_simd_span_integer!(i32);
impl_simd_span_integer!(i64);
