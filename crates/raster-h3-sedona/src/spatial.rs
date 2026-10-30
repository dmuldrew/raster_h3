//! `Bbox` pushdown from spatial predicates such as `ST_Intersects(wkb, <constant geometry>)`.
//!
//! The core `bbox` keeps only the *samples* inside the box, so pushing the query geometry's
//! envelope down unchanged would aggregate the cells on its edge from part of their pixels
//! and return the wrong `mean` / `count` for them. The pushed box is therefore the envelope
//! widened by [`cell_margin_km`], more than the diameter of any cell the table can emit:
//! every cell that touches the envelope then lies wholly inside the box and keeps all its
//! samples. The partial cells near the widened edge do not touch the envelope, so they fail
//! the predicate, which DataFusion re-applies (the pushdown is `Inexact`).

use datafusion::common::ScalarValue;
use datafusion::logical_expr::expr::ScalarFunction;
use datafusion::logical_expr::Expr;
use h3o::Resolution;

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

/// Kilometres per degree of latitude, rounded down so degree margins come out larger.
const KM_PER_DEG_LAT: f64 = 110.0;
/// Kilometres per degree of longitude at the equator, rounded down likewise.
const KM_PER_DEG_LON_EQUATOR: f64 = 111.0;
/// Beyond this latitude the widened box spans every longitude.
const POLAR_LAT: f64 = 89.0;

/// A distance larger than the diameter of any H3 cell at `resolution`, including the
/// samples a cell owns along its geodesic edges.
///
/// The largest cells have edges about 1.4× the average and a diameter about 2× their edge,
/// so roughly 2.8× the average edge; 4× leaves headroom (checked by
/// `margin_exceeds_every_cell_diameter` over every cell at resolutions 0 to 3).
pub(crate) fn cell_margin_km(resolution: u8) -> Option<f64> {
    Resolution::try_from(resolution)
        .ok()
        .map(|r| 4.0 * r.edge_length_km())
}

/// Translate a spatial predicate on the `wkb` column into a lon/lat box to push down, or
/// `None` when it is not one (or the geometry is not a lon/lat `geoarrow.wkb` literal).
pub(crate) fn translate_spatial(expr: &Expr, margin_km: f64) -> Option<[f64; 4]> {
    let Expr::ScalarFunction(ScalarFunction { func, args }) = expr else {
        return None;
    };
    let name = func.name().to_ascii_lowercase();
    if !BBOX_PREDICATES.contains(&name.as_str()) {
        return None;
    }
    let [a, b] = args.as_slice() else {
        return None;
    };
    let geometry = match (a, b) {
        (Expr::Column(c), g) | (g, Expr::Column(c)) if c.name == WKB_COLUMN => g,
        _ => return None,
    };
    let Expr::Literal(value, Some(metadata)) = geometry else {
        return None;
    };
    let metadata = metadata.inner();
    if metadata.get("ARROW:extension:name").map(String::as_str) != Some(GEOARROW_WKB_EXTENSION)
        || !is_planar_lnglat(metadata.get("ARROW:extension:metadata")?)
    {
        return None;
    }
    let bytes = match value {
        ScalarValue::Binary(Some(b))
        | ScalarValue::LargeBinary(Some(b))
        | ScalarValue::BinaryView(Some(b)) => b,
        _ => return None,
    };
    expand(wkb_envelope(bytes)?, margin_km)
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

/// Widen `[min_lon, min_lat, max_lon, max_lat]` by `margin_km` on every side, producing an
/// antimeridian-crossing box (`min_lon > max_lon`) when it wraps. `None` for envelopes
/// outside lon/lat range, which the core box test would wrap differently than the query.
pub(crate) fn expand(envelope: [f64; 4], margin_km: f64) -> Option<[f64; 4]> {
    let [min_lon, min_lat, max_lon, max_lat] = envelope;
    let in_range =
        |lon: f64, lat: f64| (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat);
    if !(in_range(min_lon, min_lat) && in_range(max_lon, max_lat)) {
        return None;
    }
    let lat_margin = margin_km / KM_PER_DEG_LAT;
    let lo_lat = (min_lat - lat_margin).max(-90.0);
    let hi_lat = (max_lat + lat_margin).min(90.0);
    let widest = lo_lat.abs().max(hi_lat.abs());
    if widest >= POLAR_LAT {
        return Some([-180.0, lo_lat, 180.0, hi_lat]);
    }
    let lon_margin = margin_km / (KM_PER_DEG_LON_EQUATOR * widest.to_radians().cos());
    if max_lon - min_lon + 2.0 * lon_margin >= 360.0 {
        return Some([-180.0, lo_lat, 180.0, hi_lat]);
    }
    let mut lo_lon = min_lon - lon_margin;
    let mut hi_lon = max_lon + lon_margin;
    if lo_lon < -180.0 {
        lo_lon += 360.0;
    }
    if hi_lon > 180.0 {
        hi_lon -= 360.0;
    }
    Some([lo_lon, lo_lat, hi_lon, hi_lat])
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
    fn expand_wraps_and_saturates() {
        let m = 110.0; // one degree of latitude
        assert_eq!(
            expand([0.0, 0.0, 1.0, 1.0], 0.0),
            Some([0.0, 0.0, 1.0, 1.0])
        );
        let [lo, s, hi, n] = expand([10.0, 10.0, 11.0, 11.0], m).unwrap();
        assert_eq!((s, n), (9.0, 12.0));
        assert!(lo < 9.0 && hi > 12.0);
        // Crossing the antimeridian yields min_lon > max_lon.
        let [lo, _, hi, _] = expand([179.5, 0.0, 179.9, 1.0], m).unwrap();
        assert!(lo > hi && lo < 179.5 && hi > -180.0);
        // Near a pole, or wider than the globe, every longitude is kept.
        assert_eq!(expand([0.0, 88.5, 1.0, 88.6], m).unwrap()[0], -180.0);
        assert_eq!(expand([-179.5, 0.0, 179.5, 1.0], m).unwrap()[2], 180.0);
        assert_eq!(expand([190.0, 0.0, 191.0, 1.0], m), None);
    }

    #[test]
    fn margin_exceeds_every_cell_diameter() {
        for res in 0..=3u8 {
            let margin = cell_margin_km(res).unwrap();
            let r = Resolution::try_from(res).unwrap();
            let widest = CellIndex::base_cells()
                .flat_map(|c| c.children(r))
                .map(|cell| {
                    let b: Vec<_> = cell.boundary().iter().copied().collect();
                    b.iter()
                        .flat_map(|p| b.iter().map(move |q| p.distance_km(*q)))
                        .fold(0.0, f64::max)
                })
                .fold(0.0, f64::max);
            assert!(
                widest * 1.25 < margin,
                "res {res}: widest {widest} km, margin {margin} km"
            );
        }
    }

    fn predicate(name: &str, args: Vec<Expr>) -> Expr {
        use datafusion::arrow::datatypes::DataType;
        use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
        let udf = create_udf(
            name,
            vec![DataType::Binary, DataType::Binary],
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
    fn spatial_predicates_translate_to_widened_boxes() {
        use datafusion::prelude::col;
        let pt = || geometry(point_wkb(-122.4, 37.7), "OGC:CRS84");
        let bbox =
            translate_spatial(&predicate("ST_Intersects", vec![col("wkb"), pt()]), 2.0).unwrap();
        assert!(bbox[0] < -122.4 && bbox[2] > -122.4 && bbox[1] < 37.7 && bbox[3] > 37.7);
        // Either argument order; any envelope-implying predicate.
        assert_eq!(
            translate_spatial(&predicate("st_within", vec![pt(), col("wkb")]), 2.0),
            Some(bbox)
        );
        for rejected in [
            predicate("st_disjoint", vec![col("wkb"), pt()]),
            predicate("st_intersects", vec![col("other"), pt()]),
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
        ] {
            assert_eq!(translate_spatial(&rejected, 2.0), None, "{rejected}");
        }
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
