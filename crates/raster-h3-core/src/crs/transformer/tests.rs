use super::*;
use crate::raster::geotransform::GeoTransform;

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
    let tf =
        CrsTransformer::from_proj_string("+proj=longlat +datum=WGS84 +pm=paris +no_defs").unwrap();
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

#[test]
fn albers_public_source_fields_remain_coherent_after_mutation() {
    let mut albers = AlbersConicFast::epsg_5070();
    // Preserve downstream struct-literal construction using only source fields.
    let southern = AlbersConicFast::with_offsets(-18.0, -36.0, -25.0, 132.0, 12345.0, -9876.0);
    let replacement = AlbersConicFast {
        lat_origin_rad: southern.lat_origin_rad,
        lon_origin_rad: southern.lon_origin_rad,
        x_0: southern.x_0,
        y_0: southern.y_0,
        n: southern.n,
        c: southern.c,
        rho0: southern.rho0,
        e: southern.e,
        e2: southern.e2,
        a: southern.a,
        qp: southern.qp,
    };
    for source in [replacement, AlbersConicFast::new(55.0, 65.0, 50.0, -154.0)] {
        albers.lat_origin_rad = source.lat_origin_rad;
        albers.lon_origin_rad = source.lon_origin_rad;
        albers.x_0 = source.x_0;
        albers.y_0 = source.y_0;
        albers.n = source.n;
        albers.c = source.c;
        albers.rho0 = source.rho0;
        albers.e = source.e;
        albers.e2 = source.e2;
        albers.a = source.a;
        albers.qp = source.qp;
        let lon0 = albers.lon_origin_rad * RAD_TO_DEG;
        let lat0 = albers.lat_origin_rad * RAD_TO_DEG;
        for (lon, lat) in [
            (lon0 + 8.0, lat0 + 5.0),
            (lon0 - 2.0, lat0 - 6.0),
            (lon0, 89.9_f64.copysign(albers.n)),
        ] {
            let (x, y) = albers.project_point(lon, lat);
            let actual = albers.transform_point(x, y);
            assert_eq!(actual, source.transform_point(x, y));
            assert!((actual.0 - lon).abs() < 1e-8, "{actual:?}");
            assert!((actual.1 - lat).abs() < 1e-8, "{actual:?}");
        }
    }
}
