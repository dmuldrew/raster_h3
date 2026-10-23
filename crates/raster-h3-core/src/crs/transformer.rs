//! Coordinate Reference System (CRS) transformations to WGS84.
//!
//! This module implements a 3-tier CRS transformation hierarchy:
//!
//! - **Tier 1: `Wgs84Identity`** — Zero-cost passthrough for EPSG:4326/4269 data.
//! - **Tier 2: `WebMercatorFast` and `AlbersConic`** — Fast analytical transformations with direct formulas (no PROJ4 overhead).
//! - **Tier 3: `Proj4`** — General-purpose fallback using `proj4rs` for arbitrary CRS.

use crate::error::{RasterH3Error, Result};
use crate::raster::geotransform::GeoTransform;
use proj4rs::proj::Proj;

const WGS84_A: f64 = 6378137.0; // WGS84 semi-major axis in meters
const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

/// Spherical Web Mercator (EPSG:3857) easting to longitude in degrees.
#[inline(always)]
pub fn web_mercator_lon(x: f64) -> f64 {
    (x / WGS84_A) * RAD_TO_DEG
}

/// Spherical Web Mercator (EPSG:3857) northing to latitude in degrees.
#[inline(always)]
pub fn web_mercator_lat(y: f64) -> f64 {
    (2.0 * (y / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2) * RAD_TO_DEG
}

/// Normalize longitude to [-180.0, 180.0) degrees
#[inline]
pub fn wrap_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// Precomputed constants for analytical, closed-form inverse Albers Equal Area Conic projection
#[derive(Debug, Clone, Copy)]
pub struct AlbersConicFast {
    /// Latitude of origin in radians.
    pub lat_origin_rad: f64,
    /// Longitude of central meridian in radians.
    pub lon_origin_rad: f64,
    /// False easting in meters.
    pub x_0: f64,
    /// False northing in meters.
    pub y_0: f64,
    /// Cone constant.
    pub n: f64,
    /// Albers constant C.
    pub c: f64,
    /// Polar distance at origin.
    pub rho0: f64,
    /// First eccentricity of the ellipsoid.
    pub e: f64,
    /// Eccentricity squared.
    pub e2: f64,
    /// Semi-major axis in meters.
    pub a: f64,
    /// Authalic latitude parameter at poles.
    pub qp: f64,
    /// Precomputed 1 / n.
    pub inv_n: f64,
    /// Precomputed (n / a)^2.
    pub n_over_a_sq: f64,
    /// Precomputed 1 / qp.
    pub inv_qp: f64,
    /// Horner coefficients for 4-term authalic-to-geodetic latitude series in cos(2*beta).
    pub apa: [f64; 4],
}

/// Tokenize a PROJ string into key-value pairs.
pub(crate) fn tokenize_proj_string(src: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for token in src.split_whitespace() {
        let token = token.trim().trim_start_matches('+');
        if token.is_empty() {
            continue;
        }
        if let Some((k, v)) = token.split_once('=') {
            map.insert(k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
        } else {
            map.insert(token.to_ascii_lowercase(), String::new());
        }
    }
    map
}

/// Keys every fast path tolerates because they carry no geometric meaning.
const NEUTRAL_KEYS: &[&str] = &["proj", "no_defs", "wktext", "type"];

/// True when every token in `tokens` is understood by a fast path that
/// accepts `keys` in addition to [`NEUTRAL_KEYS`]. Linear units must be
/// metres, any prime meridian Greenwich and any datum shift zero; everything
/// else (`+axis`, `+pm`, `+geoc`, `+lon_wrap`, `+over`, foot units, Helmert
/// shifts, ...) routes to proj4rs, which honours those parameters, rather
/// than being silently dropped.
fn fast_path_accepts(tokens: &std::collections::HashMap<String, String>, keys: &[&str]) -> bool {
    tokens.iter().all(|(k, v)| match k.as_str() {
        "towgs84" => is_all_zero_towgs84(v),
        "nadgrids" => v == "@null" || v.is_empty(),
        "units" => v == "m",
        "to_meter" => v.parse::<f64>().is_ok_and(|m| m == 1.0),
        "pm" => v == "greenwich" || v.parse::<f64>().is_ok_and(|d| d == 0.0),
        k => NEUTRAL_KEYS.contains(&k) || keys.contains(&k),
    })
}

/// Parse an optional numeric token; returns `Some(default)` when absent and
/// `None` when present but not a finite `f64`, so malformed numbers never
/// silently fall back to defaults on a fast path.
fn parse_opt_f64(
    tokens: &std::collections::HashMap<String, String>,
    key: &str,
    default: f64,
) -> Option<f64> {
    match tokens.get(key) {
        Some(v) => v.parse::<f64>().ok().filter(|x| x.is_finite()),
        None => Some(default),
    }
}

/// Helper to check if a +towgs84 parameter list is effectively all-zero (e.g., "0,0,0" or "0,0,0,0,0,0,0").
fn is_all_zero_towgs84(val: &str) -> bool {
    let mut count = 0;
    for part in val.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<f64>() {
            Ok(num) => {
                if num != 0.0 {
                    return false;
                }
                count += 1;
            }
            Err(_) => return false,
        }
    }
    count > 0
}

impl AlbersConicFast {
    /// Initialize with standard 2 parallels and origin (in degrees) on GRS80/WGS84 spheroid
    pub fn new(lat1_deg: f64, lat2_deg: f64, lat0_deg: f64, lon0_deg: f64) -> Self {
        Self::with_offsets(lat1_deg, lat2_deg, lat0_deg, lon0_deg, 0.0, 0.0)
    }

    /// Initialize with standard parallels, origin, and false easting/northing offsets (in meters)
    pub fn with_offsets(
        lat1_deg: f64,
        lat2_deg: f64,
        lat0_deg: f64,
        lon0_deg: f64,
        x_0: f64,
        y_0: f64,
    ) -> Self {
        let a: f64 = 6378137.0; // GRS80/WGS84 semi-major axis
        let f: f64 = 1.0 / 298.257222101; // GRS80 flattening
        let e2: f64 = 2.0 * f - f * f;
        let e: f64 = e2.sqrt();

        let deg_to_rad: f64 = std::f64::consts::PI / 180.0;
        let phi1 = lat1_deg * deg_to_rad;
        let phi2 = lat2_deg * deg_to_rad;
        let phi0 = lat0_deg * deg_to_rad;
        let lam0 = lon0_deg * deg_to_rad;

        let m = |phi: f64| -> f64 {
            let sin_phi = phi.sin();
            phi.cos() / (1.0 - e2 * sin_phi * sin_phi).sqrt()
        };

        let q = |phi: f64| -> f64 {
            let sin_phi = phi.sin();
            let e_sin = e * sin_phi;
            let ratio: f64 = (1.0 - e_sin) / (1.0 + e_sin);
            (1.0 - e2) * (sin_phi / (1.0 - e_sin * e_sin) - (1.0 / (2.0 * e)) * ratio.ln())
        };

        let m1 = m(phi1);
        let m2 = m(phi2);
        let q1 = q(phi1);
        let q2 = q(phi2);
        let q0 = q(phi0);
        let qp = q(std::f64::consts::FRAC_PI_2);

        let n = if (phi1 - phi2).abs() < 1e-10 {
            phi1.sin()
        } else {
            (m1 * m1 - m2 * m2) / (q2 - q1)
        };

        let c = m1 * m1 + n * q1;
        let rho0 = a * (c - n * q0).max(0.0).sqrt() / n;

        let e4 = e2 * e2;
        let e6 = e4 * e2;
        let e8 = e4 * e4;
        // 4-term authalic-to-geodetic latitude series (Snyder 1987 eq. 3-16 + e^8 term):
        // phi = beta + p1*sin(2*beta) + p2*sin(4*beta) + p3*sin(6*beta) + p4*sin(8*beta)
        let p1 = (1.0 / 3.0) * e2
            + (31.0 / 180.0) * e4
            + (517.0 / 5040.0) * e6
            + (120389.0 / 1814400.0) * e8;
        let p2 = (23.0 / 360.0) * e4 + (251.0 / 3780.0) * e6 + (102283.0 / 1814400.0) * e8;
        let p3 = (761.0 / 45360.0) * e6 + (47561.0 / 1814400.0) * e8;
        let p4 = (6059.0 / 1209600.0) * e8;
        // Chebyshev Horner form in c2 = cos(2*beta) = 1 - 2*sin^2(beta):
        // sum = sin(2*beta) * (apa[0] + c2 * (apa[1] + c2 * (apa[2] + c2 * apa[3])))
        let apa = [p1 - p3, 2.0 * p2 - 4.0 * p4, 4.0 * p3, 8.0 * p4];
        let n_over_a = n / a;

        Self {
            lat_origin_rad: phi0,
            lon_origin_rad: lam0,
            x_0,
            y_0,
            n,
            c,
            rho0,
            e,
            e2,
            a,
            qp,
            inv_n: 1.0 / n,
            n_over_a_sq: n_over_a * n_over_a,
            inv_qp: 1.0 / qp,
            apa,
        }
    }

    /// Preconfigured for EPSG:5070 (USA_Contiguous_Albers_Equal_Area_Conic)
    pub fn epsg_5070() -> Self {
        Self::new(29.5, 45.5, 23.0, -96.0)
    }

    /// Parse PROJ string parameters for an Albers Equal Area projection (+proj=aea) from token map
    pub fn from_proj_tokens(tokens: &std::collections::HashMap<String, String>) -> Option<Self> {
        if tokens.get("proj").map(|s| s.as_str()) != Some("aea") {
            return None;
        }
        let keys = [
            "lat_1", "lat_2", "lat_0", "lon_0", "x_0", "y_0", "ellps", "datum", "a", "rf", "f",
        ];
        if !fast_path_accepts(tokens, &keys) {
            return None;
        }

        // Must NOT have nadgrids (unless @null or empty)
        if let Some(nadgrids) = tokens.get("nadgrids") {
            if nadgrids != "@null" && !nadgrids.is_empty() {
                return None;
            }
        }

        // Must NOT have towgs84 (unless all-zero)
        if let Some(towgs84) = tokens.get("towgs84") {
            if !is_all_zero_towgs84(towgs84) {
                return None;
            }
        }

        // Accept the fast path only when the ellipsoid is GRS80 or WGS84
        // (by ellps=, datum=NAD83|WGS84, or a≈6378137 && rf≈298.257).
        let ellps = tokens.get("ellps").map(|s| s.as_str());
        let datum = tokens.get("datum").map(|s| s.as_str());

        // If an explicit a, rf, or f is present but not a valid finite number, reject fast path
        if tokens
            .get("a")
            .is_some_and(|v| v.parse::<f64>().ok().is_none_or(|x| !x.is_finite()))
            || tokens
                .get("rf")
                .is_some_and(|v| v.parse::<f64>().ok().is_none_or(|x| !x.is_finite()))
            || tokens.get("f").is_some_and(|v| {
                v.parse::<f64>()
                    .ok()
                    .is_none_or(|x| !x.is_finite() || x == 0.0)
            })
        {
            return None;
        }

        let a_val = tokens.get("a").and_then(|v| v.parse::<f64>().ok());
        let rf_val = tokens
            .get("rf")
            .and_then(|v| v.parse::<f64>().ok())
            .or_else(|| {
                tokens
                    .get("f")
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|f| 1.0 / f)
            });

        // If an explicit incompatible ellps or datum is present, reject fast path
        if let Some(e) = ellps {
            if e != "grs80" && e != "wgs84" {
                return None;
            }
        }
        if let Some(d) = datum {
            if d != "nad83" && d != "wgs84" {
                return None;
            }
        }

        let is_grs80_or_wgs84_ellps = matches!(ellps, Some("grs80") | Some("wgs84"));

        let is_grs80_or_wgs84_datum = matches!(datum, Some("nad83") | Some("wgs84"));

        let is_grs80_or_wgs84_params = match (a_val, rf_val) {
            (Some(a), Some(rf)) => (a - 6378137.0).abs() < 1.0 && (rf - 298.257).abs() < 0.02,
            _ => false,
        };

        let is_valid_ellipsoid =
            is_grs80_or_wgs84_ellps || is_grs80_or_wgs84_datum || is_grs80_or_wgs84_params;

        if !is_valid_ellipsoid {
            return None;
        }

        let l1 = tokens
            .get("lat_1")?
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite() && (-90.0..=90.0).contains(x))?;
        let l2 = match tokens.get("lat_2") {
            Some(v) => v
                .parse::<f64>()
                .ok()
                .filter(|x| x.is_finite() && (-90.0..=90.0).contains(x))?,
            None => l1,
        };
        let l0_val = parse_opt_f64(tokens, "lat_0", 0.0).filter(|x| (-90.0..=90.0).contains(x))?;
        let ln0 = tokens
            .get("lon_0")?
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())?;
        let x_0 = parse_opt_f64(tokens, "x_0", 0.0)?;
        let y_0 = parse_opt_f64(tokens, "y_0", 0.0)?;

        let albers = Self::with_offsets(l1, l2, l0_val, ln0, x_0, y_0);
        // Symmetric parallels (phi1 == -phi2) yield n == 0 (cylindrical limit);
        // route that degenerate case to proj4rs rather than dividing by zero.
        (albers.n.abs() >= 1e-10 && albers.rho0.is_finite()).then_some(albers)
    }

    /// Parse PROJ string parameters for an Albers Equal Area projection (+proj=aea)
    pub fn from_proj_string(src: &str) -> Option<Self> {
        let tokens = tokenize_proj_string(src);
        Self::from_proj_tokens(&tokens)
    }

    /// Forward transformation from (lon, lat) in degrees to projected (x, y).
    pub fn project_point(&self, lon_deg: f64, lat_deg: f64) -> (f64, f64) {
        let phi = lat_deg / RAD_TO_DEG;
        let sin_phi = phi.sin();
        let e_sin = self.e * sin_phi;
        let q = (1.0 - self.e2)
            * (sin_phi / (1.0 - e_sin * e_sin)
                - (1.0 / (2.0 * self.e)) * ((1.0 - e_sin) / (1.0 + e_sin)).ln());
        let rho = self.a * (self.c - self.n * q).max(0.0).sqrt() / self.n;
        let d_lam = wrap_lon(lon_deg - self.lon_origin_rad * RAD_TO_DEG) / RAD_TO_DEG;
        let theta = self.n * d_lam;
        (
            self.x_0 + rho * theta.sin(),
            self.y_0 + self.rho0 - rho * theta.cos(),
        )
    }

    /// Analytical inverse transformation from projected (x, y) to (lon, lat) in WGS84 degrees
    #[inline(always)]
    pub fn transform_point(&self, x: f64, y: f64) -> (f64, f64) {
        let y_p = self.rho0 - (y - self.y_0);
        self.transform_with_yp(x, y_p, y_p * y_p)
    }

    /// Analytical inverse transformation when `y_p = rho0 - (y - y_0)` and `y_p_sq = y_p * y_p`
    /// have already been hoisted for a constant-Y scanline row.
    #[inline(always)]
    pub fn transform_with_yp(&self, x: f64, y_p: f64, y_p_sq: f64) -> (f64, f64) {
        let x_p = x - self.x_0;
        let rho_sq = x_p.mul_add(x_p, y_p_sq);
        let theta = if self.n >= 0.0 {
            x_p.atan2(y_p)
        } else {
            (-x_p).atan2(-y_p)
        };

        let lon_rad = self.lon_origin_rad + theta * self.inv_n;
        let q = (self.c - rho_sq * self.n_over_a_sq) * self.inv_n;

        // 4-term Chebyshev Horner series from authalic sin(beta) = q / qp to geodetic phi
        let sin_beta = (q * self.inv_qp).clamp(-1.0, 1.0);
        let sin2_beta = sin_beta * sin_beta;
        let cos_beta = (1.0 - sin2_beta).sqrt();
        let s2 = 2.0 * sin_beta * cos_beta;
        let c2 = 1.0 - 2.0 * sin2_beta;
        let poly = self.apa[3]
            .mul_add(c2, self.apa[2])
            .mul_add(c2, self.apa[1])
            .mul_add(c2, self.apa[0]);
        let phi = s2.mul_add(poly, sin_beta.asin());

        let lon_deg = lon_rad * RAD_TO_DEG;
        let lat_deg = phi * RAD_TO_DEG;
        (lon_deg, lat_deg)
    }
}

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
    fn new(from: &Proj, src: &str) -> Option<Self> {
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

///
/// proj4rs normalizes axes but skips the prime-meridian shift for geographic
/// sources, so the shift cannot simply be added to the raw input: it belongs
/// to canonical longitude, after axis normalization and before any datum
/// transformation, exactly as in PROJ. The wrapper therefore performs both
/// steps itself and hands proj4rs a canonical, Greenwich definition.
#[derive(Debug, Clone, Copy)]
pub struct GeoFrame {
    axis: proj4rs::proj::Axis,
    /// Prime meridian, degrees east of Greenwich.
    pm: f64,
}

impl GeoFrame {
    /// Source-order (a, b) to canonical (east, north).
    fn normalize(&self, a: f64, b: f64) -> (f64, f64) {
        let (mut e, mut n) = (a, b);
        for (axe, v) in self.axis.iter().zip([a, b]) {
            match axe {
                b'e' => e = v,
                b'w' => e = -v,
                b'n' => n = v,
                b's' => n = -v,
                _ => {}
            }
        }
        (e, n)
    }

    /// Canonical (east, north) back to source order.
    fn denormalize(&self, e: f64, n: f64) -> (f64, f64) {
        let mut out = [e, n];
        for (slot, axe) in out.iter_mut().zip(self.axis.iter()) {
            *slot = match axe {
                b'e' => e,
                b'w' => -e,
                b'n' => n,
                b's' => -n,
                _ => *slot,
            };
        }
        (out[0], out[1])
    }
}

/// Marks a [`CrsTransformer::Proj4`] as one of the built-in EPSG:3413/3031
/// polar stereographic definitions, whose rectangle bounds come from
/// radial/angular extrema (see [`CrsTransformer::bounds_certified`]). It can
/// only be created inside this crate, so a caller cannot certify an arbitrary
/// PROJ string.
#[derive(Debug, Clone, Copy)]
pub struct PolarCertificate {
    _private: (),
}

/// High-performance CRS to WGS84 coordinate transformer
#[derive(Clone)]
pub enum CrsTransformer {
    /// Native WGS84 (EPSG:4326) - Zero math, zero overhead
    Wgs84Identity,
    /// Fast analytical Web Mercator (EPSG:3857 / EPSG:900913)
    WebMercatorFast,
    /// Fast analytical Albers Equal Area Conic (EPSG:5070 CONUS Albers)
    AlbersConic(AlbersConicFast),
    /// Pure Rust PROJ4 transformation for arbitrary projections. Boxed so the
    /// common fast-path variants stay small.
    Proj4 {
        from: Box<Proj>,
        to: Box<Proj>,
        /// Axis order and prime meridian of a geographic source, applied by
        /// this wrapper; `from` is then built without `+axis`/`+pm`.
        geo: Option<GeoFrame>,

        /// Certified-bounds constants when the source is transverse Mercator.
        tm: Option<TmCertificate>,

        /// Analytic radial/angular bounds for the built-in EPSG:3413/3031
        /// definitions only. Arbitrary PROJ strings keep failing open.
        polar: Option<PolarCertificate>,
    },
}

impl CrsTransformer {
    /// Create a transformer from an optional EPSG code or PROJ string
    pub fn from_crs_or_epsg(epsg: Option<u32>, proj_str: Option<&str>) -> Result<Self> {
        let parsed_epsg = proj_str.and_then(|s| {
            let s = s.trim();
            if let Some(rest) = s.strip_prefix("EPSG:").or_else(|| s.strip_prefix("epsg:")) {
                rest.trim().parse::<u32>().ok()
            } else if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
                s.parse::<u32>().ok()
            } else {
                None
            }
        });

        let effective_epsg = epsg.or(parsed_epsg);

        if let Some(code) = effective_epsg {
            return Self::from_epsg_code(code);
        }

        if let Some(s) = proj_str {
            return Self::from_proj_string(s);
        }

        Err(RasterH3Error::CrsNotDetected(
            "No CRS detected in raster metadata. A CRS must be specified explicitly (e.g. crs := 'EPSG:4326', crs := 'EPSG:5070').".into(),
        ))
    }

    /// Resolve an EPSG code to a fast-path or Proj4 transformer.
    pub fn from_epsg_code(code: u32) -> Result<Self> {
        match code {
            // EPSG:4326 is WGS84 native.
            // EPSG:4269 is NAD83 (GRS80 ellipsoid). Treated as Wgs84Identity, accepting the
            // ~1–2 m continental difference between NAD83 and WGS84 as proj4rs does not
            // support grid-shift datum transformations.
            4326 | 4269 => Ok(Self::Wgs84Identity),
            3857 | 900913 | 3785 => Ok(Self::WebMercatorFast),
            5070 => Ok(Self::AlbersConic(AlbersConicFast::epsg_5070())),
            3338 => Ok(Self::AlbersConic(AlbersConicFast::new(
                55.0, 65.0, 50.0, -154.0,
            ))),
            32601..=32660 => {
                let zone = code - 32600;
                let p_str = format!("+proj=utm +zone={} +datum=WGS84 +units=m +no_defs", zone);
                Self::from_proj_string(&p_str)
            }
            32701..=32760 => {
                let zone = code - 32700;
                let p_str = format!(
                    "+proj=utm +zone={} +south +datum=WGS84 +units=m +no_defs",
                    zone
                );
                Self::from_proj_string(&p_str)
            }
            3413 => Self::polar_stereographic(
                "+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs",
            ),
            3031 => Self::polar_stereographic(
                "+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs",
            ),
            // Codes below carry a Helmert datum shift chosen from the EPSG
            // registry (accuracy as published there); see
            // docs/crs-and-projection.md.
            // British National Grid: OSGB36 to WGS 84 (6), EPSG:1314, 2 m.
            27700 => Self::from_proj_string(
                "+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 \
                 +ellps=airy +towgs84=446.448,-125.157,542.06,0.15,0.247,0.842,-20.489 \
                 +units=m +no_defs",
            ),
            // Amersfoort / RD New: Amersfoort to WGS 84 (4), EPSG:4833, 1 m.
            // Published as coordinate frame; rotations negated for +towgs84.
            28992 => Self::from_proj_string(
                "+proj=sterea +lat_0=52.1561605555556 +lon_0=5.38763888888889 +k=0.9999079 \
                 +x_0=155000 +y_0=463000 +ellps=bessel \
                 +towgs84=565.4171,50.3319,465.5524,-0.398957388243134,0.343987817378283,-1.87740163998045,4.0725 \
                 +units=m +no_defs",
            ),
            // DHDN / 3-degree Gauss-Kruger zones 2-5: DHDN to WGS 84 (2),
            // EPSG:1777, 3 m (former West Germany).
            31466..=31469 => {
                let zone = code - 31464;
                Self::from_proj_string(&format!(
                    "+proj=tmerc +lat_0=0 +lon_0={} +k=1 +x_0={} +y_0=0 +ellps=bessel \
                     +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7 +units=m +no_defs",
                    zone * 3,
                    zone * 1_000_000 + 500_000
                ))
            }
            // ED50 / UTM zones 28-38: ED50 to WGS 84 (1), EPSG:1133, 10 m.
            23028..=23038 => Self::from_proj_string(&format!(
                "+proj=utm +zone={} +ellps=intl +towgs84=-87,-98,-121 +units=m +no_defs",
                code - 23000
            )),
            // ETRS89 is treated as WGS84 (sub-metre, time-dependent offset).
            // RGF93 / Lambert-93.
            2154 => Self::from_proj_string(
                "+proj=lcc +lat_0=46.5 +lon_0=3 +lat_1=49 +lat_2=44 +x_0=700000 +y_0=6600000 \
                 +ellps=GRS80 +units=m +no_defs",
            ),
            // ETRS89-extended / LAEA Europe.
            3035 => Self::from_proj_string(
                "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80 +units=m +no_defs",
            ),
            // ETRS89 / UTM zones 28-38.
            25828..=25838 => Self::from_proj_string(&format!(
                "+proj=utm +zone={} +ellps=GRS80 +units=m +no_defs",
                code - 25800
            )),
            // GDA2020 / MGA zones 46-59. GDA2020 is treated as WGS84 under the
            // same metre-level datum approximation as EPSG:4269.
            7846..=7859 => {
                let zone = code - 7800;
                let p_str = format!("+proj=utm +zone={} +south +ellps=GRS80 +units=m +no_defs", zone);
                Self::from_proj_string(&p_str)
            }
            // WGS 84 / NSIDC EASE-Grid 2.0 Global.
            6933 => Self::from_proj_string(
                "+proj=cea +lat_ts=30 +lon_0=0 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs",
            ),
            // proj4rs is built without an EPSG database, so any other code
            // must be supplied as a PROJ string.
            _ => Err(RasterH3Error::UnsupportedEpsg {
                code,
                detail: format!(
                    "Unsupported or unrecognized EPSG code: {}. Supply the CRS as a PROJ string instead.",
                    code
                ),
            }),
        }
    }

    /// Construct from arbitrary PROJ string to WGS84
    pub fn from_proj_string(src_proj: &str) -> Result<Self> {
        let trimmed = src_proj.trim();

        // Plain EPSG:NNNN or bare integer check
        let parsed_epsg = if let Some(rest) = trimmed
            .strip_prefix("EPSG:")
            .or_else(|| trimmed.strip_prefix("epsg:"))
        {
            rest.trim().parse::<u32>().ok()
        } else if trimmed.chars().all(|c| c.is_ascii_digit()) && !trimmed.is_empty() {
            trimmed.parse::<u32>().ok()
        } else {
            None
        };

        if let Some(code) = parsed_epsg {
            return Self::from_epsg_code(code);
        }

        let tokens = tokenize_proj_string(trimmed);

        // Helmert shifts (`+towgs84`, explicit or implied by a named datum
        // such as `+datum=OSGB36`) are applied by proj4rs. Grid shifts are
        // not: no grid files are bundled.
        if let Some(nadgrids) = tokens.get("nadgrids") {
            if nadgrids != "@null" && !nadgrids.is_empty() {
                return Err(RasterH3Error::CrsError(format!(
                    "Datum transformations via grid shifts (+nadgrids={}) are not supported",
                    nadgrids
                )));
            }
        }
        if tokens.get("datum").map(|s| s.as_str()) == Some("nad27") {
            return Err(RasterH3Error::CrsError(
                "+datum=NAD27 requires NADCON/NTv2 grid shifts, which are not supported; \
                 supply an explicit +ellps=clrk66 +towgs84=... approximation instead"
                    .into(),
            ));
        }

        // +init=epsg:NNNN -> treat as EPSG code
        if let Some(init_val) = tokens.get("init") {
            if let Some(code_str) = init_val.strip_prefix("epsg:") {
                if let Ok(code) = code_str.parse::<u32>() {
                    return Self::from_epsg_code(code);
                }
            }
        }

        if let Some(proj) = tokens.get("proj") {
            match proj.as_str() {
                "longlat" | "latlong" => {
                    let valid_datum = matches!(
                        tokens.get("datum").map(|s| s.as_str()),
                        None | Some("wgs84") | Some("grs80") | Some("nad83")
                    );
                    let valid_ellps = matches!(
                        tokens.get("ellps").map(|s| s.as_str()),
                        None | Some("wgs84") | Some("grs80")
                    );
                    let valid_a = tokens.get("a").is_none_or(|v| {
                        v.parse::<f64>()
                            .is_ok_and(|a| a.is_finite() && (a - WGS84_A).abs() < 1.0)
                    });
                    let valid_rf = tokens.get("rf").is_none_or(|v| {
                        v.parse::<f64>()
                            .is_ok_and(|rf| rf.is_finite() && (rf - 298.257).abs() < 0.01)
                    });

                    let keys = ["datum", "ellps", "a", "rf"];
                    if valid_datum
                        && valid_ellps
                        && valid_a
                        && valid_rf
                        && fast_path_accepts(&tokens, &keys)
                    {
                        return Ok(Self::Wgs84Identity);
                    }
                }
                "merc" => {
                    let a_val = tokens.get("a").and_then(|v| v.parse::<f64>().ok());
                    let b_val = tokens.get("b").and_then(|v| v.parse::<f64>().ok());
                    let rf_val = tokens.get("rf");
                    let f_val = tokens.get("f");
                    let ellps_val = tokens.get("ellps").map(|s| s.as_str());

                    let a_is_6378137 = a_val.is_some_and(|a| (a - 6378137.0).abs() < 1e-3);
                    let has_b = tokens.contains_key("b");
                    let b_is_6378137 =
                        !has_b || b_val.is_some_and(|b| (b - 6378137.0).abs() < 1e-3);
                    let absent_rf_f_b = rf_val.is_none() && f_val.is_none() && !has_b;
                    let ellps_sphere_or_absent = matches!(ellps_val, None | Some("sphere"));

                    let is_sphere_6378137 = a_is_6378137
                        && ((has_b && b_is_6378137) || (absent_rf_f_b && ellps_sphere_or_absent));

                    let k_opt = match (tokens.get("k"), tokens.get("k_0")) {
                        (Some(v), None) | (None, Some(v)) => {
                            v.parse::<f64>().ok().filter(|x| x.is_finite())
                        }
                        (Some(v1), Some(v2)) => {
                            let k1 = v1.parse::<f64>().ok().filter(|x| x.is_finite());
                            let k2 = v2.parse::<f64>().ok().filter(|x| x.is_finite());
                            k1.zip(k2)
                                .and_then(|(a, b)| ((a - b).abs() < 1e-12).then_some(a))
                        }
                        (None, None) => Some(1.0),
                    };

                    let is_standard_merc = match (
                        parse_opt_f64(&tokens, "lon_0", 0.0),
                        parse_opt_f64(&tokens, "x_0", 0.0),
                        parse_opt_f64(&tokens, "y_0", 0.0),
                        k_opt,
                        parse_opt_f64(&tokens, "lat_ts", 0.0),
                    ) {
                        (Some(lon_0), Some(x_0), Some(y_0), Some(k), Some(lat_ts)) => {
                            lon_0.abs() < 1e-6
                                && x_0.abs() < 1e-6
                                && y_0.abs() < 1e-6
                                && (k - 1.0).abs() < 1e-6
                                && lat_ts.abs() < 1e-6
                        }
                        _ => false,
                    };

                    let keys = [
                        "a", "b", "ellps", "lon_0", "x_0", "y_0", "k", "k_0", "lat_ts",
                    ];
                    if is_sphere_6378137 && is_standard_merc && fast_path_accepts(&tokens, &keys) {
                        return Ok(Self::WebMercatorFast);
                    }
                }
                "aea" => {
                    if let Some(albers) = AlbersConicFast::from_proj_tokens(&tokens) {
                        return Ok(Self::AlbersConic(albers));
                    }
                }
                _ => {}
            }
        }

        let parse = |def: &str| {
            Proj::from_proj_string(def).map_err(|e| {
                RasterH3Error::CrsError(format!(
                    "Failed to parse source PROJ string '{}': {:?}",
                    src_proj, e
                ))
            })
        };
        let mut from = parse(src_proj)?;
        let mut geo = None;
        if from.is_latlong() {
            geo = Some(GeoFrame {
                axis: *from.axis(),
                pm: from.from_greenwich(),
            });
            // Canonical, Greenwich copy; the wrapper applies axis and pm.
            let canonical: Vec<&str> = src_proj
                .split_whitespace()
                .filter(|t| {
                    let key = t.trim_start_matches('+').split('=').next().unwrap_or("");
                    !key.eq_ignore_ascii_case("axis") && !key.eq_ignore_ascii_case("pm")
                })
                .collect();
            from = parse(&canonical.join(" "))?;
        }
        // proj4rs adds a degree-valued prime meridian to radian longitudes on
        // projected sources; refuse rather than shift by the wrong amount.
        if !from.is_latlong() && from.from_greenwich() != 0.0 {
            return Err(RasterH3Error::CrsError(format!(
                "Non-Greenwich prime meridians on projected CRSs are not supported: '{}'",
                src_proj
            )));
        }
        let to = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").map_err(|e| {
            RasterH3Error::CrsError(format!(
                "Failed to initialize WGS84 target projection: {:?}",
                e
            ))
        })?;

        let tm = TmCertificate::new(&from, src_proj);
        Ok(Self::Proj4 {
            from: Box::new(from),
            to: Box::new(to),
            geo,
            tm,
            polar: None,
        })
    }

    /// A built-in WGS84 polar stereographic definition centred on the pole
    /// with no false origin, the geometry [`Self::polar_rect_bounds`] assumes.
    fn polar_stereographic(p_str: &str) -> Result<Self> {
        let mut transformer = Self::from_proj_string(p_str)?;
        if let Self::Proj4 { polar, .. } = &mut transformer {
            *polar = Some(PolarCertificate { _private: () });
        }
        Ok(transformer)
    }

    /// Transform a single (x, y) point to (lon, lat) in WGS84 degrees
    #[inline(always)]
    pub fn transform_point(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        match self {
            Self::Wgs84Identity => Ok((x, y)),
            Self::WebMercatorFast => Ok((web_mercator_lon(x), web_mercator_lat(y))),
            Self::AlbersConic(albers) => Ok(albers.transform_point(x, y)),
            Self::Proj4 { from, to, geo, .. } => {
                // proj4rs reads and writes geographic coordinates in radians.
                let mut point_3d = match geo {
                    Some(g) => {
                        let (e, n) = g.normalize(x, y);
                        ((e + g.pm) / RAD_TO_DEG, n / RAD_TO_DEG, 0.0)
                    }
                    None => (x, y, 0.0),
                };
                proj4rs::transform::transform(from, to, &mut point_3d).map_err(|e| {
                    RasterH3Error::CrsError(format!(
                        "Reprojection error for point ({}, {}): {:?}",
                        x, y, e
                    ))
                })?;
                let lon_deg = point_3d.0 * RAD_TO_DEG;
                let lat_deg = point_3d.1 * RAD_TO_DEG;
                // Reject results outside the geographic domain instead of
                // handing callers coordinates that only look like degrees.
                if !lon_deg.is_finite() || !lat_deg.is_finite() || lat_deg.abs() > 90.0 + 1e-9 {
                    return Err(RasterH3Error::CrsError(format!(
                        "Reprojection of ({}, {}) left the geographic domain: ({}, {})",
                        x, y, lon_deg, lat_deg
                    )));
                }
                Ok((lon_deg, lat_deg.clamp(-90.0, 90.0)))
            }
        }
    }

    /// Inverse of [`Self::transform_point`]: WGS84 (lon, lat) in degrees to
    /// source CRS coordinates.
    pub fn wgs84_to_source(&self, lon: f64, lat: f64) -> Result<(f64, f64)> {
        match self {
            Self::Wgs84Identity => Ok((lon, lat)),
            Self::WebMercatorFast => {
                let phi = lat.clamp(-89.999999, 89.999999) / RAD_TO_DEG;
                Ok((
                    WGS84_A * lon / RAD_TO_DEG,
                    WGS84_A * (std::f64::consts::FRAC_PI_4 + phi / 2.0).tan().ln(),
                ))
            }
            Self::AlbersConic(albers) => Ok(albers.project_point(lon, lat)),
            Self::Proj4 { from, to, geo, .. } => {
                let mut point_3d = (lon / RAD_TO_DEG, lat / RAD_TO_DEG, 0.0);
                proj4rs::transform::transform(to, from, &mut point_3d).map_err(|e| {
                    RasterH3Error::CrsError(format!(
                        "Inverse reprojection error for point ({}, {}): {:?}",
                        lon, lat, e
                    ))
                })?;
                let (x, y) = match geo {
                    Some(g) => {
                        g.denormalize(point_3d.0 * RAD_TO_DEG - g.pm, point_3d.1 * RAD_TO_DEG)
                    }
                    None => (point_3d.0, point_3d.1),
                };
                if x.is_finite() && y.is_finite() {
                    Ok((x, y))
                } else {
                    Err(RasterH3Error::CrsError(format!(
                        "Inverse reprojection of ({}, {}) is outside the source CRS domain",
                        lon, lat
                    )))
                }
            }
        }
    }

    /// Transform a batch of (x, y) coordinates into destination (lon, lat) slices
    #[inline]
    pub fn transform_batch(
        &self,
        xs: &[f64],
        ys: &[f64],
        out_lon: &mut [f64],
        out_lat: &mut [f64],
    ) -> Result<()> {
        let count = xs.len().min(ys.len()).min(out_lon.len()).min(out_lat.len());
        for i in 0..count {
            let (lon, lat) = self.transform_point(xs[i], ys[i])?;
            out_lon[i] = lon;
            out_lat[i] = lat;
        }
        Ok(())
    }

    /// Whether [`Self::transform_rect_bounds`] is a proven enclosure of the
    /// rectangle's image. Identity, Web Mercator and Albers bounds are
    /// analytic, built-in polar stereographic bounds use radial/angular extrema,
    /// and transverse Mercator bounds carry a Lipschitz certificate
    /// (see [`TmCertificate`]); other PROJ bounds come from finitely many
    /// samples, which can miss interior extrema and singularities however
    /// densely they are placed.
    pub fn bounds_certified(&self) -> bool {
        !matches!(
            self,
            Self::Proj4 {
                tm: None,
                polar: None,
                ..
            }
        )
    }

    /// Bounds from [`Self::transform_rect_bounds`] that are safe to use to
    /// *reject* data (bbox pruning, mosaic overlap filtering). Data dropped by
    /// a rejection test never reaches the exact per-sample test, so
    /// uncertified bounds fail open to the whole globe and leave the decision
    /// to that exact test.
    pub fn rejection_bounds(&self, b: [f64; 4]) -> [f64; 4] {
        if self.bounds_certified() {
            b
        } else {
            [-180.0, -90.0, 180.0, 90.0]
        }
    }

    /// Certified bounds for a transverse Mercator source; see
    /// [`TmCertificate`]. Returns the whole globe when the rectangle leaves
    /// the certified band around the central meridian or any sample fails, so
    /// the result is always safe for rejection.
    fn tm_rect_bounds(
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

    /// Polar stereographic inverse latitude is monotonic in radius, while
    /// longitude is atan2(x, +/-y) plus a constant. Enclose the affine image
    /// in a source-axis rectangle: radius is minimized at the point nearest
    /// the origin, maximized at a corner, and angular extrema occur at
    /// corners unless the rectangle contains the origin. This includes
    /// rotated/sheared rasters conservatively. Only the fixed WGS84 polar
    /// definitions may use this proof (no datum shift, offsets or axis swap).
    fn polar_rect_bounds(
        &self,
        gt: &GeoTransform,
        col: f64,
        row: f64,
        width: f64,
        height: f64,
    ) -> [f64; 4] {
        const GLOBE: [f64; 4] = [-180.0, -90.0, 180.0, 90.0];
        // proj4rs' inverse iteration stops at 1e-10 radians; this slack is
        // larger in degrees and also covers floating-point corner evaluation.
        const SLACK: f64 = 1e-7;
        let (mut xmin, mut xmax, mut ymin, mut ymax) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for (c, r) in [
            (col, row),
            (col + width, row),
            (col, row + height),
            (col + width, row + height),
        ] {
            let (x, y) = gt.pixel_to_coord(c, r);
            if !x.is_finite() || !y.is_finite() {
                return GLOBE;
            }
            xmin = xmin.min(x);
            xmax = xmax.max(x);
            ymin = ymin.min(y);
            ymax = ymax.max(y);
        }
        let closest = (0.0f64.clamp(xmin, xmax), 0.0f64.clamp(ymin, ymax));
        let mut bounds = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for (x, y) in [
            (xmin, ymin),
            (xmin, ymax),
            (xmax, ymin),
            (xmax, ymax),
            closest,
        ] {
            let Ok((lon, lat)) = self.transform_point(x, y) else {
                return GLOBE;
            };
            if !lon.is_finite() || !lat.is_finite() {
                return GLOBE;
            }
            let lon = wrap_lon(lon);
            bounds[0] = bounds[0].min(lon);
            bounds[2] = bounds[2].max(lon);
            bounds[1] = bounds[1].min(lat);
            bounds[3] = bounds[3].max(lat);
        }
        bounds[1] = (bounds[1] - SLACK).max(-90.0);
        bounds[3] = (bounds[3] + SLACK).min(90.0);
        if closest == (0.0, 0.0)
            || bounds[2] - bounds[0] >= 180.0
            || bounds[0] - SLACK <= -180.0
            || bounds[2] + SLACK >= 180.0
        {
            bounds[0] = -180.0;
            bounds[2] = 180.0;
        } else {
            bounds[0] -= SLACK;
            bounds[2] += SLACK;
        }
        bounds
    }

    /// Calculate WGS84 [min_lon, min_lat, max_lon, max_lat] spatial bounds for a raster pixel rectangle.
    ///
    /// For EPSG:4326 and EPSG:3857 (Web Mercator), latitude is affine or monotonic in y,
    /// so the 4 corners are mathematically exact extrema.
    ///
    /// For Albers Conic projections, parallels of latitude are circular arcs centered at the cone apex.
    /// Along each bounding edge, this computes the exact closed-form analytical critical point
    /// minimizing distance to the apex, capturing the true maximum latitude at the central meridian.
    ///
    /// For arbitrary PROJ4 projections, boundary edges are densified with interior samples
    /// and a small latitude headroom is applied. This is an estimate, not a certificate
    /// (see [`Self::bounds_certified`]); use [`Self::rejection_bounds`] before rejecting data.
    ///
    /// If any probe fails to project or yields a non-finite coordinate, the whole globe
    /// is returned.
    pub fn transform_rect_bounds(
        &self,
        gt: &GeoTransform,
        col: f64,
        row: f64,
        width: f64,
        height: f64,
    ) -> [f64; 4] {
        if let Self::Proj4 { tm: Some(tm), .. } = self {
            return self.tm_rect_bounds(tm, gt, col, row, width, height);
        }
        if let Self::Proj4 { polar: Some(_), .. } = self {
            return self.polar_rect_bounds(gt, col, row, width, height);
        }
        let p0 = gt.pixel_to_coord(col, row);
        let p1 = gt.pixel_to_coord(col + width, row);
        let p2 = gt.pixel_to_coord(col + width, row + height);
        let p3 = gt.pixel_to_coord(col, row + height);

        let corners = [p0, p1, p2, p3];

        // Any probe that fails or is non-finite means part of the rectangle is
        // outside the projection's domain or numerically unstable; the
        // surviving probes would then under-report the extent.
        let mut failed = false;
        let mut probe = |x: f64, y: f64| match self.transform_point(x, y) {
            Ok((lon, lat)) if lon.is_finite() && lat.is_finite() => Some((lon, lat)),
            _ => {
                failed = true;
                None
            }
        };

        let mut min_lon = f64::INFINITY;
        let mut min_lat = f64::INFINITY;
        let mut max_lon = f64::NEG_INFINITY;
        let mut max_lat = f64::NEG_INFINITY;

        for &(x, y) in &corners {
            if let Some((lon, lat)) = probe(x, y) {
                let w_lon = wrap_lon(lon);
                min_lon = min_lon.min(w_lon);
                max_lon = max_lon.max(w_lon);
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
            }
        }

        match self {
            Self::Wgs84Identity | Self::WebMercatorFast => {
                if gt.b != 0.0 || gt.d != 0.0 {
                    let edges = [
                        (p0, p1, 16usize),
                        (p1, p2, 16usize),
                        (p2, p3, 16usize),
                        (p3, p0, 16usize),
                    ];
                    for ((x1, y1), (x2, y2), steps) in edges {
                        let dx = x2 - x1;
                        let dy = y2 - y1;
                        for step in 1..steps {
                            let t = step as f64 / steps as f64;
                            let px = x1 + t * dx;
                            let py = y1 + t * dy;
                            if let Some((lon, lat)) = probe(px, py) {
                                let w_lon = wrap_lon(lon);
                                min_lon = min_lon.min(w_lon);
                                max_lon = max_lon.max(w_lon);
                                min_lat = min_lat.min(lat);
                                max_lat = max_lat.max(lat);
                            }
                        }
                    }
                }
            }
            Self::AlbersConic(albers) => {
                // Parallels are circular arcs centered at the cone apex (x_0, y_0 + rho0).
                let apex_x = albers.x_0;
                let apex_y = albers.y_0 + albers.rho0;

                // If the cone apex lies inside the pixel rectangle's bounding box,
                // the chunk contains the pole (North Pole for n >= 0, South Pole for n < 0).
                let min_x = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
                let max_x = corners
                    .iter()
                    .map(|p| p.0)
                    .fold(f64::NEG_INFINITY, f64::max);
                let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
                let max_y = corners
                    .iter()
                    .map(|p| p.1)
                    .fold(f64::NEG_INFINITY, f64::max);
                if apex_x >= min_x && apex_x <= max_x && apex_y >= min_y && apex_y <= max_y {
                    if albers.n >= 0.0 {
                        max_lat = 90.0;
                    } else {
                        min_lat = -90.0;
                    }
                    min_lon = -180.0;
                    max_lon = 180.0;
                }

                let edges = [(p0, p1), (p1, p2), (p2, p3), (p3, p0)];
                for ((x1, y1), (x2, y2)) in edges {
                    let dx = x2 - x1;
                    let dy = y2 - y1;
                    let denom = dx * dx + dy * dy;
                    if denom > 1e-12 {
                        let t = -(dx * (x1 - apex_x) + dy * (y1 - apex_y)) / denom;
                        if t > 0.0 && t < 1.0 {
                            let cx = x1 + t * dx;
                            let cy = y1 + t * dy;
                            if let Some((lon, lat)) = probe(cx, cy) {
                                let w_lon = wrap_lon(lon);
                                min_lon = min_lon.min(w_lon);
                                max_lon = max_lon.max(w_lon);
                                min_lat = min_lat.min(lat);
                                max_lat = max_lat.max(lat);
                            }
                        }
                    }
                }
            }
            Self::Proj4 { .. } => {
                // Densify boundary edges: 32 samples on horizontal edges, 16 on vertical edges
                let edges = [
                    (p0, p1, 32usize),
                    (p1, p2, 16usize),
                    (p2, p3, 32usize),
                    (p3, p0, 16usize),
                ];
                for ((x1, y1), (x2, y2), steps) in edges {
                    let dx = x2 - x1;
                    let dy = y2 - y1;
                    for step in 1..steps {
                        let t = step as f64 / steps as f64;
                        let px = x1 + t * dx;
                        let py = y1 + t * dy;
                        if let Some((lon, lat)) = probe(px, py) {
                            let w_lon = wrap_lon(lon);
                            min_lon = min_lon.min(w_lon);
                            max_lon = max_lon.max(w_lon);
                            min_lat = min_lat.min(lat);
                            max_lat = max_lat.max(lat);
                        }
                    }
                }

                // Densify interior with a 5x5 grid (including exact center at i=3, j=3)
                // to detect interior extrema (e.g., polar stereographic or LAEA projections)
                for i in 1..=5 {
                    let tx = i as f64 / 6.0;
                    for j in 1..=5 {
                        let ty = j as f64 / 6.0;
                        let px = p0.0 * (1.0 - tx) * (1.0 - ty)
                            + p1.0 * tx * (1.0 - ty)
                            + p2.0 * tx * ty
                            + p3.0 * (1.0 - tx) * ty;
                        let py = p0.1 * (1.0 - tx) * (1.0 - ty)
                            + p1.1 * tx * (1.0 - ty)
                            + p2.1 * tx * ty
                            + p3.1 * (1.0 - tx) * ty;
                        if let Some((lon, lat)) = probe(px, py) {
                            let w_lon = wrap_lon(lon);
                            min_lon = min_lon.min(w_lon);
                            max_lon = max_lon.max(w_lon);
                            min_lat = min_lat.min(lat);
                            max_lat = max_lat.max(lat);
                        }
                    }
                }

                // If any probe approaches the pole (within ~55km / 0.5°), clamp to full polar bounds
                if max_lat >= 89.5 {
                    max_lat = 90.0;
                    min_lon = -180.0;
                    max_lon = 180.0;
                }
                if min_lat <= -89.5 {
                    min_lat = -90.0;
                    min_lon = -180.0;
                    max_lon = 180.0;
                }

                // Add 0.0005° (~55m) safety margin to max_lat/min_lat for Proj4 projections
                if max_lat.is_finite() && max_lat < 90.0 {
                    max_lat = (max_lat + 0.0005).min(90.0);
                }
                if min_lat.is_finite() && min_lat > -90.0 {
                    min_lat = (min_lat - 0.0005).max(-90.0);
                }
            }
        }

        // Longitude is affine in pixel space for Wgs84Identity/WebMercatorFast and
        // monotone in polar angle around the cone apex for AlbersConic (when the apex
        // is outside the chunk), so unwrapped corner longitudes detect antimeridian
        // crossings even when wrapped corners span <= 180 degrees.
        if matches!(
            self,
            Self::Wgs84Identity | Self::WebMercatorFast | Self::AlbersConic(_)
        ) {
            let lons: Vec<f64> = corners
                .iter()
                .filter_map(|&(x, y)| self.transform_point(x, y).ok().map(|p| p.0))
                .collect();
            if lons.len() == corners.len() && lons.iter().all(|v| v.is_finite()) {
                let lo = lons.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = lons.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                if lo < -180.0 || hi > 180.0 {
                    (min_lon, max_lon) = (-180.0, 180.0);
                } else if matches!(self, Self::Wgs84Identity | Self::WebMercatorFast) {
                    // Per-sample bbox tests wrap every longitude, which can move
                    // an in-range value by an ulp. Wrapping is monotone, so the
                    // wrapped corners bound the wrapped samples, except that
                    // values within rounding below 180 wrap to -180. An edge at
                    // exactly 180 stays: samples lie strictly inside pixels.
                    let west = wrap_lon(lo);
                    let east = if hi == 180.0 { hi } else { wrap_lon(hi) };
                    (min_lon, max_lon) = if east < west || hi - lo > 180.0 {
                        (-180.0, 180.0)
                    } else {
                        (west, east)
                    };
                }
            }
        }

        // Handle rasters spanning across the antimeridian
        if max_lon - min_lon > 180.0 {
            min_lon = -180.0;
            max_lon = 180.0;
        }

        if failed || max_lat == f64::NEG_INFINITY {
            // Some probe failed to project. Return the whole globe rather than
            // bounds built from the survivors (or a plausible-looking `[0,0,0,0]`):
            // this keeps bbox pruning conservative and sorts the chunk first in the
            // north-to-south horizon order, so no cell can be evicted before the
            // chunk is processed.
            [-180.0, -90.0, 180.0, 90.0]
        } else {
            [min_lon, min_lat, max_lon, max_lat]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wgs84_identity() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(4326), None).unwrap();
        let (lon, lat) = tf.transform_point(-122.4194, 37.7749).unwrap();
        assert!((lon - -122.4194).abs() < 1e-6);
        assert!((lat - 37.7749).abs() < 1e-6);
    }

    #[test]
    fn test_web_mercator() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
        // San Francisco Web Mercator coords
        let (lon, lat) = tf.transform_point(-13627665.27, 4547675.35).unwrap();
        assert!((lon - -122.4194).abs() < 1e-2);
        assert!((lat - 37.7749).abs() < 1e-2);
    }

    #[test]
    fn test_albers_epsg5070_accuracy() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
        // Point in Washington DC area: (x = 1580000.0, y = 1940000.0) in EPSG:5070
        let (lon, lat) = tf.transform_point(1580000.0, 1940000.0).unwrap();
        // Should be approximately Lon -77.0, Lat 38.9
        assert!((lon - (-77.05)).abs() < 0.5);
        assert!((lat - 38.88).abs() < 0.5);

        // Compare against PROJ4 reference
        let proj4_tf = CrsTransformer::from_proj_string("+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=GRS80 +units=m +no_defs").unwrap();
        let (p4_lon, p4_lat) = proj4_tf.transform_point(1580000.0, 1940000.0).unwrap();

        assert!(
            (lon - p4_lon).abs() < 1e-5,
            "Lon mismatch: {} vs {}",
            lon,
            p4_lon
        );
        assert!(
            (lat - p4_lat).abs() < 1e-5,
            "Lat mismatch: {} vs {}",
            lat,
            p4_lat
        );
    }

    #[test]
    fn test_utm_reprojection() {
        // UTM Zone 32N (EPSG:32632)
        let tf = CrsTransformer::from_crs_or_epsg(Some(32632), None).unwrap();
        let (lon, lat) = tf.transform_point(500000.0, 4500000.0).unwrap();
        assert!((lon - 9.0).abs() < 0.1);
        assert!((lat - 40.65).abs() < 0.5);
    }

    #[test]
    fn test_hawaii_albers_parsing_and_accuracy() {
        let proj_str = "+proj=aea +lat_1=8 +lat_2=18 +lat_0=13 +lon_0=-157 +x_0=0 +y_0=0 +datum=NAD83 +units=m +no_defs";
        let tf = CrsTransformer::from_crs_or_epsg(None, Some(proj_str)).unwrap();

        // Verify it was parsed as AlbersConicFast
        match tf {
            CrsTransformer::AlbersConic(_) => {}
            _ => panic!("Expected AlbersConicFast for Hawaii Albers PROJ string"),
        }

        // Test origin point (0, 0) -> should be exactly (-157.0, 13.0)
        let (lon, lat) = tf.transform_point(0.0, 0.0).unwrap();
        assert!((lon - -157.0).abs() < 1e-6);
        assert!((lat - 13.0).abs() < 1e-6);
    }

    #[test]
    fn test_antimeridian_utm_zone_1_and_60_continuity() {
        // UTM Zone 1N (EPSG:32601): central meridian = -177°, spans -180° to -174°
        let tf_z1 = CrsTransformer::from_crs_or_epsg(Some(32601), None).unwrap();
        let (lon_cm, lat_cm) = tf_z1.transform_point(500000.0, 0.0).unwrap();
        assert!(
            (lon_cm - -177.0).abs() < 1e-3,
            "Zone 1 central meridian should be -177°"
        );
        assert!(lat_cm.abs() < 1e-3, "Equator latitude should be 0°");

        // West edge of Zone 1 near the antimeridian: spans -180° to -174°
        // Note: PROJ4 normalizes -180° and +180° to the antimeridian meridian
        let (lon_west, lat_w) = tf_z1.transform_point(166021.0, 0.0).unwrap();
        assert!(
            (lon_west.abs() - 180.0).abs() < 0.05,
            "West edge should be at antimeridian (±180°), got {}",
            lon_west
        );
        assert!(lat_w.abs() < 1e-3);

        // Point inside Zone 1: x = 250000m (approx -179.24°)
        let (lon_in1, _) = tf_z1.transform_point(250000.0, 0.0).unwrap();
        assert!(
            lon_in1 > -180.0 && lon_in1 < -177.0,
            "Inside Zone 1 should be between -180° and -177°: got {}",
            lon_in1
        );

        // UTM Zone 60N (EPSG:32660): central meridian = +177°, spans +174° to +180°
        let tf_z60 = CrsTransformer::from_crs_or_epsg(Some(32660), None).unwrap();
        let (lon_cm60, lat_cm60) = tf_z60.transform_point(500000.0, 0.0).unwrap();
        assert!(
            (lon_cm60 - 177.0).abs() < 1e-3,
            "Zone 60 central meridian should be +177°"
        );
        assert!(lat_cm60.abs() < 1e-3, "Equator latitude should be 0°");

        // East edge of Zone 60 near +180° antimeridian
        let (lon_east, _) = tf_z60.transform_point(833978.0, 0.0).unwrap();
        assert!(
            (lon_east.abs() - 180.0).abs() < 0.05,
            "East edge should be at antimeridian (±180°), got {}",
            lon_east
        );

        // Point inside Zone 60: x = 750000m (approx +179.24°)
        let (lon_in60, _) = tf_z60.transform_point(750000.0, 0.0).unwrap();
        assert!(
            lon_in60 > 177.0 && lon_in60 < 180.0,
            "Inside Zone 60 should be between +177° and +180°: got {}",
            lon_in60
        );
    }

    #[test]
    fn test_polar_stereographic_and_latitude_bounds() {
        // South Pole Stereographic (+proj=stere +lat_0=-90)
        let sp_proj = "+proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
        let tf_sp = CrsTransformer::from_proj_string(sp_proj).unwrap();
        let (_sp_lon, sp_lat) = tf_sp.transform_point(0.0, 0.0).unwrap();
        assert!(
            (sp_lat - -90.0).abs() < 1e-5,
            "South pole latitude should be -90°: got {}",
            sp_lat
        );

        // North Pole Stereographic (+proj=stere +lat_0=90)
        let np_proj = "+proj=stere +lat_0=90 +lat_ts=71 +lon_0=0 +k=1 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
        let tf_np = CrsTransformer::from_proj_string(np_proj).unwrap();
        let (_np_lon, np_lat) = tf_np.transform_point(0.0, 0.0).unwrap();
        assert!(
            (np_lat - 90.0).abs() < 1e-5,
            "North pole latitude should be +90°: got {}",
            np_lat
        );

        // Web Mercator extreme Y points approach ±85.051129° without NaN
        let tf_wm = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
        let (_lon_n, lat_n) = tf_wm.transform_point(0.0, 20000000.0).unwrap();
        assert!(!lat_n.is_nan());
        assert!(
            lat_n > 85.0 && lat_n <= 90.0,
            "Latitude must remain bounded: {}",
            lat_n
        );

        let (_lon_s, lat_s) = tf_wm.transform_point(0.0, -20000000.0).unwrap();
        assert!(!lat_s.is_nan());
        assert!(
            (-90.0..-85.0).contains(&lat_s),
            "Latitude must remain bounded: {}",
            lat_s
        );
    }

    #[test]
    fn test_malformed_proj_strings_and_error_handling() {
        let invalid_strings = [
            "+proj=nonexistent_projection_abc_123",
            "completely invalid syntax without plus",
            "+proj=utm +zone=999 +datum=WGS84",
        ];

        for s in &invalid_strings {
            let res = CrsTransformer::from_proj_string(s);
            assert!(
                res.is_err(),
                "Expected error for invalid PROJ string: {}",
                s
            );
            match res.err().unwrap() {
                RasterH3Error::CrsError(msg) => {
                    assert!(!msg.is_empty(), "CrsError must contain diagnostic message");
                }
                other => panic!("Expected RasterH3Error::CrsError, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_transform_batch_bounds_safety() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(4326), None).unwrap();
        let xs = [10.0, 20.0, 30.0, 40.0, 50.0];
        let ys = [1.0, 2.0, 3.0, 4.0, 5.0];

        // Mismatched destination buffer: smaller than inputs
        let mut out_lon = [0.0; 3];
        let mut out_lat = [0.0; 3];
        tf.transform_batch(&xs, &ys, &mut out_lon, &mut out_lat)
            .unwrap();

        assert_eq!(out_lon, [10.0, 20.0, 30.0]);
        assert_eq!(out_lat, [1.0, 2.0, 3.0]);

        // Empty slices
        let mut empty_lon = [];
        let mut empty_lat = [];
        tf.transform_batch(&[], &[], &mut empty_lon, &mut empty_lat)
            .unwrap();
    }

    #[test]
    fn test_unsupported_epsg_error() {
        let res = CrsTransformer::from_crs_or_epsg(Some(999999), None);
        assert!(res.is_err(), "Expected error for unsupported EPSG code");
        match res.err().unwrap() {
            RasterH3Error::UnsupportedEpsg { code, detail } => {
                assert_eq!(code, 999999);
                assert!(detail.contains("999999"));
            }
            other => panic!("Expected RasterH3Error::UnsupportedEpsg, got {:?}", other),
        }
    }

    #[test]
    fn test_transform_rect_bounds_wgs84_and_mercator() {
        let gt = GeoTransform {
            c0: -120.0,
            a: 0.1,
            b: 0.0,
            f0: 38.0,
            d: 0.0,
            e: -0.1,
        };
        let tf_wgs = CrsTransformer::Wgs84Identity;
        let b_wgs = tf_wgs.transform_rect_bounds(&gt, 0.0, 0.0, 10.0, 10.0);
        assert!((b_wgs[0] - (-120.0)).abs() < 1e-6);
        assert!((b_wgs[1] - 37.0).abs() < 1e-6);
        assert!((b_wgs[2] - (-119.0)).abs() < 1e-6);
        assert!((b_wgs[3] - 38.0).abs() < 1e-6);

        let tf_wm = CrsTransformer::WebMercatorFast;
        let gt_wm = GeoTransform {
            c0: -13000000.0,
            a: 1000.0,
            b: 0.0,
            f0: 4500000.0,
            d: 0.0,
            e: -1000.0,
        };
        let b_wm = tf_wm.transform_rect_bounds(&gt_wm, 0.0, 0.0, 10.0, 10.0);
        assert!(b_wm[3] > b_wm[1]);
        assert!(b_wm[2] > b_wm[0]);
    }

    #[test]
    fn test_transform_rect_bounds_conus_albers_strip_captures_central_meridian() {
        // EPSG:5070 CONUS Albers strip matching BP_CONUS strip row 0
        let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
        let gt = GeoTransform {
            c0: -2362395.0,
            a: 30.0,
            b: 0.0,
            f0: 3267405.0,
            d: 0.0,
            e: -30.0,
        };

        // Strip width: 156335 pixels (~4690 km), height: 1 pixel
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 156335.0, 1.0);
        let max_lat = bounds[3];

        // The top corners (west and east) have latitude around 46.0°:
        let (_, lat_nw) = tf.transform_point(-2362395.0, 3267405.0).unwrap();
        let (_, lat_ne) = tf
            .transform_point(-2362395.0 + 156335.0 * 30.0, 3267405.0)
            .unwrap();
        let (_, lat_cm) = tf.transform_point(0.0, 3267405.0).unwrap();
        let corner_max = lat_nw.max(lat_ne);
        assert!((corner_max - 48.856).abs() < 0.01);
        assert!((lat_cm - 52.482).abs() < 0.01);

        // transform_rect_bounds MUST capture the true maximum latitude at the central meridian,
        // which is ~3.6° higher than the corner maximum:
        assert_eq!(max_lat, lat_cm);
        assert!(
            max_lat > corner_max + 3.0,
            "max_lat ({}) should exceed corner max ({}) by at least 3.0°",
            max_lat,
            corner_max
        );
    }

    #[test]
    fn test_transform_rect_bounds_rotated_albers() {
        let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
        // Rotated geotransform: 45 degree rotation
        let angle = std::f64::consts::FRAC_PI_4;
        let cos_a = angle.cos() * 30.0;
        let sin_a = angle.sin() * 30.0;
        let gt_rot = GeoTransform {
            c0: -100000.0,
            a: cos_a,
            b: -sin_a,
            f0: 2000000.0,
            d: sin_a,
            e: cos_a,
        };

        let bounds = tf.transform_rect_bounds(&gt_rot, 0.0, 0.0, 1000.0, 1000.0);
        assert!(bounds[3] > bounds[1]);
        assert!(bounds[2] > bounds[0]);
    }

    #[test]
    fn test_transform_rect_bounds_rotated_wgs84() {
        let tf = CrsTransformer::Wgs84Identity;
        // 45-degree rotated geotransform
        let angle = std::f64::consts::FRAC_PI_4;
        let cos_a = angle.cos();
        let sin_a = angle.sin();
        let gt_rot = GeoTransform {
            c0: 0.0,
            a: cos_a,
            b: -sin_a,
            f0: 45.0,
            d: sin_a,
            e: cos_a,
        };

        // For a 45°-rotated WGS84 grid, corner max of baseline corners
        let p0 = gt_rot.pixel_to_coord(0.0, 0.0);
        let p3 = gt_rot.pixel_to_coord(0.0, 10.0);
        let corner_max = p0.1.max(p3.1);

        let bounds = tf.transform_rect_bounds(&gt_rot, 0.0, 0.0, 10.0, 10.0);
        let max_lat = bounds[3];
        let mid_lat = max_lat;

        assert!((bounds[3] - mid_lat).abs() < 1e-9);
        assert!(bounds[3] > corner_max);
    }

    #[test]
    fn test_transform_rect_bounds_antimeridian_wrap() {
        let tf = CrsTransformer::Wgs84Identity;
        // Raster crossing antimeridian: col 0 at 175°, width 10° -> col 10 at 185° (-175°)
        let gt = GeoTransform {
            c0: 175.0,
            a: 1.0,
            b: 0.0,
            f0: 10.0,
            d: 0.0,
            e: -1.0,
        };
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 10.0, 10.0);
        // Span across 180° exceeds 180° when wrapped (from -175° to 175° naive span is 350°),
        // so bounds must report [-180, 0, 180, 10]
        assert_eq!(bounds[0], -180.0);
        assert_eq!(bounds[2], 180.0);

        // A 0..360 grid spanning 100..300 wraps to corners 100 and -60, whose
        // naive span (160) would hide the crossing.
        let gt = GeoTransform {
            c0: 100.0,
            a: 1.0,
            b: 0.0,
            f0: 10.0,
            d: 0.0,
            e: -1.0,
        };
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 200.0, 10.0);
        assert_eq!((bounds[0], bounds[2]), (-180.0, 180.0));

        // Ending exactly at 180 does not cross.
        let gt = GeoTransform { c0: 160.0, ..gt };
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 20.0, 10.0);
        assert_eq!((bounds[0], bounds[2]), (160.0, 180.0));
    }

    #[test]
    fn test_transform_rect_bounds_polar_stereographic() {
        // EPSG:3413 NSIDC Sea Ice Polar Stereographic North
        let tf = CrsTransformer::from_crs_or_epsg(Some(3413), None).unwrap();
        // A chunk containing the North Pole at (0, 0) in projected coords
        let gt = GeoTransform {
            c0: -50000.0,
            a: 1000.0,
            b: 0.0,
            f0: 50000.0,
            d: 0.0,
            e: -1000.0,
        };
        let bounds = tf.transform_rect_bounds(&gt, 0.0, 0.0, 100.0, 100.0);
        // Pole (lat = 90.0) is interior to this chunk. transform_rect_bounds must capture max_lat = 90.0
        assert_eq!(bounds[3], 90.0, "Polar chunk must report max_lat = 90.0");
        assert_eq!(
            bounds[0], -180.0,
            "Polar chunk must span full longitude [-180, 180]"
        );
        assert_eq!(
            bounds[2], 180.0,
            "Polar chunk must span full longitude [-180, 180]"
        );
    }

    #[test]
    fn test_tokenized_crs_detection_utm_with_5070_in_param() {
        let proj_str = "+proj=utm +zone=17 +x_0=5070000 +datum=WGS84 +units=m +no_defs";
        let tf = CrsTransformer::from_crs_or_epsg(None, Some(proj_str)).unwrap();
        match tf {
            CrsTransformer::Proj4 { .. } => {}
            _ => panic!("Expected Proj4 transformer, got non-Proj4 for UTM with +x_0=5070000"),
        }
    }

    #[test]
    fn test_ellipsoidal_mercator_routes_to_proj4() {
        // cs2cs -f "%.10f" "+proj=merc +a=6378137 +rf=298.257223563 +lon_0=0" +to "+proj=longlat +datum=WGS84 +no_defs" <<< "1113194.9079 6800125.4544"
        // Output: 9.9999999997 52.1864260950 0.0000000000
        let proj_str = "+proj=merc +a=6378137 +rf=298.257223563 +lon_0=0";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::Proj4 { .. } => {}
            _ => panic!("Expected Proj4 transformer for ellipsoidal Mercator"),
        }

        let (lon, lat) = tf.transform_point(1113194.9079, 6800125.4544).unwrap();
        let golden_lon = 9.9999999997;
        let golden_lat = 52.1864260950;

        // Ground distance tolerance <= 0.01 m
        let deg_to_rad = std::f64::consts::PI / 180.0;
        let dlat_m = (lat - golden_lat) * deg_to_rad * 6378137.0;
        let dlon_m = (lon - golden_lon) * deg_to_rad * 6378137.0 * (golden_lat * deg_to_rad).cos();
        let dist_m = (dlat_m * dlat_m + dlon_m * dlon_m).sqrt();
        assert!(
            dist_m <= 0.01,
            "Transformed point ({}, {}) differs from golden cs2cs ({}, {}) by {} m (> 0.01 m)",
            lon,
            lat,
            golden_lon,
            golden_lat,
            dist_m
        );
    }

    #[test]
    fn test_spherical_mercator_with_nadgrids_null_is_fast_path() {
        let proj_str = "+proj=merc +a=6378137 +b=6378137 +k=1 +lon_0=0 +nadgrids=@null";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::WebMercatorFast => {}
            _ => panic!("Expected WebMercatorFast transformer for spherical Mercator"),
        }
    }

    #[test]
    fn test_albers_intl_ellipsoid_routes_to_proj4() {
        // cs2cs -f "%.10f" "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=intl +units=m +no_defs" +to "+proj=longlat +ellps=intl +no_defs" <<< "1000000.0 2000000.0"
        // Golden cs2cs output: -84.0779390722 40.4478144304 0.0000000000
        let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=intl +units=m +no_defs";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::Proj4 { .. } => {}
            _ => {
                panic!("Expected Proj4 transformer for Albers on non-GRS80/WGS84 ellipsoid (intl)")
            }
        }

        let (lon, lat) = tf.transform_point(1000000.0, 2000000.0).unwrap();
        let golden_lon = -84.0779390722;
        let golden_lat = 40.4478144304;

        // Ground distance tolerance <= 0.01 m on International 1924 ellipsoid (a = 6378388.0)
        let a_intl = 6378388.0;
        let deg_to_rad = std::f64::consts::PI / 180.0;
        let dlat_m = (lat - golden_lat) * deg_to_rad * a_intl;
        let dlon_m = (lon - golden_lon) * deg_to_rad * a_intl * (golden_lat * deg_to_rad).cos();
        let dist_m = (dlat_m * dlat_m + dlon_m * dlon_m).sqrt();
        assert!(
            dist_m <= 0.01,
            "Transformed point ({}, {}) differs from golden cs2cs ({}, {}) by {} m (> 0.01 m)",
            lon,
            lat,
            golden_lon,
            golden_lat,
            dist_m
        );
    }

    #[test]
    fn test_albers_grs80_with_zero_towgs84_is_fast_path() {
        let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=GRS80 +towgs84=0,0,0 +units=m +no_defs";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        match tf {
            CrsTransformer::AlbersConic(_) => {}
            _ => panic!("Expected AlbersConic fast path for GRS80 with all-zero towgs84"),
        }
    }

    #[test]
    fn test_albers_with_nonzero_towgs84_uses_proj4() {
        let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +ellps=clrk66 +towgs84=-8,160,176 +units=m +no_defs";
        let tf = CrsTransformer::from_proj_string(proj_str).unwrap();
        assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
    }

    #[test]
    fn test_geographic_fallback_converts_degrees_to_radians() {
        // Audit probe: a non-WGS84 ellipsoid without datum parameters skips
        // the identity fast path and must still return degrees.
        let tf = CrsTransformer::from_proj_string("+proj=longlat +ellps=clrk66 +no_defs").unwrap();
        assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
        let (lon, lat) = tf.transform_point(2.0, 45.0).unwrap();
        assert!(
            (lon - 2.0).abs() < 1e-9 && (lat - 45.0).abs() < 1e-9,
            "({lon}, {lat})"
        );
    }

    #[test]
    fn test_prime_meridian_is_honoured() {
        let tf = CrsTransformer::from_proj_string("+proj=longlat +datum=WGS84 +pm=paris +no_defs")
            .unwrap();
        assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
        let (lon, lat) = tf.transform_point(2.0, 45.0).unwrap();
        // PROJ: (4.3372291667, 45)
        assert!((lon - 4.3372291667).abs() < 1e-8, "lon = {lon}");
        assert!((lat - 45.0).abs() < 1e-9);
    }

    #[test]
    fn test_albers_in_us_feet_is_not_fast_path() {
        let ft = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +datum=NAD83 +units=us-ft";
        let tf = CrsTransformer::from_proj_string(ft).unwrap();
        assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
        let (lon, lat) = tf.transform_point(1_580_000.0, 1_940_000.0).unwrap();
        // PROJ inverse to NAD83 geographic: (-91.1030319046, 28.2800931283)
        assert!((lon + 91.1030319046).abs() < 1e-6, "lon = {lon}");
        assert!((lat - 28.2800931283).abs() < 1e-6, "lat = {lat}");
        // Explicit metres keep the analytical path.
        let m = ft.replace("us-ft", "m");
        assert!(matches!(
            CrsTransformer::from_proj_string(&m).unwrap(),
            CrsTransformer::AlbersConic(_)
        ));
    }

    #[test]
    fn test_unknown_modifiers_leave_fast_paths() {
        for s in [
            "+proj=longlat +datum=WGS84 +axis=neu",
            "+proj=longlat +datum=WGS84 +geoc",
            "+proj=merc +a=6378137 +b=6378137 +units=ft",
            "+proj=aea +lat_1=29.5 +lat_2=45.5 +lon_0=-96 +ellps=GRS80 +to_meter=0.3048",
        ] {
            let tf = CrsTransformer::from_proj_string(s).unwrap();
            assert!(
                matches!(tf, CrsTransformer::Proj4 { .. }),
                "{s} took a fast path"
            );
        }
    }

    #[test]
    fn test_projected_prime_meridian_is_rejected() {
        let s = "+proj=utm +zone=31 +ellps=intl +pm=paris +units=m";
        assert!(CrsTransformer::from_proj_string(s).is_err());
    }

    #[test]
    fn test_documented_epsg_codes_resolve() {
        for code in [6933, 7856] {
            assert!(CrsTransformer::from_epsg_code(code).is_ok(), "EPSG:{code}");
        }
        // EASE-Grid 2.0 origin is (0, 0).
        let tf = CrsTransformer::from_epsg_code(6933).unwrap();
        let (lon, lat) = tf.transform_point(0.0, 0.0).unwrap();
        assert!(lon.abs() < 1e-9 && lat.abs() < 1e-9);
        // MGA zone 56 central meridian is 153E.
        let tf = CrsTransformer::from_epsg_code(7856).unwrap();
        let (lon, _) = tf.transform_point(500_000.0, 6_250_000.0).unwrap();
        assert!((lon - 153.0).abs() < 1e-9);
    }

    #[test]
    fn test_wgs84_to_source_round_trips() {
        let cases: Vec<(CrsTransformer, (f64, f64))> = vec![
            (
                CrsTransformer::from_epsg_code(3857).unwrap(),
                (-1.3e7, 4.5e6),
            ),
            (
                CrsTransformer::from_epsg_code(5070).unwrap(),
                (1.58e6, 1.94e6),
            ),
            (
                CrsTransformer::from_epsg_code(3338).unwrap(),
                (-2.0e5, 1.2e6),
            ),
            (
                CrsTransformer::from_epsg_code(32633).unwrap(),
                (450_000.0, 5_500_000.0),
            ),
            (
                CrsTransformer::from_proj_string("+proj=longlat +datum=WGS84 +pm=paris").unwrap(),
                (2.0, 45.0),
            ),
        ];
        for (tf, (x, y)) in cases {
            let (lon, lat) = tf.transform_point(x, y).unwrap();
            let (x2, y2) = tf.wgs84_to_source(lon, lat).unwrap();
            let tol = 1e-6 * x.abs().max(y.abs()).max(1.0);
            assert!(
                (x - x2).abs() < tol && (y - y2).abs() < tol,
                "({x},{y}) -> ({x2},{y2})"
            );
        }
    }

    #[test]
    fn test_prime_meridian_with_non_canonical_axes() {
        // PROJ: (4.3372291667, 45) for Paris longitude 2, latitude 45.
        let cases = [
            ("+axis=neu", (45.0, 2.0)),
            ("+axis=wnu", (-2.0, 45.0)),
            ("+axis=swu", (-45.0, -2.0)),
            ("", (2.0, 45.0)),
        ];
        for (axis, input) in cases {
            let def = format!("+proj=longlat +datum=WGS84 +pm=paris {axis} +no_defs");
            let tf = CrsTransformer::from_proj_string(&def).unwrap();
            let (lon, lat) = tf.transform_point(input.0, input.1).unwrap();
            assert!((lon - 4.3372291667).abs() < 1e-8, "{def}: lon = {lon}");
            assert!((lat - 45.0).abs() < 1e-9, "{def}: lat = {lat}");
            let (x, y) = tf.wgs84_to_source(lon, lat).unwrap();
            assert!(
                (x - input.0).abs() < 1e-9 && (y - input.1).abs() < 1e-9,
                "{def}: ({x}, {y})"
            );
        }
    }

    #[test]
    fn test_axis_without_prime_meridian() {
        let tf = CrsTransformer::from_proj_string("+proj=longlat +datum=WGS84 +axis=neu").unwrap();
        let (lon, lat) = tf.transform_point(45.0, 2.0).unwrap();
        assert!(
            (lon - 2.0).abs() < 1e-9 && (lat - 45.0).abs() < 1e-9,
            "({lon}, {lat})"
        );
    }

    #[test]
    fn test_symmetric_albers_equal_and_opposite_parallels_rejected_without_nan() {
        // Symmetric standard parallels (lat_1 = -lat_2) have cone constant n = 0
        // (ProjErrConicLatEqual in PROJ). Fast path must reject it so proj4rs
        // reports CrsError rather than dividing by zero and returning NaN.
        let s = "+proj=aea +lat_1=-20 +lat_2=20 +lat_0=0 +lon_0=0 +datum=WGS84 +units=m +no_defs";
        assert!(CrsTransformer::from_proj_string(s).is_err());
    }

    #[test]
    fn test_malformed_numeric_fast_path_parameters_and_unsupported_datums_rejected() {
        for bad in [
            "+proj=longlat +datum=WGS84 +a=not_a_number",
            "+proj=longlat +datum=WGS84 +rf=not_a_number",
            "+proj=merc +a=6378137 +b=6378137 +lon_0=not_a_number",
            "+proj=merc +a=6378137 +b=6378137 +k=not_a_number",
            "+proj=aea +lat_1=29.5 +lat_2=45.5 +lon_0=-96 +datum=WGS84 +x_0=not_a_number",
            "+proj=utm +zone=10 +datum=NAD27 +units=m",
            "+proj=longlat +datum=not_a_datum",
        ] {
            let res = CrsTransformer::from_proj_string(bad);
            assert!(res.is_err(), "Expected CrsError for '{bad}'");
        }
    }

    #[test]
    fn test_rejection_bounds_fail_open_when_uncertified() {
        let b = [10.0, 20.0, 11.0, 21.0];
        let globe = [-180.0, -90.0, 180.0, 90.0];
        for code in [4326, 3857, 5070] {
            let tf = CrsTransformer::from_epsg_code(code).unwrap();
            assert!(tf.bounds_certified(), "EPSG:{code}");
            assert_eq!(tf.rejection_bounds(b), b, "EPSG:{code}");
        }
        let lcc = CrsTransformer::from_epsg_code(2154).unwrap();
        assert!(!lcc.bounds_certified());
        assert_eq!(lcc.rejection_bounds(b), globe);
    }

    /// Deterministic uniform samples in [0, 1).
    fn lcg(state: &mut u64) -> f64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*state >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Assert that `b` encloses the image of the pixel rectangle, checked on
    /// a dense grid plus random interior points.
    fn assert_encloses(tf: &CrsTransformer, gt: &GeoTransform, w: f64, h: f64, b: [f64; 4]) {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut pts: Vec<(f64, f64)> = (0..=120)
            .flat_map(|j| (0..=120).map(move |i| (w * i as f64 / 120.0, h * j as f64 / 120.0)))
            .collect();
        pts.extend((0..4000).map(|_| (w * lcg(&mut seed), h * lcg(&mut seed))));
        for (c, r) in pts {
            let (x, y) = gt.pixel_to_coord(c, r);
            let Ok((lon, lat)) = tf.transform_point(x, y) else {
                assert_eq!(b, [-180.0, -90.0, 180.0, 90.0], "failed sample needs globe");
                continue;
            };
            assert!(
                b[1] <= lat && lat <= b[3],
                "lat {lat} outside {b:?} at ({c}, {r})"
            );
            let lon = wrap_lon(lon);
            let lon_ok = b[0] <= lon && lon <= b[2] || (b[2] - b[0] >= 360.0);
            assert!(
                lon_ok || (lon == -180.0 && b[2] == 180.0),
                "lon {lon} outside {b:?}"
            );
        }
    }

    #[test]
    fn test_tm_bounds_are_certified_and_enclose_dense_samples() {
        let rot = |s: f64, deg: f64, c0: f64, f0: f64| {
            let (sin, cos) = deg.to_radians().sin_cos();
            GeoTransform {
                c0,
                a: s * cos,
                b: s * sin,
                f0,
                d: s * sin,
                e: -s * cos,
            }
        };
        let cases: Vec<(&str, GeoTransform, f64, f64)> = vec![
            // Typical UTM chunk and whole tile.
            (
                "EPSG:32633",
                rot(30.0, 0.0, 400_000.0, 5_600_000.0),
                512.0,
                512.0,
            ),
            (
                "EPSG:32633",
                rot(10.0, 0.0, 300_000.0, 5_600_000.0),
                10980.0,
                10980.0,
            ),
            // Rotated grid, far edge of the zone, high latitude.
            (
                "EPSG:32633",
                rot(250.0, 17.0, 50_000.0, 8_000_000.0),
                400.0,
                300.0,
            ),
            // Rectangle around the North Pole.
            (
                "EPSG:32633",
                rot(1000.0, 0.0, 480_000.0, 10_020_000.0),
                40.0,
                60.0,
            ),
            // Southern hemisphere, near the antimeridian.
            (
                "EPSG:32760",
                rot(100.0, -8.0, 650_000.0, 5_000_000.0),
                2000.0,
                2000.0,
            ),
            // Zone 1 chunk crossing the antimeridian.
            (
                "EPSG:32601",
                rot(500.0, 0.0, 150_000.0, 1_500_000.0),
                600.0,
                400.0,
            ),
            // Helmert-shifted national grid.
            (
                "EPSG:27700",
                rot(50.0, 0.0, 300_000.0, 700_000.0),
                4000.0,
                4000.0,
            ),
            // US survey feet, non-UTM scale factor.
            (
                "+proj=tmerc +lat_0=31 +lon_0=-110.1666667 +k=0.9999 +x_0=213360 +y_0=0 \
                 +ellps=GRS80 +to_meter=0.3048006096012192 +no_defs",
                rot(100.0, 3.0, 500_000.0, 1_000_000.0),
                3000.0,
                3000.0,
            ),
            (
                "+proj=etmerc +lon_0=12 +ellps=bessel",
                rot(20.0, 0.0, -50_000.0, 6_000_000.0),
                5000.0,
                500.0,
            ),
        ];
        for (crs, gt, w, h) in cases {
            let tf = CrsTransformer::from_crs_or_epsg(None, Some(crs)).unwrap();
            assert!(tf.bounds_certified(), "{crs}");
            let b = tf.transform_rect_bounds(&gt, 0.0, 0.0, w, h);
            assert_eq!(tf.rejection_bounds(b), b, "{crs}");
            assert_encloses(&tf, &gt, w, h, b);
        }
    }

    #[test]
    fn test_tm_bounds_are_tight_enough_to_prune() {
        let tf = CrsTransformer::from_epsg_code(32633).unwrap();
        let gt = GeoTransform {
            c0: 400_000.0,
            a: 30.0,
            b: 0.0,
            f0: 5_600_000.0,
            d: 0.0,
            e: -30.0,
        };
        let b = tf.transform_rect_bounds(&gt, 0.0, 0.0, 512.0, 512.0);
        let (lon0, lat0) = tf.transform_point(400_000.0, 5_600_000.0).unwrap();
        let (lon1, lat1) = tf.transform_point(415_360.0, 5_584_640.0).unwrap();
        // Corner-to-corner extent plus at most ~0.03 degree of certificate padding.
        assert!(
            b[0] > lon0.min(lon1) - 0.03 && b[2] < lon0.max(lon1) + 0.03,
            "{b:?}"
        );
        assert!(b[1] > lat1 - 0.03 && b[3] < lat0 + 0.03, "{b:?}");
        // A bbox a degree away is rejected; one over the chunk is not.
        let r = tf.rejection_bounds(b);
        let far = [b[2] + 1.0, b[1], b[2] + 2.0, b[3]];
        let near = [b[0], b[1], b[0] + 0.01, b[1] + 0.01];
        let hit = |q: [f64; 4]| r[0] <= q[2] && q[0] <= r[2] && r[1] <= q[3] && q[1] <= r[3];
        assert!(!hit(far) && hit(near));
    }

    #[test]
    fn test_tm_bounds_fail_open_outside_certified_band() {
        let tf = CrsTransformer::from_epsg_code(32633).unwrap();
        // 4000 km east of the central meridian.
        let gt = GeoTransform {
            c0: 4_500_000.0,
            a: 100.0,
            b: 0.0,
            f0: 5_000_000.0,
            d: 0.0,
            e: -100.0,
        };
        let b = tf.transform_rect_bounds(&gt, 0.0, 0.0, 100.0, 100.0);
        assert_eq!(b, [-180.0, -90.0, 180.0, 90.0]);
    }

    #[test]
    fn test_tm_certificate_only_for_exact_named_ellipsoid_variants() {
        for crs in [
            "+proj=tmerc +lon_0=9 +approx +ellps=WGS84",
            "+proj=tmerc +lon_0=9 +algo=evenden_snyder +ellps=WGS84",
            "+proj=tmerc +lon_0=9 +R=6371000",
            "+proj=tmerc +lon_0=9 +ellps=sphere",
            "+proj=tmerc +lon_0=9 +a=6378137 +rf=298.257223563",
            "+proj=utm +zone=33 +ellps=intl +towgs84=-6000000,0,0",
            "+proj=utm +zone=33 +ellps=intl +towgs84=0,0,0,0,0,0,5000",
            "+proj=utm +zone=33 +ellps=intl +towgs84=0,0,0,60,0,0,0",
        ] {
            let tf = CrsTransformer::from_crs_or_epsg(None, Some(crs)).unwrap();
            assert!(!tf.bounds_certified(), "{crs}");
        }
        for crs in [
            "+proj=utm +zone=10 +ellps=GRS80",
            "+proj=tmerc +lon_0=9 +datum=WGS84",
            "+proj=utm +zone=33 +ellps=intl +towgs84=-87,-98,-121",
            "+proj=tmerc +lon_0=-2 +datum=OSGB36",
        ] {
            let tf = CrsTransformer::from_crs_or_epsg(None, Some(crs)).unwrap();
            assert!(tf.bounds_certified(), "{crs}");
        }
    }

    #[test]
    fn test_transform_rect_bounds_fail_open_when_a_probe_fails() {
        // Geostationary view: the visible Earth disk has radius ~5.4e6 m. The
        // rectangle reaches past the disk edge, so its outer probes cannot be
        // projected while the inner ones can.
        let tf =
            CrsTransformer::from_proj_string("+proj=geos +h=35785831 +lon_0=0 +R=6378137").unwrap();
        assert!(tf.transform_point(7_000_000.0, 0.0).is_err());
        assert!(tf.transform_point(1_000_000.0, 0.0).is_ok());
        let gt = GeoTransform {
            c0: 0.0,
            a: 100_000.0,
            b: 0.0,
            f0: 1_000_000.0,
            d: 0.0,
            e: -100_000.0,
        };
        let partial = tf.transform_rect_bounds(&gt, 0.0, 0.0, 70.0, 20.0);
        assert_eq!(partial, [-180.0, -90.0, 180.0, 90.0]);

        // Fully inside the disk: ordinary finite bounds.
        let inside = tf.transform_rect_bounds(&gt, 0.0, 0.0, 20.0, 20.0);
        assert!(inside[0] > -1.0 && inside[2] < 30.0, "{inside:?}");
        assert!(inside[1] > -15.0 && inside[3] < 15.0, "{inside:?}");
    }
}
