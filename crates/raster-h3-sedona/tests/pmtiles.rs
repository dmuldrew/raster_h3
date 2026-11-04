//! `COPY (query) TO '*.pmtiles'` and `write_pmtiles` against a plain DataFusion
//! `SessionContext`.

mod common;

use std::io::Read;
use std::path::Path;

use datafusion::arrow::array::{Array, AsArray};
use datafusion::arrow::datatypes::UInt64Type;
use datafusion::prelude::SessionContext;
use raster_h3_core::pmtiles::pyramid::h3_res_to_zoom;
use raster_h3_sedona::{register_all, write_pmtiles, write_record_batches, PmtilesWriteOptions};

use common::{collect, gradient_tif, rows, source};

fn context() -> SessionContext {
    let ctx = SessionContext::new();
    register_all(&ctx);
    ctx
}

/// The single `count` value `COPY` returns.
async fn copy(ctx: &SessionContext, sql: &str) -> u64 {
    let out = collect(ctx, sql).await;
    assert_eq!(rows(&out), 1, "{sql}");
    out[0].column(0).as_primitive::<UInt64Type>().value(0)
}

async fn copy_err(ctx: &SessionContext, sql: &str) -> String {
    match ctx.sql(sql).await {
        Err(e) => e.to_string(),
        Ok(df) => df
            .collect()
            .await
            .expect_err(&format!("{sql} should fail"))
            .to_string(),
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// The archive's gzipped JSON metadata.
fn metadata(archive: &[u8]) -> serde_json::Value {
    assert_eq!(&archive[..7], b"PMTiles");
    assert_eq!(archive[7], 3, "PMTiles spec version");
    let offset = u64::from_le_bytes(archive[24..32].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(archive[32..40].try_into().unwrap()) as usize;
    let mut json = String::new();
    flate2::read::GzDecoder::new(&archive[offset..offset + len])
        .read_to_string(&mut json)
        .unwrap();
    serde_json::from_str(&json).unwrap()
}

/// Addressed tile count from the header.
fn tile_count(archive: &[u8]) -> u64 {
    u64::from_le_bytes(archive[72..80].try_into().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_writes_every_cell_with_its_columns() {
    let tif = gradient_tif(64, 64);
    let src = source(&tif);
    let ctx = context();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dem.pmtiles");
    let query = format!(
        "SELECT h3_index, mean, count, h3_to_string(h3_index) AS label, wkb \
         FROM h3_raster_continuous_aggregate('{src}', 8)"
    );

    let cells = rows(&collect(&ctx, &query).await) as u64;
    assert!(cells > 1);
    let written = copy(&ctx, &format!("COPY ({query}) TO '{}'", s(&out))).await;
    assert_eq!(written, cells);

    let archive = std::fs::read(&out).unwrap();
    assert!(tile_count(&archive) > 0);
    let meta = metadata(&archive);
    let fields = &meta["vector_layers"][0]["fields"];
    assert_eq!(fields["mean"], "Number");
    assert_eq!(fields["count"], "Number");
    assert_eq!(fields["label"], "String");
    assert!(
        fields.get("wkb").is_none(),
        "binary geometry is not a property: {fields}"
    );

    // The sink (blocking worker + object-store upload) builds the same bytes as writing
    // the collected batches straight to a local file.
    let batches = collect(&ctx, &query).await;
    let direct = dir.path().join("direct.pmtiles");
    let summary = write_record_batches(
        &batches[0].schema(),
        batches.into_iter().map(Ok),
        &direct,
        PmtilesWriteOptions::default(),
    )
    .unwrap();
    assert_eq!(summary.valid_features as u64, cells);
    assert_eq!(std::fs::read(&direct).unwrap(), archive);
}

#[tokio::test(flavor = "multi_thread")]
async fn stored_as_options_and_zoom_range() {
    let tif = gradient_tif(64, 64);
    let src = source(&tif);
    let ctx = context();
    let dir = tempfile::tempdir().unwrap();
    let query = format!(
        "SELECT h3_index AS cell_id, mean, resolution \
         FROM h3_raster_continuous_aggregate('{src}', 7, 8)"
    );
    let res8 = rows(
        &collect(
            &ctx,
            &format!("SELECT * FROM ({query}) WHERE resolution = 8"),
        )
        .await,
    ) as u64;
    let all = rows(&collect(&ctx, &query).await) as u64;
    assert!(0 < res8 && res8 < all);

    // Another extension with STORED AS; the H3 column named explicitly; only zoom of res 8.
    let out = dir.path().join("cells.bin");
    let z8 = h3_res_to_zoom(8);
    let written = copy(
        &ctx,
        &format!(
            "COPY ({query}) TO '{}' STORED AS PMTILES \
             OPTIONS (h3_column 'cell_id', min_zoom {z8}, memory_budget_mb 1)",
            s(&out)
        ),
    )
    .await;
    assert_eq!(written, res8);
    let meta = metadata(&std::fs::read(&out).unwrap());
    assert_eq!(meta["minzoom"].as_u64(), Some(u64::from(z8)));
    assert!(meta["vector_layers"][0]["fields"].get("cell_id").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn null_cells_are_skipped_and_signed_and_hex_columns_work() {
    let ctx = context();
    let dir = tempfile::tempdir().unwrap();
    let values = "VALUES (CAST(613196570331971583 AS BIGINT), 'a'), (NULL, 'b'), \
                  (CAST(613196570340360191 AS BIGINT), 'c')";
    let out = dir.path().join("signed.pmtiles");
    let written = copy(
        &ctx,
        &format!(
            "COPY (SELECT column1 AS h3_index, column2 AS name FROM ({values})) TO '{}'",
            s(&out)
        ),
    )
    .await;
    assert_eq!(written, 2);

    let hex = dir.path().join("hex.pmtiles");
    let written = copy(
        &ctx,
        &format!(
            "COPY (SELECT h3_to_string(CAST(column1 AS BIGINT UNSIGNED)) AS h3_hex, column2 AS name \
             FROM ({values})) TO '{}'",
            s(&hex)
        ),
    )
    .await;
    assert_eq!(written, 2);
    assert_eq!(std::fs::read(&hex).unwrap().len() > 127, true);
}

#[tokio::test(flavor = "multi_thread")]
async fn write_pmtiles_dataframe_api() {
    let tif = gradient_tif(32, 32);
    let src = source(&tif);
    let ctx = context();
    let dir = tempfile::tempdir().unwrap();
    let df = ctx
        .sql(&format!(
            "SELECT h3_index, mean FROM h3_raster_continuous_aggregate('{src}', 8) WHERE count >= 2"
        ))
        .await
        .unwrap();
    let expected = df.clone().count().await.unwrap() as u64;
    let out = dir.path().join("df.pmtiles");
    let written = write_pmtiles(df, s(&out), PmtilesWriteOptions::default())
        .await
        .unwrap();
    assert_eq!(written, expected);
    assert_eq!(
        metadata(&std::fs::read(&out).unwrap())["vector_layers"][0]["fields"]["mean"],
        "Number"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_query_writes_a_valid_archive() {
    let tif = gradient_tif(32, 32);
    let src = source(&tif);
    let ctx = context();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("empty.pmtiles");
    let written = copy(
        &ctx,
        &format!(
            "COPY (SELECT h3_index FROM h3_raster_continuous_aggregate('{src}', 8) WHERE count > 1e9) \
             TO '{}'",
            s(&out)
        ),
    )
    .await;
    assert_eq!(written, 0);
    assert_eq!(tile_count(&std::fs::read(&out).unwrap()), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_copies_fail_with_a_message() {
    let tif = gradient_tif(32, 32);
    let src = source(&tif);
    let ctx = context();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("x.pmtiles");
    let out = s(&out);
    let cells = format!(
        "SELECT h3_index, mean, resolution FROM h3_raster_continuous_aggregate('{src}', 8)"
    );

    for (sql, needle) in [
        (
            format!("COPY ({cells}) TO '{out}' OPTIONS (h3_column 'nope')"),
            "h3_column 'nope' not found",
        ),
        (
            format!("COPY ({cells}) TO '{out}' OPTIONS (zoom 3)"),
            "Unknown PMTiles option 'zoom'",
        ),
        (
            format!("COPY ({cells}) TO '{out}' OPTIONS (min_zoom 9, max_zoom 3)"),
            "must not be greater than max_zoom",
        ),
        (
            format!(
                "COPY (SELECT mean, h3_index FROM ({cells})) TO '{out}' OPTIONS (h3_column 'mean')"
            ),
            "has type Float64",
        ),
        (
            format!(
                "COPY ({cells}) TO '{}/' STORED AS PMTILES",
                dir.path().join("as_dir").display()
            ),
            "single file",
        ),
        (
            format!("COPY ({cells}) TO '{out}' PARTITIONED BY (resolution)"),
            "PARTITIONED BY is not supported",
        ),
        (
            format!("COPY (SELECT CAST(42 AS BIGINT UNSIGNED) AS h3_index) TO '{out}'"),
            "42 is not a valid H3 cell",
        ),
    ] {
        let err = copy_err(&ctx, &sql).await;
        assert!(err.contains(needle), "{sql}\n{err}");
    }
    assert!(
        !Path::new(out).exists(),
        "a failed COPY must not leave an archive"
    );

    // PMTiles is write-only: no table factory is registered for it.
    let err = copy_err(
        &ctx,
        &format!("CREATE EXTERNAL TABLE t STORED AS PMTILES LOCATION '{out}'"),
    )
    .await;
    assert!(err.contains("PMTILES"), "{err}");
}
