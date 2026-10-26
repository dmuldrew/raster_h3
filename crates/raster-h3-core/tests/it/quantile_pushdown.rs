//! Batched quantile evaluation and quantile pushdown.
//!
//! `QuantileSketch::quantiles_batch` and `QuantilePlan` must return exactly
//! what per-target `quantile()` / `iqr()` return, including NaN, endpoint,
//! out-of-range, duplicate and unsorted targets. Readers must skip building
//! sketches when no quantile column is projected, without changing any other
//! output.

use crate::helpers::create_wave_test_geotiff;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use raster_h3_core::aggregator::accumulator::H3Accumulator;
use raster_h3_core::aggregator::multi_horizon::{
    MultiResolutionConfig, MultiScanHorizonStreamer, QuantilePlan, QuantileTarget,
};
use raster_h3_core::columnar::{
    quantiles_projected, ColumnArray, ContinuousBatchReader, RecordBatch,
};
use raster_h3_core::raster::mosaic::MosaicReader;

/// Bit-for-bit equality, treating every NaN as equal.
fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

/// Near-equality for values from two separate (parallel) aggregation runs,
/// whose float summation order is not guaranteed.
fn close(a: f64, b: f64) -> bool {
    same(a, b) || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn close_opt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => close(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn accumulator(values: impl IntoIterator<Item = f64>) -> H3Accumulator {
    let mut acc = H3Accumulator::with_quantiles();
    for v in values {
        acc.update(v);
    }
    acc
}

/// Accumulators covering empty, single-value, negative, zero, duplicate and wide-range data.
fn datasets() -> Vec<(&'static str, H3Accumulator)> {
    vec![
        ("empty", H3Accumulator::with_quantiles()),
        ("single", accumulator([42.0])),
        ("positive", accumulator((1..=1000).map(f64::from))),
        ("negative", accumulator((1..=500).map(|i| -f64::from(i)))),
        (
            "mixed_with_zeros",
            accumulator((-100..=100).map(f64::from).chain([0.0; 50])),
        ),
        (
            "duplicates",
            accumulator(std::iter::repeat_n(0.5, 1000).chain([1e6; 3])),
        ),
        (
            "wide_range",
            accumulator((-90..=90).map(|e| {
                let mag = 10f64.powf(f64::from(e) / 10.0);
                if e % 3 == 0 {
                    -mag
                } else {
                    mag
                }
            })),
        ),
    ]
}

/// Unsorted, repeated, endpoint, out-of-range and non-finite targets.
const TARGETS: [f64; 17] = [
    0.5,
    0.01,
    0.99,
    0.0,
    1.0,
    -0.25,
    1.5,
    f64::NAN,
    0.5,
    0.25,
    0.75,
    1e-12,
    1.0 - 1e-12,
    0.333,
    f64::INFINITY,
    f64::NEG_INFINITY,
    0.9,
];

#[test]
fn quantiles_batch_matches_individual_quantiles() {
    for (name, acc) in datasets() {
        let sketch = acc.quantiles.as_deref().unwrap();
        let bounds = [
            (acc.min, acc.max),
            // Degenerate and near-degenerate clamp ranges.
            (5.0, 5.0),
            (5.0, 5.0 + 1e-13),
        ];
        for (min, max) in bounds {
            let batch = sketch.quantiles_batch(&TARGETS, min, max);
            assert_eq!(batch.len(), TARGETS.len());
            for (i, &q) in TARGETS.iter().enumerate() {
                let single = sketch.quantile(q, min, max);
                assert!(
                    same(batch[i], single),
                    "{name} [{min}, {max}] q={q}: batch {} vs single {single}",
                    batch[i]
                );
            }
        }
        assert!(sketch.quantiles_batch(&[], acc.min, acc.max).is_empty());
    }
}

#[test]
fn quantiles_batch_survives_all_nan_targets() {
    let acc = accumulator((1..=100).map(f64::from));
    let sketch = acc.quantiles.as_deref().unwrap();
    let out = sketch.quantiles_batch(&[f64::NAN; 5], acc.min, acc.max);
    assert!(out.iter().all(|v| v.is_nan()));
}

#[test]
fn quantile_plan_matches_quantile_and_iqr() {
    let mut targets = QuantileTarget::parse_list("p50, p90, iqr, p25, p75, median, p01").unwrap();
    // Built directly: the parser no longer accepts NaN, but the enum is public.
    targets.push(QuantileTarget::Percentile(f64::NAN, "pnan".into()));
    let plan = QuantilePlan::new(&targets);
    assert_eq!(plan.len(), targets.len());

    let mut no_sketch = H3Accumulator::default();
    no_sketch.update(3.0);
    let mut cases = datasets();
    cases.push(("no_sketch", no_sketch));

    let mut out = Vec::new();
    for (name, acc) in &cases {
        plan.evaluate_into(acc, &mut out);
        assert_eq!(out.len(), targets.len());
        for (target, &got) in targets.iter().zip(&out) {
            let want = match target {
                QuantileTarget::Percentile(q, _) => acc.quantile(*q),
                QuantileTarget::Iqr(_) => acc.iqr(),
            };
            assert!(
                same(got, want),
                "{name} {}: plan {got} vs direct {want}",
                target.column_name()
            );
        }
    }

    let empty = QuantilePlan::new(std::iter::empty());
    assert!(empty.is_empty());
    empty.evaluate_into(&cases[2].1, &mut out);
    assert!(out.is_empty());
}

#[test]
fn single_probability_plan_matches_quantile() {
    let mut cases = datasets();
    let mut no_sketch = H3Accumulator::default();
    no_sketch.update(3.0);
    cases.push(("no_sketch", no_sketch));

    for (name, acc) in &cases {
        for &q in &TARGETS {
            for repeats in [1, 3] {
                let targets = vec![QuantileTarget::Percentile(q, "q".into()); repeats];
                let plan = QuantilePlan::new(&targets);
                let mut out = vec![123.0; 4];
                plan.evaluate_into(acc, &mut out);
                assert_eq!(out.len(), repeats);
                assert!(
                    out.iter().all(|&got| same(got, acc.quantile(q))),
                    "{name} q={q} repeats={repeats}: {out:?}"
                );
            }
        }
        let mut out = vec![123.0];
        QuantilePlan::default().evaluate_into(acc, &mut out);
        assert!(out.is_empty());
    }
}

#[test]
fn parser_rejects_non_finite_percentiles() {
    for spec in [
        "nan", "pnan", "NaN", "pNaN", "-nan", "p-nan", "inf", "pinf", "-inf",
    ] {
        assert!(
            QuantileTarget::parse_list(spec).is_err(),
            "{spec:?} should be rejected"
        );
    }
    // Valid specs still parse.
    assert_eq!(QuantileTarget::parse_list("p99.5, 0.05").unwrap().len(), 2);
}

#[test]
fn quantiles_projected_selects_only_quantile_columns() {
    assert!(!quantiles_projected(None, 0));
    assert!(quantiles_projected(None, 3));
    assert!(!quantiles_projected(Some(&[0, 2, 9][..]), 3));
    assert!(quantiles_projected(Some(&[0, 10][..]), 3));
    assert!(quantiles_projected(Some(&[12][..]), 3));
    assert!(!quantiles_projected(Some(&[13][..]), 3));
    assert!(!quantiles_projected(Some(&[][..]), 3));
}

// ---------------------------------------------------------------------------
// Reader-level pushdown
// ---------------------------------------------------------------------------

/// Continuous columns: h3_index = 0, mean = 2, count = 4; quantiles start at 10.
const H3_INDEX: usize = 0;
const MEAN: usize = 2;
const COUNT: usize = 4;

fn quantile_config() -> MultiResolutionConfig {
    let mut config = MultiResolutionConfig::new(vec![8]);
    config.quantiles = QuantileTarget::parse_list("p50, p90, iqr").unwrap();
    config
}

fn open(
    path: &Path,
    config: &MultiResolutionConfig,
    projection: Option<Vec<usize>>,
) -> ContinuousBatchReader {
    ContinuousBatchReader::open(&[path.to_path_buf()], config, projection, 256, None).unwrap()
}

fn u64_col(c: &ColumnArray) -> &[u64] {
    match c {
        ColumnArray::UInt64 { values, .. } => values,
        _ => panic!("expected UInt64"),
    }
}

/// Float column as `Option`s, honoring validity.
fn f64_col(c: &ColumnArray) -> Vec<Option<f64>> {
    match c {
        ColumnArray::Float64 { values, validity } => values
            .iter()
            .enumerate()
            .map(|(i, &v)| validity.as_ref().is_none_or(|m| m[i]).then_some(v))
            .collect(),
        _ => panic!("expected Float64"),
    }
}

/// Map h3_index -> remaining projected float columns.
fn collect(reader: ContinuousBatchReader) -> HashMap<u64, Vec<Option<f64>>> {
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    let mut out = HashMap::new();
    for b in &batches {
        let cells = u64_col(&b.columns[0]);
        let rest: Vec<Vec<Option<f64>>> = b.columns[1..].iter().map(f64_col).collect();
        for (r, &cell) in cells.iter().enumerate() {
            let row = rest.iter().map(|col| col[r]).collect();
            assert!(out.insert(cell, row).is_none(), "duplicate cell {cell}");
        }
    }
    out
}

#[test]
fn reader_skips_sketches_without_projected_quantiles() {
    let tif = create_wave_test_geotiff(64, 64);
    let path: PathBuf = tif.path().to_path_buf();
    let config = quantile_config();

    let lean = open(&path, &config, Some(vec![H3_INDEX, MEAN, COUNT]));
    assert!(!lean.tracks_quantiles(), "no quantile column projected");
    assert_eq!(lean.schema().fields.len(), 3);
    let lean = collect(lean);
    assert!(!lean.is_empty());

    // The same columns from a reader that does build sketches.
    let full = open(&path, &config, Some(vec![H3_INDEX, MEAN, COUNT, 11]));
    assert!(full.tracks_quantiles());
    let full = collect(full);

    assert_eq!(lean.len(), full.len());
    for (cell, row) in &lean {
        let other = &full[cell];
        assert!(
            row.iter().zip(&other[..2]).all(|(&a, &b)| close_opt(a, b)),
            "mean/count differ for cell {cell}: {row:?} vs {other:?}"
        );
    }

    assert!(open(&path, &config, None).tracks_quantiles());
    assert!(!open(&path, &MultiResolutionConfig::new(vec![8]), None).tracks_quantiles());
}

#[test]
fn reader_quantile_columns_match_per_record_quantiles() {
    let tif = create_wave_test_geotiff(64, 64);
    let path: PathBuf = tif.path().to_path_buf();
    let config = quantile_config();

    // Project quantiles out of order and repeated: iqr, p50, p90, p50.
    let got = collect(open(&path, &config, Some(vec![H3_INDEX, 12, 10, 11, 10])));

    let mosaic = Arc::new(
        MosaicReader::open(
            std::slice::from_ref(&path),
            config.bbox,
            config.custom_crs.as_deref(),
            config.overlap_rule,
        )
        .unwrap(),
    );
    let mut streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config).unwrap();
    let mut seen = 0;
    loop {
        let batch = streamer.fetch_next_batch(512).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = &rec.accumulator;
            let want = [
                acc.iqr(),
                acc.quantile(0.5),
                acc.quantile(0.9),
                acc.quantile(0.5),
            ];
            let row = &got[&rec.h3_index];
            for (g, w) in row.iter().zip(want) {
                match g {
                    Some(v) => assert!(close(*v, w), "cell {}: {v} vs {w}", rec.h3_index),
                    None => assert!(!w.is_finite(), "null for finite {w}"),
                }
            }
            seen += 1;
        }
    }
    assert_eq!(seen, got.len());
}
