//! WGS84 bounds used to reject data (bbox pruning, mosaic overlap) must
//! enclose the true image of a pixel rectangle: certified transverse Mercator
//! bounds, fail-open behaviour for uncertified or partly unprojectable
//! rectangles, scale-independent affine inversion, and rejection of invalid
//! affines when a GeoTIFF is opened.

use raster_h3_core::aggregator::chunk_intersects_bbox;
use raster_h3_core::crs::transformer::CrsTransformer;
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

#[test]
fn tm_certificate_matches_effective_scale_parameters() {
    for (params, effective_scale) in [
        ("+k_0=100", 1.0), // ignored by proj4rs, not a scale of 100
        ("+k0=0.01", 0.01),
        ("+k=0.01", 0.01),
        ("+k=100 +k0=0.01 +k_0=200", 0.01),
        ("+k0=0.01 +k=100", 0.01),
    ] {
        let tf = CrsTransformer::from_proj_string(&format!("+proj=tmerc +ellps=WGS84 {params}"))
            .unwrap();
        assert!(tf.bounds_certified(), "{params}");
        // Pole lies between probes; incorrect scale padding misses its latitude.
        let gt = north_up(
            -20_000.0 * effective_scale,
            10_021_000.0 * effective_scale,
            1000.0 * effective_scale,
        );
        let b = tf.transform_rect_bounds(&gt, 0.0, 0.0, 40.0, 60.0);
        assert_eq!(b[3], 90.0, "{params}: {b:?}");
        assert_encloses(&tf, &gt, 40.0, 60.0, b);
        let chunk = RasterChunk {
            col_offset: 0,
            row_offset: 0,
            width: 40,
            height: 60,
        };
        assert!(chunk_intersects_bbox(
            &chunk,
            &gt,
            &tf,
            &[-180.0, 89.995, 180.0, 90.0]
        ));
    }
}

#[test]
fn ambiguous_tm_parameters_cannot_certify_rejection_bounds() {
    for params in [
        "+towgs84=-6370000,0,0 +towgs84=0,0,0",
        "+towgs84=0,0,0 +towgs84=-6370000,0,0",
        "+k=1 +k=100",
        "+k0=0.01 +k0=100",
        "+x_0=1000000 +x_0=0",
        "+K=100",
        "+k=\"0.01\"",
        "+k = 0.01",
    ] {
        let tf = CrsTransformer::from_proj_string(&format!("+proj=tmerc +ellps=WGS84 {params}"))
            .unwrap();
        assert!(!tf.bounds_certified(), "{params}");
        let gt = north_up(-20_000.0, 10_021_000.0, 1000.0);
        let b = tf.transform_rect_bounds(&gt, 0.0, 0.0, 40.0, 60.0);
        assert_eq!(tf.rejection_bounds(b), GLOBE, "{params}");
        let chunk = RasterChunk {
            col_offset: 0,
            row_offset: 0,
            width: 40,
            height: 60,
        };
        assert!(chunk_intersects_bbox(
            &chunk,
            &gt,
            &tf,
            &[-180.0, 89.995, 180.0, 90.0]
        ));
    }
}
