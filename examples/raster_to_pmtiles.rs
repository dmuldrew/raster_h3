//! Standalone CLI Tool & Benchmark for GeoTIFF to PMTiles v3 Conversion
//!
//! Example usage:
//!   cargo run --release --example raster_to_pmtiles -- --input data/sample.tif --output data/sample.pmtiles --resolutions 7,8,9

use raster_h3::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use std::env;
use std::fs::File;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = env::args().collect();

    let mut input_path = String::new();
    let mut output_path = String::new();
    let mut res_list = vec![7u8, 8u8];
    let mut sampling_str = "center".to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--input" | "-i" => {
                if i + 1 < args.len() {
                    input_path = args[i + 1].clone();
                    i += 1;
                }
            }
            "--output" | "-o" => {
                if i + 1 < args.len() {
                    output_path = args[i + 1].clone();
                    i += 1;
                }
            }
            "--resolutions" | "-r" => {
                if i + 1 < args.len() {
                    res_list = args[i + 1]
                        .split(',')
                        .filter_map(|s| s.trim().parse::<u8>().ok())
                        .collect();
                    i += 1;
                }
            }
            "--sampling" | "-s" => {
                if i + 1 < args.len() {
                    sampling_str = args[i + 1].clone();
                    i += 1;
                }
            }
            "--help" | "-h" => {
                println!("raster_h3 PMTiles v3 Generator");
                println!("Usage:");
                println!("  raster_to_pmtiles --input <path> --output <path> [--resolutions 7,8,9] [--sampling center]");
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }

    let _temp_dir_holder;
    if input_path.is_empty() {
        // Run with an automated synthetic benchmark if no input was specified
        println!("No input GeoTIFF provided. Generating synthetic 1024x1024 GeoTIFF benchmark...");
        let temp_dir = tempfile::tempdir()?;
        let temp_tiff = temp_dir.path().join("benchmark_input.tif");
        let temp_pmtiles = temp_dir.path().join("benchmark_output.pmtiles");

        // Write synthetic TIFF
        use tiff::encoder::*;
        use tiff::tags::Tag;
        let mut file = File::create(&temp_tiff)?;
        let mut encoder = TiffEncoder::new(&mut file)?;
        let mut image = encoder.new_image::<colortype::Gray32Float>(1024, 1024)?;
        image.encoder().write_tag(
            Tag::ModelTiepointTag,
            &[0.0_f64, 0.0, 0.0, -122.5, 37.8, 0.0][..],
        )?;
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.0005_f64, 0.0005, 0.0][..])?;

        let pixels: Vec<f32> = (0..(1024 * 1024))
            .map(|i| (i % 1000) as f32 + 50.0)
            .collect();
        image.write_data(&pixels)?;

        input_path = temp_tiff.to_str().unwrap().to_string();
        output_path = temp_pmtiles.to_str().unwrap().to_string();
        _temp_dir_holder = Some(temp_dir);
    } else {
        _temp_dir_holder = None;
    }

    if output_path.is_empty() {
        output_path = "output.pmtiles".to_string();
    }

    println!("===========================================================");
    println!("  raster_h3: Pure-Rust GeoTIFF -> PMTiles v3 Generator");
    println!("===========================================================");
    println!("  Input GeoTIFF   : {}", input_path);
    println!("  Output PMTiles  : {}", output_path);
    println!("  H3 Resolutions  : {:?}", res_list);
    println!("  Sampling Preset : {}", sampling_str);
    println!("-----------------------------------------------------------");

    let sampling = SamplingPattern::parse(&sampling_str);
    let mut config = MultiResolutionConfig::new(res_list);
    config.sampling = sampling;

    let start = Instant::now();
    let total_hexagons =
        H3PmtilesTiler::process_geotiff_to_pmtiles(&input_path, &output_path, config)?;
    let elapsed = start.elapsed();

    let pmtiles_file = File::open(&output_path)?;
    let file_size_kb = (pmtiles_file.metadata()?.len() as f64) / 1024.0;

    println!("  Conversion Complete!");
    println!("  Total Hexagons Written : {}", total_hexagons);
    println!(
        "  PMTiles Archive Size   : {:.2} KB ({:.2} MB)",
        file_size_kb,
        file_size_kb / 1024.0
    );
    println!(
        "  Total Elapsed Time     : {:.3} s ({:.1} ms)",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0
    );
    println!("===========================================================");

    Ok(())
}
