//! Certified rectangle bounds for ellipsoidal transverse Mercator sources.

use super::proj_tokens::parse_opt_f64;
use super::{wrap_lon, CrsTransformer, RAD_TO_DEG};
use crate::raster::geotransform::GeoTransform;
use proj4rs::proj::Proj;

/// Lower bound on the meridional radius of curvature (and hence on the
/// normal radius, which is never smaller) of every named ellipsoid proj4rs
/// knows; the extremes are CPM (a = 6375.7 km) and Maupertius (f = 1/191),
/// both above 6.33e6 m.
///
/// With a datum shift, latitude and longitude are read on WGS84 (M >= 6.335e6 m)
/// for a point lifted off it by at most a few tens of kilometres: the
/// [`TM_MAX_HELMERT_*`](TM_MAX_HELMERT_TRANSLATION_M) limits, plus the gap
/// between the source ellipsoid and WGS84 (under 35 km for the extreme
/// named ellipsoids). Lengths grow by at most the Helmert scale. Both effects
/// stay inside this margin and the 0.1% reduction of `k0`.
const TM_MIN_RADIUS_M: f64 = 6.3e6;

/// Largest Helmert translation (metres, vector norm) a certified transverse
/// Mercator source may carry. Every named datum in proj4rs is under 800 m.
const TM_MAX_HELMERT_TRANSLATION_M: f64 = 1500.0;
/// Largest Helmert rotation (arc-seconds, per axis); named datums reach 5.3".
const TM_MAX_HELMERT_ROTATION_ARCSEC: f64 = 10.0;
/// Largest Helmert scale change (ppm); named datums reach 20.5 ppm.
const TM_MAX_HELMERT_SCALE_PPM: f64 = 50.0;

/// Whether a `+towgs84` value is a 3- or 7-parameter Helmert shift within the
/// limits above. Larger shifts can move points far from the target
/// ellipsoid's surface, where latitude and longitude change much faster than
/// the certificate's radius bound allows.
fn helmert_within_tm_limits(towgs84: &str) -> bool {
    let p: Option<Vec<f64>> = towgs84
        .split(',')
        .map(|v| v.trim().parse::<f64>().ok().filter(|x| x.is_finite()))
        .collect();
    let Some(p) = p else {
        return false;
    };
    let (shift, rot, scale) = match p.as_slice() {
        [x, y, z] => ([*x, *y, *z], [0.0; 3], 0.0),
        [x, y, z, rx, ry, rz, s] => ([*x, *y, *z], [*rx, *ry, *rz], *s),
        _ => return false,
    };
    shift.iter().map(|v| v * v).sum::<f64>().sqrt() <= TM_MAX_HELMERT_TRANSLATION_M
        && rot
            .iter()
            .all(|r| r.abs() <= TM_MAX_HELMERT_ROTATION_ARCSEC)
        && scale.abs() <= TM_MAX_HELMERT_SCALE_PPM
}

/// Certified transverse Mercator bounds hold within this many metres of
/// the central meridian, measured in projection-plane metres divided by
/// `k0` (about 22 degrees of arc). There the Poder/Engsager series used by
/// proj4rs agrees with the exact conformal map to well below a millimetre,
/// and the point scale is at least `k0`.
const TM_MAX_EASTING_M: f64 = 2.5e6;

/// Samples per rectangle edge for certified transverse Mercator bounds.
const TM_GRID: usize = 10;

/// Constants for certified bounds of an ellipsoidal transverse Mercator
/// source (`+proj=utm`, `+proj=etmerc`, exact `+proj=tmerc`).
///
/// Transverse Mercator is conformal with point scale `k >= k0`, so a
/// straight segment of length `L` source units maps to a ground curve no
/// longer than `L * to_meter / k0`. Along such a curve latitude changes by at
/// most `length / M` and longitude by at most `length / (N cos(lat))`, with
/// `M <= N` the ellipsoid's radii of curvature. Sampling the rectangle on a
/// grid and widening by these Lipschitz bounds therefore encloses its image
/// exactly, not just approximately.
#[derive(Debug, Clone, Copy)]
pub struct TmCertificate {
    /// Upper bound on ground metres per source unit: `to_meter / k0`, with
    /// `k0` reduced by 0.1% as a safety margin.
    ground_per_unit: f64,
    /// Source units to projection-plane metres.
    to_meter: f64,
    /// False easting in metres.
    x0: f64,
    /// Central scale factor.
    k0: f64,
}

impl TmCertificate {
    /// Build a certificate for `from` when it is an ellipsoidal transverse
    /// Mercator on a named ellipsoid with standard axes; `None` otherwise
    /// (spherical or Evenden/Snyder variants, custom ellipsoid sizes, swapped
    /// axes, `+towgs84` shifts beyond the Helmert limits), leaving its bounds
    /// uncertified.
    pub(super) fn new(from: &Proj, src: &str) -> Option<Self> {
        let utm = match from.projname() {
            "utm" => true,
            "etmerc" | "tmerc" => false,
            _ => return None,
        };
        // Certify only simple PROJ syntax we can interpret like proj4rs.
        // Its parser is case-sensitive, accepts quotes and whitespace around
        // '=', and keeps the FIRST duplicate. The fast-path tokenizer
        // lowercases and keeps the LAST, so cannot establish a certificate.
        let mut tokens = std::collections::HashMap::new();
        for token in src.split_whitespace() {
            let token = token.strip_prefix('+')?;
            let (key, value) = token.split_once('=').unwrap_or((token, ""));
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
                || value.contains('"')
                || (token.contains('=') && value.is_empty())
                || tokens.insert(key.to_owned(), value.to_owned()).is_some()
            {
                return None;
            }
        }
        // `tmerc` falls back to the Evenden/Snyder series on a sphere or on
        // request; custom ellipsoid sizes escape the radius bound. Parameters
        // with uppercase keys (including `+R` / `+R_A`) fail the syntax gate above.
        let blocked = ["approx", "a", "b", "rf", "f", "es", "e", "r"];
        if blocked.iter().any(|k| tokens.contains_key(*k))
            || tokens.keys().any(|k| k.starts_with("r_"))
            || tokens.get("ellps").is_some_and(|e| e == "sphere")
            || tokens.get("algo").is_some_and(|a| a != "poder_engsager")
            // Named datums imply Helmert shifts inside the limits; explicit
            // ones must be checked.
            || tokens
                .get("towgs84")
                .is_some_and(|t| !helmert_within_tm_limits(t))
            || !from.is_normalized_axis()
        {
            return None;
        }
        let (k0, x0) = if utm {
            // proj4rs fixes these for UTM regardless of +k / +x_0.
            (0.9996, 500_000.0)
        } else {
            // proj4rs 0.1.10 reads `k0` first, then `k`; it ignores `k_0`.
            // Match the effective projection, including coexisting aliases.
            let key = if tokens.contains_key("k0") { "k0" } else { "k" };
            (
                parse_opt_f64(&tokens, key, 1.0)?,
                parse_opt_f64(&tokens, "x_0", 0.0)?,
            )
        };
        let to_meter = from.to_meter();
        if !(k0 > 0.0 && k0.is_finite() && to_meter > 0.0 && to_meter.is_finite()) {
            return None;
        }
        Some(Self {
            ground_per_unit: to_meter / (0.999 * k0),
            to_meter,
            x0,
            k0,
        })
    }
}

impl CrsTransformer {
    /// Certified bounds for a transverse Mercator source; see
    /// [`TmCertificate`]. Returns the whole globe when the rectangle leaves
    /// the certified band around the central meridian or any sample fails, so
    /// the result is always safe for rejection.
    pub(super) fn tm_rect_bounds(
        &self,
        tm: &TmCertificate,
        gt: &GeoTransform,
        col: f64,
        row: f64,
        width: f64,
        height: f64,
    ) -> [f64; 4] {
        const GLOBE: [f64; 4] = [-180.0, -90.0, 180.0, 90.0];
        const N: usize = TM_GRID;
        // Easting is affine in pixel space, so the corners bound it.
        for (c, r) in [(0.0, 0.0), (width, 0.0), (0.0, height), (width, height)] {
            let (x, _) = gt.pixel_to_coord(col + c, row + r);
            let off = (x * tm.to_meter - tm.x0).abs() / tm.k0;
            if off.is_nan() || off > TM_MAX_EASTING_M {
                return GLOBE;
            }
        }

        let mut lons = [[0.0f64; N + 1]; N + 1];
        let mut lats = [[0.0f64; N + 1]; N + 1];
        let (mut lat_min, mut lat_max) = (f64::INFINITY, f64::NEG_INFINITY);
        for j in 0..=N {
            for i in 0..=N {
                let (x, y) = gt.pixel_to_coord(
                    col + width * i as f64 / N as f64,
                    row + height * j as f64 / N as f64,
                );
                match self.transform_point(x, y) {
                    Ok((lon, lat)) if lon.is_finite() && lat.is_finite() => {
                        lons[j][i] = lon;
                        lats[j][i] = lat;
                        lat_min = lat_min.min(lat);
                        lat_max = lat_max.max(lat);
                    }
                    _ => return GLOBE,
                }
            }
        }

        // Grid cell edge vectors in source units. Every point of a cell lies
        // within (|u| + |v|) / 2 of one of its corners, and adjacent samples
        // are at most max(|u|, |v|) apart.
        let u = (gt.a * width / N as f64).hypot(gt.d * width / N as f64);
        let v = (gt.b * height / N as f64).hypot(gt.e * height / N as f64);
        let reach_m = 0.5 * (u + v) * tm.ground_per_unit;
        let edge_m = u.max(v) * tm.ground_per_unit;
        // Absolute slack for floating-point error in the projection itself.
        const EPS_DEG: f64 = 1e-9;

        let dlat = reach_m / TM_MIN_RADIUS_M * RAD_TO_DEG + EPS_DEG;
        let south = (lat_min - dlat).max(-90.0);
        let north = (lat_max + dlat).min(90.0);
        let abs_lat = north.max(-south);
        if abs_lat >= 89.9 {
            return [-180.0, south, 180.0, north];
        }
        let lon_scale = RAD_TO_DEG / (TM_MIN_RADIUS_M * abs_lat.to_radians().cos());
        let dlon = reach_m * lon_scale + EPS_DEG;
        let dlon_edge = edge_m * lon_scale + EPS_DEG;
        if dlon_edge >= 90.0 {
            return [-180.0, south, 180.0, north];
        }

        // Lift sampled longitudes onto one continuous branch: each sample is
        // unwrapped against an already-lifted neighbour, then every adjacent
        // pair is checked against the Lipschitz bound, which proves the lift
        // follows the continuous image rather than a wrapped copy.
        let unwrap = |d: f64| d - 360.0 * (d / 360.0).round();
        let mut lifted = [[0.0f64; N + 1]; N + 1];
        for j in 0..=N {
            for i in 0..=N {
                lifted[j][i] = match (i, j) {
                    (0, 0) => wrap_lon(lons[0][0]),
                    (0, _) => lifted[j - 1][0] + unwrap(lons[j][0] - lons[j - 1][0]),
                    _ => lifted[j][i - 1] + unwrap(lons[j][i] - lons[j][i - 1]),
                };
            }
        }
        let (mut lon_min, mut lon_max) = (f64::INFINITY, f64::NEG_INFINITY);
        for j in 0..=N {
            for i in 0..=N {
                let l = lifted[j][i];
                let far = |o: f64| (l - o).abs() > dlon_edge;
                if (i < N && far(lifted[j][i + 1])) || (j < N && far(lifted[j + 1][i])) {
                    return [-180.0, south, 180.0, north];
                }
                lon_min = lon_min.min(l);
                lon_max = lon_max.max(l);
            }
        }
        let (west, east) = (lon_min - dlon, lon_max + dlon);
        // Rectangles stay non-crossing, as in the sampled path.
        if west < -180.0 || east > 180.0 || east - west > 180.0 {
            return [-180.0, south, 180.0, north];
        }
        [west, south, east, north]
    }
}
