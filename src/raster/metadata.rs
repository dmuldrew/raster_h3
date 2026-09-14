//! GeoTIFF Metadata, GeoKey Directory, and CRS Extraction
//!
//! Parses TIFF tags and GeoTIFF directory keys to extract affine geotransforms,
//! NoData values, and coordinate reference systems (EPSG codes and PROJ projection strings).

use std::collections::HashMap;
use std::io::{Read, Seek};
use tiff::decoder::Decoder;
use tiff::tags::Tag;

use crate::error::Result;
use crate::raster::geotransform::GeoTransform;

/// Extract affine geotransform from TIFF ModelTransformationTag or ModelTiepointTag / ModelPixelScaleTag
pub fn extract_geotransform<R: Read + Seek>(decoder: &mut Decoder<R>) -> Result<GeoTransform> {
    let matrix_res = decoder
        .get_tag_f64_vec(Tag::ModelTransformationTag)
        .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(34264)));
    if let Ok(matrix) = matrix_res {
        if let Some(gt) = GeoTransform::from_model_transformation(&matrix) {
            return Ok(gt);
        }
    }

    let tiepoint_res = decoder
        .get_tag_f64_vec(Tag::ModelTiepointTag)
        .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(33922)));
    let scale_res = decoder
        .get_tag_f64_vec(Tag::ModelPixelScaleTag)
        .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(33550)));

    if let (Ok(tiepoint), Ok(scale)) = (tiepoint_res, scale_res) {
        if let Some(gt) = GeoTransform::from_tiepoint_and_scale(&tiepoint, &scale) {
            return Ok(gt);
        }
    }

    Ok(GeoTransform::default())
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

/// Extract CRS from GeoKeyDirectoryTag: returns (Option<epsg>, Option<proj_string>)
pub fn extract_crs<R: Read + Seek>(decoder: &mut Decoder<R>) -> (Option<u32>, Option<String>) {
    let keys_res = decoder
        .get_tag_u16_vec(Tag::GeoKeyDirectoryTag)
        .or_else(|_| decoder.get_tag_u16_vec(Tag::Unknown(34735)));

    let mut proj_epsg: Option<u32> = None;
    let mut geo_epsg: Option<u32> = None;
    let mut is_user_defined = false;
    let mut coord_trans: Option<u16> = None;
    let mut double_param_keys = HashMap::new();

    if let Ok(keys) = keys_res {
        if keys.len() >= 4 {
            let num_keys = keys[3] as usize;
            for i in 0..num_keys {
                let offset = 4 + i * 4;
                if offset + 3 < keys.len() {
                    let key_id = keys[offset];
                    let tiff_tag_loc = keys[offset + 1];
                    let _count = keys[offset + 2];
                    let val_or_offset = keys[offset + 3];

                    if tiff_tag_loc == 0 {
                        if key_id == 3072 {
                            if val_or_offset == 32767 {
                                is_user_defined = true;
                            } else if val_or_offset > 0 {
                                proj_epsg = Some(val_or_offset as u32);
                            }
                        } else if key_id == 2048 {
                            if val_or_offset > 0 && val_or_offset != 32767 {
                                geo_epsg = Some(val_or_offset as u32);
                            }
                        } else if key_id == 3075 {
                            coord_trans = Some(val_or_offset);
                        }
                    } else if tiff_tag_loc == 34736 {
                        // Points to GeoDoubleParamsTag index (0-indexed or 1-indexed)
                        double_param_keys.insert(key_id, val_or_offset as usize);
                    }
                }
            }
        }
    }

    if let Some(epsg) = proj_epsg {
        return (Some(epsg), None);
    }

    // If User-Defined or no standard projected EPSG, parse WKT from GeoAsciiParamsTag (34737)
    let ascii_res = decoder
        .get_tag_ascii_string(Tag::GeoAsciiParamsTag)
        .or_else(|_| decoder.get_tag_ascii_string(Tag::Unknown(34737)));

    if let Ok(ascii_str) = ascii_res {
        if let Some(proj_str) = parse_wkt_or_ascii_to_proj(&ascii_str) {
            return (None, Some(proj_str));
        }
    }

    // Fallback to GeoDoubleParamsTag (34736) with GeoKey parameters
    if let Some(trans_id) = coord_trans {
        let doubles_res = decoder
            .get_tag_f64_vec(Tag::GeoDoubleParamsTag)
            .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(34736)));

        if let Ok(doubles) = doubles_res {
            let get_double = |key: u16| -> Option<f64> {
                double_param_keys
                    .get(&key)
                    .and_then(|&idx| doubles.get(idx).copied())
            };

            let datum_str = if geo_epsg == Some(4269) {
                "+datum=NAD83"
            } else {
                "+datum=WGS84"
            };

            if trans_id == 11 {
                // CT_AlbersEqualArea
                let lat_1 = get_double(3078).unwrap_or(0.0);
                let lat_2 = get_double(3079).unwrap_or(0.0);
                let lon_0 = get_double(3080).or_else(|| get_double(3084)).unwrap_or(0.0);
                let lat_0 = get_double(3081).or_else(|| get_double(3085)).unwrap_or(0.0);
                let x_0 = get_double(3082).unwrap_or(0.0);
                let y_0 = get_double(3083).unwrap_or(0.0);

                let p_str = format!(
                    "+proj=aea +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
                    lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum_str
                );
                return (None, Some(p_str));
            } else if trans_id == 8 {
                // CT_LambertConfConic_2SP
                let lat_1 = get_double(3078).unwrap_or(0.0);
                let lat_2 = get_double(3079).unwrap_or(0.0);
                let lon_0 = get_double(3080).or_else(|| get_double(3084)).unwrap_or(0.0);
                let lat_0 = get_double(3081).or_else(|| get_double(3085)).unwrap_or(0.0);
                let x_0 = get_double(3082).unwrap_or(0.0);
                let y_0 = get_double(3083).unwrap_or(0.0);

                let p_str = format!(
                    "+proj=lcc +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
                    lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum_str
                );
                return (None, Some(p_str));
            } else if trans_id == 1 {
                // CT_TransverseMercator
                let scale = get_double(3076).unwrap_or(0.9996);
                let lon_0 = get_double(3080).or_else(|| get_double(3084)).unwrap_or(0.0);
                let lat_0 = get_double(3081).or_else(|| get_double(3085)).unwrap_or(0.0);
                let x_0 = get_double(3082).unwrap_or(500000.0);
                let y_0 = get_double(3083).unwrap_or(0.0);

                let p_str = format!(
                    "+proj=tmerc +lat_0={} +lon_0={} +k={} +x_0={} +y_0={} {} +units=m +no_defs",
                    lat_0, lon_0, scale, x_0, y_0, datum_str
                );
                return (None, Some(p_str));
            }
        }
    }

    if is_user_defined {
        (None, None)
    } else {
        (geo_epsg, None)
    }
}

/// Parse WKT string or ESRI PE string found in GeoAsciiParamsTag (34737) into a PROJ string
pub fn parse_wkt_or_ascii_to_proj(s: &str) -> Option<String> {
    let upper = s.to_uppercase();

    // Determine datum
    let datum = if upper.contains("D_NORTH_AMERICAN_1983")
        || upper.contains("NAD83")
        || upper.contains("GRS_1980")
    {
        "+datum=NAD83"
    } else if upper.contains("D_NORTH_AMERICAN_1927")
        || upper.contains("NAD27")
        || upper.contains("CLARKE_1866")
    {
        "+datum=NAD27"
    } else {
        "+datum=WGS84"
    };

    let extract_param = |name: &str| -> Option<f64> {
        let pattern = format!("PARAMETER[\"{}\",", name.to_uppercase());
        if let Some(pos) = upper.find(&pattern) {
            let start = pos + pattern.len();
            let sub = &s[start..];
            if let Some(end) = sub.find(']') {
                return sub[..end].trim().parse::<f64>().ok();
            }
        }
        None
    };

    if upper.contains("PROJECTION[\"ALBERS\"]") || upper.contains("ALBERS_EQUAL_AREA_CONIC") {
        let lat_1 = extract_param("STANDARD_PARALLEL_1").unwrap_or(0.0);
        let lat_2 = extract_param("STANDARD_PARALLEL_2").unwrap_or(0.0);
        let lat_0 = extract_param("LATITUDE_OF_ORIGIN")
            .or_else(|| extract_param("LATITUDE_OF_CENTER"))
            .unwrap_or(0.0);
        let lon_0 = extract_param("CENTRAL_MERIDIAN")
            .or_else(|| extract_param("LONGITUDE_OF_CENTER"))
            .unwrap_or(0.0);
        let x_0 = extract_param("FALSE_EASTING").unwrap_or(0.0);
        let y_0 = extract_param("FALSE_NORTHING").unwrap_or(0.0);

        return Some(format!(
            "+proj=aea +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
            lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum
        ));
    }

    if upper.contains("PROJECTION[\"LAMBERT_CONFORMAL_CONIC\"]")
        || upper.contains("LAMBERT_CONFORMAL_CONIC")
    {
        let lat_1 = extract_param("STANDARD_PARALLEL_1").unwrap_or(0.0);
        let lat_2 = extract_param("STANDARD_PARALLEL_2").unwrap_or(0.0);
        let lat_0 = extract_param("LATITUDE_OF_ORIGIN")
            .or_else(|| extract_param("LATITUDE_OF_CENTER"))
            .unwrap_or(0.0);
        let lon_0 = extract_param("CENTRAL_MERIDIAN")
            .or_else(|| extract_param("LONGITUDE_OF_CENTER"))
            .unwrap_or(0.0);
        let x_0 = extract_param("FALSE_EASTING").unwrap_or(0.0);
        let y_0 = extract_param("FALSE_NORTHING").unwrap_or(0.0);

        return Some(format!(
            "+proj=lcc +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
            lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum
        ));
    }

    if upper.contains("PROJECTION[\"TRANSVERSE_MERCATOR\"]")
        || upper.contains("TRANSVERSE_MERCATOR")
    {
        let scale = extract_param("SCALE_FACTOR").unwrap_or(0.9996);
        let lat_0 = extract_param("LATITUDE_OF_ORIGIN").unwrap_or(0.0);
        let lon_0 = extract_param("CENTRAL_MERIDIAN").unwrap_or(0.0);
        let x_0 = extract_param("FALSE_EASTING").unwrap_or(500000.0);
        let y_0 = extract_param("FALSE_NORTHING").unwrap_or(0.0);

        return Some(format!(
            "+proj=tmerc +lat_0={} +lon_0={} +k={} +x_0={} +y_0={} {} +units=m +no_defs",
            lat_0, lon_0, scale, x_0, y_0, datum
        ));
    }

    None
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
}
