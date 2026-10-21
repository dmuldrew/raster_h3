//! Scan-horizon streaming across raster sample types, NoData handling, and error paths.

use crate::helpers;
use crate::helpers::{create_constant_f32_geotiff, TestGeoTiffBuilder};
use h3o::{LatLng, Resolution};
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::crs::CrsTransformer;
use raster_h3_core::error::RasterH3Error;
use raster_h3_core::raster::GeoTiffStreamReader;
use std::path::Path;
use tempfile::NamedTempFile;

#[test]
fn test_scan_horizon_streamer_with_prefetch_and_coherence() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    create_constant_f32_geotiff(&path, 100, 100, -122.45, 37.85, 0.001, 75.0);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = MultiResolutionConfig::single(9);

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let mut all_yielded = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(10).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            all_yielded.push((rec.h3_index, rec.accumulator));
        }
    }

    assert!(!all_yielded.is_empty());
    let total_pixels: f64 = all_yielded.iter().map(|(_, acc)| acc.count).sum();
    assert_eq!(total_pixels, 10000.0);

    for (_, acc) in &all_yielded {
        assert_eq!(acc.mean(), 75.0);
        assert_eq!(acc.min, 75.0);
        assert_eq!(acc.max, 75.0);
    }
}

#[test]
fn test_web_mercator_streaming() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    TestGeoTiffBuilder::new(50, 50)
        .origin(-13630000.0, 4550000.0)
        .pixel_size(100.0)
        .epsg(3857)
        .write_constant(&path, 120.0f32)
        .unwrap();

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut config = MultiResolutionConfig::single(8);
    config.custom_crs = Some("EPSG:3857".to_string());

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_count: f64 = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(64).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in &batch {
            total_count += rec.accumulator.count;
            assert_eq!(rec.accumulator.mean(), 120.0);
        }
    }
    assert_eq!(total_count, 2500.0);
}

#[test]
fn test_categorical_horizon_streaming() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(50, 50)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_tempfile(|_c, r| if r < 25 { 10u8 } else { 50u8 });

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = MultiResolutionConfig::single(9);

    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut all_yielded = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            all_yielded.push((rec.h3_index, rec.accumulator));
        }
    }

    assert!(!all_yielded.is_empty());
    let total_pixels: f64 = all_yielded.iter().map(|(_, acc)| acc.total_count).sum();
    assert_eq!(total_pixels, 2500.0);

    for (_, acc) in &all_yielded {
        let (maj_cat, _, maj_frac) = acc.majority();
        assert!(maj_cat == 10 || maj_cat == 50);
        assert!(maj_frac > 0.0 && maj_frac <= 1.0);
    }
}

#[test]
fn test_u16_raster_streaming() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(40, 40)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_constant_tempfile(1250u16);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = MultiResolutionConfig::single(9);

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = rec.accumulator;
            total_pixels += acc.count;
            assert_eq!(acc.mean(), 1250.0);
            assert_eq!(acc.min, 1250.0);
            assert_eq!(acc.max, 1250.0);
        }
    }
    assert_eq!(total_pixels, 1600.0);
}

#[test]
fn test_f64_raster_streaming() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(30, 30)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_constant_tempfile(273.15f64);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = MultiResolutionConfig::single(9);

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            total_pixels += rec.accumulator.count;
            assert!((rec.accumulator.mean() - 273.15).abs() < 1e-6);
        }
    }
    assert_eq!(total_pixels, 900.0);
}

#[test]
fn test_nodata_filtering_preserves_statistics() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(40, 40)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .gdal_nodata("-9999")
        .create_tempfile(|_c, r| if r < 20 { 100.0f32 } else { -9999.0f32 });

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    assert_eq!(reader.metadata.nodata, Some(-9999.0));

    let config = MultiResolutionConfig::single(9);

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = rec.accumulator;
            total_pixels += acc.count;
            // All statistics must strictly reflect the valid 100.0 values, never polluted by -9999.0
            assert_eq!(acc.mean(), 100.0);
            assert_eq!(acc.min, 100.0);
            assert_eq!(acc.max, 100.0);
            assert_eq!(acc.variance(), 0.0);
            assert_eq!(acc.stddev(), 0.0);
        }
    }

    // Exactly 800 valid pixels out of 1600 total
    assert_eq!(total_pixels, 800.0);
}

#[test]
fn test_nan_filtering_in_streaming() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(20, 20)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_tempfile(|_c, r| if r < 10 { 50.0f32 } else { f32::NAN });

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = MultiResolutionConfig::single(9);

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = rec.accumulator;
            total_pixels += acc.count;
            assert_eq!(acc.mean(), 50.0);
            assert_eq!(acc.min, 50.0);
            assert_eq!(acc.max, 50.0);
        }
    }

    assert_eq!(total_pixels, 200.0);
}

#[test]
fn test_custom_nodata_override() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(30, 30)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_tempfile(|c, r| if r * 30 + c < 100 { 100.0f32 } else { 0.0f32 });

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut config = MultiResolutionConfig::single(9);
    config.custom_nodata = Some(0.0);

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            total_pixels += rec.accumulator.count;
            assert_eq!(rec.accumulator.mean(), 100.0);
        }
    }

    // Exactly 100 signal pixels counted
    assert_eq!(total_pixels, 100.0);
}

#[test]
fn test_categorical_homogeneous_raster() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(30, 30)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_constant_tempfile(40u8);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = MultiResolutionConfig::single(9);

    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut all_yielded = Vec::new();

    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            all_yielded.push((rec.h3_index, rec.accumulator));
        }
    }

    assert!(!all_yielded.is_empty());
    let total_pixels: f64 = all_yielded.iter().map(|(_, acc)| acc.total_count).sum();
    assert_eq!(total_pixels, 900.0);

    for (_, acc) in &all_yielded {
        let (maj_cat, _, maj_frac) = acc.majority();
        assert_eq!(maj_cat, 40);
        assert!((maj_frac - 1.0).abs() < 1e-6);
        let json = acc.histogram_json();
        assert_eq!(json, "{\"40\": 1.0000}");
    }
}

#[test]
fn test_all_nodata_full_stream_scan_and_categorical() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(16, 16)
        .origin(-122.4194, 37.7749)
        .pixel_size(0.001)
        .create_constant_tempfile(-9999.0f32);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut config = MultiResolutionConfig::single(7);
    config.custom_nodata = Some(-9999.0);

    // Test MultiScanHorizonStreamer on 100% nodata raster
    let mut scan_streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_accumulated_pixels = 0.0;
    loop {
        let batch = scan_streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            total_accumulated_pixels += rec.accumulator.count;
        }
    }
    assert_eq!(total_accumulated_pixels, 0.0);

    // Test MultiCategoricalHorizonStreamer on 100% nodata raster
    let reader_cat = GeoTiffStreamReader::open(&path).unwrap();
    let mut cat_streamer = MultiCategoricalHorizonStreamer::new(reader_cat, &config).unwrap();
    let mut total_categorical_pixels = 0.0;
    loop {
        let batch = cat_streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            total_categorical_pixels += rec.accumulator.total_count;
        }
    }
    assert_eq!(total_categorical_pixels, 0.0);
}

#[test]
fn test_categorical_rle_alternating_and_interspersed_nodata() {
    let width = 64;
    let height = 64;
    let mut expected_valid_pixels = 0.0;
    for row in 0..height {
        for col in 0..width {
            if row < 32 || col % 10 != 5 {
                expected_valid_pixels += 1.0;
            }
        }
    }

    let (_temp_file, path) = TestGeoTiffBuilder::new(width as u32, height as u32)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_tempfile(|col, row| {
            if row < 32 {
                if col % 2 == 0 {
                    1u8
                } else {
                    2u8
                }
            } else if col % 10 == 5 {
                255u8 // NoData
            } else {
                10u8
            }
        });

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut config = MultiResolutionConfig::single(8);
    config.custom_nodata = Some(255.0);

    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut total_accumulated = 0.0;
    let mut class_1_total = 0.0;
    let mut class_2_total = 0.0;
    let mut class_10_total = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = rec.accumulator;
            total_accumulated += acc.total_count;
            class_1_total += acc.get_class_count(1);
            class_2_total += acc.get_class_count(2);
            class_10_total += acc.get_class_count(10);
            assert_eq!(acc.get_class_count(255), 0.0); // NoData sentinel never recorded
        }
    }

    assert_eq!(total_accumulated, expected_valid_pixels);
    assert!(class_1_total > 0.0);
    assert!(class_2_total > 0.0);
    assert!(class_10_total > 0.0);
    assert_eq!(
        class_1_total + class_2_total + class_10_total,
        expected_valid_pixels
    );
}

#[test]
fn test_scalar_resolution_and_parent_calculations() {
    let lat_lng = LatLng::new(37.7749, -122.4194).unwrap();

    let resolutions = [
        (0, Resolution::Zero),
        (4, Resolution::Four),
        (8, Resolution::Eight),
        (10, Resolution::Ten),
        (12, Resolution::Twelve),
        (15, Resolution::Fifteen),
    ];

    for (res_num, res_enum) in resolutions {
        let cell = lat_lng.to_cell(res_enum);
        let cell_u64: u64 = cell.into();

        // 1. Bitshift resolution extraction
        let extracted_res = (cell_u64 >> 52) & 0x0F;
        assert_eq!(extracted_res, res_num as u64);

        // 2. Parent calculation at lower resolution (if resolution > 0)
        if res_num > 4 {
            let parent_opt = cell.parent(Resolution::Four);
            assert!(parent_opt.is_some());
            let parent_u64: u64 = parent_opt.unwrap().into();
            let parent_res = (parent_u64 >> 52) & 0x0F;
            assert_eq!(parent_res, 4);
        }
    }
}

#[test]
fn test_invalid_resolution_parameter_error() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(10, 10).create_constant_tempfile(1.0f32);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    // Resolution 16 is invalid in H3 (valid range: 0..=15)
    let mut invalid_config = MultiResolutionConfig::single(16);
    invalid_config.custom_crs = Some("EPSG:4326".to_string());

    let res_cont = MultiScanHorizonStreamer::new(reader.clone(), &invalid_config);
    assert!(res_cont.is_err());
    match res_cont {
        Err(RasterH3Error::InvalidParameter(msg)) => {
            assert!(
                msg.contains("Invalid H3 resolution: 16"),
                "Unexpected message: {}",
                msg
            );
        }
        _ => panic!("Expected RasterH3Error::InvalidParameter"),
    }

    let res_cat = MultiCategoricalHorizonStreamer::new(reader, &invalid_config);
    assert!(res_cat.is_err());
    match res_cat {
        Err(RasterH3Error::InvalidParameter(msg)) => {
            assert!(
                msg.contains("Invalid H3 resolution: 16"),
                "Unexpected message: {}",
                msg
            );
        }
        _ => panic!("Expected RasterH3Error::InvalidParameter"),
    }
}

#[test]
fn test_nonexistent_file_path_error() {
    let result =
        GeoTiffStreamReader::open(Path::new("/nonexistent_directory/nonexistent_file.tif"));
    assert!(result.is_err());
    match result {
        Err(RasterH3Error::Io(_)) => {}
        _ => panic!("Expected RasterH3Error::Io"),
    }
}

#[test]
fn test_corrupted_and_empty_tiff_error_handling() {
    use std::io::Write;

    // 1. Zero-byte empty file
    let empty_file = NamedTempFile::new().unwrap();
    let empty_path = empty_file.path().to_path_buf();
    assert!(GeoTiffStreamReader::open(&empty_path).is_err());

    // 2. Corrupted invalid header magic bytes
    let mut invalid_magic_file = NamedTempFile::new().unwrap();
    invalid_magic_file
        .write_all(b"NOT_A_VALID_TIFF_STREAM")
        .unwrap();
    invalid_magic_file.flush().unwrap();
    assert!(GeoTiffStreamReader::open(invalid_magic_file.path()).is_err());

    // 3. Truncated TIFF header (valid magic bytes II 42 but truncated pointer)
    let mut truncated_file = NamedTempFile::new().unwrap();
    truncated_file.write_all(&[0x49, 0x49, 0x2A, 0x00]).unwrap();
    truncated_file.flush().unwrap();
    assert!(GeoTiffStreamReader::open(truncated_file.path()).is_err());
}

// Independent aggregation reference: decoded pixels, full affine coordinates,
// direct CRS transformation and H3 indexing; no walker, span, or eviction code.
fn for_each_hawaii_reference_pixel(path: &Path, mut visit: impl FnMut(f64, f64, f64)) {
    use tiff::decoder::DecodingResult;
    let reader =
        GeoTiffStreamReader::open(path).expect("Required external dataset missing or invalid");
    assert_eq!(reader.metadata.samples_per_pixel, 1);
    let crs = CrsTransformer::from_crs_or_epsg(
        reader.metadata.epsg,
        reader.metadata.proj_string.as_deref(),
    )
    .unwrap();
    let gt = reader.metadata.geotransform;
    let nodata = reader.metadata.nodata;
    let mut decoder = reader.open_decoder().unwrap();
    for index in 0..reader.chunk_layout.total_chunks {
        if reader.is_sparse_chunk(index) {
            continue;
        }
        let (chunk, decoded) = decoder.read_chunk(index).unwrap();
        macro_rules! visit_pixels {
            ($values:expr, $ty:ty) => {{
                let values = $values;
                let sentinel = nodata.map(|n| n as $ty);
                assert_eq!(values.len(), (chunk.width * chunk.height) as usize);
                for (i, raw) in values.into_iter().enumerate() {
                    let value = raw as f64;
                    if !value.is_finite() || sentinel == Some(raw) {
                        continue;
                    }
                    let col = chunk.col_offset as usize + i % chunk.width as usize;
                    let row = chunk.row_offset as usize + i / chunk.width as usize;
                    let (x, y) = gt.pixel_center_to_coord(col, row);
                    let (lon, lat) = crs.transform_point(x, y).unwrap();
                    visit(lon, lat, value);
                }
            }};
        }
        match decoded {
            DecodingResult::U8(v) => visit_pixels!(v, u8),
            DecodingResult::U16(v) => visit_pixels!(v, u16),
            DecodingResult::U32(v) => visit_pixels!(v, u32),
            DecodingResult::U64(v) => visit_pixels!(v, u64),
            DecodingResult::I8(v) => visit_pixels!(v, i8),
            DecodingResult::I16(v) => visit_pixels!(v, i16),
            DecodingResult::I32(v) => visit_pixels!(v, i32),
            DecodingResult::I64(v) => visit_pixels!(v, i64),
            DecodingResult::F32(v) => visit_pixels!(v, f32),
            DecodingResult::F64(v) => visit_pixels!(v, f64),
        }
    }
}

#[test]
#[ignore = "requires external dataset data/CFL_HI.tif; run in release mode"]
fn test_parallel_chunk_aggregation_hawaii_dataset() {
    use std::collections::HashMap;
    let path = &*helpers::data_path("CFL_HI.tif");
    let mut expected = HashMap::<u64, (f64, f64, f64, f64)>::new();
    for_each_hawaii_reference_pixel(path, |lon, lat, value| {
        let cell = LatLng::new(lat, lon)
            .unwrap()
            .to_cell(Resolution::Eight)
            .into();
        let acc = expected.entry(cell).or_insert((0.0, 0.0, value, value));
        acc.0 += 1.0;
        acc.1 += value;
        acc.2 = acc.2.min(value);
        acc.3 = acc.3.max(value);
    });
    assert!(!expected.is_empty());
    let config = MultiResolutionConfig::single(8);
    let mut stream =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(path).unwrap(), &config).unwrap();
    while !stream.is_finished() {
        for record in stream.fetch_next_batch(2048).unwrap() {
            let (count, sum, min, max) = expected
                .remove(&record.h3_index)
                .expect("Unexpected or duplicate cell");
            let acc = record.accumulator;
            assert_eq!(acc.count, count);
            assert_eq!(acc.min, min);
            assert_eq!(acc.max, max);
            assert!((acc.sum - sum).abs() <= 1e-9 * sum.abs().max(1.0));
        }
    }
    assert!(expected.is_empty(), "Missing cells");
}

#[test]
#[ignore = "requires external dataset data/LF2024_FBFM40_HI.tif; run in release mode"]
fn test_parallel_categorical_aggregation_hawaii_dataset() {
    use std::collections::HashMap;
    let path = &*helpers::data_path("LF2024_FBFM40_HI.tif");
    let maui = [-156.70, 20.55, -155.95, 21.05];
    let mut expected = HashMap::<u64, HashMap<i64, f64>>::new();
    let mut expected_maui = HashMap::<u64, HashMap<i64, f64>>::new();
    for_each_hawaii_reference_pixel(path, |lon, lat, value| {
        for res in [Resolution::Seven, Resolution::Eight] {
            let cell = u64::from(LatLng::new(lat, lon).unwrap().to_cell(res));
            *expected
                .entry(cell)
                .or_default()
                .entry(value as i64)
                .or_default() += 1.0;
            if res == Resolution::Eight
                && lon >= maui[0]
                && lon <= maui[2]
                && lat >= maui[1]
                && lat <= maui[3]
            {
                *expected_maui
                    .entry(cell)
                    .or_default()
                    .entry(value as i64)
                    .or_default() += 1.0;
            }
        }
    });
    for (mut reference, resolutions, bbox) in [
        (expected, vec![7, 8], None),
        (expected_maui, vec![8], Some(maui)),
    ] {
        assert!(!reference.is_empty());
        let mut config = MultiResolutionConfig::new(resolutions);
        config.bbox = bbox;
        let mut stream =
            MultiCategoricalHorizonStreamer::new(GeoTiffStreamReader::open(path).unwrap(), &config)
                .unwrap();
        while !stream.is_finished() {
            for record in stream.fetch_next_batch(2048).unwrap() {
                let histogram = reference
                    .remove(&record.h3_index)
                    .expect("Unexpected or duplicate cell");
                let mut actual = HashMap::new();
                record.accumulator.for_each_class(|class, count| {
                    actual.insert(class, count);
                });
                assert_eq!(actual, histogram);
                assert_eq!(
                    record.accumulator.total_count,
                    histogram.values().sum::<f64>()
                );
            }
        }
        assert!(reference.is_empty(), "Missing cells");
    }
}
