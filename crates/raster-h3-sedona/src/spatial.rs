//! `Bbox` pushdown from spatial predicates such as `ST_Intersects(wkb, <constant geometry>)`.
//!
//! A predicate between a cell's geometry and a constant lon/lat geometry `g` is translated
//! into the envelope of `g` (for `ST_DWithin(..., d)`, the envelope grown by `d` degrees)
//! and pushed down as a [`FilterPredicate::Bbox`](raster_h3_datafusion::FilterPredicate).
//! `raster-h3-datafusion` widens that box past the largest cell before clipping samples, so
//! cells on the box edge keep their full aggregates. The pushdown is `Inexact`: DataFusion
//! re-applies the predicate to drop the extra cells.
//!
//! A cell's geometry is the `wkb` column or `h3_to_wkb(h3_index)` / `h3_to_wkb(h3_hex)`,
//! which `raster_h3` encodes identically. The constant is a `geoarrow.wkb` literal in a
//! lon/lat CRS with planar edges (what Sedona's `ST_GeomFromWKT` / `ST_SetSRID` fold to),
//! or `h3_to_wkb(<cell literal>)`.
//!
//! Filters that cross SedonaDB's extension ABI are serialized with `datafusion-proto`,
//! which drops literal field metadata, so a constant geometry arrives as a bare `Binary`
//! literal. SedonaDB's planner has already required every geometry argument of an `ST_*`
//! predicate to share one CRS and planar edges, so with [`Untagged::LngLat`] such literals
//! are taken to be in the cells' `OGC:CRS84`.

use datafusion::common::ScalarValue;
use datafusion::logical_expr::expr::ScalarFunction;
use datafusion::logical_expr::Expr;

use crate::convert::{GEOARROW_WKB_EXTENSION, WKB_COLUMN};

/// Predicates `P(cell, g)` that imply the envelopes of `cell` and `g` intersect.
const BBOX_PREDICATES: &[&str] = &[
    "st_intersects",
    "st_contains",
    "st_within",
    "st_covers",
    "st_coveredby",
    "st_overlaps",
    "st_touches",
    "st_crosses",
    "st_equals",
];

/// `ST_DWithin(a, b, distance)`: the planar distance between `a` and `b` is at most
/// `distance`, in CRS units (degrees for lon/lat).
const DWITHIN: &str = "st_dwithin";

/// Names (and aliases) of the `raster_h3` UDF that encodes a cell as its WKB polygon.
const H3_TO_WKB: &[&str] = &["h3_to_wkb", "h3_to_geometry", "h3_cell_to_geometry"];

/// Cell-index columns `h3_to_wkb` turns into the same polygon as the `wkb` column.
const CELL_COLUMNS: &[&str] = &["h3_index", "h3_hex"];

/// How to read a `Binary` literal without `geoarrow.wkb` metadata in a spatial predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Untagged {
    /// Its CRS is unknown: do not push the predicate down.
    #[default]
    Reject,
    /// A WKB geometry in the cells' CRS (`OGC:CRS84`, planar).
    LngLat,
}

/// Translate a spatial predicate on a cell's geometry into a lon/lat box to push down, or
/// `None` when it is not one (or the other geometry is not a constant lon/lat geometry).
pub(crate) fn translate_spatial(expr: &Expr, untagged: Untagged) -> Option<[f64; 4]> {
    let Expr::ScalarFunction(ScalarFunction { func, args }) = expr else {
        return None;
    };
    let name = func.name().to_ascii_lowercase();
    let (a, b, distance) = match args.as_slice() {
        [a, b] if BBOX_PREDICATES.contains(&name.as_str()) => (a, b, 0.0),
        [a, b, d] if name == DWITHIN => (a, b, distance_literal(d)?),
        _ => return None,
    };
    let geometry = match (a, b) {
        (cell, g) | (g, cell) if is_cell_geometry(cell) => g,
        _ => return None,
    };
    let envelope = constant_envelope(geometry, untagged)?;
    grow(envelope, distance)
}

/// The `wkb` column, or `h3_to_wkb` of a cell-index column.
fn is_cell_geometry(expr: &Expr) -> bool {
    match expr {
        Expr::Column(c) => c.name == WKB_COLUMN,
        Expr::ScalarFunction(ScalarFunction { func, args }) => {
            is_h3_to_wkb(func.name())
                && matches!(args.as_slice(), [Expr::Column(c)] if CELL_COLUMNS.contains(&c.name.as_str()))
        }
        _ => false,
    }
}

fn is_h3_to_wkb(name: &str) -> bool {
    H3_TO_WKB.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// A non-negative, finite numeric literal.
fn distance_literal(expr: &Expr) -> Option<f64> {
    let Expr::Literal(value, _) = expr else {
        return None;
    };
    if !value.data_type().is_numeric() {
        return None;
    }
    match value
        .cast_to(&datafusion::arrow::datatypes::DataType::Float64)
        .ok()?
    {
        ScalarValue::Float64(Some(d)) if d.is_finite() && d >= 0.0 => Some(d),
        _ => None,
    }
}

/// Envelope of a constant lon/lat geometry: a `geoarrow.wkb` literal, or `h3_to_wkb` of a
/// cell literal (which DataFusion may not have folded).
fn constant_envelope(expr: &Expr, untagged: Untagged) -> Option<[f64; 4]> {
    match expr {
        Expr::Literal(
            ScalarValue::Binary(Some(b))
            | ScalarValue::LargeBinary(Some(b))
            | ScalarValue::BinaryView(Some(b)),
            None,
        ) if untagged == Untagged::LngLat => wkb_envelope(b),
        Expr::Literal(value, Some(metadata)) => {
            let metadata = metadata.inner();
            if metadata.get("ARROW:extension:name").map(String::as_str)
                != Some(GEOARROW_WKB_EXTENSION)
                || !is_planar_lnglat(metadata.get("ARROW:extension:metadata")?)
            {
                return None;
            }
            match value {
                ScalarValue::Binary(Some(b))
                | ScalarValue::LargeBinary(Some(b))
                | ScalarValue::BinaryView(Some(b)) => wkb_envelope(b),
                _ => None,
            }
        }
        Expr::ScalarFunction(ScalarFunction { func, args }) if is_h3_to_wkb(func.name()) => {
            let [Expr::Literal(cell, _)] = args.as_slice() else {
                return None;
            };
            let wkb = match cell {
                ScalarValue::Utf8(Some(s))
                | ScalarValue::Utf8View(Some(s))
                | ScalarValue::LargeUtf8(Some(s)) => raster_h3_core::scalar_ops::h3_to_wkb_str(s),
                v if v.data_type().is_integer() => match v
                    .cast_to(&datafusion::arrow::datatypes::DataType::UInt64)
                    .ok()?
                {
                    ScalarValue::UInt64(Some(c)) => raster_h3_core::scalar_ops::h3_to_wkb_u64(c),
                    _ => None,
                },
                _ => None,
            }?;
            wkb_envelope(&wkb)
        }
        _ => None,
    }
}

/// Whether GeoArrow extension metadata declares planar edges in a lon/lat CRS. An unknown
/// or missing CRS is rejected: pruning with coordinates in another CRS would drop rows.
fn is_planar_lnglat(metadata: &str) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return false;
    };
    // Spherical (geography) edges bulge past their vertices' envelope.
    if json.get("edges").is_some_and(|e| e != "planar") {
        return false;
    }
    let is_lnglat_code = |s: &str| matches!(s, "OGC:CRS84" | "EPSG:4326");
    match json.get("crs") {
        Some(serde_json::Value::String(s)) => is_lnglat_code(s),
        // PROJJSON: identify the CRS by its `id`.
        Some(crs) => crs.get("id").is_some_and(|id| {
            let authority = id.get("authority").and_then(|a| a.as_str());
            let code = id.get("code").map(|c| match c {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            matches!(
                (authority, code.as_deref()),
                (Some("OGC"), Some("CRS84")) | (Some("EPSG"), Some("4326"))
            )
        }),
        None => false,
    }
}

/// Grow `[min_lon, min_lat, max_lon, max_lat]` by `distance` degrees on every side, clamped
/// to lon/lat range. `None` for envelopes outside that range, which the core box test would
/// wrap differently than the query.
///
/// Planar distance does not wrap: cell polygons lie within [-180, 180], so a cell within
/// `distance` of the geometry has an envelope meeting the grown box, and clamping keeps
/// every such cell.
pub(crate) fn grow(envelope: [f64; 4], distance: f64) -> Option<[f64; 4]> {
    let [min_lon, min_lat, max_lon, max_lat] = envelope;
    let in_range =
        |lon: f64, lat: f64| (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat);
    if !(in_range(min_lon, min_lat) && in_range(max_lon, max_lat)) {
        return None;
    }
    Some([
        (min_lon - distance).max(-180.0),
        (min_lat - distance).max(-90.0),
        (max_lon + distance).min(180.0),
        (max_lat + distance).min(90.0),
    ])
}

/// Envelope `[min_x, min_y, max_x, max_y]` of any WKB / EWKB geometry (2D, Z, M, ZM; either
/// byte order). `None` for malformed input or an empty geometry.
pub(crate) fn wkb_envelope(bytes: &[u8]) -> Option<[f64; 4]> {
    let mut reader = Reader { bytes, pos: 0 };
    let mut env = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    reader.geometry(&mut env, 0)?;
    (reader.pos == bytes.len() && env[0] <= env[2] && env[1] <= env[3]).then_some(env)
}

/// Nesting limit for geometry collections, so hostile input cannot overflow the stack.
const MAX_DEPTH: usize = 32;

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let out = self.bytes.get(self.pos..self.pos.checked_add(N)?)?;
        self.pos += N;
        out.try_into().ok()
    }

    fn u32(&mut self, little: bool) -> Option<u32> {
        let b = self.take::<4>()?;
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self, little: bool) -> Option<f64> {
        let b = self.take::<8>()?;
        Some(if little {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }

    /// A count that cannot exceed the bytes left (each item is at least `min_size` bytes).
    fn count(&mut self, little: bool, min_size: usize) -> Option<usize> {
        let n = self.u32(little)? as usize;
        (n.checked_mul(min_size)? <= self.bytes.len() - self.pos).then_some(n)
    }

    fn points(&mut self, n: usize, dims: usize, little: bool, env: &mut [f64; 4]) -> Option<()> {
        for _ in 0..n {
            let x = self.f64(little)?;
            let y = self.f64(little)?;
            for _ in 2..dims {
                self.f64(little)?;
            }
            // `POINT EMPTY` is encoded as NaN coordinates.
            if !x.is_nan() && !y.is_nan() {
                env[0] = env[0].min(x);
                env[1] = env[1].min(y);
                env[2] = env[2].max(x);
                env[3] = env[3].max(y);
            }
        }
        Some(())
    }

    fn geometry(&mut self, env: &mut [f64; 4], depth: usize) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        let little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let code = self.u32(little)?;
        // EWKB flag bits, then ISO 1000/2000/3000 dimension offsets.
        let (ewkb_z, ewkb_m, srid) = (
            code & 0x8000_0000 != 0,
            code & 0x4000_0000 != 0,
            code & 0x2000_0000 != 0,
        );
        let iso = code & 0x0FFF_FFFF;
        let (base, iso_z, iso_m) = match iso / 1000 {
            0 => (iso, false, false),
            1 => (iso % 1000, true, false),
            2 => (iso % 1000, false, true),
            3 => (iso % 1000, true, true),
            _ => return None,
        };
        if srid {
            self.take::<4>()?;
        }
        let dims = 2 + usize::from(ewkb_z || iso_z) + usize::from(ewkb_m || iso_m);
        let point_size = 8 * dims;
        match base {
            1 => self.points(1, dims, little, env),
            2 => {
                let n = self.count(little, point_size)?;
                self.points(n, dims, little, env)
            }
            3 => {
                for _ in 0..self.count(little, 4)? {
                    let n = self.count(little, point_size)?;
                    self.points(n, dims, little, env)?;
                }
                Some(())
            }
            4..=7 => {
                for _ in 0..self.count(little, 5)? {
                    self.geometry(env, depth + 1)?;
                }
                Some(())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h3o::CellIndex;

    fn point_wkb(x: f64, y: f64) -> Vec<u8> {
        let mut out = vec![1u8];
        out.extend(1u32.to_le_bytes());
        out.extend(x.to_le_bytes());
        out.extend(y.to_le_bytes());
        out
    }

    #[test]
    fn envelopes_of_points_lines_and_collections() {
        assert_eq!(
            wkb_envelope(&point_wkb(1.0, 2.0)),
            Some([1.0, 2.0, 1.0, 2.0])
        );

        // Big-endian LINESTRING Z.
        let mut line = vec![0u8];
        line.extend(1002u32.to_be_bytes());
        line.extend(2u32.to_be_bytes());
        for v in [-1.0f64, 5.0, 100.0, 3.0, -4.0, 100.0] {
            line.extend(v.to_be_bytes());
        }
        assert_eq!(wkb_envelope(&line), Some([-1.0, -4.0, 3.0, 5.0]));

        // EWKB MULTIPOINT with SRID holding the two points above.
        let mut multi = vec![1u8];
        multi.extend((4u32 | 0x2000_0000).to_le_bytes());
        multi.extend(4326u32.to_le_bytes());
        multi.extend(2u32.to_le_bytes());
        multi.extend(point_wkb(1.0, 2.0));
        multi.extend(point_wkb(-3.0, 7.0));
        assert_eq!(wkb_envelope(&multi), Some([-3.0, 2.0, 1.0, 7.0]));

        // A real H3 polygon from the core encoder.
        let cell = CellIndex::try_from(0x8828308281fffff).unwrap();
        let wkb = raster_h3_core::scalar_ops::h3_to_wkb_u64(u64::from(cell)).unwrap();
        let env = wkb_envelope(&wkb).unwrap();
        let center = h3o::LatLng::from(cell);
        assert!(env[0] < center.lng() && center.lng() < env[2]);
        assert!(env[1] < center.lat() && center.lat() < env[3]);
    }

    #[test]
    fn malformed_and_empty_wkb_is_rejected() {
        let p = point_wkb(1.0, 2.0);
        assert_eq!(wkb_envelope(&p[..p.len() - 1]), None);
        assert_eq!(wkb_envelope(&[p.clone(), vec![0]].concat()), None);
        assert_eq!(wkb_envelope(&point_wkb(f64::NAN, f64::NAN)), None);
        // A collection claiming more members than bytes remain.
        let mut bogus = vec![1u8];
        bogus.extend(7u32.to_le_bytes());
        bogus.extend(u32::MAX.to_le_bytes());
        assert_eq!(wkb_envelope(&bogus), None);
    }

    #[test]
    fn grow_clamps_to_lnglat_range() {
        assert_eq!(grow([0.0, 0.0, 1.0, 1.0], 0.0), Some([0.0, 0.0, 1.0, 1.0]));
        assert_eq!(
            grow([10.0, 10.0, 11.0, 11.0], 0.5),
            Some([9.5, 9.5, 11.5, 11.5])
        );
        assert_eq!(
            grow([179.5, 89.5, 179.9, 89.9], 1.0),
            Some([178.5, 88.5, 180.0, 90.0])
        );
        assert_eq!(grow([190.0, 0.0, 191.0, 1.0], 0.0), None);
    }

    fn predicate(name: &str, args: Vec<Expr>) -> Expr {
        use datafusion::arrow::datatypes::DataType;
        use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
        let udf = create_udf(
            name,
            vec![DataType::Binary; args.len()],
            DataType::Boolean,
            Volatility::Immutable,
            std::sync::Arc::new(|_: &[ColumnarValue]| unreachable!()),
        );
        Expr::ScalarFunction(ScalarFunction::new_udf(std::sync::Arc::new(udf), args))
    }

    fn geometry(wkb: Vec<u8>, crs: &str) -> Expr {
        let metadata = std::collections::BTreeMap::from([
            (
                "ARROW:extension:name".to_string(),
                GEOARROW_WKB_EXTENSION.to_string(),
            ),
            (
                "ARROW:extension:metadata".to_string(),
                format!(r#"{{"crs":"{crs}"}}"#),
            ),
        ]);
        Expr::Literal(
            ScalarValue::Binary(Some(wkb)),
            Some(datafusion::common::metadata::FieldMetadata::new(metadata)),
        )
    }

    #[test]
    fn spatial_predicates_translate_to_envelopes() {
        use datafusion::prelude::{col, lit};
        let pt = || geometry(point_wkb(-122.4, 37.7), "OGC:CRS84");
        let point_box = Some([-122.4, 37.7, -122.4, 37.7]);
        assert_eq!(
            translate_spatial(
                &predicate("ST_Intersects", vec![col("wkb"), pt()]),
                Untagged::Reject
            ),
            point_box
        );
        // Either argument order; any envelope-implying predicate.
        assert_eq!(
            translate_spatial(
                &predicate("st_within", vec![pt(), col("wkb")]),
                Untagged::Reject
            ),
            point_box
        );
        // `h3_to_wkb` of a cell column is the cell's geometry, under any alias.
        for (f, c) in [("h3_to_wkb", "h3_index"), ("h3_cell_to_geometry", "h3_hex")] {
            let cell = predicate(f, vec![col(c)]);
            assert_eq!(
                translate_spatial(
                    &predicate("st_intersects", vec![cell, pt()]),
                    Untagged::Reject
                ),
                point_box
            );
        }
        // `ST_DWithin` grows the envelope by the distance (degrees).
        let [lo_lon, lo_lat, hi_lon, hi_lat] = translate_spatial(
            &predicate("st_dwithin", vec![col("wkb"), pt(), lit(0.25)]),
            Untagged::Reject,
        )
        .unwrap();
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(lo_lon, -122.65) && close(hi_lon, -122.15));
        assert!(close(lo_lat, 37.45) && close(hi_lat, 37.95));
        assert_eq!(
            translate_spatial(
                &predicate("st_dwithin", vec![pt(), col("wkb"), lit(0)]),
                Untagged::Reject
            ),
            point_box
        );
        // A constant `h3_to_wkb(<cell>)` contributes the cell's envelope.
        let cell = CellIndex::try_from(0x8828308281fffff).unwrap();
        let expected =
            wkb_envelope(&raster_h3_core::scalar_ops::h3_to_wkb_u64(u64::from(cell)).unwrap());
        for literal in [lit(u64::from(cell)), lit(cell.to_string())] {
            let g = predicate("h3_to_wkb", vec![literal]);
            assert_eq!(
                translate_spatial(
                    &predicate("st_intersects", vec![col("wkb"), g]),
                    Untagged::Reject
                ),
                expected
            );
        }

        for rejected in [
            predicate("st_disjoint", vec![col("wkb"), pt()]),
            predicate("st_intersects", vec![col("other"), pt()]),
            predicate("st_intersects", vec![col("wkb"), col("wkb")]),
            predicate(
                "st_intersects",
                vec![predicate("h3_to_wkb", vec![col("mean")]), pt()],
            ),
            predicate(
                "st_intersects",
                vec![col("wkb"), geometry(point_wkb(0.0, 0.0), "EPSG:3857")],
            ),
            predicate(
                "st_intersects",
                vec![
                    col("wkb"),
                    Expr::Literal(ScalarValue::Binary(Some(point_wkb(0.0, 0.0))), None),
                ],
            ),
            // A distance that is negative, NULL, non-numeric or not a literal.
            predicate("st_dwithin", vec![col("wkb"), pt(), lit(-1.0)]),
            predicate(
                "st_dwithin",
                vec![col("wkb"), pt(), lit(ScalarValue::Float64(None))],
            ),
            predicate("st_dwithin", vec![col("wkb"), pt(), lit("1")]),
            predicate("st_dwithin", vec![col("wkb"), pt(), col("mean")]),
            predicate("st_dwithin", vec![col("wkb"), pt()]),
            predicate("st_intersects", vec![col("wkb"), pt(), lit(1.0)]),
        ] {
            assert_eq!(
                translate_spatial(&rejected, Untagged::Reject),
                None,
                "{rejected}"
            );
        }

        // An untagged WKB literal (as filters arrive over SedonaDB's extension ABI) is
        // pushed only when the table opts in.
        let bare = predicate(
            "st_intersects",
            vec![
                col("wkb"),
                Expr::Literal(ScalarValue::Binary(Some(point_wkb(-122.4, 37.7))), None),
            ],
        );
        assert_eq!(translate_spatial(&bare, Untagged::Reject), None);
        assert_eq!(translate_spatial(&bare, Untagged::LngLat), point_box);
        // A literal tagged with another CRS is still rejected.
        let other_crs = predicate(
            "st_intersects",
            vec![col("wkb"), geometry(point_wkb(0.0, 0.0), "EPSG:3857")],
        );
        assert_eq!(translate_spatial(&other_crs, Untagged::LngLat), None);
    }

    #[test]
    fn crs_metadata() {
        assert!(is_planar_lnglat(r#"{"crs":"OGC:CRS84"}"#));
        assert!(is_planar_lnglat(r#"{"crs":"EPSG:4326","edges":"planar"}"#));
        assert!(is_planar_lnglat(
            r#"{"crs":{"id":{"authority":"EPSG","code":4326}}}"#
        ));
        assert!(!is_planar_lnglat(
            r#"{"crs":"OGC:CRS84","edges":"spherical"}"#
        ));
        assert!(!is_planar_lnglat(r#"{"crs":"EPSG:3857"}"#));
        assert!(!is_planar_lnglat("{}"));
        assert!(!is_planar_lnglat(""));
    }
}
