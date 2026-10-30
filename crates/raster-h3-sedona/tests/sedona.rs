//! The adapter inside a real SedonaDB `SedonaContext`, composed with Sedona's `ST_*` functions.
#![cfg(feature = "sedona-e2e")]

mod common;

use datafusion::arrow::array::{AsArray, RecordBatch};
use datafusion::arrow::datatypes::Int64Type;
use datafusion::common::ScalarValue;
use sedona::context::SedonaContext;

use common::{gradient_tif, rows, source};

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
