use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::{Gray16, Gray32Float, Gray64Float, Gray8};
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use h3o::{LatLng, Resolution};
use raster_h3::aggregator::{
    chunk_intersects_bbox, compute_cell_south_lat, is_chunk_all_nodata, AggregationConfig,
    CategoricalAccumulator, CategoricalHorizonStreamer, H3Accumulator,
    SamplingPattern, ScanHorizonStreamer,
};
use raster_h3::crs::CrsTransformer;
use raster_h3::error::RasterH3Error;
use raster_h3::functions::fast_hex_u64;
use raster_h3::raster::{ChunkLayout, GeoTiffStreamReader, GeoTransform, PrefetchedChunkReader};

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
fn test_accumulator_operations() {
    let mut acc1 = H3Accumulator::new(10.0);
    acc1.update(20.0);
    acc1.update(30.0);
    assert_eq!(acc1.count, 3.0);
    assert_eq!(acc1.sum, 60.0);
    assert_eq!(acc1.mean(), 20.0);
    assert_eq!(acc1.min, 10.0);
    assert_eq!(acc1.max, 30.0);

    let mut acc2 = H3Accumulator::new(40.0);
    acc2.update(50.0);

    acc1.merge(&acc2);
    assert_eq!(acc1.count, 5.0);
    assert_eq!(acc1.sum, 150.0);
    assert_eq!(acc1.mean(), 30.0);
    assert_eq!(acc1.min, 10.0);
    assert_eq!(acc1.max, 50.0);
    assert_eq!(acc1.variance(), 250.0);
    assert!((acc1.stddev() - 250.0f64.sqrt()).abs() < 1e-9);

    // Test weighted updates
    let mut acc_w = H3Accumulator::new_weighted(100.0, 0.4);
    acc_w.update_weighted(200.0, 0.6);
    assert_eq!(acc_w.count, 1.0);
    assert_eq!(acc_w.sum, 160.0);
    assert_eq!(acc_w.mean(), 160.0);
}

#[test]
fn test_subpixel_sampling_patterns() {
    let center = SamplingPattern::parse("center");
    assert!(center.is_single_point());

    let presets = ["rgss", "5point", "gaussian", "hex", "8rooks", "9point", "16point"];
    for name in presets {
        let pattern = SamplingPattern::parse(name);
        assert!(!pattern.is_single_point());
        let sum_w: f64 = pattern.points.iter().map(|p| p.weight).sum();
        assert!((sum_w - 1.0).abs() < 1e-9, "Weights did not sum to 1.0 for {}", name);
    }
}

#[test]
fn test_fast_hex_formatting() {
    let mut buf = [0u8; 16];
    let cell_u64 = 0x8828308281fffffu64;
    let hex_slice = fast_hex_u64(cell_u64, &mut buf);
    let hex_str = std::str::from_utf8(hex_slice).unwrap();
    assert_eq!(hex_str, format!("{:x}", cell_u64));

    // Test bidirectional parsing
    let parsed = raster_h3::functions::parse_hex_u64(hex_str).unwrap();
    assert_eq!(parsed, cell_u64);
}

#[test]
fn test_bitshift_resolution() {
    let lat_lng = h3o::LatLng::new(37.7749, -122.4194).unwrap();
    let cell = lat_lng.to_cell(h3o::Resolution::Eight);
    let cell_u64: u64 = cell.into();

    let res_bitshift = (cell_u64 >> 52) & 0x0F;
    assert_eq!(res_bitshift, 8);
    assert_eq!(u8::from(cell.resolution()), res_bitshift as u8);
}

#[test]
fn test_is_chunk_all_nodata() {
    let empty_slice = vec![-9999.0f32; 100];
    assert!(is_chunk_all_nodata(&empty_slice, Some(-9999.0), |x| x as f64));

    let mut mixed_slice = vec![-9999.0f32; 100];
    mixed_slice[50] = 12.0;
    assert!(!is_chunk_all_nodata(&mixed_slice, Some(-9999.0), |x| x as f64));
}

#[test]
fn test_compute_cell_south_lat() {
    let lat_lng = h3o::LatLng::new(37.7749, -122.4194).unwrap();
    let cell = lat_lng.to_cell(h3o::Resolution::Eight);
    let cell_u64: u64 = cell.into();

    // Now returns center lat as a fast proxy (always >= true south vertex lat)
    let south_lat = compute_cell_south_lat(cell_u64);
    assert!(south_lat > 37.76, "Center lat should be near input: {}", south_lat);
    assert!(south_lat < 37.79, "Center lat should be near input: {}", south_lat);
}

#[test]
fn test_scan_horizon_streamer_with_prefetch_and_coherence() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 100;
    let height = 100;
    let data = vec![75.0f32; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
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
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        custom_crs: None,
        custom_nodata: None,
        bbox: None,
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();

    let mut all_yielded = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(10);
        if batch.is_empty() {
            break;
        }
        all_yielded.extend(batch);
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
fn test_bounding_box_pruning() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // 100x100 raster from lon [-122.5, -122.4], lat [37.7, 37.8]
    let width = 100;
    let height = 100;
    let data = vec![50.0f32; width * height];

    {
        let file = File::create(&path).unwrap();
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

        let geokeys: [u16; 12] = [
            1, 1, 0, 2,
            1024, 0, 1, 2,
            2048, 0, 1, 4326,
        ];
        image.encoder().write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();

    // Query only a small sub-rectangle: lon [-122.48, -122.46], lat [37.72, 37.74]
    let config = AggregationConfig {
        resolution: 9,
        custom_crs: None,
        custom_nodata: None,
        bbox: Some([-122.48, 37.72, -122.46, 37.74]),
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut filtered_pixels: f64 = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            filtered_pixels += acc.count;
        }
    }

    // Should only cover the sub-rectangle (approx 20x20 = 400 pixels out of 10,000)
    assert!(filtered_pixels > 0.0);
    assert!(filtered_pixels < 10000.0);
}

#[test]
fn test_web_mercator_hoisted_streaming() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 50;
    let height = 50;
    let data = vec![120.0f32; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        // Web Mercator coords around San Francisco
        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -13630000.0, 4550000.0, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::Unknown(33550), &[100.0f64, 100.0, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [
            1, 1, 0, 2,
            1024, 0, 1, 1, // Projected
            3072, 0, 1, 3857, // EPSG:3857
        ];
        image.encoder().write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 8,
        custom_crs: Some("EPSG:3857".to_string()),
        custom_nodata: None,
        bbox: None,
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_count: f64 = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(64);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in &batch {
            total_count += acc.count;
            assert_eq!(acc.mean(), 120.0);
        }
    }
    assert_eq!(total_count, 2500.0);
}

#[test]
fn test_prefetched_chunk_reader() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 20;
    let height = 20;
    let data = vec![1.0f32; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let total_chunks = reader.chunk_layout.total_chunks;
    let indices = (0..total_chunks).collect();

    let prefetcher = PrefetchedChunkReader::spawn(reader, indices, 2);
    let mut count = 0;
    while let Some(res) = prefetcher.next_chunk() {
        assert!(res.is_ok());
        count += 1;
    }
    assert_eq!(count, total_chunks);
}

#[test]
fn test_prefetched_chunk_reader_multi_worker_ordering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi_worker_test.tif");
    let width = 64usize;
    let height = 64usize;
    let data: Vec<f32> = (0..width * height).map(|v| v as f32).collect();

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let total_chunks = reader.chunk_layout.total_chunks;
    let indices: Vec<u32> = (0..total_chunks).collect();

    // Verify ordering and correctness across 1, 2, and 4 worker threads
    for num_workers in [1, 2, 4] {
        let prefetcher = PrefetchedChunkReader::spawn_with_workers(
            reader.clone(),
            indices.clone(),
            4,
            num_workers,
        );

        let mut expected_chunk_idx = 0;
        while let Some(res) = prefetcher.next_chunk() {
            let (idx, bounds, _data) = res.expect("chunk decoding should succeed");
            assert_eq!(idx, expected_chunk_idx, "Chunk out of order for num_workers={}", num_workers);
            let expected_bounds = reader.chunk_layout.get_chunk_bounds(
                idx,
                reader.metadata.width,
                reader.metadata.height,
            );
            assert_eq!(bounds.col_offset, expected_bounds.col_offset);
            assert_eq!(bounds.row_offset, expected_bounds.row_offset);
            expected_chunk_idx += 1;
        }
        assert_eq!(expected_chunk_idx, total_chunks);
    }
}

#[test]
fn test_prefetched_chunk_reader_early_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("early_drop_test.tif");
    let width = 64usize;
    let height = 64usize;
    let data: Vec<f32> = (0..width * height).map(|v| v as f32).collect();

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let total_chunks = reader.chunk_layout.total_chunks;
    let indices: Vec<u32> = (0..total_chunks).collect();

    // Drop prefetcher after consuming only 1 chunk with 4 workers running
    let prefetcher = PrefetchedChunkReader::spawn_with_workers(reader, indices, 2, 4);
    assert!(prefetcher.next_chunk().is_some());
    drop(prefetcher); // should gracefully exit all threads without deadlock
}

#[test]
fn test_categorical_accumulator_operations() {
    let mut acc1 = CategoricalAccumulator::new();
    // Hex with 70 pixels of class 10 (Forest) and 30 pixels of class 20 (Grassland)
    for _ in 0..70 {
        acc1.update(10);
    }
    for _ in 0..30 {
        acc1.update(20);
    }

    assert_eq!(acc1.total_count, 100.0);
    assert_eq!(acc1.unique_classes(), 2);

    let (maj_cat, maj_cnt, maj_frac) = acc1.majority();
    assert_eq!(maj_cat, 10);
    assert_eq!(maj_cnt, 70.0);
    assert!((maj_frac - 0.70).abs() < 1e-6);

    let json = acc1.histogram_json();
    assert!(json.contains("\"10\": 0.7000"));
    assert!(json.contains("\"20\": 0.3000"));

    // Test weighted updates (e.g. sub-pixel sampling)
    let mut acc2 = CategoricalAccumulator::new();
    acc2.update_weighted(50, 0.5); // 0.5 of class 50 (Urban)
    acc2.update_weighted(10, 0.5);

    acc1.merge(&acc2);
    assert_eq!(acc1.total_count, 101.0);
    assert_eq!(acc1.unique_classes(), 3);
}

#[test]
fn test_categorical_horizon_streaming() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 50;
    let height = 50;
    // Upper half is class 10 (Forest), lower half is class 50 (Urban)
    let mut data = vec![10u8; width * height];
    for i in (width * height / 2)..(width * height) {
        data[i] = 50;
    }

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray8>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
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
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        custom_crs: None,
        custom_nodata: None,
        bbox: None,
        ..Default::default()
    };

    let mut streamer = CategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut all_yielded = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        all_yielded.extend(batch);
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
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 40;
    let height = 40;
    let data = vec![1250u16; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray16>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
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
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 30;
    let height = 30;
    let data = vec![273.15f64; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray64Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_pixels += acc.count;
            assert!((acc.mean() - 273.15).abs() < 1e-6);
        }
    }
    assert_eq!(total_pixels, 900.0);
}

#[test]
fn test_subpixel_rgss_streaming() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 40;
    let height = 40;
    let data = vec![25.0f32; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        sampling: SamplingPattern::rgss(),
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    let mut has_fractional_cell = false;

    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_pixels += acc.count;
            assert!((acc.mean() - 25.0).abs() < 1e-6);
            if (acc.count.fract() - 0.0).abs() > 1e-4 {
                has_fractional_cell = true;
            }
        }
    }

    // RGSS 4-point weights (0.25 each) must sum to the exact total pixels (1600.0)
    assert!((total_pixels - 1600.0).abs() < 1e-3, "Total RGSS area weight mismatch: {}", total_pixels);
    // Boundary hexagons should have fractional pixel area contributions
    assert!(has_fractional_cell, "Expected boundary hexagons to have fractional counts under RGSS super-sampling");
}

#[test]
fn test_subpixel_hex_streaming() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 30;
    let height = 30;
    let data = vec![42.0f32; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        sampling: SamplingPattern::hex_seven_point(),
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_pixels += acc.count;
            assert!((acc.mean() - 42.0).abs() < 1e-6);
        }
    }

    assert!((total_pixels - 900.0).abs() < 1e-3, "Total Hex 7-point area weight mismatch: {}", total_pixels);
}

#[test]
fn test_nodata_filtering_preserves_statistics() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 40;
    let height = 40;
    // Upper half is valid 100.0, lower half is -9999.0 (NoData)
    let mut data = vec![100.0f32; width * height];
    for i in (width * height / 2)..(width * height) {
        data[i] = -9999.0;
    }

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.encoder().write_tag(Tag::GdalNodata, "-9999").unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    assert_eq!(reader.metadata.nodata, Some(-9999.0));

    let config = AggregationConfig {
        resolution: 9,
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
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
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 20;
    let height = 20;
    let mut data = vec![50.0f32; width * height];
    // Set half the pixels to NaN
    for i in (width * height / 2)..(width * height) {
        data[i] = f32::NAN;
    }

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
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
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 30;
    let height = 30;
    // 0.0 is background/nodata, 100.0 is signal
    let mut data = vec![0.0f32; width * height];
    for i in 0..100 {
        data[i] = 100.0;
    }

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        custom_nodata: Some(0.0),
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_pixels += acc.count;
            assert_eq!(acc.mean(), 100.0);
        }
    }

    // Exactly 100 signal pixels counted
    assert_eq!(total_pixels, 100.0);
}

#[test]
fn test_crs_southern_hemisphere_utm() {
    // UTM Zone 33S (EPSG:32733) - Southern Africa / Namibia area
    let transformer = CrsTransformer::from_crs_or_epsg(Some(32733), None).unwrap();
    let (lon, lat) = transformer.transform_point(500000.0, 6200000.0).unwrap();
    // Central meridian for Zone 33 is 15°E
    assert!((lon - 15.0).abs() < 0.1, "Expected lon near 15°E, got: {}", lon);
    // Northing 6,200,000 in Southern hemisphere maps to ~ -34.3° S
    assert!(lat < -30.0 && lat > -40.0, "Expected negative latitude in S hemisphere, got: {}", lat);
}

#[test]
fn test_crs_custom_albers_proj_string() {
    // Standard CONUS Albers Equal-Area projection
    let proj_str = "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=37.5 +lon_0=-96 +x_0=0 +y_0=0 +datum=NAD83 +units=m +no_defs";
    let transformer = CrsTransformer::from_proj_string(proj_str).unwrap();

    // Origin (0.0, 0.0) in projection meters maps to (-96.0, 37.5) in WGS84
    let (lon, lat) = transformer.transform_point(0.0, 0.0).unwrap();
    assert!((lon - -96.0).abs() < 0.01, "Expected lon near -96.0, got: {}", lon);
    assert!((lat - 37.5).abs() < 0.01, "Expected lat near 37.5, got: {}", lat);
}

#[test]
fn test_crs_invalid_proj_string_error_handling() {
    let result = CrsTransformer::from_proj_string("+proj=totally_invalid_nonexistent_proj");
    assert!(result.is_err(), "Expected error on invalid PROJ string");
    match result {
        Err(RasterH3Error::CrsError(msg)) => {
            assert!(msg.contains("Failed to parse source PROJ string"), "Unexpected message: {}", msg);
        }
        _ => panic!("Expected RasterH3Error::CrsError"),
    }
}

#[test]
fn test_categorical_tied_majority() {
    let mut acc = CategoricalAccumulator::new();
    // Exactly 50 pixels of class 10 and 50 pixels of class 20
    for _ in 0..50 {
        acc.update(10);
    }
    for _ in 0..50 {
        acc.update(20);
    }

    assert_eq!(acc.total_count, 100.0);
    assert_eq!(acc.unique_classes(), 2);

    let (maj_cat, maj_cnt, maj_frac) = acc.majority();
    // Deterministically returns one of the two top classes
    assert!(maj_cat == 10 || maj_cat == 20);
    assert_eq!(maj_cnt, 50.0);
    assert!((maj_frac - 0.50).abs() < 1e-6);
}

#[test]
fn test_categorical_homogeneous_raster() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 30;
    let height = 30;
    // 100% class 40 (Cropland)
    let data = vec![40u8; width * height];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray8>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();

        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        image.encoder().write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 9,
        ..Default::default()
    };

    let mut streamer = CategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut all_yielded = Vec::new();

    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        all_yielded.extend(batch);
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
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let data = vec![1.0f32; 100];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let image = encoder.new_image::<Gray32Float>(10, 10).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    // Resolution 16 is invalid in H3 (valid range: 0..=15)
    let invalid_config = AggregationConfig {
        resolution: 16,
        ..Default::default()
    };

    let res_cont = ScanHorizonStreamer::new(reader.clone(), &invalid_config);
    assert!(res_cont.is_err());
    match res_cont {
        Err(RasterH3Error::InvalidParameter(msg)) => {
            assert!(msg.contains("Invalid H3 resolution: 16"), "Unexpected message: {}", msg);
        }
        _ => panic!("Expected RasterH3Error::InvalidParameter"),
    }

    let res_cat = CategoricalHorizonStreamer::new(reader, &invalid_config);
    assert!(res_cat.is_err());
    match res_cat {
        Err(RasterH3Error::InvalidParameter(msg)) => {
            assert!(msg.contains("Invalid H3 resolution: 16"), "Unexpected message: {}", msg);
        }
        _ => panic!("Expected RasterH3Error::InvalidParameter"),
    }
}

#[test]
fn test_nonexistent_file_path_error() {
    let result = GeoTiffStreamReader::open(Path::new("/nonexistent_directory/nonexistent_file.tif"));
    assert!(result.is_err());
    match result {
        Err(RasterH3Error::Io(_)) => {}
        _ => panic!("Expected RasterH3Error::Io"),
    }
}

#[test]
fn test_plain_tiff_without_geokeys() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    let data = vec![42.0f32; 100];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let image = encoder.new_image::<Gray32Float>(10, 10).unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    // Non-georeferenced images should gracefully fallback to default GeoTransform & identity CRS
    assert_eq!(reader.metadata.epsg, None);
    assert_eq!(reader.metadata.geotransform, GeoTransform::default());

    let config = AggregationConfig {
        resolution: 4,
        ..Default::default()
    };

    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_pixels += acc.count;
            assert_eq!(acc.mean(), 42.0);
        }
    }
    assert_eq!(total_pixels, 100.0);
}

#[test]
fn test_tiled_chunk_layout_calculation() {
    // 512x512 image tiled into 256x256 tiles -> 2 cols x 2 rows = 4 tiles total
    let layout = ChunkLayout::new_tiled(512, 512, 256, 256);
    assert_eq!(layout.total_chunks, 4);
    assert_eq!(layout.chunks_across, 2);
    assert_eq!(layout.chunks_down, 2);

    // Tile (0, 0)
    let chunk0 = layout.get_chunk_bounds(0, 512, 512);
    assert_eq!(chunk0.col_offset, 0);
    assert_eq!(chunk0.row_offset, 0);
    assert_eq!(chunk0.width, 256);
    assert_eq!(chunk0.height, 256);

    // Tile (1, 0)
    let chunk1 = layout.get_chunk_bounds(1, 512, 512);
    assert_eq!(chunk1.col_offset, 256);
    assert_eq!(chunk1.row_offset, 0);
    assert_eq!(chunk1.width, 256);
    assert_eq!(chunk1.height, 256);

    // Tile (0, 1)
    let chunk2 = layout.get_chunk_bounds(2, 512, 512);
    assert_eq!(chunk2.col_offset, 0);
    assert_eq!(chunk2.row_offset, 256);
    assert_eq!(chunk2.width, 256);
    assert_eq!(chunk2.height, 256);

    // Tile (1, 1)
    let chunk3 = layout.get_chunk_bounds(3, 512, 512);
    assert_eq!(chunk3.col_offset, 256);
    assert_eq!(chunk3.row_offset, 256);
    assert_eq!(chunk3.width, 256);
    assert_eq!(chunk3.height, 256);
}

#[test]
fn test_chunk_intersects_bbox_tile_filtering() {
    let layout = ChunkLayout::new_tiled(200, 200, 100, 100);
    let gt = GeoTransform {
        c0: 0.0,
        a: 1.0,
        b: 0.0,
        f0: 200.0,
        d: 0.0,
        e: -1.0,
    };
    let transformer = CrsTransformer::Wgs84Identity;

    // Tile 0 covers cols [0..100], rows [0..100] -> x: [0..100], y: [100..200]
    let chunk0 = layout.get_chunk_bounds(0, 200, 200);
    // Tile 3 covers cols [100..200], rows [100..200] -> x: [100..200], y: [0..100]
    let chunk3 = layout.get_chunk_bounds(3, 200, 200);

    // Query bbox in upper-left: [10, 150, 50, 180]
    let bbox_upper_left = [10.0, 150.0, 50.0, 180.0];
    assert!(chunk_intersects_bbox(&chunk0, &gt, &transformer, &bbox_upper_left));
    assert!(!chunk_intersects_bbox(&chunk3, &gt, &transformer, &bbox_upper_left));

    // Query bbox in lower-right: [120, 10, 180, 50]
    let bbox_lower_right = [120.0, 10.0, 180.0, 50.0];
    assert!(!chunk_intersects_bbox(&chunk0, &gt, &transformer, &bbox_lower_right));
    assert!(chunk_intersects_bbox(&chunk3, &gt, &transformer, &bbox_lower_right));
}

#[test]
fn test_accumulator_single_sample_variance_and_empty_default() {
    // Empty accumulator
    let acc_empty = H3Accumulator::default();
    assert_eq!(acc_empty.count, 0.0);
    assert!(acc_empty.mean().is_nan());
    assert!(acc_empty.variance().is_nan());
    assert!(acc_empty.stddev().is_nan());
    assert_eq!(acc_empty.min, f64::INFINITY);
    assert_eq!(acc_empty.max, f64::NEG_INFINITY);

    // Single sample accumulator (variance must be exactly 0.0)
    let acc_single = H3Accumulator::new(99.0);
    assert_eq!(acc_single.count, 1.0);
    assert_eq!(acc_single.mean(), 99.0);
    assert_eq!(acc_single.variance(), 0.0);
    assert_eq!(acc_single.stddev(), 0.0);
    assert_eq!(acc_single.min, 99.0);
    assert_eq!(acc_single.max, 99.0);
}

#[test]
fn test_categorical_accumulator_merge_operations() {
    let mut acc1 = CategoricalAccumulator::new();
    acc1.update(10);
    acc1.update(10);
    acc1.update(20);

    let mut acc2 = CategoricalAccumulator::new();
    acc2.update(20);
    acc2.update(30);

    acc1.merge(&acc2);

    assert_eq!(acc1.total_count, 5.0);
    assert_eq!(acc1.unique_classes(), 3);
    assert_eq!(acc1.get_class_count(10), 2.0);
    assert_eq!(acc1.get_class_count(20), 2.0);
    assert_eq!(acc1.get_class_count(30), 1.0);

    let (maj_cat, maj_cnt, maj_frac) = acc1.majority();
    assert!(maj_cat == 10 || maj_cat == 20);
    assert_eq!(maj_cnt, 2.0);
    assert_eq!(maj_frac, 0.40);
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
fn test_tiled_uneven_chunk_layout_and_bounds() {
    // 500x300 image tiled into 256x256 tiles -> 2 cols x 2 rows = 4 tiles total
    // Uneven edges: right column width = 244, bottom row height = 44
    let layout = ChunkLayout::new_tiled(500, 300, 256, 256);
    assert_eq!(layout.total_chunks, 4);
    assert_eq!(layout.chunks_across, 2);
    assert_eq!(layout.chunks_down, 2);

    // Tile (0, 0): top-left full tile
    let chunk0 = layout.get_chunk_bounds(0, 500, 300);
    assert_eq!(chunk0.col_offset, 0);
    assert_eq!(chunk0.row_offset, 0);
    assert_eq!(chunk0.width, 256);
    assert_eq!(chunk0.height, 256);

    // Tile (1, 0): top-right partial tile (width = 500 - 256 = 244)
    let chunk1 = layout.get_chunk_bounds(1, 500, 300);
    assert_eq!(chunk1.col_offset, 256);
    assert_eq!(chunk1.row_offset, 0);
    assert_eq!(chunk1.width, 244);
    assert_eq!(chunk1.height, 256);

    // Tile (0, 1): bottom-left partial tile (height = 300 - 256 = 44)
    let chunk2 = layout.get_chunk_bounds(2, 500, 300);
    assert_eq!(chunk2.col_offset, 0);
    assert_eq!(chunk2.row_offset, 256);
    assert_eq!(chunk2.width, 256);
    assert_eq!(chunk2.height, 44);

    // Tile (1, 1): bottom-right corner partial tile (width = 244, height = 44)
    let chunk3 = layout.get_chunk_bounds(3, 500, 300);
    assert_eq!(chunk3.col_offset, 256);
    assert_eq!(chunk3.row_offset, 256);
    assert_eq!(chunk3.width, 244);
    assert_eq!(chunk3.height, 44);
}

#[test]
fn test_all_nodata_full_stream_scan_and_categorical() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // Create a 16x16 GeoTIFF where all values are -9999.0 (NoData)
    let width = 16u32;
    let height = 16u32;
    let nodata_val = -9999.0f32;
    let data = vec![nodata_val; (width * height) as usize];

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width, height).unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.4194, 37.7749, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(33550), &[0.001f64, 0.001, 0.0][..])
            .unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 7,
        custom_nodata: Some(-9999.0),
        ..Default::default()
    };

    // Test ScanHorizonStreamer on 100% nodata raster
    let mut scan_streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_accumulated_pixels = 0.0;
    loop {
        let batch = scan_streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_accumulated_pixels += acc.count;
        }
    }
    assert_eq!(total_accumulated_pixels, 0.0);

    // Test CategoricalHorizonStreamer on 100% nodata raster
    let reader_cat = GeoTiffStreamReader::open(&path).unwrap();
    let mut cat_streamer = CategoricalHorizonStreamer::new(reader_cat, &config).unwrap();
    let mut total_categorical_pixels = 0.0;
    loop {
        let batch = cat_streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_categorical_pixels += acc.total_count;
        }
    }
    assert_eq!(total_categorical_pixels, 0.0);
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
    invalid_magic_file.write_all(b"NOT_A_VALID_TIFF_STREAM").unwrap();
    invalid_magic_file.flush().unwrap();
    assert!(GeoTiffStreamReader::open(invalid_magic_file.path()).is_err());

    // 3. Truncated TIFF header (valid magic bytes II 42 but truncated pointer)
    let mut truncated_file = NamedTempFile::new().unwrap();
    truncated_file.write_all(&[0x49, 0x49, 0x2A, 0x00]).unwrap();
    truncated_file.flush().unwrap();
    assert!(GeoTiffStreamReader::open(truncated_file.path()).is_err());
}

#[test]
fn test_crs_unspecified_fallback_and_invalid_proj_error() {
    // 1. Unspecified CRS (None, None) gracefully defaults to identity WGS84
    let default_transformer = CrsTransformer::from_crs_or_epsg(None, None).unwrap();
    let (lon, lat) = default_transformer.transform_point(-122.4, 37.8).unwrap();
    assert!((lon - (-122.4)).abs() < 1e-9);
    assert!((lat - 37.8).abs() < 1e-9);

    // 2. Explicit invalid PROJ definition returns a typed CrsError
    let invalid_result = CrsTransformer::from_crs_or_epsg(None, Some("+proj=nonexistent_invalid_crs +units=m"));
    assert!(invalid_result.is_err());
    match invalid_result {
        Err(RasterH3Error::CrsError(msg)) => {
            assert!(msg.contains("Failed to parse") || msg.contains("nonexistent_invalid_crs"));
        }
        _ => panic!("Expected RasterH3Error::CrsError for invalid PROJ string"),
    }
}

#[test]
fn test_sampling_pattern_fallback_to_center() {
    let default_pattern = SamplingPattern::parse("unknown_nonexistent_mode");
    assert!(default_pattern.is_single_point());
    assert_eq!(default_pattern.points.len(), 1);
    assert_eq!(default_pattern.points[0].weight, 1.0);
    assert_eq!(default_pattern.points[0].dx, 0.5);
    assert_eq!(default_pattern.points[0].dy, 0.5);
}

#[test]
fn test_categorical_rle_alternating_and_interspersed_nodata() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let width = 64;
    let height = 64;
    let mut data = Vec::with_capacity(width * height);

    // Row pattern:
    // Half alternating [1, 2, 1, 2...]
    // Half contiguous [10, 10, 10, NoData(255), 10, 10...]
    let mut expected_valid_pixels = 0.0;
    for row in 0..height {
        for col in 0..width {
            if row < 32 {
                let cat = if col % 2 == 0 { 1u8 } else { 2u8 };
                data.push(cat);
                expected_valid_pixels += 1.0;
            } else {
                if col % 10 == 5 {
                    data.push(255u8); // NoData
                } else {
                    data.push(10u8);
                    expected_valid_pixels += 1.0;
                }
            }
        }
    }

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray8>(width as u32, height as u32).unwrap();

        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
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
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 8,
        custom_nodata: Some(255.0),
        ..Default::default()
    };

    let mut streamer = CategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut total_accumulated = 0.0;
    let mut class_1_total = 0.0;
    let mut class_2_total = 0.0;
    let mut class_10_total = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
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
    assert_eq!(class_1_total + class_2_total + class_10_total, expected_valid_pixels);
}

#[test]
fn test_parallel_chunk_aggregation_hawaii_dataset() {
    let tiff_path = Path::new("data/CFL_HI.tif");
    if !tiff_path.exists() {
        return;
    }

    let reader = GeoTiffStreamReader::open(tiff_path).unwrap();
    let config = AggregationConfig {
        resolution: 8,
        ..Default::default()
    };

    let map = raster_h3::aggregator::h3_map::aggregate_raster_stream(&reader, &config).unwrap();
    assert_eq!(map.len(), 30959);

    let mut total_samples = 0.0;
    let mut total_sum = 0.0;
    for (_idx, acc) in &map {
        total_samples += acc.count;
        total_sum += acc.sum;
    }

    assert!(total_samples > 20_000_000.0);
    assert!(total_sum > 100_000_000.0);
}

#[test]
fn test_spatial_filter_pushdown_chunk_skipping() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // Create a 200x200 GeoTIFF spanning [-122.5, -122.3] lon, [37.6, 37.8] lat
    let width = 200;
    let height = 200;
    let data = vec![100.0f32; width * height];

    {
        let file = File::create(&path).unwrap();
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

        let geokeys: [u16; 12] = [
            1, 1, 0, 2,
            1024, 0, 1, 2,
            2048, 0, 1, 4326,
        ];
        image.encoder().write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    // 1. Full Scan (No Pushdown)
    let reader_full = GeoTiffStreamReader::open(&path).unwrap();
    let config_full = AggregationConfig {
        resolution: 8,
        bbox: None,
        ..Default::default()
    };
    let mut streamer_full = ScanHorizonStreamer::new(reader_full, &config_full).unwrap();
    let mut full_pixels = 0.0f64;
    loop {
        let batch = streamer_full.fetch_next_batch(100);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            full_pixels += acc.count;
        }
    }
    assert_eq!(full_pixels, 40000.0);

    // 2. Filter Pushdown with Targeted Spatial Bounding Box
    // Query quarter of the raster: lon [-122.45, -122.35], lat [37.65, 37.75]
    let reader_filtered = GeoTiffStreamReader::open(&path).unwrap();
    let target_bbox = [-122.45, 37.65, -122.35, 37.75];
    let config_filtered = AggregationConfig {
        resolution: 8,
        bbox: Some(target_bbox),
        ..Default::default()
    };

    let mut streamer_filtered = ScanHorizonStreamer::new(reader_filtered, &config_filtered).unwrap();
    let mut filtered_pixels = 0.0f64;
    loop {
        let batch = streamer_filtered.fetch_next_batch(100);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            filtered_pixels += acc.count;
        }
    }

    // Filtered pixel mass must strictly match the spatial subset (~10,000 pixels)
    assert!(filtered_pixels > 0.0);
    assert!(filtered_pixels < full_pixels);
    assert!((filtered_pixels - 10000.0).abs() < 500.0);
}

#[test]
fn test_antimeridian_crossing_and_wrap() {
    use raster_h3::pmtiles::tiler::lon_lat_to_tile_xy;
    use raster_h3::pmtiles::mvt::MercatorPoint;

    // 1. Longitude wrapping and clamping on Date Line
    let (tx_west, ty_west) = lon_lat_to_tile_xy(-179.999, 51.5, 8);
    let (tx_east, ty_east) = lon_lat_to_tile_xy(179.999, 51.5, 8);

    assert_eq!(tx_west, 0, "Westmost longitude should map to tile column 0");
    assert_eq!(tx_east, 255, "Eastmost longitude should map to tile column 255 (2^8 - 1)");
    assert_eq!(ty_west, ty_east, "Identical latitudes must share tile row Y");

    // 2. Normalized Mercator coordinates
    let merc_west = MercatorPoint::from_lat_lng(51.5, -180.0);
    let merc_east = MercatorPoint::from_lat_lng(51.5, 180.0);
    assert!((merc_west.x - 0.0).abs() < 1e-6);
    assert!((merc_east.x - 1.0).abs() < 1e-6);

    // 3. H3 cell indexing across Date Line
    let cell_west = LatLng::new(51.5, -179.99).unwrap().to_cell(Resolution::Eight);
    let cell_east = LatLng::new(51.5, 179.99).unwrap().to_cell(Resolution::Eight);
    let south_west = compute_cell_south_lat(cell_west.into());
    let south_east = compute_cell_south_lat(cell_east.into());
    assert!(south_west.is_finite());
    assert!(south_east.is_finite());
}

#[test]
fn test_extreme_polar_latitudes_clamping() {
    use raster_h3::pmtiles::tiler::lon_lat_to_tile_xy;
    use raster_h3::pmtiles::mvt::MercatorPoint;

    // 1. Extreme North Pole (+89.99°)
    let (tx_north, ty_north) = lon_lat_to_tile_xy(0.0, 89.99, 10);
    assert_eq!(ty_north, 0, "North pole must clamp safely to tile Y=0");
    assert!(tx_north < 1024);

    // 2. Extreme South Pole (-89.99°)
    let (tx_south, ty_south) = lon_lat_to_tile_xy(0.0, -89.99, 10);
    assert_eq!(ty_south, 1023, "South pole must clamp safely to tile Y=1023 (2^10 - 1)");
    assert!(tx_south < 1024);

    // 3. Mercator coordinate finite clamping
    let merc_north = MercatorPoint::from_lat_lng(90.0, 0.0);
    let merc_south = MercatorPoint::from_lat_lng(-90.0, 0.0);
    assert!(merc_north.y >= 0.0 && merc_north.y <= 1.0);
    assert!(merc_south.y >= 0.0 && merc_south.y <= 1.0);
    assert!(!merc_north.y.is_nan() && !merc_north.y.is_infinite());
    assert!(!merc_south.y.is_nan() && !merc_south.y.is_infinite());
}

#[test]
fn test_welford_accumulator_extreme_mixed_bathymetry_numerical_stability() {
    let mut acc = H3Accumulator::default();

    // 1,000,000 samples spanning from Mariana Trench (-10,928m) to Mt Everest (+8,848.86m)
    let samples = [-10928.0f64, -5000.0, -100.0, 0.0, 500.0, 2500.0, 5895.0, 8848.86];
    let num_repeats = 125_000; // 8 * 125,000 = 1,000,000 samples
    let total_count = (samples.len() * num_repeats) as f64;

    let true_sum: f64 = samples.iter().sum::<f64>() * (num_repeats as f64);
    let true_mean: f64 = true_sum / total_count;
    let true_var: f64 = samples
        .iter()
        .map(|&x| (x - true_mean) * (x - true_mean))
        .sum::<f64>() * (num_repeats as f64) / (total_count - 1.0);

    for _ in 0..num_repeats {
        for &val in &samples {
            acc.update(val);
        }
    }

    assert_eq!(acc.count, total_count);
    assert_eq!(acc.min, -10928.0);
    assert_eq!(acc.max, 8848.86);

    // Welford online mean and sample variance must match 2-pass exact reference
    assert!((acc.mean() - true_mean).abs() < 1e-8, "Mean error must be < 1e-8");
    assert!((acc.variance() - true_var).abs() < 1e-4, "Variance error must be < 1e-4");
    assert!(acc.variance() >= 0.0, "Variance must never be negative");
    assert!((acc.stddev() - true_var.sqrt()).abs() < 1e-6);
}

#[test]
fn test_categorical_accumulator_high_cardinality_shannon_entropy() {
    let mut acc = CategoricalAccumulator::default();

    // Ingest 256 unique categories with uniform frequency (10 pixels each = 2560 pixels total)
    for class_id in 0..=255i64 {
        for _ in 0..10 {
            acc.update(class_id);
        }
    }

    assert_eq!(acc.unique_classes(), 256);
    assert_eq!(acc.total_count, 2560.0);
    let (_maj_cat, maj_count, maj_frac) = acc.majority();
    assert_eq!(maj_count, 10.0);
    assert_eq!(maj_frac, 10.0 / 2560.0);

    // Analytical Shannon entropy for uniform distribution over N=256 is ln(256) nats
    let entropy = acc.shannon_entropy();
    let expected_entropy = (256.0f64).ln();
    assert!((entropy - expected_entropy).abs() < 1e-10, "Entropy of uniform 256 classes must equal ln(256), got {}", entropy);

    // Test extreme single-class concentration (100% pure class) -> Shannon entropy must be exactly 0.0
    let mut pure_acc = CategoricalAccumulator::default();
    for _ in 0..1000 {
        pure_acc.update(42);
    }
    assert_eq!(pure_acc.unique_classes(), 1);
    let (_pure_cat, pure_count, pure_frac) = pure_acc.majority();
    assert_eq!(pure_count, 1000.0);
    assert_eq!(pure_frac, 1.0);
    assert_eq!(pure_acc.shannon_entropy(), 0.0);
}

#[test]
fn test_wkb_ogc_compliance() {
    use raster_h3::functions::{cell_to_wkb, h3_index_to_wkb};

    let coord = LatLng::new(21.3069, -157.8583).unwrap(); // Honolulu
    let cell = coord.to_cell(Resolution::Eight);

    let mut buf = [0u8; 128];
    let len = cell_to_wkb(cell, &mut buf);
    assert_eq!(len, 125, "Hexagon WKB must be exactly 125 bytes");

    // Check OGC WKB header
    assert_eq!(buf[0], 1, "Byte order must be Little Endian (1)");
    let geom_type = u32::from_le_bytes(buf[1..5].try_into().unwrap());
    assert_eq!(geom_type, 3, "Geometry type must be wkbPolygon (3)");

    let num_rings = u32::from_le_bytes(buf[5..9].try_into().unwrap());
    assert_eq!(num_rings, 1, "Ring count must be 1");

    let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
    assert_eq!(num_points, 7, "Hexagon ring must have 7 points (6 vertices + 1 closing)");

    // Check ring closure (point 0 == point 6)
    let p0_x = f64::from_le_bytes(buf[13..21].try_into().unwrap());
    let p0_y = f64::from_le_bytes(buf[21..29].try_into().unwrap());
    let p6_x = f64::from_le_bytes(buf[109..117].try_into().unwrap());
    let p6_y = f64::from_le_bytes(buf[117..125].try_into().unwrap());
    assert_eq!(p0_x, p6_x, "Ring must be closed: X0 == X6");
    assert_eq!(p0_y, p6_y, "Ring must be closed: Y0 == Y6");

    // Coordinates should match Honolulu
    assert!(p0_x > -158.5 && p0_x < -157.0);
    assert!(p0_y > 21.0 && p0_y < 22.0);

    // Also verify h3_index_to_wkb matches
    let mut buf2 = [0u8; 128];
    let len2 = h3_index_to_wkb(cell.into(), &mut buf2).expect("valid cell u64");
    assert_eq!(len, len2);
    assert_eq!(&buf[..len], &buf2[..len2]);
}

#[test]
fn test_simd_span_f32_accuracy_and_welford_equivalence() {
    use raster_h3::aggregator::simd::SimdSpanAccumulate;

    // 1. Test arbitrary floating-point values
    let raw_vals: Vec<f32> = (0..127).map(|i| ((i * 7 + 13) % 97) as f32 * 1.5).collect();
    let mut welford = H3Accumulator::default();
    for &v in &raw_vals {
        welford.update(v as f64);
    }

    let simd_acc = f32::accumulate_span(&raw_vals, None);

    assert_eq!(simd_acc.count, welford.count);
    assert!((simd_acc.sum - welford.sum).abs() < 1e-4);
    assert!((simd_acc.mean() - welford.mean()).abs() < 1e-6);
    assert!((simd_acc.variance() - welford.variance()).abs() < 1e-4);
    assert_eq!(simd_acc.min, welford.min);
    assert_eq!(simd_acc.max, welford.max);

    // 2. Test uniform span shortcut (zero variance)
    let uniform_vals = vec![42.5f32; 100];
    let uniform_acc = f32::accumulate_span(&uniform_vals, None);
    assert_eq!(uniform_acc.count, 100.0);
    assert_eq!(uniform_acc.min, 42.5);
    assert_eq!(uniform_acc.max, 42.5);
    assert_eq!(uniform_acc.m2, 0.0);
    assert_eq!(uniform_acc.variance(), 0.0);

    // 3. Test with NoData filtering
    let mut nodata_vals = raw_vals.clone();
    nodata_vals[5] = -9999.0;
    nodata_vals[15] = -9999.0;
    nodata_vals[50] = -9999.0;

    let nd_acc = f32::accumulate_span(&nodata_vals, Some(-9999.0));
    assert_eq!(nd_acc.count, 124.0);
    assert!((nd_acc.sum - (welford.sum - raw_vals[5] as f64 - raw_vals[15] as f64 - raw_vals[50] as f64)).abs() < 1e-4);
}

#[test]
fn test_simd_span_integer_types() {
    use raster_h3::aggregator::simd::SimdSpanAccumulate;

    // Test u8
    let u8_vals: Vec<u8> = (0..200).map(|i| (i % 50) as u8).collect();
    let u8_acc = u8::accumulate_span(&u8_vals, Some(0));
    assert_eq!(u8_acc.count, 196.0); // 4 zeroes skipped

    // Test u16
    let u16_vals: Vec<u16> = (0..500).map(|i| (i * 10) as u16).collect();
    let u16_acc = u16::accumulate_span(&u16_vals, None);
    assert_eq!(u16_acc.count, 500.0);
    assert_eq!(u16_acc.min, 0.0);
    assert_eq!(u16_acc.max, 4990.0);

    // Test i32
    let i32_vals: Vec<i32> = vec![-100, 200, 500, -300, 1000];
    let i32_acc = i32::accumulate_span(&i32_vals, Some(-300));
    assert_eq!(i32_acc.count, 4.0);
    assert_eq!(i32_acc.sum, 1600.0);
}

#[test]
fn test_categorical_8_slot_inline_and_heap_spillover() {
    use raster_h3::aggregator::categorical::CategoricalAccumulator;

    let mut cat = CategoricalAccumulator::new();

    // 1. Add 8 distinct classes: should remain 100% inline with zero heap allocation
    for c in 1..=8 {
        cat.update_weighted(c, (c * 10) as f64);
    }

    assert_eq!(cat.inline_len, 8);
    assert!(cat.heap_counts.is_none(), "Must stay inline up to 8 classes");
    assert_eq!(cat.unique_classes(), 8);
    assert_eq!(cat.total_count, 360.0);
    assert_eq!(cat.get_class_count(5), 50.0);

    let (maj_cls, maj_count, maj_frac) = cat.majority();
    assert_eq!(maj_cls, 8);
    assert_eq!(maj_count, 80.0);
    assert!((maj_frac - (80.0 / 360.0)).abs() < 1e-6);

    // 2. Add 9th class: should spill to heap
    cat.update_weighted(9, 100.0);
    assert!(cat.heap_counts.is_some(), "Must spill to heap on 9th class");
    assert_eq!(cat.unique_classes(), 9);
    assert_eq!(cat.total_count, 460.0);
    assert_eq!(cat.get_class_count(9), 100.0);
    assert_eq!(cat.majority().0, 9);

    // 3. Merge with another inline accumulator
    let mut cat2 = CategoricalAccumulator::new();
    cat2.update_weighted(1, 20.0);
    cat2.update_weighted(10, 50.0);

    cat.merge(&cat2);
    assert_eq!(cat.unique_classes(), 10);
    assert_eq!(cat.get_class_count(1), 30.0);
    assert_eq!(cat.get_class_count(10), 50.0);
    assert_eq!(cat.total_count, 530.0);
}





