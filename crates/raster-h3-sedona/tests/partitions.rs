//! Partitioned scans in SQL: `h3_raster_plan_partitions`, a partition spec argument to the
//! aggregate table functions, and the `h3_merge_partials` aggregates, compared with an
//! unpartitioned query.

#[path = "../../raster-h3-core/tests/it/helpers.rs"]
mod helpers;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use datafusion::arrow::array::{Array, AsArray, RecordBatch};
use datafusion::arrow::compute::concat_batches;
use datafusion::arrow::datatypes::{Float64Type, Int64Type, UInt64Type, UInt8Type};
use datafusion::datasource::MemTable;
use datafusion::prelude::SessionContext;
use raster_h3_sedona::register_all;

use helpers::{write_geotiff, Crs, Layout};

const SF: Crs = Crs::Geographic {
    lon: -122.45,
    lat: 37.80,
    pixel_deg: 0.0005,
};

fn context() -> SessionContext {
    let ctx = SessionContext::new();
    register_all(&ctx);
    ctx
}

async fn collect(ctx: &SessionContext, sql: &str) -> Vec<RecordBatch> {
    ctx.sql(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .collect()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn plan(ctx: &SessionContext, src: &str, target: usize) -> Vec<String> {
    let batches = collect(
        ctx,
        &format!("SELECT spec FROM h3_raster_plan_partitions('{src}', {target}) ORDER BY partition_index"),
    )
    .await;
    batches
        .iter()
        .flat_map(|b| {
            let col = b.column(0).as_string::<i32>();
            (0..b.num_rows())
                .map(|i| col.value(i).to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `(resolution, h3_index)` -> the selected value columns, rendered as text.
type Rows = BTreeMap<(u8, u64), Vec<String>>;

fn text(array: &dyn Array, i: usize) -> String {
    if array.is_null(i) {
        return "NULL".into();
    }
    if let Some(a) = array.as_primitive_opt::<Float64Type>() {
        return format!("{:.10e}", a.value(i));
    }
    if let Some(a) = array.as_primitive_opt::<Int64Type>() {
        return a.value(i).to_string();
    }
    if let Some(a) = array.as_string_opt::<i32>() {
        // Histogram JSON: compare as a parsed object.
        let v: serde_json::Value = serde_json::from_str(a.value(i)).unwrap_or_default();
        return v.to_string();
    }
    panic!("unexpected column type {}", array.data_type());
}

/// Rows of batches whose first two columns are `resolution` and `h3_index`.
fn add_rows(rows: &mut Rows, batches: &[RecordBatch]) {
    for b in batches {
        let res = b.column(0).as_primitive::<UInt8Type>();
        let h3 = b.column(1).as_primitive::<UInt64Type>();
        for i in 0..b.num_rows() {
            let values = (2..b.num_columns()).map(|c| text(b.column(c), i)).collect();
            assert!(
                rows.insert((res.value(i), h3.value(i)), values).is_none(),
                "cell emitted twice"
            );
        }
    }
}

/// Scan every partition in SQL, merge the partial rows in SQL, and return all final rows.
async fn partitioned(
    ctx: &SessionContext,
    function: &str,
    merge: &str,
    src: &str,
    resolutions: &str,
    columns: &[&str],
    target: usize,
) -> (Rows, usize) {
    let specs = plan(ctx, src, target).await;
    let mut rows = Rows::new();
    let mut partials = Vec::new();
    let cols = columns.join(", ");
    for spec in &specs {
        let table = format!("{function}('{src}', {resolutions}, '{spec}')");
        let finals = collect(
            ctx,
            &format!(
                "SELECT resolution, h3_index, {cols} FROM {table} WHERE partial_state IS NULL"
            ),
        )
        .await;
        add_rows(&mut rows, &finals);
        partials.extend(
            collect(
                ctx,
                &format!("SELECT resolution, h3_index, partial_state FROM {table} WHERE partial_state IS NOT NULL"),
            )
            .await,
        );
    }
    let schema = partials[0].schema();
    let all = concat_batches(&schema, &partials).unwrap();
    let mem = MemTable::try_new(schema, vec![vec![all]]).unwrap();
    ctx.deregister_table("partials").unwrap();
    ctx.register_table("partials", Arc::new(mem)).unwrap();
    let fields = columns
        .iter()
        .map(|c| format!("cell.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let merged = collect(
        ctx,
        &format!(
            "SELECT resolution, h3_index, {fields} FROM (
                SELECT resolution, h3_index, {merge}(partial_state) AS cell
                FROM partials GROUP BY resolution, h3_index)"
        ),
    )
    .await;
    add_rows(&mut rows, &merged);
    (rows, specs.len())
}

fn tiled(dir: &Path) -> String {
    let path = dir.join("gradient.tif");
    write_geotiff(
        &path,
        (160, 120),
        Layout::Tiles(32, 32),
        SF,
        "-9999",
        |c, r| ((c * 7 + r * 3) % 50) as f32 + 1.0,
    );
    path.to_string_lossy().into_owned()
}

fn striped_classes(dir: &Path) -> String {
    let path = dir.join("classes.tif");
    write_geotiff(&path, (160, 120), Layout::Strips(8), SF, "0", |c, r| {
        ((c / 5 + r / 7) % 4 + 1) as u8
    });
    path.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn continuous_partitions_merge_in_sql_to_the_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let src = tiled(dir.path());
    let ctx = context();
    let columns = ["mean", "stddev", "count", "min", "max", "sum"];
    let mut single = Rows::new();
    add_rows(
        &mut single,
        &collect(
            &ctx,
            &format!(
                "SELECT resolution, h3_index, {} FROM h3_raster_continuous_aggregate('{src}', 8, 9)",
                columns.join(", ")
            ),
        )
        .await,
    );
    for target in [2, 6] {
        let (rows, n) = partitioned(
            &ctx,
            "h3_raster_continuous_aggregate",
            "h3_merge_partials",
            &src,
            "8, 9",
            &columns,
            target,
        )
        .await;
        assert!(n > 1);
        assert_eq!(rows, single, "target {target}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn categorical_partitions_merge_in_sql_to_the_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let src = striped_classes(dir.path());
    let ctx = context();
    let columns = [
        "majority_class",
        "majority_fraction",
        "total_count",
        "histogram",
        "shannon_entropy",
    ];
    let mut single = Rows::new();
    add_rows(
        &mut single,
        &collect(
            &ctx,
            &format!(
                "SELECT resolution, h3_index, {} FROM h3_raster_categorical_aggregate('{src}', 9)",
                columns.join(", ")
            ),
        )
        .await,
    );
    let (rows, n) = partitioned(
        &ctx,
        "h3_raster_categorical_aggregate",
        "h3_merge_categorical_partials",
        &src,
        "9",
        &columns,
        3,
    )
    .await;
    assert!(n > 1);
    assert_eq!(rows, single);
}

#[tokio::test(flavor = "multi_thread")]
async fn partitioned_queries_stay_correct_with_filters_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let src = tiled(dir.path());
    let ctx = context();
    let spec = plan(&ctx, &src, 4).await.remove(0);
    let table = format!("h3_raster_continuous_aggregate('{src}', 8, 9, '{spec}')");

    // A value filter is applied by DataFusion, not pushed into the scan, so it sees every row
    // (partial rows included) exactly as written.
    let all = collect(&ctx, &format!("SELECT count FROM {table}")).await;
    let expected: usize = all
        .iter()
        .map(|b| {
            b.column(0)
                .as_primitive::<Float64Type>()
                .iter()
                .filter(|v| v.unwrap() >= 20.0)
                .count()
        })
        .sum();
    let filtered = collect(
        &ctx,
        &format!("SELECT count FROM {table} WHERE count >= 20"),
    )
    .await;
    assert_eq!(
        filtered.iter().map(|b| b.num_rows()).sum::<usize>(),
        expected
    );

    // Resolution filters and projections without the merge keys work.
    let res9 = collect(
        &ctx,
        &format!("SELECT mean, resolution FROM {table} WHERE resolution = 9"),
    )
    .await;
    assert!(res9.iter().all(|b| b
        .column(1)
        .as_primitive::<UInt8Type>()
        .iter()
        .all(|r| r == Some(9))));
    let limited = collect(&ctx, &format!("SELECT h3_index FROM {table} LIMIT 3")).await;
    assert_eq!(limited.iter().map(|b| b.num_rows()).sum::<usize>(), 3);

    // A spec for another layout is refused; a bad spec is a planning error.
    let other = dir.path().join("other.tif");
    write_geotiff(
        &other,
        (100, 100),
        Layout::Tiles(32, 32),
        SF,
        "-9999",
        |_, _| 1.0f32,
    );
    let other = other.to_string_lossy();
    let err = ctx
        .sql(&format!(
            "SELECT * FROM h3_raster_continuous_aggregate('{other}', 8, '{spec}')"
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("different mosaic"), "{err}");
    assert!(ctx
        .sql(&format!(
            "SELECT * FROM h3_raster_continuous_aggregate('{src}', 8, '{{oops')"
        ))
        .await
        .is_err());

    // Merging a continuous state as categorical fails rather than misreading it.
    let partial = collect(
        &ctx,
        &format!("SELECT partial_state FROM {table} WHERE partial_state IS NOT NULL LIMIT 1"),
    )
    .await;
    let mem = MemTable::try_new(partial[0].schema(), vec![partial]).unwrap();
    ctx.register_table("one_partial", Arc::new(mem)).unwrap();
    assert!(ctx
        .sql("SELECT h3_merge_categorical_partials(partial_state) FROM one_partial")
        .await
        .unwrap()
        .collect()
        .await
        .is_err());
    // Null states are skipped; a group of only nulls merges to NULL.
    let nulls = collect(
        &ctx,
        "SELECT h3_merge_partials(CAST(NULL AS BYTEA)) AS cell",
    )
    .await;
    assert!(nulls[0].column(0).is_null(0));
}
