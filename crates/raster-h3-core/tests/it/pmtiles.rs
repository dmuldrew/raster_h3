//! Tests PMTiles v3 archive generation and Mapbox Vector Tile (MVT) encoding.
//!
//! Validates Hilbert curve tile ID addressing, directory structures, MVT protobuf serialization,
//! selective property filtering, parent zoom aggregation, and GeoTIFF/Parquet-to-PMTiles pipelines.

use flate2::read::GzDecoder;
use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::pmtiles::mvt::{
    FeatureProperties, MercatorPoint, MvtLayer, MvtValue, PropertyFilter,
};
use raster_h3_core::pmtiles::tiler::H3PmtilesTiler;
use raster_h3_core::pmtiles::writer::{zxy_to_tile_id, PmtilesWriter};
use std::fs::File;
use std::io::Read;
use tempfile::NamedTempFile;

use crate::helpers;
#[path = "support/mvt.rs"]
mod mvt_decode;
use helpers::create_temp_geotiff;

#[test]
fn test_mvt_protobuf_encoding() {
    let mut layer = MvtLayer::new("h3_hexagons");

    let v1 = h3o::LatLng::new(37.7749, -122.4194).unwrap();
    let cell = v1.to_cell(h3o::Resolution::Seven);
    let vertices: Vec<h3o::LatLng> = cell.boundary().iter().copied().collect();

    let properties = vec![
        ("h3_index".into(), MvtValue::UInt(cell.into())),
        (
            "h3_hex".into(),
            MvtValue::String("8728308281fffff".to_string()),
        ),
        ("mean".into(), MvtValue::Double(123.45)),
        ("count".into(), MvtValue::Double(42.0)),
    ];

    layer.add_hexagon(
        cell.into(),
        &vertices,
        -123.0,
        -122.0,
        37.0,
        38.0,
        properties,
    );

    let mvt_bytes = layer.encode();
    assert!(!mvt_bytes.is_empty(), "Encoded MVT bytes must not be empty");
    assert!(
        mvt_bytes.len() > 20,
        "MVT bytes should contain layer headers and features"
    );
}

#[test]
fn test_pmtiles_v3_header_and_archive_validation() {
    let bbox = [-122.5, 37.7, -122.3, 37.9];
    let metadata_json = r#"{"name":"test_archive","vector_layers":[{"id":"h3_hexagons"}]}"#;
    let mut writer = PmtilesWriter::new(10, 12, bbox, metadata_json.to_string()).unwrap();

    let uncompressed_mvt = b"mock_mvt_tile_payload_bytes_for_testing";
    writer.add_tile(10, 163, 395, uncompressed_mvt).unwrap();
    writer.add_tile(11, 327, 791, uncompressed_mvt).unwrap();

    let tmp_file = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = tmp_file.to_str().unwrap().to_string();

    writer.finish(&pmtiles_path).unwrap();

    // Read back and validate PMTiles v3 binary specification
    let mut file = File::open(&pmtiles_path).unwrap();
    let mut header = [0u8; 127];
    file.read_exact(&mut header).unwrap();

    // 1. Magic bytes
    assert_eq!(
        &header[0..7],
        b"PMTiles",
        "Header must start with PMTiles magic bytes"
    );
    // 2. Version 3
    assert_eq!(header[7], 3, "PMTiles version must be 3");

    // 3. Offsets & Counts
    let root_dir_offset = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let root_dir_len = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let json_metadata_offset = u64::from_le_bytes(header[24..32].try_into().unwrap());
    let json_metadata_len = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let tile_data_offset = u64::from_le_bytes(header[56..64].try_into().unwrap());
    let tile_data_len = u64::from_le_bytes(header[64..72].try_into().unwrap());
    let addressed_tiles = u64::from_le_bytes(header[72..80].try_into().unwrap());

    assert_eq!(
        root_dir_offset, 127,
        "Root directory must start immediately after header"
    );
    assert!(root_dir_len > 0, "Root directory length must be positive");
    assert_eq!(json_metadata_offset, root_dir_offset + root_dir_len);
    assert!(
        json_metadata_len > 0,
        "JSON metadata length must be positive"
    );
    assert_eq!(tile_data_offset, json_metadata_offset + json_metadata_len);
    assert!(tile_data_len > 0, "Tile data length must be positive");
    assert_eq!(
        addressed_tiles, 2,
        "Should contain exactly 2 addressed tiles"
    );

    // 4. Validate JSON metadata decompression
    let mut full_file = Vec::new();
    let mut file_read = File::open(&pmtiles_path).unwrap();
    file_read.read_to_end(&mut full_file).unwrap();

    let meta_slice = &full_file
        [(json_metadata_offset as usize)..((json_metadata_offset + json_metadata_len) as usize)];
    let mut gz = GzDecoder::new(meta_slice);
    let mut decompressed_json = String::new();
    gz.read_to_string(&mut decompressed_json).unwrap();
    assert!(
        decompressed_json.contains("h3_hexagons"),
        "Metadata must contain vector layer definition"
    );
}

#[test]
fn test_geotiff_to_pmtiles_end_to_end() {
    let tiff_tmp = NamedTempFile::new().unwrap();
    let tiff_path = tiff_tmp.path().to_str().unwrap().to_string();
    create_temp_geotiff(&tiff_path, 256, 256, tiff::tags::CompressionMethod::None).unwrap();

    let pmtiles_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = pmtiles_tmp.to_str().unwrap().to_string();

    let config = MultiResolutionConfig::new(vec![7, 8]);
    let total_hexagons =
        H3PmtilesTiler::process_geotiff_to_pmtiles(&tiff_path, &pmtiles_path, config).unwrap();

    assert!(
        total_hexagons > 0,
        "Should have generated H3 hexagons across resolutions 7 and 8"
    );

    // Verify PMTiles file size and header
    let file = File::open(&pmtiles_path).unwrap();
    let metadata = file.metadata().unwrap();
    assert!(
        metadata.len() > 127,
        "PMTiles file must be larger than the 127-byte header"
    );

    let mut header = [0u8; 127];
    let mut f = File::open(&pmtiles_path).unwrap();
    f.read_exact(&mut header).unwrap();
    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);
}

#[test]
fn test_hilbert_zxy_tile_id_ordering() {
    use raster_h3_core::pmtiles::writer::tile_id_to_zxy;

    assert_eq!(zxy_to_tile_id(0, 0, 0), 0);
    assert_eq!(zxy_to_tile_id(1, 0, 0), 1);
    assert_eq!(zxy_to_tile_id(1, 0, 1), 2);
    assert_eq!(zxy_to_tile_id(1, 1, 1), 3);
    assert_eq!(zxy_to_tile_id(1, 1, 0), 4);
    assert_eq!(zxy_to_tile_id(2, 0, 0), 5);

    // Test roundtrip conversion for all tiles up to zoom 7
    for z in 0..=7 {
        for x in 0..(1 << z) {
            for y in 0..(1 << z) {
                let id = zxy_to_tile_id(z, x, y);
                let (rz, rx, ry) = tile_id_to_zxy(id);
                assert_eq!((rz, rx, ry), (z, x, y));
            }
        }
    }
}

#[test]
fn test_export_generic_h3_features_with_validation() {
    use raster_h3_core::pmtiles::mvt::MvtValue;
    use raster_h3_core::pmtiles::tiler::H3Feature;

    let pmtiles_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = pmtiles_tmp.to_str().unwrap().to_string();

    let valid_cell_1 = 0x8828308281fffffu64; // SF Res 8
    let valid_cell_2 = 0x8828308283fffffu64; // SF Res 8 neighbor
    let invalid_cell_1 = 0u64;
    let invalid_cell_2 = 0xFFFFFFFFFFFFFFFFu64;

    let features = vec![
        H3Feature::new(
            valid_cell_1,
            vec![
                ("population".to_string(), MvtValue::Double(1420.0)),
                (
                    "category".to_string(),
                    MvtValue::String("Urban".to_string()),
                ),
            ],
        ),
        H3Feature::new(
            invalid_cell_1,
            vec![("population".to_string(), MvtValue::Double(0.0))],
        ),
        H3Feature::new(
            valid_cell_2,
            vec![
                ("population".to_string(), MvtValue::Double(980.0)),
                (
                    "category".to_string(),
                    MvtValue::String("Suburban".to_string()),
                ),
            ],
        ),
        H3Feature::new(
            invalid_cell_2,
            vec![("population".to_string(), MvtValue::Double(12.0))],
        ),
    ];

    let summary = H3PmtilesTiler::export_h3_features(features, &pmtiles_path).unwrap();

    assert_eq!(summary.total_features, 4);
    assert_eq!(summary.valid_features, 2);
    assert_eq!(summary.invalid_features_dropped, 2);
    assert!(summary.total_tiles > 0);

    // Verify PMTiles v3 archive
    let mut header = [0u8; 127];
    let mut f = File::open(&pmtiles_path).unwrap();
    f.read_exact(&mut header).unwrap();
    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);
}

#[test]
fn test_h3_res_to_zoom_monotonicity_all_levels() {
    use raster_h3_core::pmtiles::tiler::h3_res_to_zoom;

    let mut prev_zoom = 0u8;
    for res in 0..=15 {
        let zoom = h3_res_to_zoom(res);
        assert!(
            zoom >= prev_zoom,
            "Zoom must be monotonically non-decreasing with H3 resolution (res: {}, zoom: {})",
            res,
            zoom
        );
        prev_zoom = zoom;
    }

    assert_eq!(h3_res_to_zoom(0), 0);
    assert_eq!(h3_res_to_zoom(8), 13);
    assert_eq!(h3_res_to_zoom(10), 16);
    assert_eq!(h3_res_to_zoom(15), 23);
}

#[test]
fn test_parquet_to_pmtiles_end_to_end() {
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    let parquet_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let parquet_path = parquet_tmp.to_str().unwrap().to_string();

    let message_type = "
        message schema {
            REQUIRED INT64 h3_index;
            REQUIRED DOUBLE population;
            REQUIRED BYTE_ARRAY city (UTF8);
        }
    ";
    let schema = Arc::new(parse_message_type(message_type).unwrap());
    let props = Arc::new(WriterProperties::builder().build());
    let file = File::create(&parquet_path).unwrap();
    let mut writer = SerializedFileWriter::new(file, schema, props).unwrap();
    let mut row_group = writer.next_row_group().unwrap();

    // Write column 0 (h3_index)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    col_writer
        .typed::<parquet::data_type::Int64Type>()
        .write_batch(&[0x8828308281fffff, 0x8828308283fffff], None, None)
        .unwrap();
    col_writer.close().unwrap();

    // Write column 1 (population)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    col_writer
        .typed::<parquet::data_type::DoubleType>()
        .write_batch(&[850000.0, 120000.0], None, None)
        .unwrap();
    col_writer.close().unwrap();

    // Write column 2 (city)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    let val1 = parquet::data_type::ByteArray::from("San Francisco");
    let val2 = parquet::data_type::ByteArray::from("Oakland");
    col_writer
        .typed::<parquet::data_type::ByteArrayType>()
        .write_batch(&[val1, val2], None, None)
        .unwrap();
    col_writer.close().unwrap();

    row_group.close().unwrap();
    writer.close().unwrap();

    // Test process_parquet_to_pmtiles
    let pmtiles_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = pmtiles_tmp.to_str().unwrap().to_string();

    let summary =
        H3PmtilesTiler::process_parquet_to_pmtiles(&parquet_path, &pmtiles_path, None).unwrap();
    assert_eq!(summary.total_features, 2);
    assert_eq!(summary.valid_features, 2);
    assert_eq!(summary.invalid_features_dropped, 0);
    assert!(summary.total_tiles > 0);

    // Verify PMTiles v3 archive header
    let mut header = [0u8; 127];
    let mut f = File::open(&pmtiles_path).unwrap();
    f.read_exact(&mut header).unwrap();
    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);
}

#[test]
fn test_parquet_to_pmtiles_custom_col_and_hex_string() {
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    let parquet_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let parquet_path = parquet_tmp.to_str().unwrap().to_string();

    let message_type = "
        message schema {
            REQUIRED BYTE_ARRAY custom_hex_id (UTF8);
            REQUIRED DOUBLE metric;
        }
    ";
    let schema = Arc::new(parse_message_type(message_type).unwrap());
    let props = Arc::new(WriterProperties::builder().build());
    let file = File::create(&parquet_path).unwrap();
    let mut writer = SerializedFileWriter::new(file, schema, props).unwrap();
    let mut row_group = writer.next_row_group().unwrap();

    // Write column 0 (custom_hex_id as hex string)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    let val1 = parquet::data_type::ByteArray::from("8828308281fffff");
    let val2 = parquet::data_type::ByteArray::from("8828308283fffff");
    col_writer
        .typed::<parquet::data_type::ByteArrayType>()
        .write_batch(&[val1, val2], None, None)
        .unwrap();
    col_writer.close().unwrap();

    // Write column 1 (metric)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    col_writer
        .typed::<parquet::data_type::DoubleType>()
        .write_batch(&[99.5, 42.1], None, None)
        .unwrap();
    col_writer.close().unwrap();

    row_group.close().unwrap();
    writer.close().unwrap();

    let pmtiles_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = pmtiles_tmp.to_str().unwrap().to_string();

    let summary = H3PmtilesTiler::process_parquet_to_pmtiles(
        &parquet_path,
        &pmtiles_path,
        Some("custom_hex_id"),
    )
    .unwrap();
    assert_eq!(summary.total_features, 2);
    assert_eq!(summary.valid_features, 2);
    assert_eq!(summary.invalid_features_dropped, 0);
    assert!(summary.total_tiles > 0);
}

#[test]
fn test_parquet_to_pmtiles_multi_row_group_eviction() {
    use h3o::{LatLng, Resolution};
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    let parquet_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let parquet_path = parquet_tmp.to_str().unwrap().to_string();

    let message_type = "
        message schema {
            REQUIRED INT64 h3_index;
            REQUIRED DOUBLE val;
        }
    ";
    let schema = Arc::new(parse_message_type(message_type).unwrap());
    let props = Arc::new(WriterProperties::builder().build());
    let file = File::create(&parquet_path).unwrap();
    let mut writer = SerializedFileWriter::new(file, schema, props).unwrap();

    // RG 0: Los Angeles (South, lat ~34.0)
    let la_cell = LatLng::new(34.0522, -118.2437)
        .unwrap()
        .to_cell(Resolution::Eight);
    let mut rg0 = writer.next_row_group().unwrap();
    let mut c0 = rg0.next_column().unwrap().unwrap();
    c0.typed::<parquet::data_type::Int64Type>()
        .write_batch(&[u64::from(la_cell) as i64], None, None)
        .unwrap();
    c0.close().unwrap();
    let mut c1 = rg0.next_column().unwrap().unwrap();
    c1.typed::<parquet::data_type::DoubleType>()
        .write_batch(&[10.0], None, None)
        .unwrap();
    c1.close().unwrap();
    rg0.close().unwrap();

    // RG 1: Seattle (North, lat ~47.6)
    let sea_cell = LatLng::new(47.6062, -122.3321)
        .unwrap()
        .to_cell(Resolution::Eight);
    let mut rg1 = writer.next_row_group().unwrap();
    let mut c0 = rg1.next_column().unwrap().unwrap();
    c0.typed::<parquet::data_type::Int64Type>()
        .write_batch(&[u64::from(sea_cell) as i64], None, None)
        .unwrap();
    c0.close().unwrap();
    let mut c1 = rg1.next_column().unwrap().unwrap();
    c1.typed::<parquet::data_type::DoubleType>()
        .write_batch(&[20.0], None, None)
        .unwrap();
    c1.close().unwrap();
    rg1.close().unwrap();

    // RG 2: San Francisco (Mid, lat ~37.7)
    let sf_cell = LatLng::new(37.7749, -122.4194)
        .unwrap()
        .to_cell(Resolution::Eight);
    let mut rg2 = writer.next_row_group().unwrap();
    let mut c0 = rg2.next_column().unwrap().unwrap();
    c0.typed::<parquet::data_type::Int64Type>()
        .write_batch(&[u64::from(sf_cell) as i64], None, None)
        .unwrap();
    c0.close().unwrap();
    let mut c1 = rg2.next_column().unwrap().unwrap();
    c1.typed::<parquet::data_type::DoubleType>()
        .write_batch(&[30.0], None, None)
        .unwrap();
    c1.close().unwrap();
    rg2.close().unwrap();

    writer.close().unwrap();

    let pmtiles_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = pmtiles_tmp.to_str().unwrap().to_string();

    let summary =
        H3PmtilesTiler::process_parquet_to_pmtiles(&parquet_path, &pmtiles_path, None).unwrap();
    assert_eq!(summary.total_features, 3);
    assert_eq!(summary.valid_features, 3);
    assert_eq!(summary.invalid_features_dropped, 0);
    assert!(summary.total_tiles >= 3);

    // Verify PMTiles v3 archive header
    let mut header = [0u8; 127];
    let mut f = File::open(&pmtiles_path).unwrap();
    f.read_exact(&mut header).unwrap();
    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);
}

#[test]
fn test_coarse_zoom_parent_mapping() {
    use raster_h3_core::pmtiles::tiler::h3_res_for_zoom;

    // Verify natural H3 resolution assignment per zoom level
    assert_eq!(h3_res_for_zoom(0), 0);
    assert_eq!(h3_res_for_zoom(1), 0);
    assert_eq!(h3_res_for_zoom(2), 1);
    assert_eq!(h3_res_for_zoom(3), 1);
    assert_eq!(h3_res_for_zoom(4), 2);
    assert_eq!(h3_res_for_zoom(5), 3);
    assert_eq!(h3_res_for_zoom(6), 4);
    assert_eq!(h3_res_for_zoom(7), 4);
    assert_eq!(h3_res_for_zoom(8), 5);
    assert_eq!(h3_res_for_zoom(9), 5);
    assert_eq!(h3_res_for_zoom(10), 6);
    assert_eq!(h3_res_for_zoom(11), 7);
    assert_eq!(h3_res_for_zoom(12), 7);
    assert_eq!(h3_res_for_zoom(13), 8);
    assert_eq!(h3_res_for_zoom(14), 9);
}

#[test]
fn test_pmtiles_coarse_zoom_parent_aggregation_content() {
    let tiff_tmp = NamedTempFile::new().unwrap();
    let tiff_path = tiff_tmp.path().to_str().unwrap().to_string();
    create_temp_geotiff(&tiff_path, 64, 64, tiff::tags::CompressionMethod::None).unwrap();

    let pmtiles_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = pmtiles_tmp.to_str().unwrap().to_string();

    // Export fine resolutions 7 and 8
    let config = MultiResolutionConfig::new(vec![7, 8]);
    let total_hexagons =
        H3PmtilesTiler::process_geotiff_to_pmtiles(&tiff_path, &pmtiles_path, config).unwrap();

    assert!(total_hexagons > 0);

    // Read header and inspect tile count and archive structure
    let mut file = File::open(&pmtiles_path).unwrap();
    let mut header = [0u8; 127];
    file.read_exact(&mut header).unwrap();

    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);

    let addressed_tiles = u64::from_le_bytes(header[72..80].try_into().unwrap());
    assert!(
        addressed_tiles > 0,
        "Archive should contain multiple pyramid zoom tiles"
    );

    let min_zoom = header[100];
    let max_zoom = header[101];
    assert_eq!(
        min_zoom, 0,
        "Min zoom should cover coarse zooms starting at 0"
    );
    assert!(
        max_zoom >= 13,
        "Max zoom should reach fine resolution zoom >= 13"
    );
}

#[test]
fn test_all_nodata_geotiff_to_pmtiles_export() {
    let tiff_tmp = NamedTempFile::new().unwrap();
    let tiff_path = tiff_tmp.path().to_str().unwrap().to_string();

    // Create a 64x64 GeoTIFF where all values are NoData (-9999.0)
    let width = 64;
    let height = 64;
    let nodata_val = -9999.0f32;
    let data = vec![nodata_val; width * height];

    {
        use std::io::BufWriter;
        use tiff::encoder::colortype::Gray32Float;
        use tiff::encoder::TiffEncoder;
        use tiff::tags::Tag;

        let file = File::create(&tiff_path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder
            .new_image::<Gray32Float>(width as u32, height as u32)
            .unwrap();

        image
            .encoder()
            .write_tag(
                Tag::Unknown(33922),
                &[-0.0f64, 0.0, 0.0, -122.50, 37.80, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(33550), &[0.001f64, 0.001, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(42113), "-9999")
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image
            .encoder()
            .write_tag(Tag::Unknown(34735), &geokeys[..])
            .unwrap();
        image.write_data(&data).unwrap();
    }

    let pmtiles_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_path = pmtiles_tmp.to_str().unwrap().to_string();

    let config = MultiResolutionConfig::new(vec![7, 8]);
    let total_hexagons =
        H3PmtilesTiler::process_geotiff_to_pmtiles(&tiff_path, &pmtiles_path, config).unwrap();

    // Should complete cleanly with 0 hexagons emitted
    assert_eq!(total_hexagons, 0);

    // Archive should be a valid PMTiles v3 archive with 0 addressed tiles
    let mut file = File::open(&pmtiles_path).unwrap();
    let mut header = [0u8; 127];
    file.read_exact(&mut header).unwrap();

    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);
    let addressed_tiles = u64::from_le_bytes(header[72..80].try_into().unwrap());
    assert_eq!(addressed_tiles, 0);
}

#[test]
fn test_hilbert_zxy_tile_id_bijective_roundtrip() {
    use raster_h3_core::pmtiles::writer::{tile_id_to_zxy, zxy_to_tile_id};

    // 1. Base case: Zoom 0 root tile
    assert_eq!(zxy_to_tile_id(0, 0, 0), 0);
    assert_eq!(tile_id_to_zxy(0), (0, 0, 0));

    // 2. Comprehensive bijection across zoom levels 0 through 14
    for z in 0..=14 {
        let max_coord = 1u32 << z;
        let test_coords = [
            (0, 0),
            (0, max_coord - 1),
            (max_coord - 1, 0),
            (max_coord - 1, max_coord - 1),
            (max_coord / 2, max_coord / 2),
            (max_coord / 3, (2 * max_coord) / 3),
            ((3 * max_coord) / 4, max_coord / 4),
        ];

        for &(x, y) in &test_coords {
            if x < max_coord && y < max_coord {
                let tile_id = zxy_to_tile_id(z, x, y);
                let (dec_z, dec_x, dec_y) = tile_id_to_zxy(tile_id);
                assert_eq!(
                    (dec_z, dec_x, dec_y),
                    (z, x, y),
                    "Hilbert mapping failed round-trip for ({}, {}, {}) -> ID {} -> ({}, {}, {})",
                    z,
                    x,
                    y,
                    tile_id,
                    dec_z,
                    dec_x,
                    dec_y
                );
            }
        }
    }

    // 3. Monotonicity: Tile IDs for zoom level Z are strictly less than Tile IDs for zoom level Z+1
    let max_id_z4 = zxy_to_tile_id(4, 15, 15);
    let min_id_z5 = zxy_to_tile_id(5, 0, 0);
    assert!(
        max_id_z4 < min_id_z5,
        "Hilbert IDs across zoom levels must be strictly monotonic"
    );
}

#[test]
fn test_property_filter_and_mvt_encoding() {
    let filter = PropertyFilter::parse("mean,count");
    assert!(filter.is_custom);
    assert!(filter.has_continuous(raster_h3_core::pmtiles::mvt::PROP_MEAN));
    assert!(filter.has_continuous(raster_h3_core::pmtiles::mvt::PROP_COUNT));
    assert!(!filter.has_continuous(raster_h3_core::pmtiles::mvt::PROP_STDDEV));
    assert!(!filter.needs_stddev());

    let mut layer = MvtLayer::with_filter("h3_hexagons", filter);
    let cell_idx: u64 = 0x8828308281fffff;
    let vertices = [
        MercatorPoint { x: 0.1, y: 0.1 },
        MercatorPoint { x: 0.2, y: 0.1 },
        MercatorPoint { x: 0.2, y: 0.2 },
    ];
    let props = FeatureProperties::Continuous {
        h3_index: cell_idx,
        resolution: 8,
        mean: 42.5,
        sum: 850.0,
        stddev: 1.2,
        count: 20.0,
        min: 40.0,
        max: 45.0,
    };
    layer.add_hexagon_mercator(cell_idx, &vertices, 8, 10, 20, props);

    let mvt_bytes = layer.encode();
    assert!(!mvt_bytes.is_empty());

    // Protobuf inspection: keys "mean" and "count" should be encoded, while "stddev", "min", "max" should not
    let mvt_str = String::from_utf8_lossy(&mvt_bytes);
    assert!(mvt_str.contains("mean"), "Should contain key 'mean'");
    assert!(mvt_str.contains("count"), "Should contain key 'count'");
    assert!(
        !mvt_str.contains("stddev"),
        "Should NOT contain key 'stddev'"
    );
    assert!(!mvt_str.contains("sum"), "Should NOT contain key 'sum'");
    assert!(!mvt_str.contains("min"), "Should NOT contain key 'min'");
    assert!(!mvt_str.contains("max"), "Should NOT contain key 'max'");
}

#[test]
fn test_geotiff_to_pmtiles_selective_properties() {
    let tiff_tmp = NamedTempFile::new().unwrap();
    let tiff_path = tiff_tmp.path().to_str().unwrap().to_string();
    create_temp_geotiff(&tiff_path, 256, 256, tiff::tags::CompressionMethod::None).unwrap();

    // 1. Generate full archive with all properties
    let pmtiles_all_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_all_path = pmtiles_all_tmp.to_str().unwrap().to_string();
    let config_all = MultiResolutionConfig::new(vec![7, 8]);
    let hex_all =
        H3PmtilesTiler::process_geotiff_to_pmtiles(&tiff_path, &pmtiles_all_path, config_all)
            .unwrap();
    let size_all = File::open(&pmtiles_all_path)
        .unwrap()
        .metadata()
        .unwrap()
        .len();

    // 2. Generate selective archive with only "mean,count"
    let pmtiles_sel_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let pmtiles_sel_path = pmtiles_sel_tmp.to_str().unwrap().to_string();
    let mut config_sel = MultiResolutionConfig::new(vec![7, 8]);
    config_sel.properties = Some("mean,count".to_string());
    let hex_sel =
        H3PmtilesTiler::process_geotiff_to_pmtiles(&tiff_path, &pmtiles_sel_path, config_sel)
            .unwrap();
    let size_sel = File::open(&pmtiles_sel_path)
        .unwrap()
        .metadata()
        .unwrap()
        .len();

    assert_eq!(hex_all, hex_sel, "Hexagon count should be identical");
    assert!(
        size_sel < size_all,
        "Selective properties archive ({}) should be smaller than full archive ({})",
        size_sel,
        size_all
    );

    // Read and validate JSON metadata fields in selective archive
    let mut f = File::open(&pmtiles_sel_path).unwrap();
    let mut header = [0u8; 127];
    f.read_exact(&mut header).unwrap();
    let json_metadata_offset = u64::from_le_bytes(header[24..32].try_into().unwrap());
    let json_metadata_len = u64::from_le_bytes(header[32..40].try_into().unwrap());

    let mut full_file = Vec::new();
    let mut file_read = File::open(&pmtiles_sel_path).unwrap();
    file_read.read_to_end(&mut full_file).unwrap();
    let meta_slice = &full_file
        [(json_metadata_offset as usize)..((json_metadata_offset + json_metadata_len) as usize)];
    let mut gz = GzDecoder::new(meta_slice);
    let mut decompressed_json = String::new();
    gz.read_to_string(&mut decompressed_json).unwrap();

    let v: serde_json::Value = serde_json::from_str(&decompressed_json).unwrap();
    let fields = &v["vector_layers"][0]["fields"];
    assert!(
        fields.get("mean").is_some(),
        "Metadata fields must contain 'mean'"
    );
    assert!(
        fields.get("count").is_some(),
        "Metadata fields must contain 'count'"
    );
    assert!(
        fields.get("stddev").is_none(),
        "Metadata fields must NOT contain 'stddev'"
    );
    assert!(
        fields.get("sum").is_none(),
        "Metadata fields must NOT contain 'sum'"
    );
    assert!(
        fields.get("min").is_none(),
        "Metadata fields must NOT contain 'min'"
    );
    assert!(
        fields.get("max").is_none(),
        "Metadata fields must NOT contain 'max'"
    );
    assert!(
        fields.get("h3_hex").is_none(),
        "Metadata fields must NOT contain 'h3_hex'"
    );
}

#[test]
fn test_cell_to_tile_coordinate_mapping() {
    use h3o::{CellIndex, LatLng};
    use raster_h3_core::pmtiles::tiler::lon_lat_to_tile_xy;

    let cell_u64 = 0x83464efffffffffu64;
    let cell = CellIndex::try_from(cell_u64).expect("valid cell index");
    let center: LatLng = cell.into();

    let (tx5, ty5) = lon_lat_to_tile_xy(center.lng(), center.lat(), 5);
    let (tx7, ty7) = lon_lat_to_tile_xy(center.lng(), center.lat(), 7);

    // Zoom 7 coordinates should be consistent with Zoom 5 parent tile (4x4 sub-tiles)
    assert_eq!(tx7 >> 2, tx5, "Z7 tile x shifted by 2 must match Z5 tile x");
    assert_eq!(ty7 >> 2, ty5, "Z7 tile y shifted by 2 must match Z5 tile y");
}

#[test]
fn test_pmtiles_writer_finish_with_bare_filename() {
    use raster_h3_core::pmtiles::writer::PmtilesWriter;
    use std::fs;
    use std::path::Path;

    let bare_filename = "test_bare_output_archive.pmtiles";
    let _ = fs::remove_file(bare_filename);

    let mut writer =
        PmtilesWriter::new(0, 1, [-180.0, -90.0, 180.0, 90.0], "{}".to_string()).unwrap();
    writer.add_tile(0, 0, 0, b"fake_mvt_payload").unwrap();

    let finish_res = writer.finish(Path::new(bare_filename));
    assert!(
        finish_res.is_ok(),
        "finish with bare filename must succeed: {:?}",
        finish_res.err()
    );

    let path = Path::new(bare_filename);
    assert!(path.exists(), "PMTiles archive must exist at bare path");
    assert!(path.metadata().unwrap().len() > 127);

    // Clean up
    let _ = fs::remove_file(bare_filename);
}

#[test]
fn test_mvt_class_iii_pentagon_10_vertices_preserved() {
    use h3o::CellIndex;
    use raster_h3_core::pmtiles::pyramid::cell_boundary_mercator;

    // Resolution 7 pentagon has 10 vertices due to Class III icosahedron face crossings
    let pentagon_res7 = CellIndex::try_from(0x870800000ffffffu64).unwrap();
    assert!(pentagon_res7.is_pentagon());
    let (v_merc, v_count) = cell_boundary_mercator(pentagon_res7);
    assert_eq!(v_count, 10);

    let center: h3o::LatLng = pentagon_res7.into();
    let (tx, ty) =
        raster_h3_core::pmtiles::tiler::lon_lat_to_tile_xy(center.lng(), center.lat(), 11);
    let props = FeatureProperties::Continuous {
        h3_index: pentagon_res7.into(),
        resolution: 7,
        mean: 10.0,
        sum: 100.0,
        stddev: 1.0,
        count: 10.0,
        min: 9.0,
        max: 11.0,
    };

    let feat = raster_h3_core::pmtiles::mvt::MvtFeature::from_mercator(
        pentagon_res7.into(),
        &v_merc[..v_count],
        11,
        tx,
        ty,
        4096,
        props,
    );
    assert_eq!(
        feat.num_points, 10,
        "Class III pentagon must retain all 10 vertices in MvtFeature"
    );

    let mut layer = MvtLayer::new("h3_hexagons");
    layer.add_feature(feat);
    let encoded = layer.encode();
    let decoded = mvt_decode::decode(&encoded);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].id, u64::from(pentagon_res7));
    assert_eq!(decoded[0].rings.len(), 1);
    assert_eq!(decoded[0].rings[0].len(), 10);
}

#[test]
fn test_pmtiles_antimeridian_straddling_cell_tile_range_and_unwrap() {
    use h3o::{LatLng, Resolution};
    use raster_h3_core::pmtiles::mvt::{MercatorPoint, MvtFeature};
    use raster_h3_core::pmtiles::pyramid::{cell_boundary_mercator, for_each_cell_tile_mercator};

    let cell = LatLng::new(-18.0, 180.0).unwrap().to_cell(Resolution::Five);
    let center: LatLng = cell.into();
    let center_merc = MercatorPoint::from_lat_lng(center.lat(), center.lng());
    let (v_merc, v_count) = cell_boundary_mercator(cell);
    let vertices_merc = &v_merc[..v_count];

    let z = 8u8;
    let n = 1u32 << z; // 256 tiles across
    let mut touched_tiles = Vec::new();
    for_each_cell_tile_mercator(center_merc, vertices_merc, z, |tx, ty| {
        touched_tiles.push((tx, ty));
    });

    // Must only touch tiles immediately adjacent to the antimeridian (tx == 0 or tx == 255),
    // never all 256 tiles across the world!
    assert!(
        touched_tiles.len() <= 4,
        "Antimeridian cell should touch at most 4 tiles at z=8, got {}",
        touched_tiles.len()
    );
    assert!(touched_tiles.iter().any(|&(x, _)| x == 0));
    assert!(touched_tiles.iter().any(|&(x, _)| x == n - 1));
    assert_eq!(
        touched_tiles
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        touched_tiles.len()
    );
    for &(tx, _ty) in &touched_tiles {
        assert!(
            tx == 0 || tx == n - 1,
            "Unexpected tile x={tx} for antimeridian cell at z=8"
        );
    }

    // Verify local MVT coordinates are continuous and not stretched across 256 * 4096 units
    for &(tx, ty) in &touched_tiles {
        let feat =
            MvtFeature::from_mercator(cell.into(), vertices_merc, z, tx, ty, 4096, Vec::new());
        let xs = &feat.polygon_x[..feat.num_points as usize];
        let min_x = *xs.iter().min().unwrap();
        let max_x = *xs.iter().max().unwrap();
        assert!(
            max_x - min_x < 4096,
            "Tile (z={z}, tx={tx}) polygon x span {} should be < 4096",
            max_x - min_x
        );
    }
}

#[test]
fn test_pmtiles_coarse_zoom_parent_aggregates_all_children() {
    use h3o::{CellIndex, LatLng, Resolution};
    use raster_h3_core::aggregator::accumulator::H3Accumulator;
    use raster_h3_core::aggregator::categorical::CategoricalAccumulator;
    use raster_h3_core::aggregator::multi_horizon::{
        MultiCategoricalRecord, MultiContinuousRecord, RecordStreamer,
    };

    struct VecStreamer<R> {
        records: std::vec::IntoIter<R>,
        resolutions: Vec<u8>,
    }

    impl<R> RecordStreamer for VecStreamer<R> {
        type Record = R;
        fn drain_completed_into<F>(
            &mut self,
            max_rows: usize,
            mut consumer: F,
        ) -> raster_h3_core::error::Result<usize>
        where
            F: FnMut(usize, Self::Record),
        {
            let mut n = 0;
            while n < max_rows.min(2) {
                let Some(rec) = self.records.next() else {
                    break;
                };
                consumer(n, rec);
                n += 1;
            }
            Ok(n)
        }
        fn current_lat_horizon(&self) -> f64 {
            if self.is_finished() {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        }
        fn is_finished(&self) -> bool {
            self.records.len() == 0
        }
        fn resolution_u8s(&self) -> &[u8] {
            &self.resolutions
        }
    }

    // Take a Res 5 parent cell and all 7 of its Res 6 children
    let parent = LatLng::new(37.7749, -122.4194)
        .unwrap()
        .to_cell(Resolution::Five);
    let children: Vec<CellIndex> = parent.children(Resolution::Six).collect();
    assert_eq!(children.len(), 7);

    // 1. Continuous: give each child a distinct value (10, 20, ..., 70) with count = 2.0
    // Total count = 14.0, total sum = 560.0, mean = 40.0, min = 10.0, max = 70.0
    let cont_records: Vec<MultiContinuousRecord> = children
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let val = ((i + 1) * 10) as f64;
            MultiContinuousRecord {
                resolution: 6,
                h3_index: c.into(),
                accumulator: H3Accumulator::new_weighted(val, 2.0),
            }
        })
        .collect();

    let cont_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let cont_path = cont_tmp.to_str().unwrap().to_string();
    let count = H3PmtilesTiler::generate_from_continuous_streamer(
        VecStreamer {
            records: cont_records.into_iter(),
            resolutions: vec![6],
        },
        &cont_path,
    )
    .unwrap();
    assert_eq!(count, 7);

    // Read the first tile in the archive (zoom 0, tile_id 0, right at tile_data_offset)
    let mut full = Vec::new();
    File::open(&cont_path)
        .unwrap()
        .read_to_end(&mut full)
        .unwrap();
    let tile_data_offset = u64::from_le_bytes(full[56..64].try_into().unwrap()) as usize;
    let mut gz = GzDecoder::new(&full[tile_data_offset..]);
    let mut mvt_z0 = Vec::new();
    gz.read_to_end(&mut mvt_z0).unwrap();

    let decoded = mvt_decode::decode(&mvt_z0);
    let expected_parent = parent.parent(Resolution::Zero).unwrap();
    assert_eq!(decoded.len(), 1, "one complete parent, without duplicates");
    let feature = &decoded[0];
    assert_eq!(feature.id, u64::from(expected_parent));
    for (key, expected) in [
        ("sum", 560.0),
        ("mean", 40.0),
        ("count", 14.0),
        ("min", 10.0),
        ("max", 70.0),
    ] {
        assert_eq!(feature.properties[key], MvtValue::Double(expected), "{key}");
    }
    let MvtValue::Double(stddev) = feature.properties["stddev"] else {
        panic!("stddev must be a double")
    };
    assert!((stddev - (5600.0_f64 / 13.0).sqrt()).abs() < 1e-10);

    // Header bounds must cover the overview parent polygon, not just the detail cells.
    let header_deg =
        |off: usize| i32::from_le_bytes(full[off..off + 4].try_into().unwrap()) as f64 / 1e7;
    let (min_lon, min_lat, max_lon, max_lat) = (
        header_deg(102),
        header_deg(106),
        header_deg(110),
        header_deg(114),
    );
    for v in expected_parent.boundary().iter() {
        assert!(
            v.lng() >= min_lon - 1e-6
                && v.lng() <= max_lon + 1e-6
                && v.lat() >= min_lat - 1e-6
                && v.lat() <= max_lat + 1e-6,
            "overview vertex ({}, {}) outside header bbox [{min_lon}, {min_lat}, {max_lon}, {max_lat}]",
            v.lng(),
            v.lat()
        );
    }

    // 2. Categorical: child 0 has class 1 (count 1), children 1..7 have class 2 (count 10 each -> 60 total)
    // If only child 0 were used, majority would be 1 and count would be 1.0; merged parent has majority 2 and count 61.0!
    let cat_records: Vec<MultiCategoricalRecord> = children
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let mut acc = CategoricalAccumulator::new();
            if i == 0 {
                acc.update_weighted(1, 1.0);
            } else {
                acc.update_weighted(2, 10.0);
            }
            MultiCategoricalRecord {
                resolution: 6,
                h3_index: c.into(),
                accumulator: acc,
            }
        })
        .collect();

    let cat_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let cat_path = cat_tmp.to_str().unwrap().to_string();
    H3PmtilesTiler::generate_from_categorical_streamer(
        VecStreamer {
            records: cat_records.into_iter(),
            resolutions: vec![6],
        },
        &cat_path,
    )
    .unwrap();

    let mut cat_full = Vec::new();
    File::open(&cat_path)
        .unwrap()
        .read_to_end(&mut cat_full)
        .unwrap();
    let cat_tile_offset = u64::from_le_bytes(cat_full[56..64].try_into().unwrap()) as usize;
    let mut cat_gz = GzDecoder::new(&cat_full[cat_tile_offset..]);
    let mut cat_mvt_z0 = Vec::new();
    cat_gz.read_to_end(&mut cat_mvt_z0).unwrap();
    let decoded = mvt_decode::decode(&cat_mvt_z0);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].id, u64::from(expected_parent));
    let props = &decoded[0].properties;
    assert_eq!(props["count"], MvtValue::Double(61.0));
    assert_eq!(props["majority"], MvtValue::Int(2));
    assert_eq!(props["majority_fraction"], MvtValue::Double(60.0 / 61.0));
}

#[test]
fn world_tile_encodes_both_antimeridian_pieces_in_one_feature() {
    use raster_h3_core::pmtiles::pyramid::cell_boundary_mercator;
    for lat in [-18.0, 52.0] {
        let cell = h3o::LatLng::new(lat, 180.0)
            .unwrap()
            .to_cell(h3o::Resolution::Zero);
        let (vertices, count) = cell_boundary_mercator(cell);
        let mut layer = MvtLayer::new("h3_hexagons");
        layer.add_hexagon_mercator(u64::from(cell), &vertices[..count], 0, 0, 0, Vec::new());
        let decoded = mvt_decode::decode(&layer.encode());
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, u64::from(cell));
        let rings = &decoded[0].rings;
        assert_eq!(rings.len(), 2);
        assert_eq!(rings[0].len(), count);
        assert_eq!(rings[1].len(), count);
        let crosses = |ring: &Vec<(i32, i32)>, edge: i32| {
            ring.iter().any(|p| p.0 < edge) && ring.iter().any(|p| p.0 > edge)
        };
        assert!(rings.iter().any(|r| crosses(r, 0)));
        assert!(rings.iter().any(|r| crosses(r, 4096)));
        let shift = rings[1][0].0 - rings[0][0].0;
        assert_eq!(shift.abs(), 4096);
        for (a, b) in rings[0].iter().zip(&rings[1]) {
            assert_eq!((b.0 - a.0, b.1 - a.1), (shift, 0));
        }
    }
}

#[test]
fn test_pmtiles_header_bounds_include_cell_vertices_and_antimeridian_center() {
    use raster_h3_core::pmtiles::tiler::H3Feature;

    // 1. Single cell in SF: header bbox must reflect cell vertex extent (not a zero-area point)
    let sf_cell = 0x85283083fffffffu64; // Res 5 SF
    let sf_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let sf_path = sf_tmp.to_str().unwrap().to_string();
    H3PmtilesTiler::export_h3_features([H3Feature::new::<&str>(sf_cell, Vec::new())], &sf_path)
        .unwrap();

    let mut header = [0u8; 127];
    File::open(&sf_path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let min_lon_e7 = i32::from_le_bytes(header[102..106].try_into().unwrap());
    let min_lat_e7 = i32::from_le_bytes(header[106..110].try_into().unwrap());
    let max_lon_e7 = i32::from_le_bytes(header[110..114].try_into().unwrap());
    let max_lat_e7 = i32::from_le_bytes(header[114..118].try_into().unwrap());
    assert!(
        max_lon_e7 > min_lon_e7 && max_lat_e7 > min_lat_e7,
        "Single-cell PMTiles header bbox must have positive width and height: [{min_lon_e7}, {min_lat_e7}, {max_lon_e7}, {max_lat_e7}]"
    );

    // 2. Antimeridian-crossing cells in Fiji: header center_lon must lie near ±180° (Pacific), not 0° (Atlantic)
    let fiji_cell = h3o::LatLng::new(-18.0, 180.0)
        .unwrap()
        .to_cell(h3o::Resolution::Five);
    let fiji_tmp = NamedTempFile::new().unwrap().into_temp_path();
    let fiji_path = fiji_tmp.to_str().unwrap().to_string();
    H3PmtilesTiler::export_h3_features(
        [H3Feature::new::<&str>(fiji_cell.into(), Vec::new())],
        &fiji_path,
    )
    .unwrap();

    File::open(&fiji_path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let fiji_min_lon = i32::from_le_bytes(header[102..106].try_into().unwrap()) as f64 / 1e7;
    let fiji_max_lon = i32::from_le_bytes(header[110..114].try_into().unwrap()) as f64 / 1e7;
    let fiji_center_lon = i32::from_le_bytes(header[119..123].try_into().unwrap()) as f64 / 1e7;
    assert!(
        fiji_min_lon == -180.0 && fiji_max_lon == 180.0,
        "Antimeridian archive bounds must be ordered and cover both pieces, got [{fiji_min_lon}, {fiji_max_lon}]"
    );
    assert!(
        fiji_center_lon.abs() > 179.0,
        "Antimeridian center_lon must be near ±180° in the Pacific, got {fiji_center_lon}"
    );
}

#[test]
fn legacy_world_tile_preserves_both_antimeridian_pieces() {
    for lat in [-18.0, 52.0] {
        let cell = h3o::LatLng::new(lat, 180.0)
            .unwrap()
            .to_cell(h3o::Resolution::Zero);
        let boundary = cell.boundary();
        let mut layer = MvtLayer::new("h3_hexagons");
        layer.add_hexagon(
            cell.into(),
            &boundary,
            -180.0,
            180.0,
            -85.05112878,
            85.05112878,
            Vec::new(),
        );
        let decoded = mvt_decode::decode(&layer.encode());
        assert_eq!(decoded.len(), 1);
        let rings = &decoded[0].rings;
        assert_eq!(rings.len(), 2);
        for edge in [0, 4096] {
            assert!(rings
                .iter()
                .any(|r| r.iter().any(|p| p.0 < edge) && r.iter().any(|p| p.0 > edge)));
        }
        let shift = rings[1][0].0 - rings[0][0].0;
        assert_eq!(shift.abs(), 4096);
        for (a, b) in rings[0].iter().zip(&rings[1]) {
            assert_eq!((b.0 - a.0, b.1 - a.1), (shift, 0));
        }
    }
}

#[test]
fn wrapped_writer_bounds_preserve_center_after_metadata_update() {
    for update_metadata in [false, true] {
        let bbox = [170.0, -25.0, -178.0, -15.0];
        let mut writer = PmtilesWriter::new(0, 0, bbox, "{}".into()).unwrap();
        if update_metadata {
            writer.set_metadata([175.0, -25.0, -177.0, -15.0], "{}".into());
        }
        writer.add_tile(0, 0, 0, b"test").unwrap();
        let tmp = NamedTempFile::new().unwrap().into_temp_path();
        writer.finish(&tmp).unwrap();
        let mut header = [0u8; 127];
        File::open(&tmp).unwrap().read_exact(&mut header).unwrap();
        let coord = |offset| {
            i32::from_le_bytes(header[offset..offset + 4].try_into().unwrap()) as f64 / 1e7
        };
        assert_eq!(coord(102), -180.0);
        assert_eq!(coord(110), 180.0);
        assert_eq!(coord(106), -25.0);
        assert_eq!(coord(114), -15.0);
        assert_eq!(coord(119), if update_metadata { 179.0 } else { 176.0 });
        assert_eq!(coord(123), -20.0);
    }
}
