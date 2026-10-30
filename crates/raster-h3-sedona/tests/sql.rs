//! End-to-end SQL tests against a plain DataFusion `SessionContext`.

mod common;

use std::sync::Arc;

use datafusion::arrow::array::{Array, AsArray};
use datafusion::arrow::datatypes::{Float64Type, Int64Type, UInt64Type};
use datafusion::prelude::SessionContext;
use raster_h3_datafusion::ContinuousRasterTableProvider;
use raster_h3_sedona::{register_all, RasterH3Table, GEOARROW_WKB_EXTENSION};

use common::{collect, gradient_tif, rows, source, write_tif};

fn context() -> SessionContext {
    let ctx = SessionContext::new();
    register_all(&ctx);
    ctx
}

#[tokio::test(flavor = "multi_thread")]
async fn table_function_counts_and_projects() {
    let tif = gradient_tif(64, 64);
    let ctx = context();
    let src = source(&tif);

    let all = collect(
        &ctx,
        &format!("SELECT h3_index, mean, wkb FROM h3_raster_continuous_aggregate('{src}', 8)"),
    )
    .await;
    let n = rows(&all);
    assert!(n > 1, "expected several cells, got {n}");
    assert_eq!(all[0].num_columns(), 3);

    // COUNT(*) scans with an empty projection.
    let counted = collect(
        &ctx,
        &format!("SELECT COUNT(*) AS n FROM h3_raster_continuous_aggregate('{src}', 8)"),
    )
    .await;
    let counted = counted[0].column(0).as_primitive::<Int64Type>().value(0);
    assert_eq!(counted as usize, n);

    let limited = collect(
        &ctx,
        &format!("SELECT h3_index FROM h3_raster_continuous_aggregate('{src}', 8) LIMIT 2"),
    )
    .await;
    assert_eq!(rows(&limited), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn pushed_down_filters_match_post_filtering() {
    let tif = gradient_tif(64, 64);
    let ctx = context();
    let src = source(&tif);
    let table = format!("h3_raster_continuous_aggregate('{src}', 7, 8)");

    // Unfiltered rows, filtered by hand.
    let all = collect(
        &ctx,
        &format!("SELECT mean, count, resolution FROM {table}"),
    )
    .await;
    let expected = all
        .iter()
        .flat_map(|b| {
            let mean = b.column(0).as_primitive::<Float64Type>().clone();
            let count = b.column(1).as_primitive::<Float64Type>().clone();
            let res = b
                .column(2)
                .as_primitive::<datafusion::arrow::datatypes::UInt8Type>()
                .clone();
            (0..b.num_rows())
                .filter(move |&i| {
                    count.value(i) > 3.0
                        && (10.0..=40.0).contains(&mean.value(i))
                        && res.value(i) == 8
                })
                .collect::<Vec<_>>()
        })
        .count();
    assert!(expected > 0 && expected < rows(&all));

    // `count > 3` is Inexact (re-applied by DataFusion); the others are Exact.
    let sql = format!(
        "SELECT mean FROM {table} WHERE count > 3 AND mean BETWEEN 10 AND 40 AND resolution = 8"
    );
    assert_eq!(rows(&collect(&ctx, &sql).await), expected);

    let plan = collect(&ctx, &format!("EXPLAIN {sql}")).await;
    let plan = datafusion::arrow::util::pretty::pretty_format_batches(&plan)
        .unwrap()
        .to_string();
    let scan = plan.lines().find(|l| l.contains("TableScan")).unwrap();
    assert!(scan.contains("resolution = UInt8(8)]"), "{scan}");
    assert!(scan.contains("mean <= Float64(40)"), "{scan}");
    assert!(
        scan.contains("partial_filters=[h3_raster_continuous_aggregate().count > Float64(3)]"),
        "{scan}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn geometry_columns_carry_geoarrow_metadata() {
    let tif = gradient_tif(16, 16);
    let ctx = context();
    let src = source(&tif);

    let df = ctx
        .sql(&format!(
            "SELECT wkb, h3_to_wkb(h3_index) AS cell FROM h3_raster_continuous_aggregate('{src}', 8)"
        ))
        .await
        .unwrap();
    let schema = df.schema().as_arrow().clone();
    for name in ["wkb", "cell"] {
        let field = schema.field_with_name(name).unwrap();
        assert_eq!(
            field
                .metadata()
                .get("ARROW:extension:name")
                .map(String::as_str),
            Some(GEOARROW_WKB_EXTENSION),
            "{name}: {field:?}"
        );
    }
    let batches = df.collect().await.unwrap();
    assert_eq!(batches[0].schema().field(0).metadata().len(), 2);
    assert_eq!(batches[0].column(0), batches[0].column(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn scalar_udfs_round_trip() {
    let tif = gradient_tif(16, 16);
    let ctx = context();
    let src = source(&tif);

    let batches = collect(
        &ctx,
        &format!(
            "SELECT h3_index, h3_hex, h3_to_string(h3_index), string_to_h3(h3_hex), \
                    h3_get_resolution(h3_index), h3_cell_to_parent(h3_index, 6), h3_is_valid(h3_hex) \
             FROM h3_raster_continuous_aggregate('{src}', 8)"
        ),
    )
    .await;
    let b = &batches[0];
    assert_eq!(b.column(0).as_ref(), b.column(3).as_ref());
    assert_eq!(b.column(1).as_ref(), b.column(2).as_ref());
    assert!(b
        .column(4)
        .as_primitive::<Int64Type>()
        .values()
        .iter()
        .all(|&r| r == 8));
    let parents = b.column(5).as_primitive::<UInt64Type>();
    assert_eq!(parents.null_count(), 0);
    assert!(b.column(6).as_boolean().iter().all(|v| v == Some(true)));

    // Constant folding: an integer literal coerces to a UInt64 cell.
    let lit = collect(&ctx, "SELECT h3_to_string(599686042433355775) AS s").await;
    assert_eq!(
        lit[0].column(0).as_string::<i32>().value(0),
        "85283473fffffff"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_provider_registers_as_table() {
    let tif = gradient_tif(32, 32);
    let ctx = context();
    let provider = ContinuousRasterTableProvider::try_new(&source(&tif), vec![8])
        .unwrap()
        .with_crs("EPSG:4326")
        .with_min_mean(20.0);
    ctx.register_table("dem", Arc::new(RasterH3Table::continuous(provider)))
        .unwrap();

    let batches = collect(&ctx, "SELECT MIN(mean) FROM dem").await;
    assert!(batches[0].column(0).as_primitive::<Float64Type>().value(0) >= 20.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn crs_argument_overrides_missing_geokeys() {
    let tif = write_tif(16, 16, false);
    let ctx = context();
    let src = source(&tif);
    let missing = ctx
        .sql(&format!(
            "SELECT * FROM h3_raster_continuous_aggregate('{src}', 8)"
        ))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(missing.is_err());
    let given = collect(
        &ctx,
        &format!("SELECT h3_index FROM h3_raster_categorical_aggregate('{src}', 8, 'EPSG:4326')"),
    )
    .await;
    assert!(rows(&given) > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_arguments_fail_at_planning() {
    let ctx = context();
    for sql in [
        "SELECT * FROM h3_raster_continuous_aggregate()",
        "SELECT * FROM h3_raster_continuous_aggregate('x.tif', 16)",
        "SELECT * FROM h3_raster_continuous_aggregate('x.tif', 8, 'EPSG:4326', 'EPSG:3857')",
        "SELECT h3_cell_to_parent('85283473fffffff')",
        "SELECT h3_to_lat(1.5)",
    ] {
        assert!(ctx.sql(sql).await.is_err(), "{sql} should not plan");
    }
}
