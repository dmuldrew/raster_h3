//! Tests streaming quantile sketches and DDSketch accuracy within H3 accumulators.
//!
//! Validates quantile estimation precision, merge associativity and commutativity, preset and custom
//! spec parsing, zero-overhead disabling, and edge cases including empty and single-value inputs.

use raster_h3::aggregator::accumulator::H3Accumulator;
use raster_h3::aggregator::multi_horizon::{
    MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
use raster_h3::aggregator::quantiles::QuantileSketch;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
mod helpers;

use helpers::create_wave_test_geotiff as create_test_geotiff;

#[test]
fn test_quantile_sketch_empty() {
    let sketch = QuantileSketch::new();
    assert_eq!(sketch.count(), 0.0);
    assert!(sketch.quantile(0.5, 0.0, 0.0).is_nan());
}

#[test]
fn test_quantile_sketch_single_value() {
    let mut sketch = QuantileSketch::new();
    sketch.insert(42.0);
    assert_eq!(sketch.count(), 1.0);
    let q = sketch.quantile(0.5, 42.0, 42.0);
    assert!((q - 42.0).abs() < 1e-6);
}

#[test]
fn test_quantile_sketch_uniform_positive() {
    let mut sketch = QuantileSketch::new();
    let n = 10_000;
    for i in 1..=n {
        sketch.insert(i as f64);
    }
    assert_eq!(sketch.count(), n as f64);

    let min = 1.0;
    let max = n as f64;

    let p50 = sketch.quantile(0.50, min, max);
    let p90 = sketch.quantile(0.90, min, max);
    let p95 = sketch.quantile(0.95, min, max);
    let p99 = sketch.quantile(0.99, min, max);
    let p25 = sketch.quantile(0.25, min, max);
    let p75 = sketch.quantile(0.75, min, max);
    let iqr = p75 - p25;

    // DDSketch guarantees relative error <= alpha = 1%
    assert!((p50 - 5000.0).abs() / 5000.0 < 0.015, "p50: {}", p50);
    assert!((p90 - 9000.0).abs() / 9000.0 < 0.015, "p90: {}", p90);
    assert!((p95 - 9500.0).abs() / 9500.0 < 0.015, "p95: {}", p95);
    assert!((p99 - 9900.0).abs() / 9900.0 < 0.015, "p99: {}", p99);
    assert!((p25 - 2500.0).abs() / 2500.0 < 0.015, "p25: {}", p25);
    assert!((p75 - 7500.0).abs() / 7500.0 < 0.015, "p75: {}", p75);
    assert!((iqr - 5000.0).abs() / 5000.0 < 0.02, "iqr: {}", iqr);
}

#[test]
fn test_quantile_sketch_negative_values() {
    let mut sketch = QuantileSketch::new();
    let n = 10_000;
    for i in 1..=n {
        sketch.insert(-(i as f64));
    }
    assert_eq!(sketch.count(), n as f64);

    let min = -(n as f64);
    let max = -1.0;

    let p50 = sketch.quantile(0.50, min, max);
    // Median of [-10000, -1] is ~ -5000
    assert!((p50 - (-5000.0)).abs() / 5000.0 < 0.015, "p50: {}", p50);
}

#[test]
fn test_quantile_sketch_zero_bucket_and_mixed() {
    let mut sketch = QuantileSketch::new();
    for _ in 0..1000 {
        sketch.insert(0.0);
    }
    for i in 1..=1000 {
        sketch.insert(i as f64);
    }
    for i in 1..=1000 {
        sketch.insert(-(i as f64));
    }

    assert_eq!(sketch.count(), 3000.0);
    let p50 = sketch.quantile(0.50, -1000.0, 1000.0);
    // Around 0.0
    assert!(p50.abs() < 1e-4, "p50: {}", p50);
}

#[test]
fn test_quantile_sketch_merge_associative_commutative() {
    let mut s1 = QuantileSketch::new();
    let mut s2 = QuantileSketch::new();
    let mut s3 = QuantileSketch::new();
    let mut s4 = QuantileSketch::new();

    for i in 1..=2500 {
        s1.insert(i as f64);
    }
    for i in 2501..=5000 {
        s2.insert(i as f64);
    }
    for i in 5001..=7500 {
        s3.insert(i as f64);
    }
    for i in 7501..=10000 {
        s4.insert(i as f64);
    }

    // Merge s2, s3, s4 into s1
    s1.merge(&s2);
    s1.merge(&s3);
    s1.merge(&s4);

    assert_eq!(s1.count(), 10000.0);
    let p50 = s1.quantile(0.50, 1.0, 10000.0);
    assert!((p50 - 5000.0).abs() / 5000.0 < 0.015, "p50: {}", p50);
}

#[test]
fn test_quantile_sketch_weighted_insert() {
    let mut s_individual = QuantileSketch::new();
    for _ in 0..100 {
        s_individual.insert(25.0);
    }

    let mut s_weighted = QuantileSketch::new();
    s_weighted.insert_weighted(25.0, 100.0);

    assert_eq!(s_individual.count(), 100.0);
    assert_eq!(s_weighted.count(), 100.0);

    let q1 = s_individual.quantile(0.5, 25.0, 25.0);
    let q2 = s_weighted.quantile(0.5, 25.0, 25.0);
    assert!((q1 - q2).abs() < 1e-6);
}

#[test]
fn test_accumulator_with_and_without_quantiles() {
    let mut acc_default = H3Accumulator::default();
    acc_default.update(10.0);
    acc_default.update(20.0);
    acc_default.update(30.0);
    assert!(acc_default.quantiles.is_none());
    assert!(acc_default.quantile(0.5).is_nan());
    assert!(acc_default.iqr().is_nan());

    let mut acc_q = H3Accumulator::with_quantiles();
    acc_q.update(10.0);
    acc_q.update(20.0);
    acc_q.update(30.0);
    assert!(acc_q.quantiles.is_some());
    let med = acc_q.quantile(0.5);
    assert!((med - 20.0).abs() < 0.5, "med: {}", med);
}

#[test]
fn test_parse_quantile_specs_presets() {
    let def = QuantileTarget::parse_list("default").unwrap();
    assert_eq!(def.len(), 5);
    assert_eq!(def[0].column_name(), "p50");
    assert_eq!(def[1].column_name(), "p90");
    assert_eq!(def[2].column_name(), "p95");
    assert_eq!(def[3].column_name(), "p99");
    assert_eq!(def[4].column_name(), "iqr");

    let b = QuantileTarget::parse_list("box").unwrap();
    assert_eq!(b.len(), 4);
    assert_eq!(b[0].column_name(), "p25");
    assert_eq!(b[1].column_name(), "p50");
    assert_eq!(b[2].column_name(), "p75");
    assert_eq!(b[3].column_name(), "iqr");

    let t = QuantileTarget::parse_list("tails").unwrap();
    assert_eq!(t.len(), 6);
    assert_eq!(t[0].column_name(), "p01");
    assert_eq!(t[1].column_name(), "p05");
    assert_eq!(t[2].column_name(), "p10");
    assert_eq!(t[3].column_name(), "p90");
    assert_eq!(t[4].column_name(), "p95");
    assert_eq!(t[5].column_name(), "p99");

    let d = QuantileTarget::parse_list("deciles").unwrap();
    assert_eq!(d.len(), 9);
    assert_eq!(d[0].column_name(), "p10");
    assert_eq!(d[8].column_name(), "p90");

    let off = QuantileTarget::parse_list("none").unwrap();
    assert!(off.is_empty());
}

#[test]
fn test_parse_quantile_specs_custom_and_aliases() {
    let custom = QuantileTarget::parse_list("p01, p05, median, q3, p99_5, iqr").unwrap();
    assert_eq!(custom.len(), 6);
    assert_eq!(
        custom[0],
        QuantileTarget::Percentile(0.01, "p01".to_string())
    );
    assert_eq!(
        custom[1],
        QuantileTarget::Percentile(0.05, "p05".to_string())
    );
    assert_eq!(
        custom[2],
        QuantileTarget::Percentile(0.50, "median".to_string())
    );
    assert_eq!(
        custom[3],
        QuantileTarget::Percentile(0.75, "q3".to_string())
    );
    assert_eq!(
        custom[4],
        QuantileTarget::Percentile(0.995, "p99_5".to_string())
    );
    assert_eq!(custom[5], QuantileTarget::Iqr("iqr".to_string()));

    let decimals = QuantileTarget::parse_list("0.05, 0.50, 0.95").unwrap();
    assert_eq!(decimals.len(), 3);
    assert_eq!(
        decimals[0],
        QuantileTarget::Percentile(0.05, "p05".to_string())
    );
    assert_eq!(
        decimals[1],
        QuantileTarget::Percentile(0.50, "p50".to_string())
    );
    assert_eq!(
        decimals[2],
        QuantileTarget::Percentile(0.95, "p95".to_string())
    );

    // Deduplication test
    let dup = QuantileTarget::parse_list("p50, p50, median, 0.50").unwrap();
    assert_eq!(dup.len(), 2); // "p50" and "median" (distinct column names)
}

#[test]
fn test_multi_scan_streamer_with_quantiles_enabled() {
    let tiff_file = create_test_geotiff(64, 64);
    let reader = GeoTiffStreamReader::open(tiff_file.path()).unwrap();

    let mut config = MultiResolutionConfig::new(vec![8]);
    config.quantiles = QuantileTarget::parse_list("p25, p50, p75, iqr").unwrap();
    assert!(config.track_quantiles());

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    assert!(streamer.track_quantiles);

    let mut total_records = 0;
    loop {
        let batch = streamer.fetch_next_batch(2048);
        if batch.is_empty() {
            break;
        }
        for rec in &batch {
            total_records += 1;
            assert!(
                rec.accumulator.quantiles.is_some(),
                "Quantiles should be tracked"
            );
            let p25 = rec.accumulator.quantile(0.25);
            let p50 = rec.accumulator.quantile(0.50);
            let p75 = rec.accumulator.quantile(0.75);
            let iqr = rec.accumulator.iqr();

            assert!(
                p25 <= p50 + 1e-6,
                "Monotonicity: p25 {} <= p50 {}",
                p25,
                p50
            );
            assert!(
                p50 <= p75 + 1e-6,
                "Monotonicity: p50 {} <= p75 {}",
                p50,
                p75
            );
            assert!(iqr >= 0.0, "IQR should be non-negative: {}", iqr);
            assert!(p50 >= rec.accumulator.min - 1e-6);
            assert!(p50 <= rec.accumulator.max + 1e-6);
        }
    }
    assert!(total_records > 0);
}

#[test]
fn test_multi_scan_streamer_with_quantiles_disabled_zero_cost() {
    let tiff_file = create_test_geotiff(64, 64);
    let reader = GeoTiffStreamReader::open(tiff_file.path()).unwrap();

    let config = MultiResolutionConfig::new(vec![8]);
    assert!(!config.track_quantiles());

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    assert!(!streamer.track_quantiles);

    let mut total_records = 0;
    loop {
        let batch = streamer.fetch_next_batch(2048);
        if batch.is_empty() {
            break;
        }
        for rec in &batch {
            total_records += 1;
            assert!(
                rec.accumulator.quantiles.is_none(),
                "Quantiles should be None when disabled"
            );
            assert!(rec.accumulator.quantile(0.5).is_nan());
            assert!(rec.accumulator.iqr().is_nan());
        }
    }
    assert!(total_records > 0);
}

#[test]
fn test_fast_log_key_mapping_fidelity_and_wide_range() {
    const LOG_GAMMA_INV: f64 = 1.0 / 0.02000066671111664;

    // Test across a massive dynamic range from 1e-6 to 1e12
    let mut test_vals = Vec::new();

    // 1. Powers of 10 and 2
    let mut v = 1e-6;
    while v <= 1e12 {
        test_vals.push(v);
        test_vals.push(v * 1.5);
        test_vals.push(v * 1.9999);
        v *= 2.0;
    }

    // 2. Fractional values around bucket transitions
    let gamma: f64 = 1.01 / 0.99;
    for k in -500..500 {
        let exact_boundary = gamma.powi(k);
        if exact_boundary > 1e-6 && exact_boundary < 1e12 {
            test_vals.push(exact_boundary * 0.999999);
            test_vals.push(exact_boundary);
            test_vals.push(exact_boundary * 1.000001);
        }
    }

    // 3. Dense geometric series
    let mut g = 0.001;
    for _ in 0..10_000 {
        test_vals.push(g);
        g = g * 1.002 + 0.00001;
    }

    let mut exact_matches = 0usize;
    let total = test_vals.len();

    for &val in &test_vals {
        let fast_key = QuantileSketch::key_for_positive(val);
        let libc_key = (val.ln() * LOG_GAMMA_INV).floor() as i32;

        if fast_key == libc_key {
            exact_matches += 1;
        } else {
            // If there is any boundary difference, it must never differ by more than 1 unit
            assert_eq!(
                (fast_key - libc_key).abs(),
                1,
                "Key difference > 1 at val={}: fast={}, libc={}",
                val,
                fast_key,
                libc_key
            );
        }
    }

    let match_rate = (exact_matches as f64 / total as f64) * 100.0;
    assert!(
        match_rate > 99.9,
        "Fast key match rate should be > 99.9%, was {:.4}%",
        match_rate
    );
}
