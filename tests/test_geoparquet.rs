use std::fs::File;
use std::io::BufWriter;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::RowAccessor;

fn create_test_geotiff(width: usize, height: usize) -> NamedTempFile {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let mut data = Vec::with_capacity(width * height);
    for row in 0..height {
        let r_f = row as f32;
        for col in 0..width {
            let c_f = col as f32;
            let val = (r_f * 0.1).sin() * 20.0 + (c_f * 0.1).cos() * 15.0 + 100.0;
            data.push(val);
        }
    }

    let file = File::create(&path).unwrap();
    let writer = BufWriter::new(file);
    let mut encoder = TiffEncoder::new(writer).unwrap();
    let mut image = encoder
        .new_image::<Gray32Float>(width as u32, height as u32)
        .unwrap();

    // Top-left: SF Bay (-122.45, 37.80)
    image
        .encoder()
        .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.45, 37.80, 0.0][..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[0.001f64, 0.001, 0.0][..])
        .unwrap();

    let geokeys: [u16; 12] = [
        1, 1, 0, 2,
        1024, 0, 1, 2,
        2048, 0, 1, 4326,
    ];
    image.encoder().write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
    image.write_data(&data).unwrap();

    temp_file
}

/// Helper to validate a 125-byte WKB 2D Polygon
fn validate_wkb_hexagon(bytes: &[u8]) {
    assert_eq!(bytes.len(), 125, "WKB hexagon polygon must be exactly 125 bytes");

    // Byte order: 1 = Little Endian
    assert_eq!(bytes[0], 0x01, "Byte order must be Little Endian (1)");

    // Type: 3 = WKB Polygon (2D)
    let geom_type = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
    assert_eq!(geom_type, 3, "WKB geometry type must be 3 (Polygon)");

    // Number of rings: 1 (exterior ring)
    let num_rings = u32::from_le_bytes(bytes[5..9].try_into().unwrap());
    assert_eq!(num_rings, 1, "Polygon must have 1 exterior ring");

    // Number of points: 7 (6 vertices + closed first point)
    let num_points = u32::from_le_bytes(bytes[9..13].try_into().unwrap());
    assert_eq!(num_points, 7, "Hexagon ring must have 7 vertices (closed)");

    // Parse coordinates
    let mut coords = Vec::with_capacity(7);
    for p in 0..7 {
        let offset = 13 + p * 16;
        let lon = f64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let lat = f64::from_le_bytes(bytes[offset + 8..offset + 16].try_into().unwrap());
        coords.push((lon, lat));
    }

    // Check closed ring: first vertex equals last vertex
    let (first_lon, first_lat) = coords[0];
    let (last_lon, last_lat) = coords[6];
    assert!((first_lon - last_lon).abs() < 1e-9, "First and last lon must match");
    assert!((first_lat - last_lat).abs() < 1e-9, "First and last lat must match");

    // Check bounds roughly around SF Bay
    for (lon, lat) in coords {
        assert!(lon >= -123.0 && lon <= -122.0, "Longitude {} out of expected range", lon);
        assert!(lat >= 37.5 && lat <= 38.0, "Latitude {} out of expected range", lat);
    }
}

#[test]
fn test_geoparquet_continuous_metadata_and_wkb() {
    let tiff_file = create_test_geotiff(60, 60);
    let tiff_path = tiff_file.path().to_str().unwrap();

    let parquet_file = NamedTempFile::new().unwrap();
    let parquet_path = parquet_file.path().to_path_buf();

    let config = MultiResolutionConfig::new(vec![8, 9]);
    let reader = GeoTiffStreamReader::open(tiff_path).unwrap();
    let streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let parquet_config = ParquetExportConfig {
        compact: false,
        row_group_size: 50,
        geoparquet: true,
        ..Default::default()
    };

    let total_written = H3ParquetWriter::write_continuous_streamer_to_parquet(
        streamer,
        &parquet_path,
        parquet_config,
    ).unwrap();
    assert!(total_written > 0, "Should have written rows");

    // Inspect Parquet file
    let file = File::open(&parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let meta = reader.metadata();

    // 1. Verify FileMetaData contains "geo" key
    let kv_meta = meta.file_metadata().key_value_metadata().expect("Should have key-value metadata");
    let geo_kv = kv_meta.iter().find(|kv| kv.key == "geo").expect("Should have 'geo' metadata key");
    let geo_val = geo_kv.value.as_ref().expect("geo metadata must have a value");

    // Parse JSON
    let geo_json: serde_json::Value = serde_json::from_str(geo_val).expect("geo metadata must be valid JSON");
    assert_eq!(geo_json["version"], "1.1.0", "GeoParquet specification version must be 1.1.0");
    assert_eq!(geo_json["primary_column"], "geometry");

    let col = &geo_json["columns"]["geometry"];
    assert_eq!(col["encoding"], "WKB");
    assert_eq!(col["geometry_types"][0], "Polygon");
    assert_eq!(col["crs"]["type"], "GeographicCRS");

    let bbox = col["bbox"].as_array().expect("bbox must be an array");
    assert_eq!(bbox.len(), 4);
    let min_lon = bbox[0].as_f64().unwrap();
    let min_lat = bbox[1].as_f64().unwrap();
    let max_lon = bbox[2].as_f64().unwrap();
    let max_lat = bbox[3].as_f64().unwrap();
    assert!(min_lon < max_lon, "min_lon < max_lon");
    assert!(min_lat < max_lat, "min_lat < max_lat");

    // 2. Verify Schema contains geometry column
    let schema = meta.file_metadata().schema_descr();
    let geom_idx = schema.columns().iter().position(|c| c.name() == "geometry")
        .expect("Schema must contain 'geometry' column");

    // 3. Verify WKB contents
    let mut rows_checked = 0usize;
    for row in reader.get_row_iter(None).unwrap() {
        let row = row.unwrap();
        let geom_bytes = row.get_bytes(geom_idx).unwrap();
        validate_wkb_hexagon(geom_bytes.data());
        rows_checked += 1;
    }
    assert_eq!(rows_checked, total_written);
}

#[test]
fn test_geoparquet_compact_continuous() {
    let tiff_file = create_test_geotiff(40, 40);
    let tiff_path = tiff_file.path().to_str().unwrap();

    let parquet_file = NamedTempFile::new().unwrap();
    let parquet_path = parquet_file.path().to_path_buf();

    let config = MultiResolutionConfig::new(vec![9]);
    let reader = GeoTiffStreamReader::open(tiff_path).unwrap();
    let streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let parquet_config = ParquetExportConfig {
        compact: true,
        row_group_size: 100,
        geoparquet: true,
        ..Default::default()
    };

    let total_written = H3ParquetWriter::write_continuous_streamer_to_parquet(
        streamer,
        &parquet_path,
        parquet_config,
    ).unwrap();
    assert!(total_written > 0);

    let file = File::open(&parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let meta = reader.metadata();

    // Compact columns: [h3_index, geometry, min_value, max_value, sum_value, avg_value, pixel_count]
    let schema = meta.file_metadata().schema_descr();
    assert_eq!(schema.num_columns(), 7, "Compact GeoParquet should have 7 columns");
    let geom_idx = schema.columns().iter().position(|c| c.name() == "geometry")
        .expect("Compact GeoParquet must have 'geometry' column");
    assert_eq!(geom_idx, 1);

    for row in reader.get_row_iter(None).unwrap() {
        let row = row.unwrap();
        let geom_bytes = row.get_bytes(geom_idx).unwrap();
        validate_wkb_hexagon(geom_bytes.data());
    }
}

#[test]
fn test_geoparquet_categorical() {
    let tiff_file = create_test_geotiff(40, 40);
    let tiff_path = tiff_file.path().to_str().unwrap();

    let parquet_file = NamedTempFile::new().unwrap();
    let parquet_path = parquet_file.path().to_path_buf();

    let config = MultiResolutionConfig::new(vec![8]);
    let reader = GeoTiffStreamReader::open(tiff_path).unwrap();
    let streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();

    let parquet_config = ParquetExportConfig {
        compact: true,
        is_categorical: true,
        geoparquet: true,
        ..Default::default()
    };

    let total_written = H3ParquetWriter::write_categorical_streamer_to_parquet(
        streamer,
        &parquet_path,
        parquet_config,
    ).unwrap();
    assert!(total_written > 0);

    let file = File::open(&parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let meta = reader.metadata();

    // Verify "geo" key in file metadata
    let kv_meta = meta.file_metadata().key_value_metadata().expect("Should have key-value metadata");
    let has_geo = kv_meta.iter().any(|kv| kv.key == "geo");
    assert!(has_geo, "Categorical GeoParquet must have 'geo' metadata key");

    // Compact categorical columns: [h3_index, geometry, majority, majority_fraction, pixel_count, distinct_classes, entropy]
    let schema = meta.file_metadata().schema_descr();
    assert_eq!(schema.num_columns(), 7, "Compact categorical GeoParquet should have 7 columns");
    let geom_idx = schema.columns().iter().position(|c| c.name() == "geometry")
        .expect("Categorical GeoParquet must have 'geometry' column");
    assert_eq!(geom_idx, 1);

    for row in reader.get_row_iter(None).unwrap() {
        let row = row.unwrap();
        let geom_bytes = row.get_bytes(geom_idx).unwrap();
        validate_wkb_hexagon(geom_bytes.data());
    }
}

#[test]
fn test_geoparquet_disabled_by_default() {
    let tiff_file = create_test_geotiff(30, 30);
    let tiff_path = tiff_file.path().to_str().unwrap();

    let parquet_file = NamedTempFile::new().unwrap();
    let parquet_path = parquet_file.path().to_path_buf();

    let config = MultiResolutionConfig::new(vec![8]);
    let reader = GeoTiffStreamReader::open(tiff_path).unwrap();
    let streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    // Default configuration: geoparquet is false
    let parquet_config = ParquetExportConfig {
        compact: true,
        ..Default::default()
    };
    assert!(!parquet_config.geoparquet);

    H3ParquetWriter::write_continuous_streamer_to_parquet(
        streamer,
        &parquet_path,
        parquet_config,
    ).unwrap();

    let file = File::open(&parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let meta = reader.metadata();

    // Ensure NO "geo" key in file metadata
    if let Some(kv_meta) = meta.file_metadata().key_value_metadata() {
        assert!(!kv_meta.iter().any(|kv| kv.key == "geo"), "Non-geoparquet must NOT have 'geo' metadata");
    }

    // Ensure NO geometry column in schema
    let schema = meta.file_metadata().schema_descr();
    assert_eq!(schema.num_columns(), 6, "Standard compact should have 6 columns");
    assert!(!schema.columns().iter().any(|c| c.name() == "geometry"), "Should not contain 'geometry' column");
}

#[test]
fn test_geoparquet_raster_source_end_to_end() {
    let tiff_file = create_test_geotiff(40, 40);
    let tiff_path = tiff_file.path().to_str().unwrap();

    let parquet_file = NamedTempFile::new().unwrap();
    let parquet_path = parquet_file.path().to_str().unwrap();

    let config = MultiResolutionConfig::new(vec![8]);
    let parquet_config = ParquetExportConfig {
        geoparquet: true,
        compact: true,
        ..Default::default()
    };

    let written = H3ParquetWriter::process_raster_source_to_parquet(
        tiff_path,
        parquet_path,
        config,
        parquet_config,
    ).unwrap();
    assert!(written > 0);

    let file = File::open(parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let meta = reader.metadata();

    let kv_meta = meta.file_metadata().key_value_metadata().unwrap();
    let geo_kv = kv_meta.iter().find(|kv| kv.key == "geo").expect("Must have geo metadata");
    let geo_json: serde_json::Value = serde_json::from_str(geo_kv.value.as_ref().unwrap()).unwrap();

    assert_eq!(geo_json["version"], "1.1.0");
    assert_eq!(geo_json["columns"]["geometry"]["encoding"], "WKB");
}
