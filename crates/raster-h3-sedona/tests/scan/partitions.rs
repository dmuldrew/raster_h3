//! Partitioned provider scans (`with_partition`) merged with `merge_partial_batches` must
//! reproduce the unpartitioned scan; pushdown and projections must stay merge-safe.

use std::path::Path;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::{CategoricalFormat, ColumnArray, RecordBatch};
use raster_h3_sedona::scan::{
    merge_partial_batches, CategoricalRasterTableProvider, ContinuousRasterTableProvider,
    FilterPredicate, FilterPushdownStatus, RasterScan, PARTIAL_STATE_COLUMN,
};

use crate::helpers::{write_geotiff, Crs, Layout};
use crate::rows::{plan_table, table, Row};

const SF: Crs = Crs::Geographic {
    lon: -122.45,
    lat: 37.80,
    pixel_deg: 0.0005,
};

fn gradient(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("gradient.tif");
    write_geotiff(
        &path,
        (160, 120),
        Layout::Tiles(32, 32),
        SF,
        "-9999",
        |c, r| ((c * 7 + r * 3) % 50) as f32 + 1.0,
    );
    path
}

fn classes(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("classes.tif");
    write_geotiff(&path, (160, 120), Layout::Strips(8), SF, "0", |c, r| {
        ((c / 5 + r / 7) % 4 + 1) as u8
    });
    path
}

/// Rows of `batch` whose `partial_state` is null (`partial == false`) or set.
fn split(batch: &RecordBatch, partial: bool) -> RecordBatch {
    let Some(ColumnArray::Binary { validity, .. }) = batch.column_by_name(PARTIAL_STATE_COLUMN)
    else {
        panic!("no partial_state column");
    };
    let keep: Vec<usize> = (0..batch.num_rows)
        .filter(|&i| validity.as_ref().is_none_or(|v| v[i]) == partial)
        .collect();
    let columns = batch.columns.iter().map(|c| take(c, &keep)).collect();
    RecordBatch::try_new(batch.schema.clone(), columns).unwrap()
}

fn take(col: &ColumnArray, idx: &[usize]) -> ColumnArray {
    macro_rules! pick {
        ($variant:ident, $values:expr, $validity:expr) => {
            ColumnArray::$variant {
                values: idx.iter().map(|&i| $values[i].clone()).collect(),
                validity: $validity
                    .as_ref()
                    .map(|v| idx.iter().map(|&i| v[i]).collect()),
            }
        };
    }
    match col {
        ColumnArray::UInt64 { values, validity } => pick!(UInt64, values, validity),
        ColumnArray::Int64 { values, validity } => pick!(Int64, values, validity),
        ColumnArray::UInt8 { values, validity } => pick!(UInt8, values, validity),
        ColumnArray::Float64 { values, validity } => pick!(Float64, values, validity),
        ColumnArray::Boolean { values, validity } => pick!(Boolean, values, validity),
        ColumnArray::Utf8 { values, validity } => pick!(Utf8, values, validity),
        ColumnArray::Binary { values, validity } => pick!(Binary, values, validity),
    }
}

/// Drop the trailing `partial_state` column.
fn without_state(batch: RecordBatch) -> RecordBatch {
    let n = batch.schema.fields.len() - 1;
    batch.project(&(0..n).collect::<Vec<_>>()).unwrap()
}

/// Scan every partition of `provider`, merge the partials, return all final rows.
fn partitioned_rows<P>(
    provider: &P,
    plan: Vec<raster_h3_sedona::scan::ScanPartition>,
    with_partition: impl Fn(P, raster_h3_sedona::scan::ScanPartition) -> P,
    categorical: bool,
    config: &MultiResolutionConfig,
) -> (Vec<String>, Vec<Row>, usize)
where
    P: RasterScan + Clone,
{
    let mut finals = Vec::new();
    let mut partials = Vec::new();
    let n = plan.len();
    for partition in plan {
        let scanned = with_partition(provider.clone(), partition);
        for batch in scanned.scan(None, &[], None).unwrap().execute().unwrap() {
            let batch = batch.unwrap();
            finals.push(without_state(split(&batch, false)));
            partials.push(split(&batch, true));
        }
    }
    let (_, merged) = merge_partial_batches(&partials, categorical, config, None, 512).unwrap();
    finals.extend(merged);
    let (names, rows) = table(&finals);
    (names, rows, n)
}

#[test]
fn continuous_partitions_match_the_unpartitioned_provider() {
    let dir = tempfile::tempdir().unwrap();
    let provider =
        ContinuousRasterTableProvider::try_new(gradient(dir.path()).to_str().unwrap(), vec![8, 9])
            .unwrap();
    let single = plan_table(provider.scan(None, &[], None).unwrap());
    for target in [1, 4, 100] {
        let plan = provider.plan_partitions(target).unwrap();
        let (names, rows, n) = partitioned_rows(
            &provider,
            plan,
            |p, part| p.with_partition(part),
            false,
            &provider.config,
        );
        assert_eq!((names, rows), single, "target {target} ({n} partitions)");
    }
}

#[test]
fn categorical_partitions_match_the_unpartitioned_provider() {
    let dir = tempfile::tempdir().unwrap();
    let provider =
        CategoricalRasterTableProvider::try_new(classes(dir.path()).to_str().unwrap(), vec![9])
            .unwrap();
    let single = plan_table(provider.scan(None, &[], None).unwrap());
    let plan = provider.plan_partitions(3).unwrap();
    assert!(plan.len() > 1);
    let (names, rows, _) = partitioned_rows(
        &provider,
        plan,
        |p, part| p.with_partition(part),
        true,
        &provider.config,
    );
    assert_eq!((names, rows), single);

    let long = provider
        .clone()
        .with_format(CategoricalFormat::Long)
        .with_partition(provider.plan_partitions(3).unwrap().remove(0));
    assert!(long.scan(None, &[], None).is_err());
}

#[test]
fn partitioned_pushdown_keeps_partial_rows_mergeable() {
    let dir = tempfile::tempdir().unwrap();
    let base =
        ContinuousRasterTableProvider::try_new(gradient(dir.path()).to_str().unwrap(), vec![8, 9])
            .unwrap();
    let spec = base.plan_partitions(4).unwrap().remove(0);
    let provider = base.clone().with_partition(spec);

    // Value thresholds and bboxes are not pushed; resolution is.
    let filters = [
        FilterPredicate::MinCount(5.0),
        FilterPredicate::MinMean(10.0),
        FilterPredicate::Bbox([-122.45, 37.75, -122.40, 37.80]),
        FilterPredicate::Resolution(9),
    ];
    assert_eq!(
        provider.supports_filters_pushdown(&filters),
        [
            FilterPushdownStatus::Unsupported,
            FilterPushdownStatus::Unsupported,
            FilterPushdownStatus::Unsupported,
            FilterPushdownStatus::Exact,
        ]
    );
    let names: Vec<_> = provider
        .schema()
        .fields
        .iter()
        .map(|f| f.name.clone())
        .collect();
    assert_eq!(names.last().map(String::as_str), Some(PARTIAL_STATE_COLUMN));

    // A projection without the merge keys still works: the reader reads them anyway.
    let mean = names.iter().position(|n| n == "mean").unwrap();
    let state = names.len() - 1;
    let batches: Vec<RecordBatch> = provider
        .scan(Some(&[mean, state]), &filters[3..], Some(3))
        .unwrap()
        .execute()
        .unwrap()
        .map(|b| b.unwrap())
        .collect();
    let out: Vec<_> = batches[0]
        .schema
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(out, ["mean", PARTIAL_STATE_COLUMN]);
    // The limit is not applied inside a partitioned scan: partial rows must all arrive.
    let rows: usize = batches.iter().map(|b| b.num_rows).sum();
    assert!(rows > 3, "{rows} rows");

    // Resolution pushdown only narrows the resolutions, which the partition allows.
    let res9: usize = provider
        .scan(None, &filters[3..], None)
        .unwrap()
        .execute()
        .unwrap()
        .map(|b| {
            let b = b.unwrap();
            match b.column_by_name("resolution") {
                Some(ColumnArray::UInt8 { values, .. }) => {
                    assert!(values.iter().all(|&r| r == 9));
                    b.num_rows
                }
                _ => panic!("resolution column"),
            }
        })
        .sum();
    assert!(res9 > 0);
}
