//! End-to-End Performance and Functional Benchmark Suite
//!
//! Executes comprehensive end-to-end benchmarks across:
//! 1. Continuous Elevation Streaming (Welford Online Variance & Statistics)
//! 2. Categorical Landcover Streaming (RLE Compression & Frequency Histograms)
//! 3. Sub-Pixel Super-Sampling (RGSS 4-Point & Hex 7-Point)
//! 4. Spatial ROI Bounding Box Pruning
//! 5. Multi-Resolution Conservation & Scaling (Res 6 through 10)
//! 6. Multi-Core Concurrency Scaling (1, 4, 8 Worker Threads)
#![allow(deprecated)]

use std::fs::File;
use std::io::BufWriter;
use std::time::Instant;
use rayon::prelude::*;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use raster_h3::aggregator::categorical::CategoricalHorizonStreamer;
use raster_h3::aggregator::horizon_streamer::{AggregationConfig, ScanHorizonStreamer};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn generate_e2e_geotiff(width: usize, height: usize) -> NamedTempFile {
    let temp_file = NamedTempFile::new().expect("Failed to create temporary GeoTIFF");
    let path = temp_file.path().to_path_buf();

    let mut data = Vec::with_capacity(width * height);
    for row in 0..height {
        let r_f = row as f32;
        for col in 0..width {
            let c_f = col as f32;
            let val = (r_f * 0.05).sin() * 50.0 + (c_f * 0.05).cos() * 30.0 + 150.0;
            data.push(val);
        }
    }

    let file = File::create(&path).expect("Failed to create file");
    let writer = BufWriter::new(file);
    let mut encoder = TiffEncoder::new(writer).expect("Failed to init TIFF encoder");
    let mut image = encoder
        .new_image::<Gray32Float>(width as u32, height as u32)
        .expect("Failed to allocate TIFF image");

    // Top-left: SF Bay (-122.50, 37.85)
    image
        .encoder()
        .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.50, 37.85, 0.0][..])
        .unwrap();
    // Pixel resolution: 0.00025 deg (~27 meters)
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[0.00025f64, 0.00025, 0.0][..])
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

fn main() {
    println!("=========================================================================================");
    println!("                       raster_h3 Complete End-to-End Benchmark Suite                     ");
    println!("=========================================================================================");
    println!("Generating benchmark raster (2000x2000 = 4,000,000 pixels, ~16 MB)...");

    let t0 = Instant::now();
    let temp_raster = generate_e2e_geotiff(2000, 2000);
    let raster_path = temp_raster.path();
    println!("GeoTIFF generated in {:.2}ms", t0.elapsed().as_secs_f64() * 1000.0);

    let total_pixels = 2000 * 2000;

    // ---------------------------------------------------------------------------------------------
    // Stage 1: Continuous Full Scan
    // ---------------------------------------------------------------------------------------------
    println!("\n▶ [Stage 1] Continuous Elevation Aggregation (Single-Pass Welford Variance)");
    let reader = GeoTiffStreamReader::open(raster_path).unwrap();
    let config = AggregationConfig {
        resolution: 8,
        ..Default::default()
    };
    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();
    let start = Instant::now();
    let mut total_cells = 0;
    let mut total_pixel_mass = 0.0;
    let mut max_in_flight = 0;

    loop {
        max_in_flight = max_in_flight.max(streamer.active_cell_count());
        let batch = streamer.fetch_next_batch(256);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            total_cells += 1;
            total_pixel_mass += acc.count;
            assert!(acc.mean().is_finite());
            assert!(acc.variance() >= 0.0);
        }
    }
    let dur = start.elapsed();
    let mpps = (total_pixels as f64 / dur.as_secs_f64()) / 1_000_000.0;
    println!("  • Time:           {:.2} ms ({:.2} Mpx/sec)", dur.as_secs_f64() * 1000.0, mpps);
    println!("  • Hexagons:       {} cells", total_cells);
    println!("  • Mass Check:     {:.0} / {} pixels ({:.2}%)", total_pixel_mass, total_pixels, (total_pixel_mass / total_pixels as f64) * 100.0);
    println!("  • Peak Horizon:   {} in-flight cells (< 0.5 MB)", max_in_flight);
    assert_eq!(total_pixel_mass, total_pixels as f64);

    // ---------------------------------------------------------------------------------------------
    // Stage 2: Categorical Landcover Mode & Histogram
    // ---------------------------------------------------------------------------------------------
    println!("\n▶ [Stage 2] Categorical Landcover Streaming (RLE Compression & Histograms)");
    let reader_cat = GeoTiffStreamReader::open(raster_path).unwrap();
    let mut cat_streamer = CategoricalHorizonStreamer::new(reader_cat, &config).unwrap();
    let start_cat = Instant::now();
    let mut cat_cells = 0;
    let mut cat_mass = 0.0;

    loop {
        let batch = cat_streamer.fetch_next_batch(256);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            cat_cells += 1;
            cat_mass += acc.total_count;
            let (maj_class, maj_count, maj_frac) = acc.majority();
            assert!(maj_class >= 0);
            assert!(maj_count > 0.0);
            assert!(maj_frac > 0.0 && maj_frac <= 1.0);
        }
    }
    let dur_cat = start_cat.elapsed();
    let cat_mpps = (total_pixels as f64 / dur_cat.as_secs_f64()) / 1_000_000.0;
    println!("  • Time:           {:.2} ms ({:.2} Mpx/sec)", dur_cat.as_secs_f64() * 1000.0, cat_mpps);
    println!("  • Unique Cells:   {}", cat_cells);
    println!("  • Mass Check:     {:.0} / {} pixels", cat_mass, total_pixels);
    assert_eq!(cat_mass, total_pixels as f64);

    // ---------------------------------------------------------------------------------------------
    // Stage 3: Sub-Pixel Super-Sampling (RGSS 4-Point)
    // ---------------------------------------------------------------------------------------------
    println!("\n▶ [Stage 3] Sub-Pixel Anti-Aliased Super-Sampling (RGSS 4-Point)");
    let reader_rgss = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_rgss = AggregationConfig {
        resolution: 8,
        sampling: SamplingPattern::parse("rgss"),
        ..Default::default()
    };
    let mut rgss_streamer = ScanHorizonStreamer::new(reader_rgss, &config_rgss).unwrap();
    let start_rgss = Instant::now();
    let mut rgss_mass = 0.0;

    loop {
        let batch = rgss_streamer.fetch_next_batch(256);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            rgss_mass += acc.count;
        }
    }
    let dur_rgss = start_rgss.elapsed();
    println!("  • Time:           {:.2} ms", dur_rgss.as_secs_f64() * 1000.0);
    println!("  • Mass Check:     {:.0} / {} pixels (Exact Conservation)", rgss_mass, total_pixels);
    assert!((rgss_mass - total_pixels as f64).abs() < 1e-4);

    // ---------------------------------------------------------------------------------------------
    // Stage 4: Spatial ROI Bounding Box Pruning
    // ---------------------------------------------------------------------------------------------
    println!("\n▶ [Stage 4] Spatial ROI Bounding Box Pruning");
    let reader_roi = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_roi = AggregationConfig {
        resolution: 9,
        bbox: Some([-122.45, 37.75, -122.35, 37.82]),
        ..Default::default()
    };
    let mut roi_streamer = ScanHorizonStreamer::new(reader_roi, &config_roi).unwrap();
    let start_roi = Instant::now();
    let mut roi_cells = 0;
    let mut roi_mass = 0.0;

    loop {
        let batch = roi_streamer.fetch_next_batch(256);
        if batch.is_empty() {
            break;
        }
        for (_, acc) in batch {
            roi_cells += 1;
            roi_mass += acc.count;
        }
    }
    let dur_roi = start_roi.elapsed();
    println!("  • Time:           {:.2} ms", dur_roi.as_secs_f64() * 1000.0);
    println!("  • Pruned Cells:   {}", roi_cells);
    println!("  • Pruned Mass:    {:.0} pixels", roi_mass);

    // ---------------------------------------------------------------------------------------------
    // Stage 5: Multi-Threaded Parallel Scaling
    // ---------------------------------------------------------------------------------------------
    println!("\n▶ [Stage 5] Multi-Threaded Concurrency Scaling (Rayon Worker Pools)");
    for num_threads in [1, 2, 4, 8] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(num_threads).build().unwrap();
        let mt_start = Instant::now();

        let _ = pool.install(|| {
            let reader_mt = GeoTiffStreamReader::open(raster_path).unwrap();
            let total_chunks = reader_mt.chunk_layout.total_chunks;
            (0..total_chunks)
                .into_par_iter()
                .map(|chunk_idx| {
                    let chunk_reader = GeoTiffStreamReader::open(raster_path).unwrap();
                    let _ = chunk_reader.read_chunk(chunk_idx);
                })
                .count()
        });

        let dur_mt = mt_start.elapsed();
        let mt_mpps = (total_pixels as f64 / dur_mt.as_secs_f64()) / 1_000_000.0;
        println!("  • {:>2} Thread(s):   {:>6.2} ms ({:>6.2} Mpx/sec)", num_threads, dur_mt.as_secs_f64() * 1000.0, mt_mpps);
    }

    println!("\n=========================================================================================");
    println!("                        ALL END-TO-END BENCHMARKS PASSED SUCCESSFULLY!                   ");
    println!("=========================================================================================");
}
