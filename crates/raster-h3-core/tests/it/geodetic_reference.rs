//! Geodetic reference tests for the raster → H3 pipeline.
//!
//! 1. Golden points for CRS fast paths and Proj4 fallbacks against PROJ 9.5.1, ≤ 0.01 m.
//! 2. Affine round-trip `coord_to_pixel(pixel_to_coord(p)) == p` for rotated/sheared grids.
//! 3. Coverage / duplicate property tests on synthetic rasters that exercise the streaming
//!    eviction horizon: the res 2–4 under-reach band, a 0–360° longitude raster, and a
//!    polar-stereographic raster containing the pole.
//! 4. WKB buffer capacity across every resolution for pentagons and icosahedron-edge cells.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use h3o::{CellIndex, LatLng, Resolution};
use tempfile::TempDir;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::crs::transformer::CrsTransformer;
use raster_h3_core::encoding::{cell_to_wkb, WkbBuf, WKB_BUF_LEN};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::geotransform::GeoTransform;
use raster_h3_core::raster::mosaic::{MosaicReader, OverlapRule};

// ---------------------------------------------------------------------------------------------
// 1. Golden points vs PROJ
// ---------------------------------------------------------------------------------------------

const WGS84_A: f64 = 6_378_137.0;
const MAX_ERR_M: f64 = 0.01;

fn wrap_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// Ground distance (m) between two lon/lat pairs; conservative (uses the semi-major axis).
fn ground_error_m(lon: f64, lat: f64, ref_lon: f64, ref_lat: f64) -> f64 {
    let d_lat = (lat - ref_lat).to_radians() * WGS84_A;
    let d_lon = wrap_lon(lon - ref_lon).to_radians() * WGS84_A * ref_lat.to_radians().cos();
    (d_lat * d_lat + d_lon * d_lon).sqrt()
}

/// Golden values generated with PROJ 9.5.1:
///   `printf "x y\n" | cs2cs -f "%.10f" <src> +to +proj=longlat <ellps/datum of src> +no_defs`
/// The reference target uses the *same* ellipsoid as the source so that only the projection
/// inverse is compared (no datum shift is applied by the engine either).
/// ((x, y) in the source CRS, (lon, lat) from cs2cs)
type GoldenCase = ((f64, f64), (f64, f64));

fn assert_golden(tf: &CrsTransformer, name: &str, cases: &[GoldenCase]) {
    for &((x, y), (ref_lon, ref_lat)) in cases {
        let (lon, lat) = tf
            .transform_point(x, y)
            .unwrap_or_else(|e| panic!("{name}: transform_point({x}, {y}) failed: {e:?}"));
        let err = ground_error_m(lon, lat, ref_lon, ref_lat);
        assert!(
            err <= MAX_ERR_M,
            "{name}: ({x}, {y}) -> ({lon:.10}, {lat:.10}) vs PROJ ({ref_lon:.10}, {ref_lat:.10}); error {err:.5} m > {MAX_ERR_M} m"
        );
    }
}

fn assert_golden_inverse(tf: &CrsTransformer, name: &str, cases: &[GoldenCase]) {
    for &((lon, lat), (ref_x, ref_y)) in cases {
        let (x, y) = tf
            .wgs84_to_source(lon, lat)
            .unwrap_or_else(|e| panic!("{name}: wgs84_to_source({lon}, {lat}) failed: {e:?}"));
        let err = (x - ref_x).hypot(y - ref_y);
        assert!(
            err <= 0.01,
            "{name}: ({lon}, {lat}) -> ({x:.4}, {y:.4}) vs PROJ ({ref_x:.4}, {ref_y:.4}); error {err:.5} m > 0.01 m"
        );
    }
}

#[test]
fn golden_web_mercator_fast_path() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
    assert!(matches!(tf, CrsTransformer::WebMercatorFast));
    // cs2cs EPSG:3857 EPSG:4326
    assert_golden(
        &tf,
        "EPSG:3857",
        &[
            (
                (-13_627_665.27, 4_547_675.35),
                (-122.4193999891, 37.7748999692),
            ),
            (
                (1_113_194.907_9, 6_800_125.454_4),
                (9.9999999997, 52.0000000000),
            ),
            (
                (-19_000_000.0, -15_000_000.0),
                (-170.6799039827, -79.1237548351),
            ),
        ],
    );
}

#[test]
fn golden_albers_conus_epsg5070_fast_path() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
    assert!(matches!(tf, CrsTransformer::AlbersConic(_)));
    // +proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +ellps=GRS80  ->  +proj=longlat +ellps=GRS80
    assert_golden(
        &tf,
        "EPSG:5070",
        &[
            ((1_580_000.0, 1_940_000.0), (-77.4444087567, 39.0923210270)),
            (
                (-2_000_000.0, 3_200_000.0),
                (-123.4558374341, 49.1898470907),
            ),
            ((0.0, 3_200_000.0), (-96.0000000000, 51.8579839641)),
            ((2_200_000.0, 500_000.0), (-74.2161711577, 25.2614512591)),
        ],
    );
}

#[test]
fn golden_albers_alaska_epsg3338_fast_path() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(3338), None).unwrap();
    assert!(matches!(tf, CrsTransformer::AlbersConic(_)));
    // +proj=aea +lat_1=55 +lat_2=65 +lat_0=50 +lon_0=-154 +ellps=GRS80  ->  +proj=longlat +ellps=GRS80
    // First point lies west of the antimeridian (Aleutians): compared after longitude wrapping.
    assert_golden(
        &tf,
        "EPSG:3338",
        &[
            ((-1_500_000.0, 1_500_000.0), (177.6977496854, 60.5646922135)),
            ((0.0, 1_000_000.0), (-154.0000000000, 58.9957342384)),
            ((800_000.0, 2_200_000.0), (-134.2052735687, 68.6828332938)),
            ((200_000.0, 300_000.0), (-151.0529753538, 52.6730336838)),
        ],
    );
}

#[test]
fn albers_high_latitude_matches_proj_forward_reference() {
    // PROJ 9.5.1 forward reference: pyproj.Proj(definition)(lon, lat).
    // Use forward values: PROJ's Albers inverse snaps to the pole near 89.99 degrees.
    let cases: &[(&str, &[GoldenCase])] = &[
        ("+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +ellps=GRS80", &[
            ((450676.4133990391, 5661834.18950936), (-86.0, 80.0)),
            ((424938.2796071812, 5905528.467452709), (-86.0, 89.0)),
            ((424669.6312847701, 5908072.088504652), (-86.0, 89.99)),
            ((424669.6044107033, 5908072.34295419), (-86.0, 89.9999)),
            ((1768724.9560328182, -6817737.58088181), (-86.0, -80.0)),
            ((1775084.408756948, -6877950.269659265), (-86.0, -89.0)),
            ((1775148.6991100132, -6878558.984778073), (-86.0, -89.99)),
            ((1775148.7055391085, -6878559.04565015), (-86.0, -89.9999)),
        ]),
        ("+proj=aea +lat_1=55 +lat_2=65 +lat_0=50 +lon_0=-154 +ellps=GRS80", &[
            ((231095.40223585846, 3280919.2226488106), (-144.0, 80.0)),
            ((145876.79036578114, 3842581.2003995725), (-144.0, 89.0)),
            ((144757.32731902576, 3849959.398836061), (-144.0, 89.99)),
            ((144757.2149367369, 3849960.139529405), (-144.0, 89.9999)),
            ((2055057.1466931035, -8740515.276841084), (-144.0, -80.0)),
            ((2062858.435070504, -8791932.296702133), (-144.0, -89.0)),
            ((2062937.2935929014, -8792452.040367046), (-144.0, -89.99)),
            ((2062937.3014788192, -8792452.092341846), (-144.0, -89.9999)),
        ]),
        ("+proj=aea +lat_1=-18 +lat_2=-36 +lat_0=-25 +lon_0=132 +x_0=100000 +y_0=200000 +ellps=GRS80", &[
            ((1696786.7662591464, 7808604.0560388155), (142.0, 80.0)),
            ((1702036.1835690036, 7875531.370081568), (142.0, 89.0)),
            ((1702089.2603257413, 7876208.07085746), (142.0, 89.99)),
            ((1702089.2656334748, 7876208.138528275), (142.0, 89.9999)),
            ((708374.5798623847, -4793132.2697139485), (142.0, -80.0)),
            ((694413.7219636991, -4971125.876573341), (142.0, -89.0)),
            ((694270.6523731373, -4972949.9387220135), (142.0, -89.99)),
            ((694270.6380640635, -4972950.121155191), (142.0, -89.9999)),
        ]),
    ];
    for (definition, points) in cases {
        let tf = CrsTransformer::from_proj_string(definition).unwrap();
        let CrsTransformer::AlbersConic(albers) = &tf else {
            panic!("expected Albers fast path");
        };
        assert_golden(&tf, definition, points);
        for &((x, y), (ref_lon, ref_lat)) in *points {
            let (y_p, y_p_sq) = albers.row_yp(y);
            let (lon, lat) = albers.transform_with_yp(x, y_p, y_p_sq);
            assert!(ground_error_m(lon, lat, ref_lon, ref_lat) <= MAX_ERR_M);
            assert!(
                lat.abs() < 90.0,
                "valid high latitude collapsed to the pole"
            );
        }
    }
}

#[test]
fn golden_utm_zone_32n_proj4() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(32632), None).unwrap();
    assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
    // +proj=utm +zone=32 +datum=WGS84  ->  +proj=longlat +datum=WGS84
    assert_golden(
        &tf,
        "EPSG:32632",
        &[
            ((500_000.0, 4_500_000.0), (9.0000000000, 40.6508565156)),
            ((300_000.0, 5_500_000.0), (6.2309422528, 49.6194171919)),
            ((700_000.0, 6_000_000.0), (12.0595936834, 54.1092064476)),
        ],
    );
}

#[test]
fn golden_polar_stereographic_epsg3413_proj4() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(3413), None).unwrap();
    assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
    // +proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +k=1 +datum=WGS84  ->  +proj=longlat +datum=WGS84
    assert_golden(
        &tf,
        "EPSG:3413",
        &[
            ((100_000.0, -100_000.0), (0.0000000000, 88.6945538352)),
            (
                (-2_000_000.0, 1_000_000.0),
                (-161.5650511771, 69.5687657566),
            ),
            ((500_000.0, -3_000_000.0), (-35.5376777920, 62.4462975282)),
        ],
    );
}

#[test]
fn golden_lambert_conformal_proj4_fallback() {
    let proj = "+proj=lcc +lat_1=49 +lat_2=77 +lat_0=49 +lon_0=-95 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
    let tf = CrsTransformer::from_proj_string(proj).unwrap();
    assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
    assert_golden(
        &tf,
        "LCC Canada",
        &[
            ((600_000.0, 300_000.0), (-86.2900057091, 51.3372285094)),
            ((-400_000.0, -200_000.0), (-100.2295396065, 47.0609005688)),
        ],
    );
}

#[test]
fn golden_polar_stereographic_epsg3031_proj4() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(3031), None).unwrap();
    assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
    // +proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +datum=WGS84 -> +proj=longlat +datum=WGS84
    assert_golden(
        &tf,
        "EPSG:3031",
        &[
            ((100_000.0, 100_000.0), (45.0000000000, -88.6984598067)),
            (
                (-2_000_000.0, 1_000_000.0),
                (-63.4349488229, -69.6286693858),
            ),
            (
                (1_500_000.0, -2_500_000.0),
                (149.0362434679, -63.6240217956),
            ),
            ((0.0, -1_300_000.0), (180.0000000000, -78.0767201432)),
        ],
    );
}

#[test]
fn golden_ease_grid2_epsg6933_proj4() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(6933), None).unwrap();
    assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
    // +proj=cea +lat_ts=30 +lon_0=0 +datum=WGS84 -> +proj=longlat +datum=WGS84
    assert_golden(
        &tf,
        "EPSG:6933",
        &[
            ((1_000_000.0, 2_000_000.0), (10.3641678112, 15.8743405482)),
            (
                (-12_000_000.0, -5_000_000.0),
                (-124.3700137346, -43.0491934997),
            ),
            ((17_000_000.0, 6_000_000.0), (176.1908527906, 54.9252751940)),
        ],
    );
}

#[test]
fn golden_mga56_epsg7856_proj4() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(7856), None).unwrap();
    assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
    // +proj=utm +zone=56 +south +ellps=GRS80 -> +proj=longlat +ellps=GRS80
    assert_golden(
        &tf,
        "EPSG:7856",
        &[
            ((334_000.0, 6_252_000.0), (151.2055146869, -33.8592617054)),
            ((500_000.0, 7_000_000.0), (153.0000000000, -27.1224696423)),
            ((700_000.0, 6_000_000.0), (155.2223913855, -36.1240958331)),
        ],
    );
}

#[test]
fn golden_utm_south_wgs84_proj4() {
    for (epsg, cases) in [
        (
            32756,
            &[
                ((334_000.0, 6_252_000.0), (151.2055146869, -33.8592617045)),
                ((600_000.0, 8_000_000.0), (153.9450469497, -18.0863946381)),
            ][..],
        ),
        (
            32719,
            &[((350_000.0, 6_300_000.0), (-70.6134912095, -33.4288982059))][..],
        ),
    ] {
        let tf = CrsTransformer::from_crs_or_epsg(Some(epsg), None).unwrap();
        assert!(matches!(tf, CrsTransformer::Proj4 { .. }));
        // +proj=utm +south +datum=WGS84 -> +proj=longlat +datum=WGS84
        assert_golden(&tf, &format!("EPSG:{epsg}"), cases);
    }
}

#[test]
fn epsg_aliases_match_their_canonical_codes() {
    let canonical = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
    let aliases = [
        CrsTransformer::from_crs_or_epsg(Some(900913), None).unwrap(),
        CrsTransformer::from_crs_or_epsg(Some(3785), None).unwrap(),
    ];
    for &(x, y) in &[
        (-13_627_665.27, 4_547_675.35),
        (1_113_194.907_9, 6_800_125.454_4),
        (-19_000_000.0, -15_000_000.0),
    ] {
        let expected = canonical.transform_point(x, y).unwrap();
        let expected_inverse = canonical.wgs84_to_source(expected.0, expected.1).unwrap();
        for alias in &aliases {
            let actual = alias.transform_point(x, y).unwrap();
            assert_eq!(actual.0.to_bits(), expected.0.to_bits());
            assert_eq!(actual.1.to_bits(), expected.1.to_bits());
            let actual_inverse = alias.wgs84_to_source(expected.0, expected.1).unwrap();
            assert_eq!(actual_inverse.0.to_bits(), expected_inverse.0.to_bits());
            assert_eq!(actual_inverse.1.to_bits(), expected_inverse.1.to_bits());
        }
    }

    assert!(matches!(
        CrsTransformer::from_crs_or_epsg(Some(4269), None).unwrap(),
        CrsTransformer::Wgs84Identity
    ));
}

#[test]
// Golden values keep PROJ's printed form: grouped integer part, ungrouped fraction.
#[allow(clippy::inconsistent_digit_grouping)]
fn golden_inverse_wgs84_to_source_all_projections() {
    let cases: &[(u32, &[GoldenCase])] = &[
        (
            3857,
            &[
                ((-122.4194, 37.7749), (-13_627_665.2712, 4_547_675.3543)),
                ((151.2093, -33.8688), (16_832_542.2792, -4_011_198.6473)),
                ((10.0, 80.0), (1_113_194.9079, 15_538_711.0963)),
            ],
        ),
        (
            5070,
            &[
                ((-77.0369, 38.9072), (1_618_355.2979, 1_926_511.4991)),
                ((-122.4194, 37.7749), (-2_275_431.9147, 1_955_935.4171)),
                ((-96.0, 30.0), (0.0, 769_374.7268)),
                ((-68.0, 47.0), (2_108_589.8319, 2_980_588.5339)),
            ],
        ),
        (
            3338,
            &[
                ((-149.9003, 61.2181), (219_349.5792, 1_255_301.5397)),
                ((177.5, 52.0), (-1_906_977.4118, 636_568.1543)),
                ((-141.0, 69.5), (512_343.2267, 2_220_220.3169)),
            ],
        ),
        (
            32632,
            &[
                ((9.19, 45.4642), (514_853.4959, 5_034_536.7962)),
                ((6.5, 50.0), (320_842.1358, 5_541_625.7498)),
                ((11.5, 54.0), (663_857.8167, 5_986_414.5332)),
            ],
        ),
        (
            3413,
            &[
                ((-51.7, 64.2), (-331_477.8122, -2_821_736.1290)),
                ((100.0, 75.0), (937_154.7796, 1_338_395.7305)),
                ((-170.0, 85.0), (-443_953.1307, 310_859.3288)),
            ],
        ),
        (
            3031,
            &[
                ((166.67, -77.85), (305_467.1627, -1_289_201.5377)),
                ((-60.0, -65.0), (-2_388_932.7669, 1_379_250.9760)),
                ((110.0, -89.0), (102_102.3853, -37_162.2291)),
            ],
        ),
        (
            6933,
            &[
                ((-122.4194, 37.7749), (-11_811_792.5365, 4_484_974.0501)),
                ((151.2093, -33.8688), (14_589_622.8963, -4_079_140.6887)),
            ],
        ),
    ];

    for &(epsg, cases) in cases {
        let tf = CrsTransformer::from_crs_or_epsg(Some(epsg), None).unwrap();
        assert_golden_inverse(&tf, &format!("EPSG:{epsg}"), cases);
    }

    let proj = "+proj=lcc +lat_1=49 +lat_2=77 +lat_0=49 +lon_0=-95 +x_0=0 +y_0=0 +datum=WGS84 +units=m +no_defs";
    let tf = CrsTransformer::from_proj_string(proj).unwrap();
    assert_golden_inverse(
        &tf,
        "LCC Canada",
        &[
            ((-79.3832, 43.6532), (1_277_388.2040, -443_572.4616)),
            ((-114.0719, 51.0447), (-1_307_786.2864, 424_115.9589)),
        ],
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Affine round trip on rotated / sheared grids
// ---------------------------------------------------------------------------------------------

#[test]
fn geotransform_round_trip_rotated_and_sheared() {
    let rotations_deg: [f64; 8] = [0.0, 7.5, 30.0, 45.0, 90.0, 137.0, -22.5, 180.0];
    let mut grids = Vec::new();
    for &deg in &rotations_deg {
        let (s, c) = deg.to_radians().sin_cos();
        // Pure rotation, non-square pixels
        grids.push(GeoTransform {
            c0: -2_362_395.0,
            a: 30.0 * c,
            b: -45.0 * s,
            f0: 3_267_405.0,
            d: 30.0 * s,
            e: 45.0 * c,
        });
        // Rotation + shear
        grids.push(GeoTransform {
            c0: 123_456.789,
            a: 10.0 * c + 2.0,
            b: -10.0 * s + 1.5,
            f0: -98_765.432_1,
            d: 10.0 * s - 0.75,
            e: -10.0 * c,
        });
    }
    // Degenerate north-up grid too (what most rasters look like)
    grids.push(GeoTransform {
        c0: -180.0,
        a: 0.008333333333333333,
        b: 0.0,
        f0: 90.0,
        d: 0.0,
        e: -0.008333333333333333,
    });

    let samples = [
        (0.0, 0.0),
        (0.5, 0.5),
        (17.25, 3.75),
        (1023.0, 511.0),
        (99_999.5, 49_999.5),
        (-3.0, 7.0),
    ];

    for gt in &grids {
        for &(col, row) in &samples {
            let (x, y) = gt.pixel_to_coord(col, row);
            let (col2, row2) = gt
                .coord_to_pixel(x, y)
                .unwrap_or_else(|| panic!("singular geotransform {gt:?}"));
            // 1e-6 px on a 1e5-pixel grid ≈ 1e-11 relative; well below any sample-placement effect.
            assert!(
                (col2 - col).abs() < 1e-6 && (row2 - row).abs() < 1e-6,
                "round trip failed for {gt:?}: ({col}, {row}) -> ({x}, {y}) -> ({col2}, {row2})"
            );
        }
        // Center convention must be the same through both directions
        let (cx, cy) = gt.pixel_center_to_coord(10, 20);
        let (c, r) = gt.coord_to_pixel(cx, cy).unwrap();
        assert!((c - 10.5).abs() < 1e-6 && (r - 20.5).abs() < 1e-6);
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Coverage / duplicate property tests through the streaming engine
// ---------------------------------------------------------------------------------------------

/// Write a Gray32Float GeoTIFF with one row per strip so every raster row is its own chunk
/// and the streaming horizon is advanced (and eviction exercised) many times.
fn write_geotiff(path: &Path, width: u32, height: u32, gt: &GeoTransform, geokeys: &[u16]) {
    assert_eq!(gt.b, 0.0);
    assert_eq!(gt.d, 0.0);
    let file = File::create(path).expect("create tif");
    let mut encoder = TiffEncoder::new(file).expect("encoder");
    let mut image = encoder
        .new_image::<colortype::Gray32Float>(width, height)
        .expect("image");
    image.rows_per_strip(1).expect("rows_per_strip");

    let tiepoint = [0.0, 0.0, 0.0, gt.c0, gt.f0, 0.0];
    let scale = [gt.a, -gt.e, 0.0];
    image
        .encoder()
        .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &scale[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), geokeys)
        .unwrap();

    // Non-constant values so mean/sum conservation is meaningful.
    let data: Vec<f32> = (0..(width * height))
        .map(|i| 1.0 + (i % 97) as f32)
        .collect();
    image.write_data(&data).expect("write data");
}

/// Write a Gray32Float GeoTIFF with constant fill value.
fn write_geotiff_constant(
    path: &Path,
    width: u32,
    height: u32,
    gt: &GeoTransform,
    geokeys: &[u16],
    fill: f32,
) {
    assert_eq!(gt.b, 0.0);
    assert_eq!(gt.d, 0.0);
    let file = File::create(path).expect("create tif");
    let mut encoder = TiffEncoder::new(file).expect("encoder");
    let mut image = encoder
        .new_image::<colortype::Gray32Float>(width, height)
        .expect("image");
    image.rows_per_strip(1).expect("rows_per_strip");

    let tiepoint = [0.0, 0.0, 0.0, gt.c0, gt.f0, 0.0];
    let scale = [gt.a, -gt.e, 0.0];
    image
        .encoder()
        .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &scale[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), geokeys)
        .unwrap();

    let data: Vec<f32> = vec![fill; (width * height) as usize];
    image.write_data(&data).expect("write data");
}

/// Brute-force reference with sub-pixel sampling: places each sub-sample at
/// `(col + sp.dx, row + sp.dy)` through `gt.pixel_to_coord` and accumulates `sp.weight`.
fn reference_counts_with_sampling(
    tf: &CrsTransformer,
    gt: &GeoTransform,
    width: u32,
    height: u32,
    resolutions: &[u8],
    sampling: &SamplingPattern,
) -> HashMap<(u8, u64), f64> {
    let mut out: HashMap<(u8, u64), f64> = HashMap::new();
    for row in 0..height as usize {
        for col in 0..width as usize {
            for sp in &sampling.points {
                let (x, y) = gt.pixel_to_coord(col as f64 + sp.dx, row as f64 + sp.dy);
                let (lon, lat) = tf.transform_point(x, y).expect("reference transform");
                let ll = LatLng::new(lat, lon).expect("finite lat/lon");
                for &r in resolutions {
                    let cell: u64 = ll.to_cell(Resolution::try_from(r).unwrap()).into();
                    *out.entry((r, cell)).or_insert(0.0) += sp.weight;
                }
            }
        }
    }
    out
}

/// Brute-force reference: every pixel center → CRS → WGS84 → H3, counted per (res, cell).
fn reference_counts(
    tf: &CrsTransformer,
    gt: &GeoTransform,
    width: u32,
    height: u32,
    resolutions: &[u8],
) -> HashMap<(u8, u64), f64> {
    reference_counts_with_sampling(
        tf,
        gt,
        width,
        height,
        resolutions,
        &SamplingPattern::center(),
    )
}

/// Stream the raster with a specified sampling pattern and assert:
///  * no (resolution, h3_index) is emitted twice,
///  * total weight per resolution == width × height (tolerance 1e-6),
///  * the emitted (cell → count) map equals the brute-force reference to 1e-9 per cell.
fn assert_coverage_and_no_duplicates_with_sampling(
    tif: &Path,
    crs: &str,
    gt: &GeoTransform,
    width: u32,
    height: u32,
    resolutions: &[u8],
    sampling: &SamplingPattern,
) {
    let tf = CrsTransformer::from_crs_or_epsg(None, Some(crs)).unwrap();
    let expected = reference_counts_with_sampling(&tf, gt, width, height, resolutions, sampling);

    let reader = GeoTiffStreamReader::open(tif).unwrap();
    let mut config = MultiResolutionConfig::new(resolutions.to_vec());
    config.custom_crs = Some(crs.to_string());
    config.sampling = sampling.clone();
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let mut emitted: HashMap<(u8, u64), f64> = HashMap::new();
    let mut batches = 0usize;
    while !streamer.is_finished() {
        let batch = streamer.fetch_next_batch(512).unwrap();
        if batch.is_empty() && streamer.is_finished() {
            break;
        }
        batches += 1;
        for rec in batch {
            let key = (rec.resolution, rec.h3_index);
            assert!(
                emitted.insert(key, rec.accumulator.count).is_none(),
                "{crs}: duplicate emission of res {} cell {:x}",
                rec.resolution,
                rec.h3_index
            );
        }
    }
    assert!(batches > 0, "{crs}: nothing emitted");

    let n_pixels = (width as f64) * (height as f64);
    for &r in resolutions {
        let total: f64 = emitted
            .iter()
            .filter(|((res, _), _)| *res == r)
            .map(|(_, c)| *c)
            .sum();
        assert!(
            (total - n_pixels).abs() < 1e-6,
            "{crs}: res {r} total weight {total} != {n_pixels}"
        );
    }

    assert_eq!(
        emitted.len(),
        expected.len(),
        "{crs}: emitted {} distinct cells, reference has {}",
        emitted.len(),
        expected.len()
    );
    for (key, ref_count) in &expected {
        let got = emitted.get(key).unwrap_or_else(|| {
            panic!(
                "{crs}: reference cell res {} {:x} missing from output",
                key.0, key.1
            )
        });
        assert!(
            (got - ref_count).abs() < 1e-9,
            "{crs}: res {} cell {:x} count {got} != reference {ref_count}",
            key.0,
            key.1
        );
    }
}

/// Stream the raster and assert:
///  * no (resolution, h3_index) is emitted twice,
///  * total count per resolution == width × height,
///  * the emitted (cell → count) map equals the brute-force reference exactly.
fn assert_coverage_and_no_duplicates(
    tif: &Path,
    crs: &str,
    gt: &GeoTransform,
    width: u32,
    height: u32,
    resolutions: &[u8],
) {
    assert_coverage_and_no_duplicates_with_sampling(
        tif,
        crs,
        gt,
        width,
        height,
        resolutions,
        &SamplingPattern::center(),
    );
}

/// Reference southern extent of a cell: H3 edges are gnomonic (great-circle) arcs, so sample
/// each edge by normalised linear interpolation of the unit vectors.
fn independent_true_south_lat(cell: CellIndex) -> f64 {
    fn xyz(ll: LatLng) -> [f64; 3] {
        let (la, lo) = (ll.lat_radians(), ll.lng_radians());
        [la.cos() * lo.cos(), la.cos() * lo.sin(), la.sin()]
    }
    let b = cell.boundary();
    let n = b.len();
    let mut min = f64::INFINITY;
    for i in 0..n {
        let (p, q) = (xyz(b[i]), xyz(b[(i + 1) % n]));
        for k in 0..=64 {
            let t = k as f64 / 64.0;
            let v = [
                p[0] * (1.0 - t) + q[0] * t,
                p[1] * (1.0 - t) + q[1] * t,
                p[2] * (1.0 - t) + q[2] * t,
            ];
            let norm = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            min = min.min((v[2] / norm).asin().to_degrees());
        }
    }
    min
}

/// GeoKeyDirectory for a geographic CRS (GTModelTypeGeoKey=2, GeographicTypeGeoKey=code).
fn geographic_geokeys(code: u16) -> [u16; 12] {
    [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, code]
}

/// GeoKeyDirectory for a projected CRS (GTModelTypeGeoKey=1, ProjectedCSTypeGeoKey=code).
fn projected_geokeys(code: u16) -> [u16; 12] {
    [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, code]
}

#[test]
fn coverage_res2_to_4_under_reach_band_wgs84() {
    // The audit's worst-case cells and the radii the old eviction table assumed for them.
    // Each cell's true southern extent lies `band` degrees south of `center_lat - old_radius`:
    // res 2: 0.1045° (11.6 km), res 3: 0.032° (3.6 km), res 4: 0.0079° (875 m).
    let cases: [(u8, u64, f64); 3] = [
        (2, 0x820407fffffffff, 1.7),
        (3, 0x833201fffffffff, 0.65),
        (4, 0x8404001ffffffff, 0.25),
    ];

    for (res, cell_u64, old_radius) in cases {
        let cell = CellIndex::try_from(cell_u64).unwrap();
        let center = LatLng::from(cell);
        // Independent of the engine: densify each great-circle edge and take the minimum.
        let true_south = independent_true_south_lat(cell);
        let engine_south =
            raster_h3_core::aggregator::horizon_streamer::compute_cell_south_lat(cell_u64);
        assert!(
            engine_south <= true_south + 1e-9,
            "res {res}: compute_cell_south_lat ({engine_south}) is north of the true extent ({true_south})"
        );
        let old_south = center.lat() - old_radius;
        let band = old_south - true_south;
        assert!(band > 0.0, "res {res}: audit cell no longer under-reaches");

        // Longitude of the southernmost boundary vertex: the raster is centred on it so that
        // scanlines inside the band actually intersect the cell's southern tip.
        let south_vertex = cell
            .boundary()
            .iter()
            .copied()
            .min_by(|a, b| a.lat().partial_cmp(&b.lat()).unwrap())
            .unwrap();

        // Rows are laid out so the under-reach band spans rows [300, 900) of 1200. Streaming
        // batches are at most 256 chunks (one row per chunk here), so at least one batch
        // boundary — and therefore one eviction pass — must fall inside the band. With the old
        // radii that pass evicts the cell while rows below still belong to it -> duplicate row.
        let (width, height) = (300u32, 1200u32);
        let dy = band / 600.0;
        let f0 = old_south + 300.0 * dy;
        let half_lon = 5.0 * band / center.lat().to_radians().cos();
        let gt = GeoTransform {
            c0: south_vertex.lng() - half_lon,
            a: 2.0 * half_lon / width as f64,
            b: 0.0,
            f0,
            d: 0.0,
            e: -dy,
        };

        let temp = TempDir::new().unwrap();
        let tif = temp.path().join(format!("band_res{res}_4326.tif"));
        write_geotiff(&tif, width, height, &gt, &geographic_geokeys(4326));

        // Sanity: the raster really does put pixels of this cell inside the band.
        let tf = CrsTransformer::Wgs84Identity;
        let in_band = reference_counts(&tf, &gt, width, height, &[res])
            .get(&(res, cell_u64))
            .copied()
            .unwrap_or(0.0);
        assert!(
            in_band > 0.0,
            "res {res}: raster does not touch cell {cell_u64:x}"
        );

        assert_coverage_and_no_duplicates(&tif, "EPSG:4326", &gt, width, height, &[res]);
    }
}

#[test]
fn coverage_0_to_360_longitude_raster_wgs84() {
    // Longitudes 170° → 190° (unnormalised, straddling the antimeridian).
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("lon360_4326.tif");
    let (width, height) = (400u32, 600u32);
    let gt = GeoTransform {
        c0: 170.0,
        a: 0.05,
        b: 0.0,
        f0: 0.0,
        d: 0.0,
        e: -0.05,
    };
    write_geotiff(&tif, width, height, &gt, &geographic_geokeys(4326));
    assert_coverage_and_no_duplicates(&tif, "EPSG:4326", &gt, width, height, &[4, 6]);

    // Every emitted cell must be the same cell that the wrapped longitude produces.
    let tf = CrsTransformer::Wgs84Identity;
    for row in (0..height as usize).step_by(37) {
        for col in (0..width as usize).step_by(29) {
            let (lon, lat) = gt.pixel_center_to_coord(col, row);
            let (lon2, lat2) = tf.transform_point(lon, lat).unwrap();
            let raw = LatLng::new(lat2, lon2).unwrap().to_cell(Resolution::Six);
            let wrapped = LatLng::new(lat2, wrap_lon(lon2))
                .unwrap()
                .to_cell(Resolution::Six);
            assert_eq!(raw, wrapped, "H3 assignment must be 360°-periodic");
        }
    }
}

#[test]
fn coverage_polar_stereographic_raster_containing_pole() {
    // EPSG:3413, pole at projected (0, 0) strictly inside the raster.
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("polar_3413.tif");
    let (width, height) = (200u32, 300u32);
    let gt = GeoTransform {
        c0: -60_000.0,
        a: 600.0,
        b: 0.0,
        f0: 90_000.0,
        d: 0.0,
        e: -600.0,
    };
    write_geotiff(&tif, width, height, &gt, &projected_geokeys(3413));

    // The chunk containing the pole must report max_lat == 90 so it sorts first.
    let tf = CrsTransformer::from_crs_or_epsg(Some(3413), None).unwrap();
    let pole_row = gt.f0 / -gt.e; // row index whose top edge is y = 0
    let b = tf.transform_rect_bounds(&gt, 0.0, pole_row - 1.0, width as f64, 2.0);
    assert_eq!(b[3], 90.0, "pole chunk must report max_lat = 90");

    assert_coverage_and_no_duplicates(&tif, "EPSG:3413", &gt, width, height, &[3, 4, 5]);
}

#[test]
fn coverage_polar_stereographic_raster_containing_south_pole() {
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("polar_3031.tif");
    let (width, height) = (200u32, 300u32);
    let gt = GeoTransform {
        c0: -60_000.0,
        a: 600.0,
        b: 0.0,
        f0: 90_000.0,
        d: 0.0,
        e: -600.0,
    };
    write_geotiff(&tif, width, height, &gt, &projected_geokeys(3031));

    let tf = CrsTransformer::from_crs_or_epsg(Some(3031), None).unwrap();
    let pole_row = gt.f0 / -gt.e;
    let b = tf.transform_rect_bounds(&gt, 0.0, pole_row - 1.0, width as f64, 2.0);
    assert_eq!(b[1], -90.0, "pole chunk must report min_lat = -90");

    assert_coverage_and_no_duplicates(&tif, "EPSG:3031", &gt, width, height, &[3, 4, 5]);
}

#[test]
fn coverage_albers_alaska_raster_crossing_antimeridian() {
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("alaska_3338_antimeridian.tif");
    let (width, height) = (700u32, 400u32);
    let gt = GeoTransform {
        c0: -1_700_000.0,
        a: 1000.0,
        b: 0.0,
        f0: 900_000.0,
        d: 0.0,
        e: -1000.0,
    };
    let tf = CrsTransformer::from_crs_or_epsg(Some(3338), None).unwrap();
    let (west_lon, west_lat) = tf.transform_point(gt.c0, gt.f0).unwrap();
    let (east_lon, east_lat) = tf
        .transform_point(gt.c0 + gt.a * width as f64, gt.f0)
        .unwrap();
    let west_lon = west_lon.rem_euclid(360.0);
    let east_lon = (east_lon + 180.0).rem_euclid(360.0) - 180.0;
    assert!(
        west_lon > 170.0 && west_lon < 180.0,
        "western raster corner should be east of the antimeridian: {west_lon}, {west_lat}"
    );
    assert!(
        east_lon < -170.0 && east_lon > -180.0,
        "eastern raster corner should be west of the antimeridian: {east_lon}, {east_lat}"
    );

    write_geotiff(&tif, width, height, &gt, &projected_geokeys(3338));
    assert_coverage_and_no_duplicates(&tif, "EPSG:3338", &gt, width, height, &[4, 5]);
}

#[test]
fn coverage_utm_south_raster() {
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("sydney_32756.tif");
    let (width, height) = (128u32, 128u32);
    let gt = GeoTransform {
        c0: 318_000.0,
        a: 250.0,
        b: 0.0,
        f0: 6_268_000.0,
        d: 0.0,
        e: -250.0,
    };
    write_geotiff(&tif, width, height, &gt, &projected_geokeys(32756));
    assert_coverage_and_no_duplicates(&tif, "EPSG:32756", &gt, width, height, &[7, 8]);
}

#[test]
fn coverage_supersampling_wgs84() {
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("supersample_4326.tif");
    let (width, height) = (60u32, 60u32);
    let gt = GeoTransform {
        c0: -122.5,
        a: 0.01,
        b: 0.0,
        f0: 38.0,
        d: 0.0,
        e: -0.01,
    };
    write_geotiff(&tif, width, height, &gt, &geographic_geokeys(4326));

    // RGSS (4-point)
    assert_coverage_and_no_duplicates_with_sampling(
        &tif,
        "EPSG:4326",
        &gt,
        width,
        height,
        &[7, 8],
        &SamplingPattern::rgss(),
    );

    // 16-point grid
    assert_coverage_and_no_duplicates_with_sampling(
        &tif,
        "EPSG:4326",
        &gt,
        width,
        height,
        &[7, 8],
        &SamplingPattern::sixteen_point(),
    );
}

#[test]
fn coverage_supersampling_polar_stereographic_epsg3413() {
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("supersample_3413.tif");
    let (width, height) = (50u32, 50u32);
    let gt = GeoTransform {
        c0: -30_000.0,
        a: 1000.0,
        b: 0.0,
        f0: 30_000.0,
        d: 0.0,
        e: -1000.0,
    };
    write_geotiff(&tif, width, height, &gt, &projected_geokeys(3413));

    // RGSS (4-point)
    assert_coverage_and_no_duplicates_with_sampling(
        &tif,
        "EPSG:3413",
        &gt,
        width,
        height,
        &[3, 4],
        &SamplingPattern::rgss(),
    );

    // 16-point grid
    assert_coverage_and_no_duplicates_with_sampling(
        &tif,
        "EPSG:3413",
        &gt,
        width,
        height,
        &[3, 4],
        &SamplingPattern::sixteen_point(),
    );
}

#[test]
fn coverage_supersampling_polar_stereographic_epsg3031() {
    let temp = TempDir::new().unwrap();
    let tif = temp.path().join("supersample_3031.tif");
    let (width, height) = (50u32, 50u32);
    let gt = GeoTransform {
        c0: -30_000.0,
        a: 1000.0,
        b: 0.0,
        f0: 30_000.0,
        d: 0.0,
        e: -1000.0,
    };
    write_geotiff(&tif, width, height, &gt, &projected_geokeys(3031));

    assert_coverage_and_no_duplicates_with_sampling(
        &tif,
        "EPSG:3031",
        &gt,
        width,
        height,
        &[3, 4],
        &SamplingPattern::rgss(),
    );
    assert_coverage_and_no_duplicates_with_sampling(
        &tif,
        "EPSG:3031",
        &gt,
        width,
        height,
        &[3, 4],
        &SamplingPattern::sixteen_point(),
    );
}

#[test]
fn coverage_mosaic_overlap_wgs84() {
    let temp = TempDir::new().unwrap();
    let p1 = temp.path().join("tile1_4326.tif");
    let p2 = temp.path().join("tile2_4326.tif");

    // Two WGS84 tiles overlapping by 20% in latitude (10 of 50 px) and 20% in longitude (10 of 50 px).
    // Using a dyadic pixel step (1/64 = 0.015625) guarantees exact IEEE-754 floating-point representation
    // with zero round-off discrepancy across adjacent tile coordinate offsets.
    let (width, height) = (50u32, 50u32);
    let step = 1.0 / 64.0;
    let gt1 = GeoTransform {
        c0: 10.0,
        a: step,
        b: 0.0,
        f0: 50.0,
        d: 0.0,
        e: -step,
    };
    let gt2 = GeoTransform {
        c0: 10.0 + 40.0 * step, // offset 40 px in X -> overlap is 10 px = 20%
        a: step,
        b: 0.0,
        f0: 50.0 - 40.0 * step, // offset 40 px in Y -> overlap is 10 px = 20%
        d: 0.0,
        e: -step,
    };

    write_geotiff_constant(&p1, width, height, &gt1, &geographic_geokeys(4326), 10.0);
    write_geotiff_constant(&p2, width, height, &gt2, &geographic_geokeys(4326), 30.0);

    let paths = vec![p1, p2];
    // |union| = 50*50 + 50*50 - 10*10 = 4900 pixels
    let union_pixels = 4900.0;

    // 1. Cutline (Voronoi bisector)
    {
        let mosaic =
            Arc::new(MosaicReader::open(&paths, None, None, OverlapRule::Cutline).unwrap());
        let mut config = MultiResolutionConfig::new(vec![8]);
        config.overlap_rule = OverlapRule::Cutline;
        let mut streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config).unwrap();

        let mut emitted: HashMap<u64, f64> = HashMap::new();
        while !streamer.is_finished() {
            let batch = streamer.fetch_next_batch(512).unwrap();
            if batch.is_empty() && streamer.is_finished() {
                break;
            }
            for rec in batch {
                assert!(
                    emitted
                        .insert(rec.h3_index, rec.accumulator.count)
                        .is_none(),
                    "Cutline: duplicate cell emission {:x}",
                    rec.h3_index
                );
            }
        }
        let total: f64 = emitted.values().sum();
        assert!(
            (total - union_pixels).abs() < 1e-6,
            "Cutline must count every pixel in union exactly once: got {total}, expected {union_pixels}"
        );
    }

    // 2. First (Tile 1 takes precedence in overlap)
    {
        let mosaic = Arc::new(MosaicReader::open(&paths, None, None, OverlapRule::First).unwrap());
        let mut config = MultiResolutionConfig::new(vec![8]);
        config.overlap_rule = OverlapRule::First;
        let mut streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config).unwrap();

        let mut emitted: HashMap<u64, (f64, f64)> = HashMap::new();
        while !streamer.is_finished() {
            let batch = streamer.fetch_next_batch(512).unwrap();
            if batch.is_empty() && streamer.is_finished() {
                break;
            }
            for rec in batch {
                assert!(
                    emitted
                        .insert(
                            rec.h3_index,
                            (rec.accumulator.count, rec.accumulator.mean())
                        )
                        .is_none(),
                    "First: duplicate cell emission {:x}",
                    rec.h3_index
                );
            }
        }
        let total: f64 = emitted.values().map(|(c, _)| *c).sum();
        assert!(
            (total - union_pixels).abs() < 1e-6,
            "First must count every pixel in union exactly once: got {total}, expected {union_pixels}"
        );

        // In overlap region (cols 40..50, rows 40..50 of Tile 1), Tile 1 takes precedence -> mean = 10.0
        let (ov_lon, ov_lat) = gt1.pixel_center_to_coord(45, 45);
        let overlap_cell: u64 = LatLng::new(ov_lat, ov_lon)
            .unwrap()
            .to_cell(Resolution::Eight)
            .into();
        let (_, mean) = emitted
            .get(&overlap_cell)
            .expect("overlap cell must be present");
        assert!(
            (mean - 10.0).abs() < 1e-6,
            "First: overlap region cell should have Tile 1 value (10.0), got {mean}"
        );
    }

    // 3. Average (Accumulate all observations across tiles into target H3 cell)
    {
        let mosaic =
            Arc::new(MosaicReader::open(&paths, None, None, OverlapRule::Average).unwrap());
        let mut config = MultiResolutionConfig::new(vec![8]);
        config.overlap_rule = OverlapRule::Average;
        let mut streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config).unwrap();

        let mut emitted: HashMap<u64, (f64, f64)> = HashMap::new();
        while !streamer.is_finished() {
            let batch = streamer.fetch_next_batch(512).unwrap();
            if batch.is_empty() && streamer.is_finished() {
                break;
            }
            for rec in batch {
                assert!(
                    emitted
                        .insert(
                            rec.h3_index,
                            (rec.accumulator.count, rec.accumulator.mean())
                        )
                        .is_none(),
                    "Average: duplicate cell emission {:x}",
                    rec.h3_index
                );
            }
        }

        // Semantics note:
        // As defined in `src/raster/mosaic.rs` (line 464) and tested in `tests/it/mosaic.rs`,
        // `OverlapRule::Average` does not discard duplicate coverage pixels at chunk level; it accumulates
        // all observations from both tiles into the H3 cell accumulators.
        // Thus, total accumulated observations across both tiles equals 50*50 + 50*50 = 5000.
        let total_accumulated: f64 = emitted.values().map(|(c, _)| *c).sum();
        let expected_accumulated = (width * height * 2) as f64;
        assert!(
            (total_accumulated - expected_accumulated).abs() < 1e-6,
            "Average must accumulate all tile observations: got {total_accumulated}, expected {expected_accumulated}"
        );

        // A cell strictly in the interior of the overlap region receives equal contributions
        // from Tile 1 (10.0) and Tile 2 (30.0) -> mean = 20.0
        let (ov_lon, ov_lat) = gt1.pixel_center_to_coord(45, 45);
        let overlap_cell: u64 = LatLng::new(ov_lat, ov_lon)
            .unwrap()
            .to_cell(Resolution::Eight)
            .into();
        let (_, mean) = emitted
            .get(&overlap_cell)
            .expect("overlap cell must be present");
        assert!(
            (mean - 20.0).abs() < 1e-6,
            "Average: overlap cell should have averaged value (20.0), got {mean}"
        );

        // A cell strictly in Tile 1 non-overlap -> mean = 10.0
        let (t1_lon, t1_lat) = gt1.pixel_center_to_coord(15, 15);
        let tile1_cell: u64 = LatLng::new(t1_lat, t1_lon)
            .unwrap()
            .to_cell(Resolution::Eight)
            .into();
        let (_, mean1) = emitted
            .get(&tile1_cell)
            .expect("tile 1 non-overlap cell must be present");
        assert!(
            (mean1 - 10.0).abs() < 1e-6,
            "Average: tile 1 non-overlap cell should have value 10.0, got {mean1}"
        );

        // A cell strictly in Tile 2 non-overlap -> mean = 30.0
        let (t2_lon, t2_lat) = gt2.pixel_center_to_coord(35, 35);
        let tile2_cell: u64 = LatLng::new(t2_lat, t2_lon)
            .unwrap()
            .to_cell(Resolution::Eight)
            .into();
        let (_, mean2) = emitted
            .get(&tile2_cell)
            .expect("tile 2 non-overlap cell must be present");
        assert!(
            (mean2 - 30.0).abs() < 1e-6,
            "Average: tile 2 non-overlap cell should have value 30.0, got {mean2}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 4. WKB capacity across every resolution
// ---------------------------------------------------------------------------------------------

#[test]
fn wkb_buffer_capacity_all_resolutions_pentagons_and_icosahedron_edges() {
    let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
    let mut max_len_seen = 0usize;
    let mut max_verts_seen = 0usize;

    for r in 0..=15u8 {
        let res = Resolution::try_from(r).unwrap();
        // The 12 icosahedron vertices are the pentagon base-cell centres.
        for base in CellIndex::base_cells().filter(|b| b.is_pentagon()) {
            let pent = base.center_child(res).expect("pentagon child");
            assert!(pent.is_pentagon());
            // grid_disk(2) around the icosahedron vertex covers the pentagon and the
            // distorted hexagons whose edges cross icosahedron edges.
            for cell in pent.grid_disk::<Vec<_>>(2) {
                let n = cell.boundary().len();
                let need = 13 + 16 * (n + 1);
                assert!(
                    n <= 10,
                    "res {r} cell {cell}: {n} boundary vertices exceeds h3o MAX_BNDRY_VERTS"
                );
                assert!(
                    need <= WKB_BUF_LEN,
                    "res {r} cell {cell}: WKB needs {need} bytes > WKB_BUF_LEN {WKB_BUF_LEN}"
                );
                let len = cell_to_wkb(cell, &mut buf);
                assert!(len <= WKB_BUF_LEN);
                let lngs = cell.boundary().iter().map(|v| v.lng()).collect::<Vec<_>>();
                let span = lngs.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                    - lngs.iter().cloned().fold(f64::INFINITY, f64::min);
                if span > 180.0 {
                    // Antimeridian cells are split into a MultiPolygon (6); the
                    // res-0 pole cells in these disks are polar-cap Polygons (3).
                    let kind = u32::from_le_bytes(buf[1..5].try_into().unwrap());
                    assert!(
                        kind == 6 || (kind == 3 && r == 0),
                        "res {r} cell {cell}: type {kind}"
                    );
                    continue;
                }
                assert_eq!(len, need, "res {r} cell {cell}: unexpected WKB length");

                // Structural checks: point count, closed ring.
                let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap()) as usize;
                assert_eq!(num_points, n + 1);
                let first = &buf[13..29];
                let last = &buf[len - 16..len];
                assert_eq!(first, last, "ring must be closed");

                max_len_seen = max_len_seen.max(len);
                max_verts_seen = max_verts_seen.max(n);
            }
        }
    }

    // Class III pentagons really do reach 10 vertices / 189 bytes; make sure the test saw them.
    assert_eq!(max_verts_seen, 10);
    assert_eq!(max_len_seen, 189);
    assert!(max_len_seen <= WKB_BUF_LEN);
}
