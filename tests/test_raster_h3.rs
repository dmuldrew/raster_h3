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
    SamplingPattern, ScanHorizonStreamer, SpatialCoherenceCache,
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
    assert_eq!(acc1.counts[&10], 2.0);
    assert_eq!(acc1.counts[&20], 2.0);
    assert_eq!(acc1.counts[&30], 1.0);

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
            if let Some(&cnt) = acc.counts.get(&1) {
                class_1_total += cnt;
            }
            if let Some(&cnt) = acc.counts.get(&2) {
                class_2_total += cnt;
            }
            if let Some(&cnt) = acc.counts.get(&10) {
                class_10_total += cnt;
            }
            assert!(acc.counts.get(&255).is_none()); // NoData sentinel never recorded
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

