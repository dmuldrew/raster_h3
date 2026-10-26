//! Long-format categorical output must expand cells incrementally.
//!
//! High-cardinality cells (hundreds of classes) are split across small batches
//! without changing the rows produced, batches never exceed the requested
//! size, and `LIMIT` stops expansion early.

use crate::helpers::TestGeoTiffBuilder;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig,
};
use raster_h3_core::columnar::{CategoricalBatchReader, CategoricalFormat, ColumnArray};
use raster_h3_core::raster::mosaic::MosaicReader;

/// Long-format column indices.
const H3_INDEX: usize = 0;
const CATEGORY: usize = 2;
const COUNT: usize = 3;
const FRACTION: usize = 4;
const TOTAL_COUNT: usize = 5;
const RESOLUTION: usize = 6;
const ENTROPY: usize = 8;
const DISTINCT: usize = 9;

const CLASSES: u32 = 300;

#[derive(Debug, Clone, Copy)]
struct Row {
    cell: u64,
    resolution: u8,
    category: i64,
    count: f64,
    fraction: f64,
    total_count: f64,
    entropy: f64,
    distinct: i64,
}

fn config() -> MultiResolutionConfig {
    MultiResolutionConfig::new(vec![7])
}

/// 200×200 Gray16 raster cycling through 300 classes, so every res-7 cell
/// holds far more categories than the small batch sizes used below.
fn high_cardinality_tif() -> (tempfile::NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(200, 200)
        .origin(-122.4, 37.8)
        .pixel_size(0.001)
        .create_tempfile(|col, row| ((row * 200 + col) % CLASSES + 1) as u16)
}

/// Expected rows built straight from the streamer's records.
fn reference_rows(path: &Path) -> Vec<Row> {
    let c = config();
    let mosaic = Arc::new(
        MosaicReader::open(
            &[path.to_path_buf()],
            c.bbox,
            c.custom_crs.as_deref(),
            c.overlap_rule,
        )
        .unwrap(),
    );
    let mut streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &c).unwrap();
    let mut rows = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(1024).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = &rec.accumulator;
            if acc.total_count <= 0.0 {
                continue;
            }
            let mut classes = Vec::new();
            acc.for_each_class(|k, v| classes.push((k, v)));
            classes.sort_unstable_by_key(|&(k, _)| k);
            for (category, count) in classes {
                rows.push(Row {
                    cell: rec.h3_index,
                    resolution: rec.resolution,
                    category,
                    count,
                    fraction: count / acc.total_count,
                    total_count: acc.total_count,
                    entropy: acc.shannon_entropy(),
                    distinct: acc.unique_classes() as i64,
                });
            }
        }
    }
    rows
}

fn u64_col(col: &ColumnArray) -> &[u64] {
    match col {
        ColumnArray::UInt64 { values, .. } => values,
        _ => panic!("expected UInt64 column"),
    }
}
fn i64_col(col: &ColumnArray) -> &[i64] {
    match col {
        ColumnArray::Int64 { values, .. } => values,
        _ => panic!("expected Int64 column"),
    }
}
fn f64_col(col: &ColumnArray) -> &[f64] {
    match col {
        ColumnArray::Float64 { values, .. } => values,
        _ => panic!("expected Float64 column"),
    }
}
fn u8_col(col: &ColumnArray) -> &[u8] {
    match col {
        ColumnArray::UInt8 { values, .. } => values,
        _ => panic!("expected UInt8 column"),
    }
}

/// Read every batch from a long-format reader over all columns, asserting no
/// batch exceeds `batch_size` and only the last one may be short.
fn read_long(path: &Path, batch_size: usize, limit: Option<usize>) -> Vec<Row> {
    let reader = CategoricalBatchReader::open(
        &[path.to_path_buf()],
        &config(),
        CategoricalFormat::Long,
        None,
        batch_size,
        limit,
    )
    .unwrap();

    let batches: Vec<_> = reader.map(|b| b.unwrap()).collect();
    let mut rows = Vec::new();
    for (i, batch) in batches.iter().enumerate() {
        assert!(batch.num_rows > 0, "empty batch {i}");
        assert!(batch.num_rows <= batch_size, "batch {i} exceeds batch_size");
        if i + 1 < batches.len() {
            assert_eq!(batch.num_rows, batch_size, "non-final batch {i} is short");
        }
        let c = &batch.columns;
        rows.extend((0..batch.num_rows).map(|r| Row {
            cell: u64_col(&c[H3_INDEX])[r],
            resolution: u8_col(&c[RESOLUTION])[r],
            category: i64_col(&c[CATEGORY])[r],
            count: f64_col(&c[COUNT])[r],
            fraction: f64_col(&c[FRACTION])[r],
            total_count: f64_col(&c[TOTAL_COUNT])[r],
            entropy: f64_col(&c[ENTROPY])[r],
            distinct: i64_col(&c[DISTINCT])[r],
        }));
    }
    rows
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn key(r: &Row) -> (u64, u8, i64) {
    (r.cell, r.resolution, r.category)
}

fn assert_same_rows(mut got: Vec<Row>, mut want: Vec<Row>) {
    got.sort_by_key(key);
    want.sort_by_key(key);
    assert_eq!(got.len(), want.len(), "row count differs");
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(key(g), key(w));
        assert_eq!(g.distinct, w.distinct, "distinct_classes for {:?}", key(w));
        for (name, a, b) in [
            ("count", g.count, w.count),
            ("fraction", g.fraction, w.fraction),
            ("total_count", g.total_count, w.total_count),
            ("entropy", g.entropy, w.entropy),
        ] {
            assert!(close(a, b), "{name} for {:?}: {a} vs {b}", key(w));
        }
    }
}

/// Within one reader's output, each cell's rows are contiguous and in
/// ascending category order, even when split across batch boundaries.
fn assert_cells_contiguous(rows: &[Row]) {
    let mut seen = std::collections::HashSet::new();
    for pair in rows.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if (a.cell, a.resolution) == (b.cell, b.resolution) {
            assert!(a.category < b.category, "categories out of order in a cell");
        } else {
            assert!(
                seen.insert((a.cell, a.resolution)),
                "cell {} reappeared after other cells",
                a.cell
            );
        }
    }
}

#[test]
fn long_format_splits_high_cardinality_cells_across_small_batches() {
    let (_tmp, path) = high_cardinality_tif();
    let want = reference_rows(&path);

    let max_distinct = want.iter().map(|r| r.distinct).max().unwrap();
    assert!(
        max_distinct > 32,
        "fixture must have cells wider than the batch sizes (max {max_distinct})"
    );

    for batch_size in [1, 7, 32, 4096] {
        let got = read_long(&path, batch_size, None);
        assert_cells_contiguous(&got);
        assert_same_rows(got, want.clone());
    }
}

#[test]
fn long_format_limit_stops_mid_cell() {
    let (_tmp, path) = high_cardinality_tif();
    let want = reference_rows(&path);

    // A limit smaller than one cell's categories, and one spanning cells.
    for (batch_size, limit) in [(32, 5), (32, 70), (1, 3)] {
        let got = read_long(&path, batch_size, Some(limit));
        assert_eq!(got.len(), limit, "batch_size {batch_size}, limit {limit}");
        assert_cells_contiguous(&got);
        for g in &got {
            let w = want
                .iter()
                .find(|w| key(w) == key(g))
                .expect("limited row not in reference output");
            assert!(close(g.count, w.count) && close(g.fraction, w.fraction));
        }
    }
}

#[test]
fn long_format_projection_without_entropy_keeps_values() {
    let (_tmp, path) = high_cardinality_tif();
    let want = reference_rows(&path);

    let reader = CategoricalBatchReader::open(
        std::slice::from_ref(&path),
        &config(),
        CategoricalFormat::Long,
        Some(vec![H3_INDEX, CATEGORY, FRACTION]),
        64,
        None,
    )
    .unwrap();

    let mut got = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        assert_eq!(batch.columns.len(), 3);
        let c = &batch.columns;
        got.extend(
            (0..batch.num_rows).map(|r| (u64_col(&c[0])[r], i64_col(&c[1])[r], f64_col(&c[2])[r])),
        );
    }
    got.sort_by_key(|r| (r.0, r.1));
    let mut expected: Vec<_> = want
        .iter()
        .map(|r| (r.cell, r.category, r.fraction))
        .collect();
    expected.sort_by_key(|r| (r.0, r.1));

    assert_eq!(got.len(), expected.len());
    for (g, w) in got.iter().zip(&expected) {
        assert_eq!((g.0, g.1), (w.0, w.1));
        assert!(close(g.2, w.2));
    }
}
