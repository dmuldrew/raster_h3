//! WGS84 bounds used to reject data (bbox pruning, mosaic overlap) must
//! enclose the true image of a pixel rectangle: certified transverse Mercator
//! bounds, fail-open behaviour for uncertified or partly unprojectable
//! rectangles, scale-independent affine inversion, and rejection of invalid
//! affines when a GeoTIFF is opened.

use proptest::prelude::*;
use raster_h3_core::aggregator::chunk_intersects_bbox;
use raster_h3_core::aggregator::multi_horizon::coordinates::eviction_north_bound;
use raster_h3_core::crs::transformer::{wrap_lon, CrsTransformer};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::{GeoTransform, RasterChunk};
use std::fs::File;
use std::io::BufWriter;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

const GLOBE: [f64; 4] = [-180.0, -90.0, 180.0, 90.0];

fn north_up(c0: f64, f0: f64, size: f64) -> GeoTransform {
    GeoTransform {
        c0,
        a: size,
        b: 0.0,
        f0,
        d: 0.0,
        e: -size,
    }
}

/// Every point of a dense grid over the rectangle lies inside `b`.
fn assert_encloses(tf: &CrsTransformer, gt: &GeoTransform, w: f64, h: f64, b: [f64; 4]) {
    for j in 0..=150 {
        for i in 0..=150 {
            let (c, r) = (w * i as f64 / 150.0, h * j as f64 / 150.0);
            let (x, y) = gt.pixel_to_coord(c, r);
            let (lon, lat) = tf.transform_point(x, y).unwrap();
            let lon = (lon + 180.0).rem_euclid(360.0) - 180.0;
            assert!(b[1] <= lat && lat <= b[3], "lat {lat} outside {b:?}");
            assert!(
                (b[0] <= lon && lon <= b[2]) || (lon == -180.0 && b[2] == 180.0),
                "lon {lon} outside {b:?}"
            );
        }
    }
}

#[test]
fn utm_bounds_are_certified_enclosing_and_prune_distant_bboxes() {
    let tf = CrsTransformer::from_epsg_code(32633).unwrap();
    assert!(tf.bounds_certified());
    let gt = north_up(400_000.0, 5_600_000.0, 30.0);
    let chunk = RasterChunk {
        col_offset: 0,
        row_offset: 0,
        width: 512,
        height: 512,
    };
    let b = tf.transform_rect_bounds(&gt, 0.0, 0.0, 512.0, 512.0);
    assert_eq!(tf.rejection_bounds(b), b);
    assert_encloses(&tf, &gt, 512.0, 512.0, b);

    // A bbox one degree east of the chunk is pruned; one over it is kept.
    assert!(!chunk_intersects_bbox(
        &chunk,
        &gt,
        &tf,
        &[b[2] + 1.0, b[1], b[2] + 2.0, b[3]]
    ));
    assert!(chunk_intersects_bbox(
        &chunk,
        &gt,
        &tf,
        &[b[0], b[1], b[0] + 0.01, b[1] + 0.01]
    ));
}

#[test]
fn utm_chunk_around_the_pole_reports_full_longitude() {
    let tf = CrsTransformer::from_epsg_code(32633).unwrap();
    let gt = north_up(480_000.0, 10_020_000.0, 1000.0);
    let b = tf.transform_rect_bounds(&gt, 0.0, 0.0, 40.0, 60.0);
    assert_eq!((b[0], b[2], b[3]), (-180.0, 180.0, 90.0));
    assert_encloses(&tf, &gt, 40.0, 60.0, b);
}

#[test]
fn helmert_shifted_national_grid_is_certified_but_huge_shifts_are_not() {
    let bng = CrsTransformer::from_epsg_code(27700).unwrap();
    assert!(bng.bounds_certified());
    let gt = north_up(300_000.0, 700_000.0, 50.0);
    let b = bng.transform_rect_bounds(&gt, 0.0, 0.0, 4000.0, 4000.0);
    assert_encloses(&bng, &gt, 4000.0, 4000.0, b);

    let huge =
        CrsTransformer::from_proj_string("+proj=utm +zone=33 +ellps=intl +towgs84=-6000000,0,0")
            .unwrap();
    assert!(!huge.bounds_certified());
    assert_eq!(huge.rejection_bounds([10.0, 40.0, 11.0, 41.0]), GLOBE);
}

#[test]
fn transverse_mercator_scale_and_datum_edge_cases_still_enclose() {
    // Extreme or duplicated scale factors stay certified; a datum shift too
    // large for the Helmert bound falls back to whole-globe rejection bounds.
    let cases = [
        ("+proj=tmerc +ellps=WGS84 +k_0=100", true),
        ("+proj=tmerc +ellps=WGS84 +k0=0.01", true),
        ("+proj=tmerc +ellps=WGS84 +k=1 +k=100", true),
        ("+proj=tmerc +ellps=WGS84 +towgs84=-6000000,0,0", false),
    ];
    for (crs, certified) in cases {
        let tf = CrsTransformer::from_proj_string(crs).unwrap();
        assert_eq!(tf.bounds_certified(), certified, "{crs}");
        let (gt, w, h) = if crs.contains("towgs84") {
            (north_up(-20_000.0, 2_250_000.0, 1000.0), 40.0, 100.0)
        } else {
            (north_up(-20_000.0, 10_020_000.0, 1000.0), 40.0, 60.0)
        };
        let b = tf.rejection_bounds(tf.transform_rect_bounds(&gt, 0.0, 0.0, w, h));
        if !certified {
            assert_eq!(b, GLOBE, "{crs}");
        }
        assert_encloses(&tf, &gt, w, h, b);
    }
}

/// A certified CRS for the generated enclosure property, with the region
/// chunk centres are drawn from.
struct CertifiedCase {
    name: &'static str,
    tf: CrsTransformer,
    /// Source units per metre.
    per_m: f64,
    lon_0: f64,
    /// Half-width of the longitude band around `lon_0`.
    lon_span: f64,
    lat_range: (f64, f64),
}

fn certified_cases() -> Vec<CertifiedCase> {
    let case = |name, tf, per_m, lon_0, lon_span, lat_range| CertifiedCase {
        name,
        tf,
        per_m,
        lon_0,
        lon_span,
        lat_range,
    };
    let epsg = |code| CrsTransformer::from_epsg_code(code).unwrap();
    let proj = |s: &str| CrsTransformer::from_proj_string(s).unwrap();
    let australia = "+proj=aea +lat_1=-18 +lat_2=-36 +lat_0=0 +lon_0=132 +x_0=0 +y_0=0 \
                     +ellps=GRS80 +units=m +no_defs";
    let florida_east = "+proj=tmerc +lat_0=24.3333333333333 +lon_0=-81 +k=0.999941177 \
                        +x_0=200000.0001016 +y_0=0 +ellps=GRS80 +units=us-ft +no_defs";
    vec![
        case(
            "EPSG:4326",
            epsg(4326),
            1.0 / 111_000.0,
            0.0,
            180.0,
            (-89.0, 89.0),
        ),
        case("EPSG:3857", epsg(3857), 1.0, 0.0, 180.0, (-84.0, 84.0)),
        case("EPSG:5070", epsg(5070), 1.0, -96.0, 179.0, (-80.0, 89.0)),
        // Alaska Albers: the Aleutians cross the antimeridian.
        case("EPSG:3338", epsg(3338), 1.0, -154.0, 179.0, (-80.0, 89.0)),
        // Southern cone (n < 0): the apex is the South Pole.
        case(
            "EPSG:3577",
            proj(australia),
            1.0,
            132.0,
            179.0,
            (-89.0, 80.0),
        ),
        // US survey feet; chunks leaving the certified band fail open.
        case(
            "EPSG:2236",
            proj(florida_east),
            1.0 / 0.3048006096012192,
            -81.0,
            12.0,
            (-80.0, 80.0),
        ),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For every certified CRS, rotated and sheared chunks of any size and
    /// position (including ones around a cone apex, across the antimeridian
    /// or past the cut opposite the central meridian) have rect bounds that
    /// contain every sample the per-pixel path computes, and an eviction
    /// bound above every sample's latitude.
    #[test]
    fn certified_rect_bounds_enclose_rotated_and_sheared_chunks(
        case in 0usize..6,
        dlon in -1.0f64..1.0, lat_t in 0.0f64..1.0,
        size_m in 10.0f64..200_000.0, aspect in 0.25f64..4.0,
        angle in 0.0f64..std::f64::consts::TAU, shear in -0.5f64..0.5,
        flip in any::<bool>(), w in 1u32..64, h in 1u32..64,
    ) {
        let cases = certified_cases();
        let CertifiedCase { name, tf, per_m, lon_0, lon_span, lat_range: (lat_lo, lat_hi) } =
            &cases[case];
        prop_assert!(tf.bounds_certified());
        let (cx, cy) = tf
            .wgs84_to_source(lon_0 + dlon * lon_span, lat_lo + lat_t * (lat_hi - lat_lo))
            .unwrap();
        let (sx, sy) = (size_m * per_m, size_m * per_m * aspect);
        let (sin, cos) = angle.sin_cos();
        let ey = if flip { sy } else { -sy };
        // Columns step along (cos, sin), rows along a sheared perpendicular.
        let (a, d) = (sx * cos, sx * sin);
        let (b, e) = (-ey * sin + shear * sx * cos, ey * cos + shear * sx * sin);
        let (wf, hf) = (f64::from(w), f64::from(h));
        let gt = GeoTransform {
            c0: cx - (a * wf + b * hf) / 2.0,
            a,
            b,
            f0: cy - (d * wf + e * hf) / 2.0,
            d,
            e,
        };
        prop_assume!(gt.is_valid());
        let bounds = tf.rejection_bounds(tf.transform_rect_bounds(&gt, 0.0, 0.0, wf, hf));
        let chunk = RasterChunk { col_offset: 0, row_offset: 0, width: w, height: h };
        let north = eviction_north_bound(&chunk, &gt, tf);
        const N: u32 = 48;
        for j in 0..=N {
            for i in 0..=N {
                let (x, y) = gt.pixel_to_coord(wf * f64::from(i) / f64::from(N), hf * f64::from(j) / f64::from(N));
                let Ok((lon, lat)) = tf.transform_point(x, y) else { continue };
                if !(lon.is_finite() && lat.is_finite()) {
                    continue;
                }
                // Wrapped as the per-sample bbox test wraps it.
                let lon = wrap_lon(lon);
                prop_assert!(bounds[1] <= lat && lat <= bounds[3],
                    "{name}: lat {lat} outside {bounds:?} at pixel ({i},{j})/{N}, gt {gt:?}");
                prop_assert!((bounds[0] <= lon && lon <= bounds[2]) || (lon == -180.0 && bounds[2] == 180.0),
                    "{name}: lon {lon} outside {bounds:?} at pixel ({i},{j})/{N}, gt {gt:?}");
                prop_assert!(lat <= north, "{name}: lat {lat} above eviction bound {north}, gt {gt:?}");
            }
        }
    }
}

#[test]
fn uncertified_projections_fail_open_for_rejection() {
    let polar = CrsTransformer::from_epsg_code(3413).unwrap();
    assert!(!polar.bounds_certified());
    assert_eq!(polar.rejection_bounds([10.0, 70.0, 20.0, 80.0]), GLOBE);
}

#[test]
fn rectangle_leaving_the_projection_domain_gets_whole_globe_bounds() {
    // The visible Earth disk of a geostationary view has radius ~5.4e6 m.
    let tf =
        CrsTransformer::from_proj_string("+proj=geos +h=35785831 +lon_0=0 +R=6378137").unwrap();
    let gt = north_up(0.0, 1_000_000.0, 100_000.0);
    assert_eq!(tf.transform_rect_bounds(&gt, 0.0, 0.0, 70.0, 20.0), GLOBE);
    assert_ne!(tf.transform_rect_bounds(&gt, 0.0, 0.0, 20.0, 20.0), GLOBE);
}

#[test]
fn fine_geographic_grid_inverts_at_any_coordinate_scale() {
    let gt = north_up(10.0, 50.0, 1e-8);
    assert!(gt.is_valid());
    let (x, y) = gt.pixel_to_coord(3.25, 7.5);
    let (col, row) = gt.coord_to_pixel(x, y).unwrap();
    assert!((col - 3.25).abs() < 1e-3 && (row - 7.5).abs() < 1e-3);
}

/// Write a 2x2 GeoTIFF whose georeferencing is the given ModelTransformationTag.
fn write_with_model_transformation(path: &std::path::Path, matrix: [f64; 16]) {
    let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
    let mut image = encoder.new_image::<colortype::Gray32Float>(2, 2).unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelTransformationTag, &matrix[..])
        .unwrap();
    image.write_data(&[1.0f32; 4]).unwrap();
}

#[test]
fn opening_a_geotiff_rejects_singular_or_non_finite_affines() {
    let dir = tempfile::tempdir().unwrap();
    let identity_tail = [0.0, 0.0, 0.0, 1.0];
    let cases: [(&str, [f64; 12]); 2] = [
        // Both pixel axes map to the same direction.
        (
            "singular",
            [
                10.0, 20.0, 0.0, 0.0, 10.0, 20.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0,
            ],
        ),
        (
            "nan",
            [
                f64::NAN,
                0.0,
                0.0,
                0.0,
                0.0,
                -1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ],
        ),
    ];
    for (name, head) in cases {
        let mut m = [0.0; 16];
        m[..12].copy_from_slice(&head);
        m[12..].copy_from_slice(&identity_tail);
        let path = dir.path().join(format!("{name}.tif"));
        write_with_model_transformation(&path, m);
        assert!(
            GeoTiffStreamReader::open(&path).is_err(),
            "{name} affine accepted"
        );
    }

    let path = dir.path().join("valid.tif");
    let mut m = [0.0; 16];
    m[..12].copy_from_slice(&[
        1e-8, 0.0, 0.0, 10.0, 0.0, -1e-8, 0.0, 50.0, 0.0, 0.0, 1.0, 0.0,
    ]);
    m[12..].copy_from_slice(&identity_tail);
    write_with_model_transformation(&path, m);
    let reader = GeoTiffStreamReader::open(&path).unwrap();
    assert!(reader.metadata.geotransform.is_valid());
}
