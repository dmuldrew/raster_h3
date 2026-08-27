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
        ("h3_index".to_string(), MvtValue::UInt(cell.into())),
        ("h3_hex".to_string(), MvtValue::String("8728308281fffff".to_string())),
        ("mean".to_string(), MvtValue::Double(123.45)),
        ("count".to_string(), MvtValue::Double(42.0)),
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
    let mut writer = PmtilesWriter::new(10, 12, bbox, metadata_json.to_string());

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
    let id_0 = zxy_to_tile_id(0, 0, 0);
    assert_eq!(id_0, 0);

    let id_1_0_0 = zxy_to_tile_id(1, 0, 0);
    let id_1_1_0 = zxy_to_tile_id(1, 1, 0);
    let id_1_0_1 = zxy_to_tile_id(1, 0, 1);
    let id_1_1_1 = zxy_to_tile_id(1, 1, 1);

    assert!(id_1_0_0 > id_0);
    assert_ne!(id_1_0_0, id_1_1_0);
    assert_ne!(id_1_1_0, id_1_0_1);
    assert_ne!(id_1_0_1, id_1_1_1);
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
