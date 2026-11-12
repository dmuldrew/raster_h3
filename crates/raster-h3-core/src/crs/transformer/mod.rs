//! Coordinate Reference System (CRS) transformations to WGS84.
//!
//! This module implements a 3-tier CRS transformation hierarchy:
//!
//! - **Tier 1: `Wgs84Identity`** — Zero-cost passthrough for EPSG:4326/4269 data.
//! - **Tier 2: `WebMercatorFast` and `AlbersConic`** — Fast analytical transformations with direct formulas (no PROJ4 overhead).
//! - **Tier 3: `Proj4`** — General-purpose fallback using `proj4rs` for arbitrary CRS.

mod albers;
mod bounds;
mod proj_tokens;
mod tm;

#[cfg(test)]
mod tests;

pub use albers::AlbersConicFast;
pub(crate) use albers::AlbersInverse;
pub(crate) use proj_tokens::tokenize_proj_string;
use proj_tokens::{fast_path_accepts, parse_opt_f64};
pub use tm::TmCertificate;

use crate::error::{RasterH3Error, Result};
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
}
