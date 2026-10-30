//! The adapter inside a real SedonaDB `SedonaContext`, composed with Sedona's `ST_*` functions.
#![cfg(feature = "sedona-e2e")]

mod common;

use datafusion::arrow::array::{AsArray, RecordBatch};
use datafusion::arrow::datatypes::Int64Type;
use datafusion::common::ScalarValue;
use sedona::context::SedonaContext;

use common::{gradient_tif, rows, source};
use raster_h3_datafusion::ContinuousRasterTableProvider;
use raster_h3_sedona::RasterH3Table;
use std::sync::Arc;

async fn sedona() -> SedonaContext {
    let ctx = SedonaContext::new_local_interactive().await.unwrap();
    raster_h3_sedona::register_all(&ctx.ctx);
    ctx
}

async fn query(ctx: &SedonaContext, sql: &str) -> Vec<RecordBatch> {
    ctx.sql(sql).await.unwrap().collect().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sedona_reads_cells_as_lnglat_geometry() {
    let tif = gradient_tif(64, 64);
    let ctx = sedona().await;
    let src = source(&tif);

    let batches = query(
        &ctx,
        &format!(
            "SELECT ST_GeometryType(wkb), ST_SRID(wkb), ST_GeometryType(h3_to_wkb(h3_index)) \
             FROM h3_raster_continuous_aggregate('{src}', 8) LIMIT 1"
        ),
    )
    .await;
    let b = &batches[0];
    let value = |i: usize| {
        ScalarValue::try_from_array(b.column(i), 0)
            .unwrap()
            .to_string()
    };
    assert_eq!(value(0), "ST_Polygon");
    assert_eq!(value(1), "4326");
    assert_eq!(value(2), "ST_Polygon");
}

#[tokio::test(flavor = "multi_thread")]
async fn sedona_spatial_predicates_on_raster_cells() {
    let tif = gradient_tif(64, 64);
    let ctx = sedona().await;
    let src = source(&tif);

    // The raster spans lon -122.45..-122.386, lat 37.736..37.80.
    let inside = query(
        &ctx,
        &format!(
            "SELECT COUNT(*) FROM h3_raster_continuous_aggregate('{src}', 8) \
             WHERE ST_Intersects(wkb, ST_SetSRID(ST_GeomFromWKT('POINT (-122.42 37.77)'), 4326))"
        ),
    )
    .await;
    assert_eq!(inside[0].column(0).as_primitive::<Int64Type>().value(0), 1);

    // A spatial join between two raster aggregations: every res-8 cell intersects its res-7 parent.
    let joined = query(
        &ctx,
        &format!(
            "SELECT c.h3_index FROM h3_raster_continuous_aggregate('{src}', 8) c \
             JOIN h3_raster_continuous_aggregate('{src}', 7) p \
               ON h3_cell_to_parent(c.h3_index, 7) = p.h3_index \
             WHERE ST_Intersects(c.wkb, p.wkb)"
        ),
    )
    .await;
    assert!(rows(&joined) > 0);
}

/// Cells of `table` intersecting a small polygon, as `h3_index,mean,count` lines.
async fn cells_in_polygon(ctx: &SedonaContext, table: &str) -> Vec<String> {
    let batches = query(
        ctx,
        &format!(
            "SELECT h3_index, mean, count FROM {table} \
             WHERE ST_Intersects(wkb, ST_SetSRID(ST_GeomFromWKT( \
               'POLYGON ((-122.40 37.72, -122.38 37.72, -122.38 37.74, -122.40 37.74, -122.40 37.72))'), 4326)) \
             ORDER BY h3_index"
        ),
    )
    .await;
    let mut out = Vec::new();
    for b in &batches {
        for i in 0..b.num_rows() {
            let v = |c: usize| {
                ScalarValue::try_from_array(b.column(c), i)
                    .unwrap()
                    .to_string()
            };
            out.push(format!("{},{},{}", v(0), v(1), v(2)));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn bbox_pushdown_keeps_edge_cells_whole() {
    // 0.2° square (lon -122.45..-122.25, lat 37.60..37.80); the query polygon is ~0.02°.
    let tif = gradient_tif(200, 200);
    let ctx = sedona().await;
    let src = source(&tif);
    let scan = format!("h3_raster_continuous_aggregate('{src}', 8)");

    // A materialised copy cannot push anything down: the reference answer.
    query(
        &ctx,
        &format!("CREATE TABLE full_scan AS SELECT * FROM {scan}"),
    )
    .await;
    let expected = cells_in_polygon(&ctx, "full_scan").await;
    assert!(expected.len() > 1);

    assert_eq!(cells_in_polygon(&ctx, &scan).await, expected);

    // The scan emitted far fewer cells than the raster has.
    let plan = query(
        &ctx,
        &format!(
            "EXPLAIN ANALYZE SELECT h3_index FROM {scan} WHERE ST_Intersects(wkb, \
             ST_SetSRID(ST_GeomFromWKT('POINT (-122.39 37.73)'), 4326))"
        ),
    )
    .await;
    let plan = datafusion::arrow::util::pretty::pretty_format_batches(&plan)
        .unwrap()
        .to_string();
    // `StreamingTableExec` records no metrics; the scan's rows are what the node directly
    // above the `FilterExec` (its input) outputs.
    let scanned: usize = plan
        .lines()
        .skip_while(|l| !l.contains("FilterExec"))
        .nth(1)
        .and_then(|l| l.split("output_rows=").nth(1))
        .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no scan output_rows in plan:\n{plan}"));
    let total = rows(&query(&ctx, "SELECT h3_index FROM full_scan").await);
    assert!(
        scanned * 5 < total,
        "scanned {scanned} of {total} cells:\n{plan}"
    );

    // Control: clipping to the polygon's own envelope truncates the edge cells' aggregates,
    // which is the bug the widened box avoids.
    let naive = ContinuousRasterTableProvider::try_new(&src, vec![8])
        .unwrap()
        .with_bbox([-122.40, 37.72, -122.38, 37.74]);
    ctx.ctx
        .register_table("naive", Arc::new(RasterH3Table::continuous(naive)))
        .unwrap();
    assert_ne!(cells_in_polygon(&ctx, "naive").await, expected);
}
