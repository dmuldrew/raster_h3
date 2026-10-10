//! GeoTIFF Metadata, GeoKey Directory, and CRS Extraction
//!
//! Parses TIFF tags and GeoTIFF directory keys to extract affine geotransforms,
//! NoData values, and coordinate reference systems (EPSG codes and PROJ projection strings).

use std::collections::HashMap;
use std::io::{Read, Seek};
use tiff::decoder::Decoder;
use tiff::tags::Tag;

use crate::error::{RasterH3Error, Result};
use crate::raster::geotransform::GeoTransform;

/// Helper: GeoKey 1025 GTRasterTypeGeoKey: 1 = RasterPixelIsArea (default), 2 = RasterPixelIsPoint
#[inline]
fn raster_type_is_point(keys: &[u16]) -> bool {
    keys.as_chunks::<4>()
        .0
        .iter()
        .skip(1)
        .any(|k| k[0] == 1025 && k[1] == 0 && k[3] == 2)
}

/// Extract affine geotransform from TIFF ModelTransformationTag or ModelTiepointTag / ModelPixelScaleTag.
/// If GeoKey 1025 (`GTRasterTypeGeoKey`) indicates `RasterPixelIsPoint` (2), the origin is shifted
/// by `-0.5 * (a + b)` and `-0.5 * (d + e)` to conform to GDAL's `PixelIsArea` convention.
pub fn extract_geotransform<R: Read + Seek>(decoder: &mut Decoder<R>) -> Result<GeoTransform> {
    let matrix_res = decoder
        .get_tag_f64_vec(Tag::ModelTransformationTag)
        .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(34264)));
    let mut gt = if let Ok(matrix) = matrix_res {
        if let Some(gt) = GeoTransform::from_model_transformation(&matrix) {
            gt
        } else {
            return Err(RasterH3Error::InvalidParameter(
                "GeoTIFF has no georeferencing tags (ModelTransformationTag or ModelTiepointTag+ModelPixelScaleTag); cannot map pixels to coordinates".into(),
            ));
        }
    } else {
        let tiepoint_res = decoder
            .get_tag_f64_vec(Tag::ModelTiepointTag)
            .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(33922)));
        let scale_res = decoder
            .get_tag_f64_vec(Tag::ModelPixelScaleTag)
            .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(33550)));

        if let (Ok(tiepoint), Ok(scale)) = (tiepoint_res, scale_res) {
            if let Some(gt) = GeoTransform::from_tiepoint_and_scale(&tiepoint, &scale) {
                gt
            } else {
                return Err(RasterH3Error::InvalidParameter(
                    "GeoTIFF has no georeferencing tags (ModelTransformationTag or ModelTiepointTag+ModelPixelScaleTag); cannot map pixels to coordinates".into(),
                ));
            }
        } else {
            return Err(RasterH3Error::InvalidParameter(
                "GeoTIFF has no georeferencing tags (ModelTransformationTag or ModelTiepointTag+ModelPixelScaleTag); cannot map pixels to coordinates".into(),
            ));
        }
    };

    // GeoKey 1025 GTRasterTypeGeoKey: 1 = PixelIsArea (default), 2 = PixelIsPoint
    let keys_res = decoder
        .get_tag_u16_vec(Tag::GeoKeyDirectoryTag)
        .or_else(|_| decoder.get_tag_u16_vec(Tag::Unknown(34735)));
    if let Ok(keys) = keys_res {
        if raster_type_is_point(&keys) {
            // Tiepoint refers to the pixel center: move the origin to the pixel corner.
            gt.c0 -= 0.5 * (gt.a + gt.b);
            gt.f0 -= 0.5 * (gt.d + gt.e);
        }
    }

    Ok(gt)
}

/// Extract NoData value from tag 42113 / GdalNodata
pub fn extract_nodata<R: Read + Seek>(decoder: &mut Decoder<R>) -> Option<f64> {
    let s_res = decoder
        .get_tag_ascii_string(Tag::GdalNodata)
        .or_else(|_| decoder.get_tag_ascii_string(Tag::Unknown(42113)));
    if let Ok(s) = s_res {
        if let Ok(val) = s.trim().parse::<f64>() {
            return Some(val);
        }
    }
    None
}

// GeoKey IDs (OGC GeoTIFF 1.1, 19-008r4).
const KEY_GEODETIC_CRS: u16 = 2048;
const KEY_GEODETIC_DATUM: u16 = 2050;
const KEY_PRIME_MERIDIAN: u16 = 2051;
const KEY_GEOG_ANGULAR_UNITS: u16 = 2054;
const KEY_ELLIPSOID: u16 = 2056;
const KEY_PROJECTED_CRS: u16 = 3072;
const KEY_PROJ_METHOD: u16 = 3075;
const KEY_PROJ_LINEAR_UNITS: u16 = 3076;
const KEY_PROJ_LINEAR_UNIT_SIZE: u16 = 3077;
const KEY_STD_PARALLEL_1: u16 = 3078;
const KEY_STD_PARALLEL_2: u16 = 3079;
const KEY_NAT_ORIGIN_LONG: u16 = 3080;
const KEY_NAT_ORIGIN_LAT: u16 = 3081;
const KEY_FALSE_EASTING: u16 = 3082;
const KEY_FALSE_NORTHING: u16 = 3083;
const KEY_FALSE_ORIGIN_LONG: u16 = 3084;
const KEY_FALSE_ORIGIN_LAT: u16 = 3085;
const KEY_FALSE_ORIGIN_EASTING: u16 = 3086;
const KEY_FALSE_ORIGIN_NORTHING: u16 = 3087;
const KEY_SCALE_AT_NAT_ORIGIN: u16 = 3092;
const USER_DEFINED: u16 = 32767;

/// Projected linear unit, rendered as PROJ `+units`/`+to_meter`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LinearUnit {
    Metre,
    Foot,
    UsSurveyFoot,
    /// Metres per unit.
    Custom(f64),
}

impl LinearUnit {
    fn to_meter(self) -> f64 {
        match self {
            Self::Metre => 1.0,
            Self::Foot => 0.3048,
            Self::UsSurveyFoot => 1200.0 / 3937.0,
            Self::Custom(m) => m,
        }
    }

    fn from_factor(m: f64) -> Option<Self> {
        if !(m.is_finite() && m > 0.0) {
            return None;
        }
        Some(if (m - 1.0).abs() < 1e-12 {
            Self::Metre
        } else if (m - 0.3048).abs() < 1e-12 {
            Self::Foot
        } else if (m - 1200.0 / 3937.0).abs() < 1e-12 {
            Self::UsSurveyFoot
        } else {
            Self::Custom(m)
        })
    }

    fn proj(self) -> String {
        match self {
            Self::Metre => "+units=m".into(),
            Self::Foot => "+units=ft".into(),
            Self::UsSurveyFoot => "+units=us-ft".into(),
            Self::Custom(m) => format!("+to_meter={}", m),
        }
    }
}

/// Build a projected PROJ string. `false_en` is in `unit`s, as GeoTIFF and
/// WKT store it; PROJ expects `+x_0`/`+y_0` in metres.
fn projected_proj_string(
    proj: &str,
    params: &[(&str, f64)],
    false_en: (f64, f64),
    datum: &str,
    unit: LinearUnit,
) -> String {
    let mut out = format!("+proj={}", proj);
    for (k, v) in params {
        out.push_str(&format!(" +{}={}", k, v));
    }
    let m = unit.to_meter();
    out.push_str(&format!(
        " +x_0={} +y_0={} {} {} +no_defs",
        false_en.0 * m,
        false_en.1 * m,
        datum,
        unit.proj()
    ));
    out
}

/// Extract CRS from GeoKeyDirectoryTag: returns (`Option<epsg>`, `Option<proj_string>`).
///
/// User-defined projections are reconstructed only for a narrow, fully
/// specified subset: Transverse Mercator, Lambert Conformal Conic (2SP) and
/// Albers Equal Area on a WGS84 or NAD83 geodetic CRS, Greenwich meridian,
/// degree angular units and a known linear unit. Anything incomplete or
/// outside that subset yields `(None, None)` so the caller must supply the
/// CRS explicitly rather than receive a silently wrong definition.
pub fn extract_crs<R: Read + Seek>(decoder: &mut Decoder<R>) -> (Option<u32>, Option<String>) {
    let keys_res = decoder
        .get_tag_u16_vec(Tag::GeoKeyDirectoryTag)
        .or_else(|_| decoder.get_tag_u16_vec(Tag::Unknown(34735)));

    let mut short_keys: HashMap<u16, u16> = HashMap::new();
    let mut double_param_keys = HashMap::new();

    if let Ok(keys) = keys_res {
        if keys.len() >= 4 {
            let num_keys = keys[3] as usize;
            for i in 0..num_keys {
                let offset = 4 + i * 4;
                if offset + 3 < keys.len() {
                    let key_id = keys[offset];
                    let tiff_tag_loc = keys[offset + 1];
                    let val_or_offset = keys[offset + 3];

                    if tiff_tag_loc == 0 {
                        short_keys.insert(key_id, val_or_offset);
                    } else if tiff_tag_loc == 34736 {
                        // Index into GeoDoubleParamsTag
                        double_param_keys.insert(key_id, val_or_offset as usize);
                    }
                }
            }
        }
    }

    let code = |key: u16| {
        short_keys
            .get(&key)
            .copied()
            .filter(|&v| v > 0 && v != USER_DEFINED)
    };
    if let Some(epsg) = code(KEY_PROJECTED_CRS) {
        return (Some(epsg as u32), None);
    }
    let geo_epsg = code(KEY_GEODETIC_CRS).map(u32::from);
    let is_projected =
        short_keys.contains_key(&KEY_PROJECTED_CRS) || short_keys.contains_key(&KEY_PROJ_METHOD);

    if is_projected {
        let doubles = decoder
            .get_tag_f64_vec(Tag::GeoDoubleParamsTag)
            .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(34736)))
            .unwrap_or_default();
        let get_double = |key: u16| -> Option<f64> {
            double_param_keys
                .get(&key)
                .and_then(|&idx| doubles.get(idx).copied())
        };
        if let Some(p) = user_defined_geokeys_to_proj(&short_keys, get_double) {
            return (None, Some(p));
        }
    }

    // Fall back to a WKT / ESRI PE definition in GeoAsciiParamsTag (34737).
    let ascii_res = decoder
        .get_tag_ascii_string(Tag::GeoAsciiParamsTag)
        .or_else(|_| decoder.get_tag_ascii_string(Tag::Unknown(34737)));
    if let Ok(ascii_str) = ascii_res {
        if let Some(proj_str) = parse_wkt_or_ascii_to_proj(&ascii_str) {
            return (None, Some(proj_str));
        }
    }

    if is_projected {
        // Never treat projected coordinates as geographic.
        (None, None)
    } else {
        (geo_epsg, None)
    }
}

/// Reconstruct a user-defined projected CRS from GeoKeys, or None when the
/// definition is incomplete or unsupported (see [`extract_crs`]).
fn user_defined_geokeys_to_proj(
    short_keys: &HashMap<u16, u16>,
    get_double: impl Fn(u16) -> Option<f64>,
) -> Option<String> {
    let short = |key: u16| short_keys.get(&key).copied();

    // Geodetic datum: an EPSG geodetic CRS, datum or ellipsoid we can name.
    let datum = match (
        short(KEY_GEODETIC_CRS),
        short(KEY_GEODETIC_DATUM),
        short(KEY_ELLIPSOID),
    ) {
        (Some(4326), _, _) | (None | Some(USER_DEFINED), Some(6326), _) => "+datum=WGS84",
        (Some(4269), _, _) | (None | Some(USER_DEFINED), Some(6269), _) => "+datum=NAD83",
        _ => return None,
    };
    if short(KEY_PRIME_MERIDIAN).is_some_and(|pm| pm != 8901) {
        return None;
    }
    if short(KEY_GEOG_ANGULAR_UNITS).is_some_and(|u| u != 9102) {
        return None;
    }
    let unit = match short(KEY_PROJ_LINEAR_UNITS) {
        None | Some(9001) => LinearUnit::Metre,
        Some(9002) => LinearUnit::Foot,
        Some(9003) => LinearUnit::UsSurveyFoot,
        Some(USER_DEFINED) => LinearUnit::from_factor(get_double(KEY_PROJ_LINEAR_UNIT_SIZE)?)?,
        Some(_) => return None,
    };

    let fe = |primary: u16, legacy: u16| {
        get_double(primary)
            .or_else(|| get_double(legacy))
            .unwrap_or(0.0)
    };
    match short(KEY_PROJ_METHOD)? {
        // CT_TransverseMercator
        1 => Some(projected_proj_string(
            "tmerc",
            &[
                ("lat_0", get_double(KEY_NAT_ORIGIN_LAT)?),
                ("lon_0", get_double(KEY_NAT_ORIGIN_LONG)?),
                ("k", get_double(KEY_SCALE_AT_NAT_ORIGIN)?),
            ],
            (
                get_double(KEY_FALSE_EASTING).unwrap_or(0.0),
                get_double(KEY_FALSE_NORTHING).unwrap_or(0.0),
            ),
            datum,
            unit,
        )),
        // CT_LambertConfConic_2SP and CT_AlbersEqualArea define their origin
        // with the FalseOrigin keys; older writers use the NatOrigin keys.
        method @ (8 | 11) => Some(projected_proj_string(
            if method == 8 { "lcc" } else { "aea" },
            &[
                ("lat_1", get_double(KEY_STD_PARALLEL_1)?),
                ("lat_2", get_double(KEY_STD_PARALLEL_2)?),
                (
                    "lat_0",
                    get_double(KEY_FALSE_ORIGIN_LAT).or_else(|| get_double(KEY_NAT_ORIGIN_LAT))?,
                ),
                (
                    "lon_0",
                    get_double(KEY_FALSE_ORIGIN_LONG)
                        .or_else(|| get_double(KEY_NAT_ORIGIN_LONG))?,
                ),
            ],
            (
                fe(KEY_FALSE_ORIGIN_EASTING, KEY_FALSE_EASTING),
                fe(KEY_FALSE_ORIGIN_NORTHING, KEY_FALSE_NORTHING),
            ),
            datum,
            unit,
        )),
        _ => None,
    }
}

/// Parse a WKT1 / ESRI PE `PROJCS[...]` string (as found in GeoAsciiParamsTag)
/// into a PROJ string.
///
/// This is deliberately a narrow parser, not a general WKT implementation:
/// it accepts only Albers, Lambert Conformal Conic and Transverse Mercator on
/// a WGS 1984 or NAD83 datum with a Greenwich prime meridian, degree angular
/// units and all defining parameters present. Anything else returns None.
pub fn parse_wkt_or_ascii_to_proj(s: &str) -> Option<String> {
    let upper = s.trim().to_uppercase();
    if !upper.starts_with("PROJCS[") {
        return None;
    }

    // Datum by name only; ellipsoid names (e.g. GRS 1980) are shared by
    // several datums and do not identify one.
    let datum_name = section_name(&upper, "DATUM[")?;
    let datum = match datum_name.replace([' ', '-'], "_").as_str() {
        "WGS_1984" | "D_WGS_1984" | "WGS84" => "+datum=WGS84",
        "NORTH_AMERICAN_DATUM_1983" | "D_NORTH_AMERICAN_1983" | "NAD83" => "+datum=NAD83",
        _ => return None,
    };

    if let Some(pm) = section_value(&upper, "PRIMEM[") {
        if pm != 0.0 {
            return None;
        }
    }

    // UNIT[...] inside GEOGCS is angular; the last UNIT is the projected one.
    let angular = section_value(&upper, "UNIT[")?;
    if (angular - std::f64::consts::PI / 180.0).abs() > 1e-10 {
        return None;
    }
    let linear = upper
        .rfind("UNIT[")
        .and_then(|i| section_value(&upper[i..], "UNIT["))
        .and_then(LinearUnit::from_factor)?;

    let param = |name: &str| -> Option<f64> {
        let pattern = format!("PARAMETER[\"{}\",", name);
        let start = upper.find(&pattern)? + pattern.len();
        let sub = &upper[start..];
        sub[..sub.find(']')?].trim().parse::<f64>().ok()
    };
    let either = |a: &str, b: &str| param(a).or_else(|| param(b));
    let false_en = (param("FALSE_EASTING")?, param("FALSE_NORTHING")?);

    let projection = section_name(&upper, "PROJECTION[")?;
    let (proj, params) = match projection.as_str() {
        "ALBERS" | "ALBERS_CONIC_EQUAL_AREA" | "ALBERS_EQUAL_AREA_CONIC" => (
            "aea",
            vec![
                ("lat_1", param("STANDARD_PARALLEL_1")?),
                ("lat_2", param("STANDARD_PARALLEL_2")?),
                ("lat_0", either("LATITUDE_OF_ORIGIN", "LATITUDE_OF_CENTER")?),
                ("lon_0", either("CENTRAL_MERIDIAN", "LONGITUDE_OF_CENTER")?),
            ],
        ),
        "LAMBERT_CONFORMAL_CONIC" | "LAMBERT_CONFORMAL_CONIC_2SP" => (
            "lcc",
            vec![
                ("lat_1", param("STANDARD_PARALLEL_1")?),
                ("lat_2", param("STANDARD_PARALLEL_2")?),
                ("lat_0", either("LATITUDE_OF_ORIGIN", "LATITUDE_OF_CENTER")?),
                ("lon_0", either("CENTRAL_MERIDIAN", "LONGITUDE_OF_CENTER")?),
            ],
        ),
        "TRANSVERSE_MERCATOR" => (
            "tmerc",
            vec![
                ("lat_0", param("LATITUDE_OF_ORIGIN")?),
                ("lon_0", param("CENTRAL_MERIDIAN")?),
                ("k", param("SCALE_FACTOR")?),
            ],
        ),
        _ => return None,
    };

    Some(projected_proj_string(
        proj, &params, false_en, datum, linear,
    ))
}

/// The quoted name of the first `tag` section, e.g. `DATUM["WGS_1984",...`.
fn section_name(upper: &str, tag: &str) -> Option<String> {
    let rest = &upper[upper.find(tag)? + tag.len()..];
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

/// The first numeric value of the first `tag` section, e.g. `UNIT["metre",1]`.
fn section_value(upper: &str, tag: &str) -> Option<f64> {
    let rest = &upper[upper.find(tag)? + tag.len()..];
    let rest = rest.strip_prefix('"')?;
    let rest = &rest[rest.find('"')? + 1..];
    let rest = rest.strip_prefix(',')?;
    let end = rest.find([',', ']'])?;
    rest[..end].trim().parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wkt_albers() {
        let wkt = r#"PROJCS["Albers_Conic_Equal_Area",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Albers"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-96.0],PARAMETER["Standard_Parallel_1",29.5],PARAMETER["Standard_Parallel_2",45.5],PARAMETER["Latitude_Of_Origin",23.0],UNIT["Meter",1.0]]"#;
        let proj = parse_wkt_or_ascii_to_proj(wkt).expect("Failed to parse Albers WKT");
        assert!(proj.contains("+proj=aea"));
        assert!(proj.contains("+lat_1=29.5"));
        assert!(proj.contains("+lat_2=45.5"));
        assert!(proj.contains("+lon_0=-96"));
        assert!(proj.contains("+datum=NAD83"));
    }

    #[test]
    fn test_parse_wkt_utm() {
        let wkt = r#"PROJCS["WGS 84 / UTM zone 10N",GEOGCS["WGS 84",DATUM["WGS_1984"],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-123],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1]]"#;
        let proj = parse_wkt_or_ascii_to_proj(wkt).expect("Failed to parse UTM WKT");
        assert!(proj.contains("+proj=tmerc"));
        assert!(proj.contains("+lon_0=-123"));
        assert!(proj.contains("+k=0.9996"));
        assert!(proj.contains("+x_0=500000"));
        assert!(proj.contains("+datum=WGS84"));
    }

    #[test]
    fn test_raster_type_is_point_detection() {
        // Standard header + key 1025 = 2 (RasterPixelIsPoint)
        let keys_point = [1, 1, 0, 1, 1025, 0, 1, 2];
        assert!(raster_type_is_point(&keys_point));

        // Key 1025 = 1 (RasterPixelIsArea)
        let keys_area = [1, 1, 0, 1, 1025, 0, 1, 1];
        assert!(!raster_type_is_point(&keys_area));

        // Different key
        let keys_other = [1, 1, 0, 1, 1024, 0, 1, 1];
        assert!(!raster_type_is_point(&keys_other));
    }

    #[test]
    fn test_extract_geotransform_pixel_is_point() {
        use std::io::Cursor;
        use tiff::encoder::{colortype, TiffEncoder};

        let mut buffer = Vec::new();
        {
            let mut encoder = TiffEncoder::new(Cursor::new(&mut buffer)).unwrap();
            let mut image = encoder.new_image::<colortype::Gray32Float>(2, 2).unwrap();

            // Tiepoint: I=0, J=0, K=0, X=100.0, Y=200.0, Z=0.0
            let tiepoint = [0.0, 0.0, 0.0, 100.0, 200.0, 0.0];
            let pixel_scale = [10.0, 10.0, 0.0];

            // GTRasterTypeGeoKey (1025) = 2 (RasterPixelIsPoint)
            let geokeys: [u16; 8] = [1, 1, 0, 1, 1025, 0, 1, 2];

            image
                .encoder()
                .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
                .unwrap();
            image
                .encoder()
                .write_tag(Tag::ModelPixelScaleTag, &pixel_scale[..])
                .unwrap();
            image
                .encoder()
                .write_tag(Tag::Unknown(34735), &geokeys[..])
                .unwrap();

            let data = vec![1.0f32; 4];
            image.write_data(&data).unwrap();
        }

        let mut decoder = Decoder::new(Cursor::new(buffer)).unwrap();
        let gt = extract_geotransform(&mut decoder).unwrap();

        // Scale: a = 10.0, e = -10.0
        assert_eq!(gt.a, 10.0);
        assert_eq!(gt.e, -10.0);

        // In PixelIsPoint: original tiepoint (100, 200) was at pixel center (0.5, 0.5)
        // With origin shifted by -0.5*(a+b) and -0.5*(d+e):
        // c0 = 100.0 - 0.5*10.0 = 95.0
        // f0 = 200.0 - 0.5*(-10.0) = 205.0
        assert_eq!(gt.c0, 95.0);
        assert_eq!(gt.f0, 205.0);

        // Crucial invariant: pixel_center_to_coord(0, 0) MUST map to the tiepoint (100.0, 200.0)!
        let (cx, cy) = gt.pixel_center_to_coord(0, 0);
        assert_eq!(cx, 100.0);
        assert_eq!(cy, 200.0);
    }

    #[test]
    fn test_extract_geotransform_missing_tags_error() {
        use std::io::Cursor;
        use tiff::encoder::{colortype, TiffEncoder};

        let mut buffer = Vec::new();
        {
            let mut encoder = TiffEncoder::new(Cursor::new(&mut buffer)).unwrap();
            let image = encoder.new_image::<colortype::Gray32Float>(2, 2).unwrap();
            let data = vec![1.0f32; 4];
            image.write_data(&data).unwrap();
        }

        let mut decoder = Decoder::new(Cursor::new(buffer)).unwrap();
        let res = extract_geotransform(&mut decoder);
        assert!(
            res.is_err(),
            "Expected error when georeferencing tags are missing"
        );
        match res.err().unwrap() {
            RasterH3Error::InvalidParameter(msg) => {
                assert!(
                    msg.contains("no georeferencing tags"),
                    "Expected 'no georeferencing tags', got: {}",
                    msg
                );
            }
            other => panic!("Expected InvalidParameter, got {:?}", other),
        }
    }

    #[test]
    fn test_extract_geotransform_degenerate_zero_scale_error() {
        use std::io::Cursor;
        use tiff::encoder::{colortype, TiffEncoder};

        let mut buffer = Vec::new();
        {
            let mut encoder = TiffEncoder::new(Cursor::new(&mut buffer)).unwrap();
            let mut image = encoder.new_image::<colortype::Gray32Float>(2, 2).unwrap();

            let tiepoint = [0.0, 0.0, 0.0, 100.0, 200.0, 0.0];
            let pixel_scale = [0.0, 0.0, 0.0]; // degenerate scale == 0

            image
                .encoder()
                .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
                .unwrap();
            image
                .encoder()
                .write_tag(Tag::ModelPixelScaleTag, &pixel_scale[..])
                .unwrap();

            let data = vec![1.0f32; 4];
            image.write_data(&data).unwrap();
        }

        let mut decoder = Decoder::new(Cursor::new(buffer)).unwrap();
        let res = extract_geotransform(&mut decoder);
        assert!(res.is_err(), "Expected error when scale is zero");
    }

    /// Encode a 1x1 GeoTIFF with the given short GeoKeys and double GeoKeys.
    fn decoder_with_geokeys(
        shorts: &[(u16, u16)],
        doubles: &[(u16, f64)],
    ) -> Decoder<std::io::Cursor<Vec<u8>>> {
        use std::io::Cursor;
        use tiff::encoder::{colortype, TiffEncoder};

        let mut dir: Vec<u16> = vec![1, 1, 0, (shorts.len() + doubles.len()) as u16];
        for &(k, v) in shorts {
            dir.extend([k, 0, 1, v]);
        }
        for (i, &(k, _)) in doubles.iter().enumerate() {
            dir.extend([k, 34736, 1, i as u16]);
        }
        let values: Vec<f64> = doubles.iter().map(|d| d.1).collect();

        let mut buffer = Vec::new();
        {
            let mut encoder = TiffEncoder::new(Cursor::new(&mut buffer)).unwrap();
            let mut image = encoder.new_image::<colortype::Gray32Float>(1, 1).unwrap();
            image
                .encoder()
                .write_tag(Tag::Unknown(34735), &dir[..])
                .unwrap();
            if !values.is_empty() {
                image
                    .encoder()
                    .write_tag(Tag::Unknown(34736), &values[..])
                    .unwrap();
            }
            image.write_data(&[0.0f32]).unwrap();
        }
        Decoder::new(Cursor::new(buffer)).unwrap()
    }

    #[test]
    fn test_user_defined_tm_reads_scale_from_key_3092() {
        // Audit probe: key 3092 = 0.9 must not be replaced by 0.9996, and
        // key 3076 (linear units) must not be read as the scale.
        let mut dec = decoder_with_geokeys(
            &[(3072, 32767), (2048, 4326), (3075, 1), (3076, 9001)],
            &[
                (3080, 9.0),
                (3081, 0.0),
                (3092, 0.9),
                (3082, 500000.0),
                (3083, 0.0),
            ],
        );
        let (epsg, proj) = extract_crs(&mut dec);
        assert_eq!(epsg, None);
        let proj = proj.unwrap();
        assert!(proj.contains("+k=0.9 "), "{proj}");
        assert!(proj.contains("+units=m"), "{proj}");
    }

    #[test]
    fn test_user_defined_feet_converts_false_origin_to_metres() {
        let mut dec = decoder_with_geokeys(
            &[(3072, 32767), (2048, 4269), (3075, 1), (3076, 9003)],
            &[
                (3080, -120.0),
                (3081, 0.0),
                (3092, 1.0),
                (3082, 1_000_000.0),
                (3083, 0.0),
            ],
        );
        let proj = extract_crs(&mut dec).1.unwrap();
        assert!(proj.contains("+units=us-ft"), "{proj}");
        assert!(proj.contains("+datum=NAD83"), "{proj}");
        let x0: f64 = proj
            .split("+x_0=")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!((x0 - 1_000_000.0 * 1200.0 / 3937.0).abs() < 1e-6, "{proj}");
    }

    #[test]
    fn test_user_defined_incomplete_or_unknown_datum_is_rejected() {
        // Missing scale factor.
        let mut dec = decoder_with_geokeys(
            &[(3072, 32767), (2048, 4326), (3075, 1)],
            &[(3080, 9.0), (3081, 0.0)],
        );
        assert_eq!(extract_crs(&mut dec), (None, None));
        // NAD27 geodetic CRS: datum shift unsupported, so no CRS is guessed.
        let mut dec = decoder_with_geokeys(
            &[(3072, 32767), (2048, 4267), (3075, 1)],
            &[(3080, 9.0), (3081, 0.0), (3092, 0.9996)],
        );
        assert_eq!(extract_crs(&mut dec), (None, None));
        // Unsupported method must not fall back to the geographic code.
        let mut dec = decoder_with_geokeys(&[(3072, 32767), (2048, 4326), (3075, 7)], &[]);
        assert_eq!(extract_crs(&mut dec), (None, None));
    }

    #[test]
    fn test_parse_wkt_preserves_feet_and_rejects_unknown_datums() {
        let wkt = r#"PROJCS["NAD83 / custom ft",GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",31],PARAMETER["central_meridian",-110.1666666666667],PARAMETER["scale_factor",0.9999],PARAMETER["false_easting",700000],PARAMETER["false_northing",0],UNIT["US survey foot",0.3048006096012192]]"#;
        let proj = parse_wkt_or_ascii_to_proj(wkt).unwrap();
        assert!(proj.contains("+units=us-ft"), "{proj}");
        assert!(proj.contains("+x_0=213360.42"), "{proj}");

        let osgb = wkt.replace("North_American_Datum_1983", "OSGB_1936");
        assert_eq!(parse_wkt_or_ascii_to_proj(&osgb), None);
        let paris = wkt.replace(r#"PRIMEM["Greenwich",0]"#, r#"PRIMEM["Paris",2.33722917]"#);
        assert_eq!(parse_wkt_or_ascii_to_proj(&paris), None);
        let no_scale = wkt.replace(r#"PARAMETER["scale_factor",0.9999],"#, "");
        assert_eq!(parse_wkt_or_ascii_to_proj(&no_scale), None);
    }
}
