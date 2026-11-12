//! Closed-form Albers Equal Area Conic projection (tier 2 fast path).

use super::proj_tokens::{
    fast_path_accepts, is_all_zero_towgs84, parse_opt_f64, tokenize_proj_string,
};
use super::{wrap_lon, RAD_TO_DEG};

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
}

/// Inverse constants tied to an immutable projection borrow, so they cannot go stale.
pub(crate) struct AlbersInverse<'a> {
    albers: &'a AlbersConicFast,
    inv_n: f64,
    n_over_a_sq: f64,
    inv_qp: f64,
    apa: [f64; 4],
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
        self.prepare_inverse().transform_point(x, y)
    }

    /// Row-invariant inverse terms, including false northing and signed cone distance.
    #[inline(always)]
    pub fn row_yp(&self, y: f64) -> (f64, f64) {
        let y_p = self.rho0 - (y - self.y_0);
        (y_p, y_p * y_p)
    }

    /// Analytical inverse transformation when `y_p = rho0 - (y - y_0)` and `y_p_sq = y_p * y_p`
    /// have already been hoisted for a constant-Y scanline row.
    #[inline(always)]
    pub fn transform_with_yp(&self, x: f64, y_p: f64, y_p_sq: f64) -> (f64, f64) {
        self.prepare_inverse().transform_with_yp(x, y_p, y_p_sq)
    }

    pub(crate) fn prepare_inverse(&self) -> AlbersInverse<'_> {
        let e2 = self.e2;
        let e4 = e2 * e2;
        let e6 = e4 * e2;
        let e8 = e4 * e4;
        // Snyder 1987 eq. 3-16 + e^8: beta + sum(p_k * sin(2*k*beta)).
        let p1 = (1.0 / 3.0) * e2
            + (31.0 / 180.0) * e4
            + (517.0 / 5040.0) * e6
            + (120389.0 / 1814400.0) * e8;
        let p2 = (23.0 / 360.0) * e4 + (251.0 / 3780.0) * e6 + (102283.0 / 1814400.0) * e8;
        let p3 = (761.0 / 45360.0) * e6 + (47561.0 / 1814400.0) * e8;
        let p4 = (6059.0 / 1209600.0) * e8;
        let n_over_a = self.n / self.a;
        AlbersInverse {
            albers: self,
            inv_n: 1.0 / self.n,
            n_over_a_sq: n_over_a * n_over_a,
            inv_qp: 1.0 / self.qp,
            apa: [p1 - p3, 2.0 * p2 - 4.0 * p4, 4.0 * p3, 8.0 * p4],
        }
    }
}

impl AlbersInverse<'_> {
    #[inline(always)]
    pub(crate) fn row_yp(&self, y: f64) -> (f64, f64) {
        self.albers.row_yp(y)
    }

    #[inline(always)]
    pub(crate) fn transform_point(&self, x: f64, y: f64) -> (f64, f64) {
        let (y_p, y_p_sq) = self.row_yp(y);
        self.transform_with_yp(x, y_p, y_p_sq)
    }

    #[inline(always)]
    pub(crate) fn transform_with_yp(&self, x: f64, y_p: f64, y_p_sq: f64) -> (f64, f64) {
        let albers = self.albers;
        let x_p = x - albers.x_0;
        let rho_sq = x_p * x_p + y_p_sq;
        let theta = if albers.n >= 0.0 {
            x_p.atan2(y_p)
        } else {
            (-x_p).atan2(-y_p)
        };

        let lon_rad = albers.lon_origin_rad + theta * self.inv_n;
        let q = (albers.c - rho_sq * self.n_over_a_sq) * self.inv_n;

        // 4-term Chebyshev Horner series from authalic sin(beta) = q / qp to geodetic phi
        let sin_beta = (q * self.inv_qp).clamp(-1.0, 1.0);
        let sin2_beta = sin_beta * sin_beta;
        let cos_beta = (1.0 - sin2_beta).sqrt();
        let s2 = 2.0 * sin_beta * cos_beta;
        let c2 = 1.0 - 2.0 * sin2_beta;
        let poly = ((self.apa[3] * c2 + self.apa[2]) * c2 + self.apa[1]) * c2 + self.apa[0];
        let phi = s2 * poly + sin_beta.asin();

        let lon_deg = lon_rad * RAD_TO_DEG;
        let lat_deg = phi * RAD_TO_DEG;
        (lon_deg, lat_deg)
    }
}
