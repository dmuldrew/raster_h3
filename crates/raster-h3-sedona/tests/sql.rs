//! End-to-end SQL tests against a plain DataFusion `SessionContext`.

mod common;

use std::sync::Arc;

use datafusion::arrow::array::{Array, AsArray};
use datafusion::arrow::datatypes::{Float64Type, Int64Type, UInt64Type};
use datafusion::catalog::TableProvider;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::prelude::{col, lit, SessionContext};
use raster_h3_datafusion::{CategoricalRasterTableProvider, ContinuousRasterTableProvider};
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

/// `(h3_index, resolution, count)` rows of `sql`, sorted.
async fn cell_counts(ctx: &SessionContext, sql: &str) -> Vec<(i64, i64, f64)> {
    let mut out: Vec<_> = collect(ctx, sql)
        .await
        .iter()
        .flat_map(|b| {
            let h = b.column(0).as_primitive::<Int64Type>().clone();
            let r = b.column(1).as_primitive::<Int64Type>().clone();
            let c = b.column(2).as_primitive::<Float64Type>().clone();
            (0..b.num_rows()).map(move |i| (h.value(i), r.value(i), c.value(i)))
        })
        .collect();
    out.sort_by_key(|c| c.0);
    out
}

/// Compacted tables refuse pushdown, so DataFusion must keep every predicate as a residual
/// filter over completed rows and apply `LIMIT` after it. Advertising them `Exact` would
/// return the unfiltered compacted table.
#[tokio::test(flavor = "multi_thread")]
async fn compacted_tables_filter_completed_rows() {
    let tif = gradient_tif(128, 128);
    let src = source(&tif);
    let continuous = ContinuousRasterTableProvider::try_new(&src, vec![8])
        .unwrap()
        .with_compact(true);
    let mut categorical = CategoricalRasterTableProvider::try_new(&src, vec![8]).unwrap();
    categorical.config.compact_h3_children = true;
    let tables = [
        ("count", RasterH3Table::continuous(continuous)),
        ("total_count", RasterH3Table::categorical(categorical)),
    ];
    for (count, table) in tables {
        let ctx = context();
        ctx.register_table("t", Arc::new(table)).unwrap();
        let select = format!(
            "SELECT CAST(h3_index AS BIGINT), CAST(resolution AS BIGINT), \
             CAST({count} AS DOUBLE) FROM t"
        );
        let full = cell_counts(&ctx, &select).await;
        let parents: Vec<_> = full.iter().filter(|c| c.1 == 7).cloned().collect();
        assert!(!parents.is_empty(), "{count}: no compacted parents");
        assert!(full.iter().any(|c| c.1 == 8), "{count}: no children left");
        let threshold = parents.iter().map(|c| c.2).fold(f64::INFINITY, f64::min);

        for (filter, keep) in [
            (
                "resolution = 7".to_string(),
                Box::new(|c: &(i64, i64, f64)| c.1 == 7) as Box<dyn Fn(&_) -> bool>,
            ),
            (
                format!("{count} >= {threshold}"),
                Box::new(move |c: &(i64, i64, f64)| c.2 >= threshold),
            ),
            (
                format!("resolution = 8 AND {count} >= 1"),
                Box::new(|c: &(i64, i64, f64)| c.1 == 8 && c.2 >= 1.0),
            ),
        ] {
            let expected: Vec<_> = full.iter().filter(|c| keep(c)).cloned().collect();
            assert!(!expected.is_empty(), "{count}: {filter} matches nothing");
            assert!(expected.len() < full.len(), "{count}: {filter} matches all");
            let filtered = cell_counts(&ctx, &format!("{select} WHERE {filter}")).await;
            assert_eq!(filtered, expected, "{count}: WHERE {filter}");
            let limited = cell_counts(&ctx, &format!("{select} WHERE {filter} LIMIT 1")).await;
            assert_eq!(limited.len(), 1, "{count}: WHERE {filter} LIMIT 1");
            assert!(
                expected.contains(&limited[0]),
                "{count}: WHERE {filter} LIMIT 1"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_defers_limits_until_residual_filters_are_applied() {
    use TableProviderFilterPushDown::{Exact, Inexact, Unsupported};

    let tif = gradient_tif(128, 128);
    let src = source(&tif);
    let ctx = context();
    for compaction in 0..=2 {
        let mut continuous = ContinuousRasterTableProvider::try_new(&src, vec![8]).unwrap();
        let mut categorical = CategoricalRasterTableProvider::try_new(&src, vec![8]).unwrap();
        for config in [&mut continuous.config, &mut categorical.config] {
            config.compact = compaction == 1;
            config.compact_h3_children = compaction == 2;
        }
        continuous.batch_size = 2;
        categorical.batch_size = 2;
        for (count, table) in [
            ("count", RasterH3Table::continuous(continuous)),
            ("total_count", RasterH3Table::categorical(categorical)),
        ] {
            for (filters, mut expected_statuses) in [
                (vec![], vec![]),
                (vec![col(count).gt_eq(lit(0.0))], vec![Exact]),
                (vec![col(count).gt(lit(0.0))], vec![Inexact]),
                (vec![col(count).lt(lit(100.0))], vec![Unsupported]),
                (
                    vec![col(count).between(lit(0.0), lit(100.0))],
                    vec![Inexact],
                ),
                (
                    vec![col(count).gt_eq(lit(0.0)), col(count).lt(lit(100.0))],
                    vec![Exact, Unsupported],
                ),
                (
                    vec![col(count).gt_eq(lit(0.0)), col(count).gt(lit(1.0))],
                    vec![Exact, Inexact],
                ),
            ] {
                let statuses = table
                    .supports_filters_pushdown(&filters.iter().collect::<Vec<_>>())
                    .unwrap();
                if compaction > 0 {
                    expected_statuses.fill(Unsupported);
                }
                assert_eq!(statuses, expected_statuses);
                let unbounded = table
                    .scan(&ctx.state(), None, &filters, None)
                    .await
                    .unwrap();
                let unbounded = datafusion::physical_plan::collect(unbounded, ctx.task_ctx())
                    .await
                    .unwrap();
                assert!(rows(&unbounded) > 1);
                let limited = table
                    .scan(&ctx.state(), None, &filters, Some(1))
                    .await
                    .unwrap();
                let limited = datafusion::physical_plan::collect(limited, ctx.task_ctx())
                    .await
                    .unwrap();
                let expected = if statuses.iter().all(|s| *s == Exact) {
                    1
                } else {
                    rows(&unbounded)
                };
                assert_eq!(
                    rows(&limited),
                    expected,
                    "{count}, compaction={compaction}, filters={filters:?}"
                );
            }
        }
    }
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

fn polygon_wkb(ring: &[(f64, f64)]) -> Vec<u8> {
    let mut wkb = vec![1u8];
    wkb.extend(3u32.to_le_bytes());
    wkb.extend(1u32.to_le_bytes());
    wkb.extend((ring.len() as u32).to_le_bytes());
    for (x, y) in ring {
        wkb.extend(x.to_le_bytes());
        wkb.extend(y.to_le_bytes());
    }
    wkb
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn cells(batches: &[datafusion::arrow::array::RecordBatch]) -> Vec<u64> {
    batches
        .iter()
        .flat_map(|b| b.column(0).as_primitive::<UInt64Type>().values().to_vec())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn polygon_to_cells_polyfills_and_joins_on_h3_index() {
    let ctx = context();
    let cell = 0x8828308281fffffu64;

    // A cell's own polygon holds exactly its own centroid at its resolution.
    let own = collect(
        &ctx,
        &format!("SELECT unnest(h3_polygon_to_cells(h3_to_wkb({cell}), 8))"),
    )
    .await;
    assert_eq!(cells(&own), vec![cell]);
    // `covers` adds the neighbours it overlaps; the list is sorted and unique.
    let covers = cells(
        &collect(
            &ctx,
            &format!("SELECT unnest(h3_polygon_to_cells(h3_to_wkb({cell}), 8, 'covers'))"),
        )
        .await,
    );
    assert!(covers.len() > 1 && covers.contains(&cell));
    assert!(covers.windows(2).all(|w| w[0] < w[1]));
    // NULL in, NULL out.
    let null = collect(&ctx, "SELECT h3_polygon_to_cells(NULL, 8) IS NULL").await;
    assert!(null[0].column(0).as_boolean().value(0));

    // Join raster cells to a polyfilled polygon on the shared index.
    let tif = gradient_tif(64, 64);
    let src = source(&tif);
    let ring = [
        (-122.44, 37.75),
        (-122.40, 37.75),
        (-122.40, 37.79),
        (-122.44, 37.79),
        (-122.44, 37.75),
    ];
    let wkb = polygon_wkb(&ring);
    let joined = cells(
        &collect(
            &ctx,
            &format!(
                "WITH aoi AS (SELECT unnest(h3_polygon_to_cells(X'{}', 8, 'covers')) AS h3_index) \
                 SELECT r.h3_index FROM h3_raster_continuous_aggregate('{src}', 8) r \
                 JOIN aoi ON r.h3_index = aoi.h3_index ORDER BY r.h3_index",
                hex(&wkb)
            ),
        )
        .await,
    );
    let expected = raster_h3_core::scalar_ops::h3_polygon_to_cells_wkb(
        &wkb,
        8,
        raster_h3_core::scalar_ops::ContainmentMode::Covers,
    )
    .unwrap();
    let all = cells(
        &collect(
            &ctx,
            &format!("SELECT h3_index FROM h3_raster_continuous_aggregate('{src}', 8)"),
        )
        .await,
    );
    let mut want: Vec<u64> = all.into_iter().filter(|c| expected.contains(c)).collect();
    want.sort_unstable();
    assert!(!want.is_empty());
    assert_eq!(joined, want);
}

#[tokio::test(flavor = "multi_thread")]
async fn polygon_to_cells_handles_bad_input_like_duckdb() {
    let ctx = context();
    let wkb = hex(&polygon_wkb(&[
        (0.0, 0.0),
        (1.0, 0.0),
        (1.0, 1.0),
        (0.0, 0.0),
    ]));
    // As in DuckDB: an invalid polygon or resolution is NULL, an unknown mode an error.
    for sql in [
        format!("SELECT h3_polygon_to_cells(X'{wkb}', 16) IS NULL"),
        "SELECT h3_polygon_to_cells(X'0102', 8) IS NULL".to_string(),
    ] {
        let b = collect(&ctx, &sql).await;
        assert!(b[0].column(0).as_boolean().value(0), "{sql}");
    }
    let unknown_mode = ctx
        .sql(&format!(
            "SELECT h3_polygon_to_cells(X'{wkb}', 8, 'nearby')"
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(
        unknown_mode.to_string().contains("contains_centroid"),
        "{unknown_mode}"
    );
    for sql in [
        "SELECT h3_polygon_to_cells('POLYGON EMPTY', 8)",
        "SELECT h3_polygon_to_cells(X'00', 8.5)",
        "SELECT h3_polygon_to_cells(X'00')",
    ] {
        assert!(ctx.sql(sql).await.is_err(), "{sql} should not plan");
    }

    // A geometry tagged with a projected CRS is refused at planning.
    use datafusion::arrow::array::BinaryArray;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    let field = Field::new("geom", DataType::Binary, false).with_metadata(
        [
            (
                "ARROW:extension:name".to_string(),
                GEOARROW_WKB_EXTENSION.to_string(),
            ),
            (
                "ARROW:extension:metadata".to_string(),
                r#"{"crs":"EPSG:3857"}"#.to_string(),
            ),
        ]
        .into(),
    );
    let batch = datafusion::arrow::array::RecordBatch::try_new(
        Arc::new(Schema::new(vec![field])),
        vec![Arc::new(BinaryArray::from_vec(vec![b"x".as_slice()]))],
    )
    .unwrap();
    ctx.register_batch("projected", batch).unwrap();
    let err = ctx
        .sql("SELECT h3_polygon_to_cells(geom, 8) FROM projected")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("lon/lat"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn polygon_to_cells_parallel_batch_matches_sequential() {
    use datafusion::arrow::array::{BinaryArray, ListArray, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use raster_h3_core::scalar_ops::{h3_polygon_to_cells_wkb, h3_to_wkb_u64, ContainmentMode};

    // 200 res-7 cell polygons around San Francisco, polyfilled at res 9, with a NULL and an
    // invalid geometry mixed in.
    let parent = h3o::CellIndex::try_from(0x8828308281fffffu64)
        .unwrap()
        .parent(h3o::Resolution::Four)
        .unwrap();
    let mut geoms: Vec<Option<Vec<u8>>> = parent
        .children(h3o::Resolution::Seven)
        .take(200)
        .map(|c| Some(h3_to_wkb_u64(u64::from(c)).unwrap()))
        .collect();
    geoms[17] = None;
    geoms[42] = Some(vec![1, 2, 3]);
    let n = geoms.len();
    let schema = Arc::new(Schema::new(vec![
        Field::new("geom", DataType::Binary, true),
        Field::new("mode", DataType::Utf8, false),
    ]));
    let batch = |modes: Vec<&str>| {
        datafusion::arrow::array::RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(BinaryArray::from_iter(geoms.iter().map(|g| g.as_deref()))),
                Arc::new(StringArray::from(modes)),
            ],
        )
        .unwrap()
    };
    let ctx = context();
    ctx.register_batch("polys", batch(vec!["covers"; n]))
        .unwrap();

    let out = collect(&ctx, "SELECT h3_polygon_to_cells(geom, 9, mode) FROM polys").await;
    let got: Vec<Option<Vec<u64>>> = out
        .iter()
        .flat_map(|b| {
            let list = b.column(0).as_any().downcast_ref::<ListArray>().unwrap();
            (0..list.len())
                .map(|i| {
                    (!list.is_null(i))
                        .then(|| list.value(i).as_primitive::<UInt64Type>().values().to_vec())
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let want: Vec<Option<Vec<u64>>> = geoms
        .iter()
        .map(|g| {
            g.as_deref()
                .and_then(|g| h3_polygon_to_cells_wkb(g, 9, ContainmentMode::Covers).ok())
        })
        .collect();
    assert_eq!(got.len(), n);
    assert!(got[17].is_none() && got[42].is_none());
    assert!(got
        .iter()
        .filter(|c| c.is_some())
        .all(|c| c.as_ref().unwrap().len() > 40));
    assert_eq!(got, want);

    // The first bad row in row order is the error reported, as in a sequential pass.
    let mut modes = vec!["covers"; n];
    modes[150] = "nearby";
    modes[60] = "sideways";
    ctx.register_batch("bad_modes", batch(modes)).unwrap();
    let err = ctx
        .sql("SELECT h3_polygon_to_cells(geom, 9, mode) FROM bad_modes")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("'sideways'"), "{err}");
}
