use std::fs::File;
use std::io::BufWriter;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::{Gray16, Gray32Float, Gray64Float, Gray8};
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use raster_h3::aggregator::{
    compute_cell_south_lat, is_chunk_all_nodata, AggregationConfig,
    CategoricalAccumulator, CategoricalHorizonStreamer, H3Accumulator,
    SamplingPattern, ScanHorizonStreamer, SpatialCoherenceCache,
};
use raster_h3::crs::CrsTransformer;
use raster_h3::functions::fast_hex_u64;
use raster_h3::raster::{GeoTiffStreamReader, GeoTransform, PrefetchedChunkReader};

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
fn test_spatial_coherence_cache_accuracy() {
    let mut cache = SpatialCoherenceCache::default();
    let res = h3o::Resolution::Eight;

    let lat = 37.7749;
    let lon = -122.4194;

    let cell_cached = cache.get_or_compute(lat, lon, res).unwrap();
    let lat_lng = h3o::LatLng::new(lat, lon).unwrap();
    let cell_direct: u64 = lat_lng.to_cell(res).into();

    assert_eq!(cell_cached, cell_direct);

    // Adjacent pixel within 2 meters should return exact same cell from cache
    let cell_adjacent = cache.get_or_compute(lat + 0.00001, lon + 0.00001, res).unwrap();
    assert_eq!(cell_adjacent, cell_direct);
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

