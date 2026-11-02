//! Continuous (Welford/SIMD span) and categorical accumulator statistics and merges.

use raster_h3_core::aggregator::{CategoricalAccumulator, H3Accumulator};

#[test]
fn test_accumulator_operations() {
    let mut acc1 = H3Accumulator::new(10.0);
    acc1.update(20.0);
    acc1.update(30.0);
    assert_eq!(acc1.count, 3.0);
    assert_eq!(acc1.sum, 60.0);
    assert_eq!(acc1.mean(), 20.0);
    assert_eq!(acc1.min, 10.0);
    assert_eq!(acc1.max, 30.0);

    let mut acc2 = H3Accumulator::new(40.0);
    acc2.update(50.0);

    acc1.merge(&acc2);
    assert_eq!(acc1.count, 5.0);
    assert_eq!(acc1.sum, 150.0);
    assert_eq!(acc1.mean(), 30.0);
    assert_eq!(acc1.min, 10.0);
    assert_eq!(acc1.max, 50.0);
    assert_eq!(acc1.variance(), 250.0);
    assert!((acc1.stddev() - 250.0f64.sqrt()).abs() < 1e-9);

    // Test weighted updates
    let mut acc_w = H3Accumulator::new_weighted(100.0, 0.4);
    acc_w.update_weighted(200.0, 0.6);
    assert_eq!(acc_w.count, 1.0);
    assert_eq!(acc_w.sum, 160.0);
    assert_eq!(acc_w.mean(), 160.0);
}

#[test]
fn test_categorical_accumulator_operations() {
    let mut acc1 = CategoricalAccumulator::new();
    // Hex with 70 pixels of class 10 (Forest) and 30 pixels of class 20 (Grassland)
    for _ in 0..70 {
        acc1.update(10);
    }
    for _ in 0..30 {
        acc1.update(20);
    }

    assert_eq!(acc1.total_count, 100.0);
    assert_eq!(acc1.unique_classes(), 2);

    let (maj_cat, maj_cnt, maj_frac) = acc1.majority();
    assert_eq!(maj_cat, 10);
    assert_eq!(maj_cnt, 70.0);
    assert!((maj_frac - 0.70).abs() < 1e-6);

    let json = acc1.histogram_json();
    assert!(json.contains("\"10\": 0.7000"));
    assert!(json.contains("\"20\": 0.3000"));

    // Test weighted updates (e.g. sub-pixel sampling)
    let mut acc2 = CategoricalAccumulator::new();
    acc2.update_weighted(50, 0.5); // 0.5 of class 50 (Urban)
    acc2.update_weighted(10, 0.5);

    acc1.merge(&acc2);
    assert_eq!(acc1.total_count, 101.0);
    assert_eq!(acc1.unique_classes(), 3);
}

#[test]
fn test_categorical_tied_majority() {
    let mut acc = CategoricalAccumulator::new();
    // Exactly 50 pixels of class 10 and 50 pixels of class 20
    for _ in 0..50 {
        acc.update(10);
    }
    for _ in 0..50 {
        acc.update(20);
    }

    assert_eq!(acc.total_count, 100.0);
    assert_eq!(acc.unique_classes(), 2);

    let (maj_cat, maj_cnt, maj_frac) = acc.majority();
    // Deterministically returns one of the two top classes
    assert!(maj_cat == 10 || maj_cat == 20);
    assert_eq!(maj_cnt, 50.0);
    assert!((maj_frac - 0.50).abs() < 1e-6);
}

#[test]
fn test_accumulator_single_sample_variance_and_empty_default() {
    // Empty accumulator
    let acc_empty = H3Accumulator::default();
    assert_eq!(acc_empty.count, 0.0);
    assert!(acc_empty.mean().is_nan());
    assert!(acc_empty.variance().is_nan());
    assert!(acc_empty.stddev().is_nan());
    assert_eq!(acc_empty.min, f64::INFINITY);
    assert_eq!(acc_empty.max, f64::NEG_INFINITY);

    // Single sample accumulator (variance must be exactly 0.0)
    let acc_single = H3Accumulator::new(99.0);
    assert_eq!(acc_single.count, 1.0);
    assert_eq!(acc_single.mean(), 99.0);
    assert_eq!(acc_single.variance(), 0.0);
    assert_eq!(acc_single.stddev(), 0.0);
    assert_eq!(acc_single.min, 99.0);
    assert_eq!(acc_single.max, 99.0);
}

#[test]
fn test_categorical_accumulator_merge_operations() {
    let mut acc1 = CategoricalAccumulator::new();
    acc1.update(10);
    acc1.update(10);
    acc1.update(20);

    let mut acc2 = CategoricalAccumulator::new();
    acc2.update(20);
    acc2.update(30);

    acc1.merge(&acc2);

    assert_eq!(acc1.total_count, 5.0);
    assert_eq!(acc1.unique_classes(), 3);
    assert_eq!(acc1.get_class_count(10), 2.0);
    assert_eq!(acc1.get_class_count(20), 2.0);
    assert_eq!(acc1.get_class_count(30), 1.0);

    let (maj_cat, maj_cnt, maj_frac) = acc1.majority();
    assert!(maj_cat == 10 || maj_cat == 20);
    assert_eq!(maj_cnt, 2.0);
    assert_eq!(maj_frac, 0.40);
}

#[test]
fn test_welford_accumulator_extreme_mixed_bathymetry_numerical_stability() {
    let mut acc = H3Accumulator::default();

    // 1,000,000 samples spanning from Mariana Trench (-10,928m) to Mt Everest (+8,848.86m)
    let samples = [
        -10928.0f64,
        -5000.0,
        -100.0,
        0.0,
        500.0,
        2500.0,
        5895.0,
        8848.86,
    ];
    let num_repeats = 125_000; // 8 * 125,000 = 1,000,000 samples
    let total_count = (samples.len() * num_repeats) as f64;

    let true_sum: f64 = samples.iter().sum::<f64>() * (num_repeats as f64);
    let true_mean: f64 = true_sum / total_count;
    let true_var: f64 = samples
        .iter()
        .map(|&x| (x - true_mean) * (x - true_mean))
        .sum::<f64>()
        * (num_repeats as f64)
        / (total_count - 1.0);

    for _ in 0..num_repeats {
        for &val in &samples {
            acc.update(val);
        }
    }

    assert_eq!(acc.count, total_count);
    assert_eq!(acc.min, -10928.0);
    assert_eq!(acc.max, 8848.86);

    // Welford online mean and sample variance must match 2-pass exact reference
    assert!(
        (acc.mean() - true_mean).abs() < 1e-8,
        "Mean error must be < 1e-8"
    );
    assert!(
        (acc.variance() - true_var).abs() < 1e-4,
        "Variance error must be < 1e-4"
    );
    assert!(acc.variance() >= 0.0, "Variance must never be negative");
    assert!((acc.stddev() - true_var.sqrt()).abs() < 1e-6);
}

#[test]
fn test_categorical_accumulator_high_cardinality_shannon_entropy() {
    let mut acc = CategoricalAccumulator::default();

    // Ingest 256 unique categories with uniform frequency (10 pixels each = 2560 pixels total)
    for class_id in 0..=255i64 {
        for _ in 0..10 {
            acc.update(class_id);
        }
    }

    assert_eq!(acc.unique_classes(), 256);
    assert_eq!(acc.total_count, 2560.0);
    let (_maj_cat, maj_count, maj_frac) = acc.majority();
    assert_eq!(maj_count, 10.0);
    assert_eq!(maj_frac, 10.0 / 2560.0);

    // Analytical Shannon entropy for uniform distribution over N=256 is ln(256) nats
    let entropy = acc.shannon_entropy();
    let expected_entropy = (256.0f64).ln();
    assert!(
        (entropy - expected_entropy).abs() < 1e-10,
        "Entropy of uniform 256 classes must equal ln(256), got {}",
        entropy
    );

    // Test extreme single-class concentration (100% pure class) -> Shannon entropy must be exactly 0.0
    let mut pure_acc = CategoricalAccumulator::default();
    for _ in 0..1000 {
        pure_acc.update(42);
    }
    assert_eq!(pure_acc.unique_classes(), 1);
    let (_pure_cat, pure_count, pure_frac) = pure_acc.majority();
    assert_eq!(pure_count, 1000.0);
    assert_eq!(pure_frac, 1.0);
    assert_eq!(pure_acc.shannon_entropy(), 0.0);
}

#[test]
fn test_simd_span_f32_accuracy_and_welford_equivalence() {
    use raster_h3_core::aggregator::simd::SimdSpanAccumulate;

    // 1. Test arbitrary floating-point values
    let raw_vals: Vec<f32> = (0..127).map(|i| ((i * 7 + 13) % 97) as f32 * 1.5).collect();
    let mut welford = H3Accumulator::default();
    for &v in &raw_vals {
        welford.update(v as f64);
    }

    let simd_acc = f32::accumulate_span(&raw_vals, None);

    assert_eq!(simd_acc.count, welford.count);
    assert!((simd_acc.sum - welford.sum).abs() < 1e-4);
    assert!((simd_acc.mean() - welford.mean()).abs() < 1e-6);
    assert!((simd_acc.variance() - welford.variance()).abs() < 1e-4);
    assert_eq!(simd_acc.min, welford.min);
    assert_eq!(simd_acc.max, welford.max);

    // 2. Test uniform span shortcut (zero variance)
    let uniform_vals = vec![42.5f32; 100];
    let uniform_acc = f32::accumulate_span(&uniform_vals, None);
    assert_eq!(uniform_acc.count, 100.0);
    assert_eq!(uniform_acc.min, 42.5);
    assert_eq!(uniform_acc.max, 42.5);
    assert_eq!(uniform_acc.m2, 0.0);
    assert_eq!(uniform_acc.variance(), 0.0);

    // 3. Test with NoData filtering
    let mut nodata_vals = raw_vals.clone();
    nodata_vals[5] = -9999.0;
    nodata_vals[15] = -9999.0;
    nodata_vals[50] = -9999.0;

    let nd_acc = f32::accumulate_span(&nodata_vals, Some(-9999.0));
    assert_eq!(nd_acc.count, 124.0);
    assert!(
        (nd_acc.sum
            - (welford.sum - raw_vals[5] as f64 - raw_vals[15] as f64 - raw_vals[50] as f64))
            .abs()
            < 1e-4
    );
}

#[test]
fn test_simd_span_f64_accuracy_and_welford_equivalence() {
    use raster_h3_core::aggregator::simd::SimdSpanAccumulate;

    let raw_vals: Vec<f64> = (0..127)
        .map(|i| ((i * 11 + 7) % 89) as f64 * 2.25)
        .collect();
    let mut welford = H3Accumulator::default();
    for &v in &raw_vals {
        welford.update(v);
    }

    let simd_acc = f64::accumulate_span(&raw_vals, None);
    assert_eq!(simd_acc.count, welford.count);
    assert!((simd_acc.sum - welford.sum).abs() < 1e-4);
    assert!((simd_acc.mean() - welford.mean()).abs() < 1e-6);
    assert!((simd_acc.variance() - welford.variance()).abs() < 1e-4);
    assert_eq!(simd_acc.min, welford.min);
    assert_eq!(simd_acc.max, welford.max);

    // Test with NoData
    let mut nodata_vals = raw_vals.clone();
    nodata_vals[10] = -999.0;
    nodata_vals[20] = -999.0;
    let nd_acc = f64::accumulate_span(&nodata_vals, Some(-999.0));
    assert_eq!(nd_acc.count, 125.0);
}

#[test]
fn test_simd_span_variable_lengths() {
    use raster_h3_core::aggregator::simd::SimdSpanAccumulate;

    // Test slice lengths from 0 to 33 to test all remainder combinations of 8-lane chunks
    for len in 0..=33 {
        let vals: Vec<f32> = (0..len).map(|i| (i as f32) + 1.0).collect();
        let mut expected = H3Accumulator::default();
        for &v in &vals {
            expected.update(v as f64);
        }

        let acc = f32::accumulate_span(&vals, None);
        assert_eq!(acc.count, expected.count, "Count mismatch for len {}", len);
        if len > 0 {
            assert!(
                (acc.sum - expected.sum).abs() < 1e-4,
                "Sum mismatch for len {}",
                len
            );
            assert!(
                (acc.mean() - expected.mean()).abs() < 1e-6,
                "Mean mismatch for len {}",
                len
            );
        }
    }
}

#[test]
fn test_simd_span_integer_types() {
    use raster_h3_core::aggregator::simd::SimdSpanAccumulate;

    // Test u8
    let u8_vals: Vec<u8> = (0..200).map(|i| (i % 50) as u8).collect();
    let u8_acc = u8::accumulate_span(&u8_vals, Some(0));
    assert_eq!(u8_acc.count, 196.0); // 4 zeroes skipped

    // Test u16
    let u16_vals: Vec<u16> = (0..500).map(|i| (i * 10) as u16).collect();
    let u16_acc = u16::accumulate_span(&u16_vals, None);
    assert_eq!(u16_acc.count, 500.0);
    assert_eq!(u16_acc.min, 0.0);
    assert_eq!(u16_acc.max, 4990.0);

    // Test i32
    let i32_vals: Vec<i32> = vec![-100, 200, 500, -300, 1000];
    let i32_acc = i32::accumulate_span(&i32_vals, Some(-300));
    assert_eq!(i32_acc.count, 4.0);
    assert_eq!(i32_acc.sum, 1600.0);
}

#[test]
fn test_simd_span_large_integers_precision_preservation() {
    use raster_h3_core::aggregator::simd::SimdSpanAccumulate;

    // Values beyond 2^53 (9_007_199_254_740_992) where f64 loses integer precision.
    // 2^53 + 1 cannot be represented distinctly in f64 from 2^53.
    let base_u64: u64 = 1u64 << 53; // 9_007_199_254_740_992
    let val_distinct = base_u64 + 1; // 9_007_199_254_740_993
    let nodata_marker = base_u64;

    // Verify in f64 that (val_distinct as f64) == (nodata_marker as f64)
    assert_eq!(
        val_distinct as f64, nodata_marker as f64,
        "IEEE 754 precision loss must hold for this test setup"
    );

    // Test that native u64 NoData check does NOT treat val_distinct as nodata
    let u64_slice = vec![base_u64, val_distinct, base_u64 + 2, base_u64];
    let u64_acc = u64::accumulate_span(&u64_slice, Some(nodata_marker));
    // Only the two occurrences of base_u64 should be skipped; val_distinct and base_u64 + 2 must be kept!
    assert_eq!(u64_acc.count, 2.0);

    // Test i64 similarly with large values
    let base_i64: i64 = 1i64 << 53;
    let i64_distinct = base_i64 + 1;
    let i64_nodata = base_i64;
    let i64_slice = vec![base_i64, i64_distinct, -base_i64, -base_i64 - 1];
    let i64_acc = i64::accumulate_span(&i64_slice, Some(i64_nodata));
    assert_eq!(i64_acc.count, 3.0);
}

#[test]
fn test_categorical_16_slot_inline_and_heap_spillover() {
    use raster_h3_core::aggregator::categorical::CategoricalAccumulator;

    let mut cat = CategoricalAccumulator::new();

    // 1. Add 16 distinct classes: should remain 100% inline with zero heap allocation
    for c in 1..=16 {
        cat.update_weighted(c, (c * 10) as f64);
    }

    assert_eq!(cat.inline_len, 16);
    assert!(
        cat.heap_counts.is_none(),
        "Must stay inline up to 16 classes"
    );
    assert_eq!(cat.unique_classes(), 16);
    let expected_sum: f64 = (1..=16).map(|c| (c * 10) as f64).sum();
    assert_eq!(cat.total_count, expected_sum);
    assert_eq!(cat.get_class_count(5), 50.0);
    assert_eq!(cat.get_class_count(16), 160.0);

    let (maj_cls, maj_count, maj_frac) = cat.majority();
    assert_eq!(maj_cls, 16);
    assert_eq!(maj_count, 160.0);
    assert!((maj_frac - (160.0 / expected_sum)).abs() < 1e-6);

    // 2. Add 17th class: should spill to heap
    cat.update_weighted(17, 200.0);
    assert!(
        cat.heap_counts.is_some(),
        "Must spill to heap on 17th class"
    );
    assert_eq!(cat.unique_classes(), 17);
    assert_eq!(cat.total_count, expected_sum + 200.0);
    assert_eq!(cat.get_class_count(17), 200.0);
    assert_eq!(cat.majority().0, 17);

    // 3. Merge with another inline accumulator
    let mut cat2 = CategoricalAccumulator::new();
    cat2.update_weighted(1, 20.0);
    cat2.update_weighted(18, 50.0);

    cat.merge(&cat2);
    assert_eq!(cat.unique_classes(), 18);
    assert_eq!(cat.get_class_count(1), 30.0);
    assert_eq!(cat.get_class_count(18), 50.0);
    assert_eq!(cat.total_count, expected_sum + 200.0 + 70.0);
}

#[test]
fn test_categorical_simd_uniformity_types_and_spans() {
    use raster_h3_core::aggregator::categorical::CategoricalUniformity;

    // Test helper for various lengths and mismatch positions
    fn check_uniformity<T: CategoricalUniformity + std::fmt::Debug>(val: T, diff_val: T) {
        // Empty & single
        assert!(T::is_uniform(&[]));
        assert!(T::is_uniform(&[val]));

        // Various span sizes: 2, 7, 8, 15, 16, 31, 32, 63, 64, 100
        for len in [2, 7, 8, 15, 16, 31, 32, 63, 64, 100] {
            let uniform_vec = vec![val; len];
            assert!(
                T::is_uniform(&uniform_vec),
                "Expected uniform for len {}",
                len
            );

            // Mismatch at start
            let mut diff_start = uniform_vec.clone();
            diff_start[0] = diff_val;
            assert!(
                !T::is_uniform(&diff_start),
                "Expected non-uniform at start for len {}",
                len
            );

            // Mismatch at end
            let mut diff_end = uniform_vec.clone();
            diff_end[len - 1] = diff_val;
            assert!(
                !T::is_uniform(&diff_end),
                "Expected non-uniform at end for len {}",
                len
            );

            // Mismatch at middle
            if len > 2 {
                let mut diff_mid = uniform_vec.clone();
                diff_mid[len / 2] = diff_val;
                assert!(
                    !T::is_uniform(&diff_mid),
                    "Expected non-uniform at mid for len {}",
                    len
                );
            }
        }
    }

    check_uniformity::<u8>(42, 99);
    check_uniformity::<i8>(-10, 20);
    check_uniformity::<u16>(1000, 2000);
    check_uniformity::<i16>(-500, 500);
    check_uniformity::<u32>(100_000, 200_000);
    check_uniformity::<i32>(-100_000, 100_000);
    check_uniformity::<u64>(1_000_000_000, 2_000_000_000);
    check_uniformity::<i64>(-1_000_000_000, 1_000_000_000);
    check_uniformity::<f32>(3.25, 2.75);
    check_uniformity::<f64>(1.45, 1.75);

    // Float +0.0 and -0.0 equivalence
    let zeros_f32 = vec![0.0f32, -0.0f32, 0.0f32, -0.0f32];
    assert!(f32::is_uniform(&zeros_f32));
    let zeros_f64 = vec![0.0f64, -0.0f64, 0.0f64, -0.0f64];
    assert!(f64::is_uniform(&zeros_f64));
}

#[test]
fn apply_affine_matches_accumulating_transformed_values() {
    let values = [3.0, -1.5, 8.25, 0.0, 4.0, 4.0, 11.5];
    for (scale, offset) in [(0.01, 5.0), (-2.5, 100.0), (1.0, -7.0)] {
        let mut raw = H3Accumulator::default();
        let mut expected = H3Accumulator::default();
        for v in values {
            raw.update(v);
            expected.update(v * scale + offset);
        }
        raw.apply_affine(scale, offset);
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0);
        assert_eq!(raw.count, expected.count);
        assert!(close(raw.sum, expected.sum), "{} {}", raw.sum, expected.sum);
        assert!(close(raw.mean(), expected.mean()));
        assert!(close(raw.variance(), expected.variance()));
        assert!(close(raw.min, expected.min) && close(raw.max, expected.max));
    }
    let mut empty = H3Accumulator::default();
    empty.apply_affine(-3.0, 1.0);
    assert_eq!(empty, H3Accumulator::default());
}
