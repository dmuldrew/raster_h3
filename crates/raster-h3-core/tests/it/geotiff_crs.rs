//! GeoTransform math, CRS reprojection and parsing, and CRS detection when opening GeoTIFF files.

use crate::helpers::TestGeoTiffBuilder;
use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::crs::transformer::CrsTransformer;
use raster_h3_core::error::RasterH3Error;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::GeoTransform;
use std::fs::File;
use std::io::BufWriter;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

const ALBERS_WKT: &str = r#"PROJCS["Albers_Conic_Equal_Area",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Albers"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-96.0],PARAMETER["Standard_Parallel_1",29.5],PARAMETER["Standard_Parallel_2",45.5],PARAMETER["Latitude_Of_Origin",23.0],UNIT["Meter",1.0]]"#;

/// GDAL writes user-defined ESRI projections as a citation of the form
/// `ESRI PE String = PROJCS[...]|` in GeoAsciiParamsTag.
#[test]
fn esri_pe_string_citation_is_detected_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("esri_pe.tif");
    let citation = format!("ESRI PE String = {ALBERS_WKT}|");
    // ModelTypeProjected, user-defined ProjectedCRS, citation in GeoAsciiParams.
    let geokeys: [u16; 16] = [
        1,
        1,
        0,
        3, //
        1024,
        0,
        1,
        1, //
        1026,
        34737,
        citation.len() as u16,
        0, //
        3072,
        0,
        1,
        32767,
    ];
    {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&path).unwrap())).unwrap();
        let mut image = encoder.new_image::<colortype::Gray32Float>(4, 4).unwrap();
        let enc = image.encoder();
        enc.write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, 1_500_000.0, 1_900_000.0, 0.0][..],
        )
        .unwrap();
        enc.write_tag(Tag::ModelPixelScaleTag, &[30.0, 30.0, 0.0][..])
            .unwrap();
        enc.write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
        enc.write_tag(Tag::Unknown(34737), citation.as_str())
            .unwrap();
        image.write_data(&[1.0f32; 16]).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    assert_eq!(reader.metadata.epsg, None);
    let proj = reader
        .metadata
        .proj_string
        .clone()
        .expect("CRS lost from ESRI PE citation");
    assert!(
        proj.contains("+proj=aea") && proj.contains("+datum=NAD83"),
        "{proj}"
    );
    let tf = CrsTransformer::from_crs_or_epsg(None, Some(&proj)).unwrap();
    let ref5070 = CrsTransformer::from_epsg_code(5070).unwrap();
    let (a, b) = (
        tf.transform_point(1_500_000.0, 1_900_000.0).unwrap(),
        ref5070.transform_point(1_500_000.0, 1_900_000.0).unwrap(),
    );
    assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9);
}

#[test]
fn test_geotransform_accuracy() {
    let gt = GeoTransform {
        c0: -122.5,
        a: 0.01,
        b: 0.0,
        f0: 37.8,
        d: 0.0,
        e: -0.01,
    };

    // Center of pixel (0, 0)
    let (x, y) = gt.pixel_center_to_coord(0, 0);
    assert!((x - (-122.495)).abs() < 1e-9);
    assert!((y - 37.795).abs() < 1e-9);

    // Coordinate to pixel inversion
    let (col, row) = gt.coord_to_pixel(x, y).unwrap();
    assert!((col - 0.5).abs() < 1e-9);
    assert!((row - 0.5).abs() < 1e-9);
}

#[test]
fn test_crs_web_mercator_to_wgs84() {
    let transformer = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
    // London coordinates in EPSG:3857
    let (lon, lat) = transformer.transform_point(-14227.0, 6711582.0).unwrap();
    assert!((lon - (-0.1278)).abs() < 0.01);
    assert!((lat - 51.5074).abs() < 0.01);
}

#[test]
fn test_crs_utm_to_wgs84() {
    // UTM Zone 33N (EPSG:32633) - Rome area
    let transformer = CrsTransformer::from_crs_or_epsg(Some(32633), None).unwrap();
    let (lon, lat) = transformer.transform_point(291240.0, 4641950.0).unwrap();
    assert!((lon - 12.496).abs() < 0.05);
    assert!((lat - 41.902).abs() < 0.05);
}

#[test]
fn test_crs_southern_hemisphere_utm() {
    // UTM Zone 33S (EPSG:32733) - Southern Africa / Namibia area
    let transformer = CrsTransformer::from_crs_or_epsg(Some(32733), None).unwrap();
    let (lon, lat) = transformer.transform_point(500000.0, 6200000.0).unwrap();
    // Central meridian for Zone 33 is 15°E
    assert!(
        (lon - 15.0).abs() < 0.1,
        "Expected lon near 15°E, got: {}",
        lon
    );
    // Northing 6,200,000 in Southern hemisphere maps to ~ -34.3° S
    assert!(
        lat < -30.0 && lat > -40.0,
        "Expected negative latitude in S hemisphere, got: {}",
        lat
    );
}

#[test]
fn test_crs_custom_albers_proj_string() {
    // Standard CONUS Albers Equal-Area projection
    let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=37.5 +lon_0=-96 +x_0=0 +y_0=0 +datum=NAD83 +units=m +no_defs";
    let transformer = CrsTransformer::from_proj_string(proj_str).unwrap();

    // Origin (0.0, 0.0) in projection meters maps to (-96.0, 37.5) in WGS84
    let (lon, lat) = transformer.transform_point(0.0, 0.0).unwrap();
    assert!(
        (lon - -96.0).abs() < 0.01,
        "Expected lon near -96.0, got: {}",
        lon
    );
    assert!(
        (lat - 37.5).abs() < 0.01,
        "Expected lat near 37.5, got: {}",
        lat
    );
}

#[test]
fn test_crs_invalid_proj_string_error_handling() {
    let result = CrsTransformer::from_proj_string("+proj=totally_invalid_nonexistent_proj");
    assert!(result.is_err(), "Expected error on invalid PROJ string");
    match result {
        Err(RasterH3Error::CrsError(msg)) => {
            assert!(
                msg.contains("Failed to parse source PROJ string"),
                "Unexpected message: {}",
                msg
            );
        }
        _ => panic!("Expected RasterH3Error::CrsError"),
    }
}

#[test]
fn test_geotransform_inverse_precision() {
    let gt = GeoTransform {
        c0: -122.4194,
        a: 0.0008333333333333334,
        b: 0.0,
        f0: 37.7749,
        d: 0.0,
        e: -0.0008333333333333334,
    };

    // Forward transform pixel (150.0, 250.0)
    let (x, y) = gt.pixel_to_coord(150.0, 250.0);
    // Reverse transform coordinate back to pixel
    let (col, row) = gt.coord_to_pixel(x, y).unwrap();

    assert!((col - 150.0).abs() < 1e-7);
    assert!((row - 250.0).abs() < 1e-7);
}

#[test]
fn test_crs_unspecified_fallback_and_invalid_proj_error() {
    // 1. Unspecified CRS (None, None) now returns a typed CrsError requiring explicit CRS
    let default_transformer = CrsTransformer::from_crs_or_epsg(None, None);
    assert!(default_transformer.is_err());
    match default_transformer {
        Err(RasterH3Error::CrsNotDetected(msg)) => {
            assert!(
                msg.contains("No CRS detected in raster metadata"),
                "Unexpected message: {}",
                msg
            );
        }
        _ => panic!("Expected RasterH3Error::CrsNotDetected for unspecified CRS"),
    }

    // 2. Explicit invalid PROJ definition returns a typed CrsError
    let invalid_result =
        CrsTransformer::from_crs_or_epsg(None, Some("+proj=nonexistent_invalid_crs +units=m"));
    assert!(invalid_result.is_err());
    match invalid_result {
        Err(RasterH3Error::CrsError(msg)) => {
            assert!(msg.contains("Failed to parse") || msg.contains("nonexistent_invalid_crs"));
        }
        _ => panic!("Expected RasterH3Error::CrsError for invalid PROJ string"),
    }
}

#[test]
fn test_plain_tiff_without_geokeys() {
    // 1. TIFF with no georeferencing tags at all MUST fail to open
    let (_unref_file, unref_path) = TestGeoTiffBuilder::new(10, 10)
        .georeferenced(false)
        .create_constant_tempfile(42.0f32);

    let unref_err = GeoTiffStreamReader::open(&unref_path);
    assert!(unref_err.is_err());
    match unref_err {
        Err(RasterH3Error::InvalidParameter(msg)) => {
            assert!(msg.contains("no georeferencing tags"));
        }
        _ => panic!("Expected RasterH3Error::InvalidParameter for unreferenced TIFF"),
    }

    // 2. TIFF with georeferencing tags but WITHOUT geokeys (no detected CRS)
    let (_temp_file, path) = TestGeoTiffBuilder::new(10, 10)
        .with_geokeys(false)
        .create_constant_tempfile(42.0f32);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    // Non-georeferenced images have no detected CRS
    assert_eq!(reader.metadata.epsg, None);
    assert_eq!(reader.metadata.proj_string, None);

    // 2a. Without specifying custom_crs, streamer initialization MUST fail with a CRS error
    let unspec_config = MultiResolutionConfig::single(4);
    let err_res = MultiScanHorizonStreamer::new(reader.clone(), &unspec_config);
    assert!(err_res.is_err());
    match err_res {
        Err(RasterH3Error::CrsNotDetected(msg)) => {
            assert!(
                msg.contains("No CRS detected in raster metadata"),
                "Unexpected message: {}",
                msg
            );
        }
        _ => panic!("Expected RasterH3Error::CrsNotDetected when CRS is not detected"),
    }

    // 2. When the user explicitly specifies a CRS, hexification succeeds
    let mut config = MultiResolutionConfig::single(4);
    config.custom_crs = Some("EPSG:4326".to_string());

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            total_pixels += rec.accumulator.count;
            assert_eq!(rec.accumulator.mean(), 42.0);
        }
    }
    assert_eq!(total_pixels, 100.0);
}
