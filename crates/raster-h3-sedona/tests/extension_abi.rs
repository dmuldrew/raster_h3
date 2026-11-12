//! `RasterH3Table` exported through SedonaDB's C extension ABI (`ExportedTableProvider`, as
//! the Python bindings do) and imported back (`ImportedTableProvider`, as SedonaDB does).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use datafusion::arrow::array::AsArray;
use datafusion::arrow::datatypes::{DataType, Int64Type};
use datafusion::catalog::TableProvider;
use datafusion::common::metadata::FieldMetadata;
use datafusion::common::ScalarValue;
use datafusion::logical_expr::expr::ScalarFunction;
use datafusion::logical_expr::{
    create_udf, ColumnarValue, Expr, TableProviderFilterPushDown, Volatility,
};
use datafusion::prelude::{col, lit, SessionContext};
use raster_h3_sedona::scan::ContinuousRasterTableProvider;
use raster_h3_sedona::{RasterH3Table, GEOARROW_WKB_EXTENSION, GEOARROW_WKB_METADATA};
use sedona_extension::extension::SedonaCTableProvider;
use sedona_extension::runtime::RuntimeHandle;
use sedona_extension::table_provider::{ExportedTableProvider, ImportedTableProvider};

use common::{collect, gradient_tif, source};

/// Export `table` over the C ABI and import it back.
fn round_trip(table: RasterH3Table) -> Arc<dyn TableProvider> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let exported = ExportedTableProvider::new(
        Arc::new(table),
        Arc::new(SessionContext::new().state()),
        Arc::new(RuntimeHandle::new(runtime)),
    );
    let ffi: SedonaCTableProvider = exported.into();
    Arc::new(ImportedTableProvider::try_new(ffi).unwrap())
}

fn table(src: &str) -> RasterH3Table {
    RasterH3Table::continuous(ContinuousRasterTableProvider::try_new(src, vec![8]).unwrap())
}

/// `st_intersects(wkb, POINT (-122.42 37.77))` with a `geoarrow.wkb` literal in `OGC:CRS84`.
fn intersects_point() -> Expr {
    let udf = create_udf(
        "st_intersects",
        vec![DataType::Binary, DataType::Binary],
        DataType::Boolean,
        Volatility::Immutable,
        Arc::new(|_: &[ColumnarValue]| unreachable!()),
    );
    let mut wkb = vec![1u8];
    wkb.extend(1u32.to_le_bytes());
    wkb.extend((-122.42f64).to_le_bytes());
    wkb.extend(37.77f64.to_le_bytes());
    let metadata = FieldMetadata::new(BTreeMap::from([
        (
            "ARROW:extension:name".to_string(),
            GEOARROW_WKB_EXTENSION.to_string(),
        ),
        (
            "ARROW:extension:metadata".to_string(),
            GEOARROW_WKB_METADATA.to_string(),
        ),
    ]));
    Expr::ScalarFunction(ScalarFunction::new_udf(
        Arc::new(udf),
        vec![
            col("wkb"),
            Expr::Literal(ScalarValue::Binary(Some(wkb)), Some(metadata)),
        ],
    ))
}

#[test]
fn spatial_pushdown_survives_the_extension_abi() {
    use TableProviderFilterPushDown::{Exact, Inexact, Unsupported};

    let tif = gradient_tif(16, 16);
    let src = source(&tif);
    let spatial = intersects_point();
    let count = col("count").gt_eq(lit(2.0));
    let filters = [&spatial, &count];

    // Direct: the tagged literal is recognised.
    assert_eq!(
        table(&src).supports_filters_pushdown(&filters).unwrap(),
        vec![Inexact, Exact]
    );
    // Across the ABI the literal loses its tag, so only an opted-in table pushes it.
    assert_eq!(
        round_trip(table(&src))
            .supports_filters_pushdown(&filters)
            .unwrap(),
        vec![Unsupported, Exact]
    );
    assert_eq!(
        round_trip(table(&src).with_untagged_wkb_as_lnglat(true))
            .supports_filters_pushdown(&filters)
            .unwrap(),
        vec![Inexact, Exact]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn imported_table_scans_like_the_original() {
    let tif = gradient_tif(64, 64);
    let src = source(&tif);
    let ctx = SessionContext::new();
    ctx.register_table("direct", Arc::new(table(&src))).unwrap();
    ctx.register_table("imported", round_trip(table(&src)))
        .unwrap();

    let count = |name: &str| {
        let ctx = ctx.clone();
        let sql = format!("SELECT COUNT(*), SUM(count) FROM {name} WHERE count >= 40");
        async move {
            let b = collect(&ctx, &sql).await;
            (
                b[0].column(0).as_primitive::<Int64Type>().value(0),
                ScalarValue::try_from_array(b[0].column(1), 0).unwrap(),
            )
        }
    };
    let direct = count("direct").await;
    assert!(direct.0 > 0);
    assert_eq!(count("imported").await, direct);
}
