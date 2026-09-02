use std::fs::File;
use std::io::Read;
use flate2::read::GzDecoder;
use raster_h3::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3::pmtiles::mvt::{MvtLayer, MvtValue};
use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use raster_h3::pmtiles::writer::{zxy_to_tile_id, PmtilesWriter};
use tempfile::NamedTempFile;

#[path = "helpers.rs"]
mod helpers;
use helpers::create_temp_geotiff;

#[test]
fn test_mvt_protobuf_encoding() {
    let mut layer = MvtLayer::new("h3_hexagons");

    let v1 = h3o::LatLng::new(37.7749, -122.4194).unwrap();
    let cell = v1.to_cell(h3o::Resolution::Seven);
    let vertices: Vec<h3o::LatLng> = cell.boundary().iter().copied().collect();

    let properties = vec![
        ("h3_index".into(), MvtValue::UInt(cell.into())),
        ("h3_hex".into(), MvtValue::String("8728308281fffff".to_string())),
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
    assert!(mvt_bytes.len() > 20, "MVT bytes should contain layer headers and features");
}

#[test]
fn test_pmtiles_v3_header_and_archive_validation() {
    let bbox = [-122.5, 37.7, -122.3, 37.9];
    let metadata_json = r#"{"name":"test_archive","vector_layers":[{"id":"h3_hexagons"}]}"#;
    let mut writer = PmtilesWriter::new(10, 12, bbox, metadata_json.to_string()).unwrap();

    let uncompressed_mvt = b"mock_mvt_tile_payload_bytes_for_testing";
    writer.add_tile(10, 163, 395, uncompressed_mvt).unwrap();
    writer.add_tile(11, 327, 791, uncompressed_mvt).unwrap();

    let tmp_file = NamedTempFile::new().unwrap();
    let pmtiles_path = tmp_file.path().to_str().unwrap().to_string();

    writer.finish(&pmtiles_path).unwrap();

    // Read back and validate PMTiles v3 binary specification
    let mut file = File::open(&pmtiles_path).unwrap();
    let mut header = [0u8; 127];
    file.read_exact(&mut header).unwrap();

    // 1. Magic bytes
    assert_eq!(&header[0..7], b"PMTiles", "Header must start with PMTiles magic bytes");
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

    assert_eq!(root_dir_offset, 127, "Root directory must start immediately after header");
    assert!(root_dir_len > 0, "Root directory length must be positive");
    assert_eq!(json_metadata_offset, root_dir_offset + root_dir_len);
    assert!(json_metadata_len > 0, "JSON metadata length must be positive");
    assert_eq!(tile_data_offset, json_metadata_offset + json_metadata_len);
    assert!(tile_data_len > 0, "Tile data length must be positive");
    assert_eq!(addressed_tiles, 2, "Should contain exactly 2 addressed tiles");

    // 4. Validate JSON metadata decompression
    let mut full_file = Vec::new();
    let mut file_read = File::open(&pmtiles_path).unwrap();
    file_read.read_to_end(&mut full_file).unwrap();

    let meta_slice = &full_file[(json_metadata_offset as usize)..((json_metadata_offset + json_metadata_len) as usize)];
    let mut gz = GzDecoder::new(meta_slice);
    let mut decompressed_json = String::new();
    gz.read_to_string(&mut decompressed_json).unwrap();
    assert!(decompressed_json.contains("h3_hexagons"), "Metadata must contain vector layer definition");
}

#[test]
fn test_geotiff_to_pmtiles_end_to_end() {
    let tiff_tmp = NamedTempFile::new().unwrap();
    let tiff_path = tiff_tmp.path().to_str().unwrap().to_string();
    create_temp_geotiff(&tiff_path, 256, 256, tiff::tags::CompressionMethod::None).unwrap();

    let pmtiles_tmp = NamedTempFile::new().unwrap();
    let pmtiles_path = pmtiles_tmp.path().to_str().unwrap().to_string();

    let config = MultiResolutionConfig::new(vec![7, 8]);
    let total_hexagons = H3PmtilesTiler::process_geotiff_to_pmtiles(
        &tiff_path,
        &pmtiles_path,
        config,
    ).unwrap();

    assert!(total_hexagons > 0, "Should have generated H3 hexagons across resolutions 7 and 8");

    // Verify PMTiles file size and header
    let file = File::open(&pmtiles_path).unwrap();
    let metadata = file.metadata().unwrap();
    assert!(metadata.len() > 127, "PMTiles file must be larger than the 127-byte header");

    let mut header = [0u8; 127];
    let mut f = File::open(&pmtiles_path).unwrap();
    f.read_exact(&mut header).unwrap();
    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);
}

#[test]
fn test_hilbert_zxy_tile_id_ordering() {
    use raster_h3::pmtiles::writer::tile_id_to_zxy;

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
    use raster_h3::pmtiles::tiler::H3Feature;
    use raster_h3::pmtiles::mvt::MvtValue;

    let pmtiles_tmp = NamedTempFile::new().unwrap();
    let pmtiles_path = pmtiles_tmp.path().to_str().unwrap().to_string();

    let valid_cell_1 = 0x8828308281fffffu64; // SF Res 8
    let valid_cell_2 = 0x8828308283fffffu64; // SF Res 8 neighbor
    let invalid_cell_1 = 0u64;
    let invalid_cell_2 = 0xFFFFFFFFFFFFFFFFu64;

    let features = vec![
        H3Feature::new(valid_cell_1, vec![
            ("population".to_string(), MvtValue::Double(1420.0)),
            ("category".to_string(), MvtValue::String("Urban".to_string())),
        ]),
        H3Feature::new(invalid_cell_1, vec![
            ("population".to_string(), MvtValue::Double(0.0)),
        ]),
        H3Feature::new(valid_cell_2, vec![
            ("population".to_string(), MvtValue::Double(980.0)),
            ("category".to_string(), MvtValue::String("Suburban".to_string())),
        ]),
        H3Feature::new(invalid_cell_2, vec![
            ("population".to_string(), MvtValue::Double(12.0)),
        ]),
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
    use raster_h3::pmtiles::tiler::h3_res_to_zoom;

    let mut prev_zoom = 0u8;
    for res in 0..=15 {
        let zoom = h3_res_to_zoom(res);
        assert!(zoom >= prev_zoom, "Zoom must be monotonically non-decreasing with H3 resolution (res: {}, zoom: {})", res, zoom);
        prev_zoom = zoom;
    }

    assert_eq!(h3_res_to_zoom(0), 0);
    assert_eq!(h3_res_to_zoom(8), 13);
    assert_eq!(h3_res_to_zoom(10), 16);
    assert_eq!(h3_res_to_zoom(15), 23);
}

#[test]
fn test_parquet_to_pmtiles_end_to_end() {
    use parquet::schema::parser::parse_message_type;
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use std::sync::Arc;

    let parquet_tmp = NamedTempFile::new().unwrap();
    let parquet_path = parquet_tmp.path().to_str().unwrap().to_string();

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
    col_writer.typed::<parquet::data_type::Int64Type>().write_batch(&[0x8828308281fffff, 0x8828308283fffff], None, None).unwrap();
    col_writer.close().unwrap();

    // Write column 1 (population)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    col_writer.typed::<parquet::data_type::DoubleType>().write_batch(&[850000.0, 120000.0], None, None).unwrap();
    col_writer.close().unwrap();

    // Write column 2 (city)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    let val1 = parquet::data_type::ByteArray::from("San Francisco");
    let val2 = parquet::data_type::ByteArray::from("Oakland");
    col_writer.typed::<parquet::data_type::ByteArrayType>().write_batch(&[val1, val2], None, None).unwrap();
    col_writer.close().unwrap();

    row_group.close().unwrap();
    writer.close().unwrap();

    // Test process_parquet_to_pmtiles
    let pmtiles_tmp = NamedTempFile::new().unwrap();
    let pmtiles_path = pmtiles_tmp.path().to_str().unwrap().to_string();

    let summary = H3PmtilesTiler::process_parquet_to_pmtiles(&parquet_path, &pmtiles_path, None).unwrap();
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
    use parquet::schema::parser::parse_message_type;
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use std::sync::Arc;

    let parquet_tmp = NamedTempFile::new().unwrap();
    let parquet_path = parquet_tmp.path().to_str().unwrap().to_string();

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
    col_writer.typed::<parquet::data_type::ByteArrayType>().write_batch(&[val1, val2], None, None).unwrap();
    col_writer.close().unwrap();

    // Write column 1 (metric)
    let mut col_writer = row_group.next_column().unwrap().unwrap();
    col_writer.typed::<parquet::data_type::DoubleType>().write_batch(&[99.5, 42.1], None, None).unwrap();
    col_writer.close().unwrap();

    row_group.close().unwrap();
    writer.close().unwrap();

    let pmtiles_tmp = NamedTempFile::new().unwrap();
    let pmtiles_path = pmtiles_tmp.path().to_str().unwrap().to_string();

    let summary = H3PmtilesTiler::process_parquet_to_pmtiles(&parquet_path, &pmtiles_path, Some("custom_hex_id")).unwrap();
    assert_eq!(summary.total_features, 2);
    assert_eq!(summary.valid_features, 2);
    assert_eq!(summary.invalid_features_dropped, 0);
    assert!(summary.total_tiles > 0);
}

#[test]
fn test_coarse_zoom_parent_mapping() {
    use raster_h3::pmtiles::tiler::h3_res_for_zoom;

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

    let pmtiles_tmp = NamedTempFile::new().unwrap();
    let pmtiles_path = pmtiles_tmp.path().to_str().unwrap().to_string();

    // Export fine resolutions 7 and 8
    let config = MultiResolutionConfig::new(vec![7, 8]);
    let total_hexagons = H3PmtilesTiler::process_geotiff_to_pmtiles(
        &tiff_path,
        &pmtiles_path,
        config,
    ).unwrap();

    assert!(total_hexagons > 0);

    // Read header and inspect tile count and archive structure
    let mut file = File::open(&pmtiles_path).unwrap();
    let mut header = [0u8; 127];
    file.read_exact(&mut header).unwrap();

    assert_eq!(&header[0..7], b"PMTiles");
    assert_eq!(header[7], 3);

    let addressed_tiles = u64::from_le_bytes(header[72..80].try_into().unwrap());
    assert!(addressed_tiles > 0, "Archive should contain multiple pyramid zoom tiles");

    let min_zoom = header[100];
    let max_zoom = header[101];
    assert_eq!(min_zoom, 0, "Min zoom should cover coarse zooms starting at 0");
    assert!(max_zoom >= 13, "Max zoom should reach fine resolution zoom >= 13");
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
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.50, 37.80, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(33550), &[0.001f64, 0.001, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(42113), "-9999")
            .unwrap();

        let geokeys: [u16; 12] = [
            1, 1, 0, 2,
            1024, 0, 1, 2,
            2048, 0, 1, 4326,
        ];
        image.encoder().write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let pmtiles_tmp = NamedTempFile::new().unwrap();
    let pmtiles_path = pmtiles_tmp.path().to_str().unwrap().to_string();

    let config = MultiResolutionConfig::new(vec![7, 8]);
    let total_hexagons = H3PmtilesTiler::process_geotiff_to_pmtiles(
        &tiff_path,
        &pmtiles_path,
        config,
    ).unwrap();

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
    use raster_h3::pmtiles::writer::{zxy_to_tile_id, tile_id_to_zxy};

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
                assert_eq!((dec_z, dec_x, dec_y), (z, x, y), "Hilbert mapping failed round-trip for ({}, {}, {}) -> ID {} -> ({}, {}, {})", z, x, y, tile_id, dec_z, dec_x, dec_y);
            }
        }
    }

    // 3. Monotonicity: Tile IDs for zoom level Z are strictly less than Tile IDs for zoom level Z+1
    let max_id_z4 = zxy_to_tile_id(4, 15, 15);
    let min_id_z5 = zxy_to_tile_id(5, 0, 0);
    assert!(max_id_z4 < min_id_z5, "Hilbert IDs across zoom levels must be strictly monotonic");
}




