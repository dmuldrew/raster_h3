#![allow(deprecated)]

use std::fs::File;
use std::io::BufWriter;
use std::time::Instant;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use raster_h3::aggregator::{
    AggregationConfig, CategoricalHorizonStreamer, SamplingPattern, ScanHorizonStreamer,
};
use raster_h3::raster::GeoTiffStreamReader;

/// Helper to generate synthetic floating point test GeoTIFF
fn create_benchmark_geotiff(width: u32, height: u32, base_val: f32) -> (NamedTempFile, std::path::PathBuf) {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let mut data = Vec::with_capacity((width * height) as usize);
    for row in 0..height {
        for col in 0..width {
            let val = base_val + (row as f32 * 0.1) + (col as f32 * 0.05);
            data.push(val);
        }
    }

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width, height).unwrap();

        // Tiepoint (top-left at San Francisco coordinates)
        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();

        // Pixel scale: 0.0005 deg per pixel (~50m resolution)
        image
            .encoder()
            .write_tag(Tag::Unknown(33550), &[0.0005f64, 0.0005, 0.0][..])
            .unwrap();

        // EPSG:4326 GeoKeys
        let geokeys: [u16; 12] = [
            1, 1, 0, 2,
            1024, 0, 1, 2,
            2048, 0, 1, 4326,
        ];
        image.encoder().write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
        image.write_data(&data).unwrap();
    }

    (temp_file, path)
}

#[test]
fn test_streaming_throughput_and_memory_bounding() {
    let width = 512u32;
    let height = 512u32;
    let total_pixels = (width * height) as f64;
    let (_tmp, path) = create_benchmark_geotiff(width, height, 100.0);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let config = AggregationConfig {
        resolution: 8,
        ..Default::default()
    };

    let start = Instant::now();
    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();

    let mut accumulated_pixels = 0.0;
    let mut total_batches = 0;
    let mut total_cells = 0;

    loop {
        let batch = streamer.fetch_next_batch(64);
        if batch.is_empty() {
            break;
        }
        total_batches += 1;
        for (_, acc) in batch {
            total_cells += 1;
            accumulated_pixels += acc.count;
            assert!(acc.mean() >= 100.0);
            assert!(acc.min >= 100.0);
            assert!(acc.max >= 100.0);
        }
    }

    let elapsed = start.elapsed();
    let throughput_mpps = (total_pixels / elapsed.as_secs_f64()) / 1_000_000.0;

    println!(
        "Continuous Streamer: 512x512 (262,144 px) -> {} cells across {} batches in {:.2?} ({:.2} Mpx/sec)",
        total_cells, total_batches, elapsed, throughput_mpps
    );

    assert_eq!(accumulated_pixels, total_pixels);
    assert!(total_cells > 0);
    assert!(total_batches > 1);
    // Reasonable time check: must finish within 2 seconds
    assert!(elapsed.as_secs() < 2, "Streaming took too long: {:?}", elapsed);
}

#[test]
fn test_multi_resolution_conservation_and_scaling() {
    let width = 256u32;
    let height = 256u32;
    let expected_pixels = (width * height) as f64;
    let (_tmp, path) = create_benchmark_geotiff(width, height, 50.0);

    // Test across resolutions 6 (coarse), 7 (medium), 8 (fine), 9 (high-res)
    let resolutions = [6u8, 7, 8, 9];
    let mut cell_counts = Vec::new();

    for &res in &resolutions {
        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let config = AggregationConfig {
            resolution: res,
            ..Default::default()
        };

        let start = Instant::now();
        let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
        let mut res_pixels = 0.0;
        let mut res_cells = 0;

        loop {
            let batch = streamer.fetch_next_batch(32);
            if batch.is_empty() {
                break;
            }
            for (_, acc) in batch {
                res_cells += 1;
                res_pixels += acc.count;
            }
        }
        let elapsed = start.elapsed();

        assert_eq!(
            res_pixels, expected_pixels,
            "Pixel conservation failed at H3 resolution {}",
            res
        );
        cell_counts.push(res_cells);

        println!(
            "Resolution {}: {} cells, conserved {:.0} pixels in {:.2?}",
            res, res_cells, res_pixels, elapsed
        );
        assert!(elapsed.as_secs() < 2);
    }

    // Finer resolution should produce monotonically more cells for the same spatial extent
    for i in 1..cell_counts.len() {
        assert!(
            cell_counts[i] >= cell_counts[i - 1],
            "Cell count should increase with resolution: res {} ({}) < res {} ({})",
            resolutions[i], cell_counts[i], resolutions[i - 1], cell_counts[i - 1]
        );
    }
}

#[test]
fn test_categorical_streaming_performance_scaling() {
    let width = 256u32;
    let height = 256u32;
    let total_pixels = (width * height) as f64;

    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // 10 distinct land cover classes (1..=10)
    let mut data = Vec::with_capacity((width * height) as usize);
    for row in 0..height {
        for col in 0..width {
            let class_id = ((row / 32) * 2 + (col / 128) + 1) as f32;
            data.push(class_id);
        }
    }

    {
        let file = File::create(&path).unwrap();
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width, height).unwrap();

        image
            .encoder()
            .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(33550), &[0.0005f64, 0.0005, 0.0][..])
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
        ..Default::default()
    };

    let start = Instant::now();
    let mut streamer = CategoricalHorizonStreamer::new(reader, &config).unwrap();

    let mut accumulated_pixels = 0.0;
    let mut total_cells = 0;

    loop {
        let batch = streamer.fetch_next_batch(32);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_cells += 1;
            accumulated_pixels += acc.total_count;
            assert!(acc.unique_classes() >= 1);
            let (maj_class, maj_count, maj_frac) = acc.majority();
            assert!(maj_class >= 1 && maj_class <= 16);
            assert!(maj_count > 0.0);
            assert!(maj_frac > 0.0 && maj_frac <= 1.0);
        }
    }

    let elapsed = start.elapsed();
    println!(
        "Categorical Streamer: 256x256 (65,536 px) -> {} cells in {:.2?}",
        total_cells, elapsed
    );

    assert_eq!(accumulated_pixels, total_pixels);
    assert!(total_cells > 0);
    assert!(elapsed.as_secs() < 2);
}

#[test]
fn test_subpixel_sampling_scaling_and_conservation() {
    let width = 128u32;
    let height = 128u32;
    let expected_pixels = (width * height) as f64;
    let (_tmp, path) = create_benchmark_geotiff(width, height, 10.0);

    let patterns = [
        ("center", SamplingPattern::center()),
        ("rgss", SamplingPattern::rgss()),
        ("hex", SamplingPattern::hex_seven_point()),
        ("16point", SamplingPattern::sixteen_point()),
    ];

    for (name, pattern) in patterns {
        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let config = AggregationConfig {
            resolution: 8,
            sampling: pattern,
            ..Default::default()
        };

        let start = Instant::now();
        let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
        let mut total_weighted_count = 0.0;
        let mut cells = 0;

        loop {
            let batch = streamer.fetch_next_batch(32);
            if batch.is_empty() {
                break;
            }
            for (_, acc) in batch {
                cells += 1;
                total_weighted_count += acc.count;
            }
        }
        let elapsed = start.elapsed();

        // In super-sampling, total weighted count must strictly equal pixel count
        assert!(
            (total_weighted_count - expected_pixels).abs() < 1e-6,
            "Pattern {} weighted count {} != expected {}",
            name, total_weighted_count, expected_pixels
        );

        println!(
            "Sub-pixel pattern '{}': {} cells, {:.0} weighted px in {:.2?}",
            name, cells, total_weighted_count, elapsed
        );
        assert!(elapsed.as_secs() < 2);
    }
}
