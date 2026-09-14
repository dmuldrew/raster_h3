//! Standalone CLI Converter: H3 Parquet to PMTiles v3
//!
//! Usage:
//!   cargo run --release --example parquet_to_pmtiles -- \
//!     --input data/census_h3.parquet \
//!     --output data/census.pmtiles \
//!     --h3-col h3_index

use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use std::env;
use std::time::Instant;

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut input_path = None;
    let mut output_path = None;
    let mut h3_col = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--input" | "-i" => {
                if i + 1 < args.len() {
                    input_path = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--output" | "-o" => {
                if i + 1 < args.len() {
                    output_path = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--h3-col" | "-c" => {
                if i + 1 < args.len() {
                    h3_col = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let input = match input_path {
        Some(p) => p,
        None => {
            eprintln!("Error: --input <path.parquet> is required");
            print_help();
            std::process::exit(1);
        }
    };

    let output = match output_path {
        Some(p) => p,
        None => {
            eprintln!("Error: --output <path.pmtiles> is required");
            print_help();
            std::process::exit(1);
        }
    };

    println!("=================================================================");
    println!("  Raster H3 - Parquet to PMTiles v3 Vector Hexagon Generator");
    println!("=================================================================");
    println!("Input Parquet:     {}", input);
    println!("Output PMTiles:    {}", output);
    if let Some(ref c) = h3_col {
        println!("H3 Column Override: {}", c);
    } else {
        println!("H3 Column:         [Auto-Detect: h3_index, h3_hex, h3, cell]");
    }
    println!("-----------------------------------------------------------------");

    let start = Instant::now();
    match H3PmtilesTiler::process_parquet_to_pmtiles(&input, &output, h3_col.as_deref()) {
        Ok(summary) => {
            let elapsed = start.elapsed();
            println!("SUCCESS!");
            println!("  Total Rows:             {}", summary.total_features);
            println!("  Valid H3 Hexagons:      {}", summary.valid_features);
            println!(
                "  Invalid Rows Dropped:   {}",
                summary.invalid_features_dropped
            );
            println!("  Vector Tiles Generated: {}", summary.total_tiles);
            println!(
                "  Zoom Range:             Z{} .. Z{}",
                summary.min_zoom, summary.max_zoom
            );
            println!("  Elapsed Time:           {:.2?}", elapsed);
            if let Ok(m) = std::fs::metadata(&output) {
                println!(
                    "  Output File Size:       {:.2} MB",
                    m.len() as f64 / 1_048_576.0
                );
            }
            println!("=================================================================");
        }
        Err(e) => {
            eprintln!("Error during Parquet to PMTiles conversion: {}", e);
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!("Usage: parquet_to_pmtiles --input <file.parquet> --output <file.pmtiles> [OPTIONS]");
    println!("");
    println!("Options:");
    println!("  -i, --input <path>     Input Parquet file containing H3 index column");
    println!("  -o, --output <path>    Output .pmtiles archive path");
    println!(
        "  -c, --h3-col <name>    Explicit name of the H3 index column (default: auto-detect)"
    );
    println!("  -h, --help             Print help information");
}
