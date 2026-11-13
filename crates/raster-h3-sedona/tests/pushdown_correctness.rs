//! Spatial pushdown returns the same cells, with the same aggregates, as an unpruned scan.
//!
//! Any predicate `P(cell, g)` that implies the envelopes of `cell` and `g` intersect may be
//! pushed down as a `Bbox`. These tests register envelope-only stand-ins for `st_intersects`
//! and `st_dwithin` (no SedonaDB needed), run many random query geometries against rasters
//! in several CRSs, resolutions and kinds, and compare every column of every matching row
//! with the same filter applied to a materialised copy of the table.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use datafusion::arrow::array::{Array, AsArray, BooleanArray, RecordBatch};
use datafusion::arrow::datatypes::DataType;
use datafusion::catalog::TableProvider;
use datafusion::common::metadata::FieldMetadata;
use datafusion::common::ScalarValue;
use datafusion::logical_expr::expr::ScalarFunction;
use datafusion::logical_expr::{
    create_udf, ColumnarValue, Expr, ScalarUDF, TableProviderFilterPushDown, Volatility,
};
use datafusion::prelude::{col, lit, SessionContext};
use raster_h3_sedona::scan::{CategoricalRasterTableProvider, ContinuousRasterTableProvider};
use raster_h3_sedona::{RasterH3Table, GEOARROW_WKB_EXTENSION, GEOARROW_WKB_METADATA};

use common::write_tif_grid;

/// Envelope `[min_x, min_y, max_x, max_y]` of little-endian 2D WKB (points, polygons, multis).
fn envelope(wkb: &[u8]) -> [f64; 4] {
    fn walk(b: &[u8], pos: &mut usize, env: &mut [f64; 4]) {
        let u32_at = |p: &mut usize| {
            let v = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        };
        let mut point = |p: &mut usize| {
            let x = f64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
            let y = f64::from_le_bytes(b[*p + 8..*p + 16].try_into().unwrap());
            *p += 16;
            env[0] = env[0].min(x);
            env[1] = env[1].min(y);
            env[2] = env[2].max(x);
            env[3] = env[3].max(y);
        };
        assert_eq!(b[*pos], 1, "little-endian WKB");
        *pos += 1;
        match u32_at(pos) {
            1 => point(pos),
            3 => {
                for _ in 0..u32_at(pos) {
                    for _ in 0..u32_at(pos) {
                        point(pos);
                    }
                }
            }
            4..=7 => {
                for _ in 0..u32_at(pos) {
                    walk(b, pos, env);
                }
            }
            t => panic!("unexpected WKB type {t}"),
        }
    }
    let mut env = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    walk(wkb, &mut 0, &mut env);
    env
}

fn binary_values(v: &ColumnarValue, rows: usize) -> Vec<Vec<u8>> {
    let array = v.to_array(rows).unwrap();
    let array = array.as_binary::<i32>();
    (0..rows).map(|i| array.value(i).to_vec()).collect()
}

/// `name(a, b [, d])`: the envelopes of `a` and `b`, the latter grown by `d`, intersect.
fn envelope_predicate(name: &str, with_distance: bool) -> Arc<ScalarUDF> {
    let mut args = vec![DataType::Binary, DataType::Binary];
    if with_distance {
        args.push(DataType::Float64);
    }
    Arc::new(create_udf(
        name,
        args,
        DataType::Boolean,
        Volatility::Immutable,
        Arc::new(move |args: &[ColumnarValue]| {
            let rows = args
                .iter()
                .find_map(|a| match a {
                    ColumnarValue::Array(a) => Some(a.len()),
                    ColumnarValue::Scalar(_) => None,
                })
                .unwrap_or(1);
            let a = binary_values(&args[0], rows);
            let b = binary_values(&args[1], rows);
            let d = match args.get(2) {
                Some(ColumnarValue::Scalar(ScalarValue::Float64(Some(d)))) => *d,
                _ => 0.0,
            };
            let out: BooleanArray = (0..rows)
                .map(|i| {
                    let (ea, eb) = (envelope(&a[i]), envelope(&b[i]));
                    Some(
                        ea[0] <= eb[2] + d
                            && ea[2] >= eb[0] - d
                            && ea[1] <= eb[3] + d
                            && ea[3] >= eb[1] - d,
                    )
                })
                .collect();
            Ok(ColumnarValue::Array(Arc::new(out)))
        }),
    ))
}

fn geometry_literal(wkb: Vec<u8>) -> Expr {
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
    Expr::Literal(ScalarValue::Binary(Some(wkb)), Some(metadata))
}

fn point_wkb(x: f64, y: f64) -> Vec<u8> {
    let mut w = vec![1u8];
    w.extend(1u32.to_le_bytes());
    w.extend(x.to_le_bytes());
    w.extend(y.to_le_bytes());
    w
}

fn box_wkb([x0, y0, x1, y1]: [f64; 4]) -> Vec<u8> {
    let mut w = vec![1u8];
    w.extend(3u32.to_le_bytes());
    w.extend(1u32.to_le_bytes());
    w.extend(5u32.to_le_bytes());
    for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)] {
        w.extend(x.to_le_bytes());
        w.extend(y.to_le_bytes());
    }
    w
}

/// Deterministic pseudo-random numbers in `[0, 1)`.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn rows_as_strings(batches: &[RecordBatch]) -> Vec<String> {
    let mut rows: Vec<String> = batches
        .iter()
        .flat_map(|b| {
            (0..b.num_rows()).map(move |i| {
                b.columns()
                    .iter()
                    .map(|c| ScalarValue::try_from_array(c, i).unwrap().to_string())
                    .collect::<Vec<_>>()
                    .join("|")
            })
        })
        .collect();
    rows.sort();
    rows
}

/// Check `table` against a materialised copy for `queries` random geometries.
async fn check(name: &str, table: RasterH3Table, seed: u64, queries: usize) {
    let ctx = SessionContext::new();
    ctx.register_udf((*envelope_predicate("st_intersects", false)).clone());
    ctx.register_udf((*envelope_predicate("st_dwithin", true)).clone());

    let table: Arc<dyn TableProvider> = Arc::new(table);
    ctx.register_table("live", Arc::clone(&table)).unwrap();
    let full = ctx.table("live").await.unwrap().collect().await.unwrap();
    assert!(
        full.iter().map(|b| b.num_rows()).sum::<usize>() > 20,
        "{name}: too few cells"
    );
    ctx.register_batch(
        "full",
        datafusion::arrow::compute::concat_batches(&full[0].schema(), &full).unwrap(),
    )
    .unwrap();

    // Query geometries around the cells' lon/lat extent.
    let extent = full.iter().fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |env, b| {
            let wkb = b.column_by_name("wkb").unwrap().as_binary::<i32>();
            (0..wkb.len()).fold(env, |e, i| {
                let c = envelope(wkb.value(i));
                [
                    e[0].min(c[0]),
                    e[1].min(c[1]),
                    e[2].max(c[2]),
                    e[3].max(c[3]),
                ]
            })
        },
    );
    let (w, h) = (extent[2] - extent[0], extent[3] - extent[1]);
    let mut rng = Lcg(seed);
    let mut matched_some = false;
    for q in 0..queries {
        // Centres up to 10% outside the extent; sizes from a point to half the extent.
        let cx = extent[0] - 0.1 * w + 1.2 * w * rng.next();
        let cy = extent[1] - 0.1 * h + 1.2 * h * rng.next();
        let (hw, hh) = (0.25 * w * rng.next(), 0.25 * h * rng.next());
        let geometry = if q % 3 == 0 {
            point_wkb(cx, cy)
        } else {
            box_wkb([cx - hw, cy - hh, cx + hw, cy + hh])
        };
        let predicate = |column: Expr| {
            let (udf, mut args) = if q % 4 == 3 {
                (
                    envelope_predicate("st_dwithin", true),
                    vec![column, geometry_literal(geometry.clone())],
                )
            } else {
                (
                    envelope_predicate("st_intersects", false),
                    vec![geometry_literal(geometry.clone()), column],
                )
            };
            if q % 4 == 3 {
                args.push(lit(0.05 * w.min(h) * rng_distance(q)));
            }
            Expr::ScalarFunction(ScalarFunction::new_udf(udf, args))
        };

        let pushed = predicate(col("wkb"));
        assert_eq!(
            table.supports_filters_pushdown(&[&pushed]).unwrap(),
            vec![TableProviderFilterPushDown::Inexact],
            "{name}: query {q} is not pushed down"
        );
        let live = ctx.table("live").await.unwrap().filter(pushed).unwrap();
        let reference = ctx
            .table("full")
            .await
            .unwrap()
            .filter(predicate(col("wkb")))
            .unwrap();
        let (live, reference) = (
            rows_as_strings(&live.collect().await.unwrap()),
            rows_as_strings(&reference.collect().await.unwrap()),
        );
        matched_some |= !reference.is_empty();
        assert_eq!(
            live,
            reference,
            "{name}: query {q} ({:?}) differs from an unpruned scan",
            envelope(&geometry)
        );
    }
    assert!(matched_some, "{name}: no query matched any cell");
}

/// The `st_dwithin` distance factor for query `q` (every fourth query), cycling through
/// zero, small and larger-than-cell distances.
fn rng_distance(q: usize) -> f64 {
    [0.0, 0.3, 1.0, 2.5][(q / 4) % 4]
}

/// Integer-valued pattern with several classes and uneven cells.
fn value(x: u32, y: u32) -> f32 {
    ((x * 7 + y * 13 + (x * y) % 11) % 23) as f32
}

fn continuous(tif: &tempfile::NamedTempFile, crs: &str, res: Vec<u8>) -> RasterH3Table {
    let p = ContinuousRasterTableProvider::try_new(tif.path().to_str().unwrap(), res)
        .unwrap()
        .with_crs(crs);
    RasterH3Table::continuous(p)
}

#[tokio::test(flavor = "multi_thread")]
async fn lnglat_raster() {
    let tif = write_tif_grid(300, 300, (-122.45, 37.80), 0.001, value);
    check(
        "EPSG:4326 res 8",
        continuous(&tif, "EPSG:4326", vec![8]),
        1,
        24,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_resolution() {
    let tif = write_tif_grid(300, 300, (-122.45, 37.80), 0.001, value);
    check(
        "EPSG:4326 res 6-8",
        continuous(&tif, "EPSG:4326", vec![6, 7, 8]),
        2,
        24,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn utm_raster() {
    // 30 m pixels, 12 km square, UTM zone 10N over San Francisco.
    let tif = write_tif_grid(400, 400, (545_000.0, 4_185_000.0), 30.0, value);
    check(
        "EPSG:32610 res 8",
        continuous(&tif, "EPSG:32610", vec![8]),
        3,
        24,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn web_mercator_raster() {
    let tif = write_tif_grid(300, 300, (-13_631_000.0, 4_548_000.0), 50.0, value);
    check(
        "EPSG:3857 res 8",
        continuous(&tif, "EPSG:3857", vec![8]),
        4,
        24,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn high_latitude_raster() {
    // Near 70°N a degree of longitude is about a third of a degree of latitude.
    let tif = write_tif_grid(300, 300, (25.0, 70.6), 0.002, value);
    check(
        "EPSG:4326 70N res 7",
        continuous(&tif, "EPSG:4326", vec![7]),
        5,
        24,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn categorical_utm_raster() {
    let tif = write_tif_grid(400, 400, (545_000.0, 4_185_000.0), 30.0, value);
    let p = CategoricalRasterTableProvider::try_new(tif.path().to_str().unwrap(), vec![8])
        .unwrap()
        .with_crs("EPSG:32610");
    check(
        "categorical EPSG:32610 res 8",
        RasterH3Table::categorical(p),
        6,
        24,
    )
    .await;
}
