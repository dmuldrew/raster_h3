//! Standalone CLI tool to download tiled GeoTIFFs from USFS GeoPlatform ImageServer
//!
//! Example usage:
//!   cargo run --release --bin download_burn_probability -- --grid 2,2 --output-dir data/burn_probability_mosaic/

use rayon::prelude::*;
use std::env;
use std::fs::{self, File};
use std::io::{copy, BufWriter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

const BASE_URL: &str = "https://imagery.geoplatform.gov/iipp/rest/services/Fire_Aviation/USFS_EDW_RMRS_WRC_BurnProbability/ImageServer/exportImage";

#[derive(Debug, Clone)]
struct TileJob {
    row: usize,
    col: usize,
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
    width_px: u32,
    height_px: u32,
    out_path: PathBuf,
}

fn print_help() {
    println!("USFS GeoPlatform Burn Probability Tiled Downloader");
    println!("Usage:");
    println!("  download_burn_probability [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --bbox <min_lon,min_lat,max_lon,max_lat>");
    println!(
        "                        Bounding box in WGS84 degrees (default: -121.5,39.0,-120.5,40.0)"
    );
    println!("  --grid <cols,rows>    Tiling grid divisions (default: 2,2 -> 4 tiles)");
    println!("  --output-dir <path>   Directory to save GeoTIFF tiles (default: data/burn_probability_mosaic/)");
    println!("  --resolution <meters> Target pixel size in meters (default: 30.0)");
    println!("  --compression <type>  TIFF compression: LZW, DEFLATE, or NONE (default: LZW)");
    println!("  --workers <n>         Concurrent download threads (default: 4)");
    println!("  --help, -h            Show this help menu");
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = env::args().collect();

    let mut bbox = [-121.5_f64, 39.0, -120.5, 40.0];
    let mut cols = 2usize;
    let mut rows = 2usize;
    let mut output_dir = PathBuf::from("data/burn_probability_mosaic");
    let mut resolution = 30.0_f64;
    let mut compression = "LZW".to_string();
    let mut num_workers = 4usize;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bbox" => {
                if i + 1 < args.len() {
                    let parts: Vec<f64> = args[i + 1]
                        .split(',')
                        .filter_map(|s| s.trim().parse::<f64>().ok())
                        .collect();
                    if parts.len() == 4 {
                        bbox = [parts[0], parts[1], parts[2], parts[3]];
                    } else {
                        eprintln!("Invalid bbox format, expected min_lon,min_lat,max_lon,max_lat");
                        std::process::exit(1);
                    }
                    i += 1;
                }
            }
            "--grid" => {
                if i + 1 < args.len() {
                    let parts: Vec<usize> = args[i + 1]
                        .split(',')
                        .filter_map(|s| s.trim().parse::<usize>().ok())
                        .collect();
                    if parts.len() == 2 {
                        cols = parts[0].max(1);
                        rows = parts[1].max(1);
                    } else {
                        eprintln!("Invalid grid format, expected cols,rows");
                        std::process::exit(1);
                    }
                    i += 1;
                }
            }
            "--output-dir" | "-o" => {
                if i + 1 < args.len() {
                    output_dir = PathBuf::from(&args[i + 1]);
                    i += 1;
                }
            }
            "--resolution" | "-r" => {
                if i + 1 < args.len() {
                    if let Ok(res) = args[i + 1].trim().parse::<f64>() {
                        resolution = res.max(1.0);
                    }
                    i += 1;
                }
            }
            "--compression" => {
                if i + 1 < args.len() {
                    compression = args[i + 1].trim().to_uppercase();
                    i += 1;
                }
            }
            "--workers" | "-w" => {
                if i + 1 < args.len() {
                    if let Ok(w) = args[i + 1].trim().parse::<usize>() {
                        num_workers = w.max(1);
                    }
                    i += 1;
                }
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }

    fs::create_dir_all(&output_dir)?;

    println!("===========================================================");
    println!("  USFS Burn Probability Tiled Mosaic Downloader");
    println!("===========================================================");
    println!(
        "  Bounding Box       : [{:.4}, {:.4}, {:.4}, {:.4}]",
        bbox[0], bbox[1], bbox[2], bbox[3]
    );
    println!(
        "  Tiling Grid        : {} columns x {} rows ({} tiles total)",
        cols,
        rows,
        cols * rows
    );
    println!("  Resolution (approx): {:.1} meters/pixel", resolution);
    println!("  Compression        : {}", compression);
    println!("  Output Directory   : {:?}", output_dir);
    println!("  Concurrent Workers : {}", num_workers);
    println!("-----------------------------------------------------------");

    let col_step = (bbox[2] - bbox[0]) / cols as f64;
    let row_step = (bbox[3] - bbox[1]) / rows as f64;

    let mut jobs = Vec::with_capacity(cols * rows);

    for r in 0..rows {
        // Row 0 is northernmost
        let max_lat = bbox[3] - (r as f64 * row_step);
        let min_lat = max_lat - row_step;

        let center_lat = (min_lat + max_lat) * 0.5;
        let meters_per_deg_lon = 111_120.0 * center_lat.to_radians().cos().abs();
        let meters_per_deg_lat = 111_120.0;

        let height_px = ((max_lat - min_lat) * meters_per_deg_lat / resolution).round() as u32;

        for c in 0..cols {
            let min_lon = bbox[0] + (c as f64 * col_step);
            let max_lon = min_lon + col_step;

            let width_px = ((max_lon - min_lon) * meters_per_deg_lon / resolution).round() as u32;

            let file_name = format!("tile_r{:02}_c{:02}.tif", r, c);
            let out_path = output_dir.join(file_name);

            jobs.push(TileJob {
                row: r,
                col: c,
                min_lon,
                min_lat,
                max_lon,
                max_lat,
                width_px: width_px.clamp(64, 100_000),
                height_px: height_px.clamp(64, 100_000),
                out_path,
            });
        }
    }

    let start_time = Instant::now();
    let downloaded_count = Arc::new(AtomicUsize::new(0));
    let skipped_count = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicUsize::new(0));

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_workers)
        .build()?;

    let compression_str = compression.clone();

    pool.install(|| {
        jobs.par_iter().for_each(|job| {
            if job.out_path.exists() {
                if let Ok(meta) = fs::metadata(&job.out_path) {
                    if meta.len() > 1024 {
                        println!("  [SKIP] Tile (r:{}, c:{}) already exists: {:?} ({:.2} KB)",
                            job.row, job.col, job.out_path.file_name().unwrap(), meta.len() as f64 / 1024.0);
                        skipped_count.fetch_add(1, Ordering::Relaxed);
                        total_bytes.fetch_add(meta.len() as usize, Ordering::Relaxed);
                        return;
                    }
                }
            }

            let url = format!(
                "{}?bbox={:.6},{:.6},{:.6},{:.6}&bboxSR=4326&imageSR=4326&size={},{}&format=tiff&compression={}&f=image",
                BASE_URL,
                job.min_lon, job.min_lat, job.max_lon, job.max_lat,
                job.width_px, job.height_px,
                compression_str
            );

            let t0 = Instant::now();
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(60))
                .build();

            match agent.get(&url).call() {
                Ok(resp) => {
                    let temp_path = job.out_path.with_extension("tif.tmp");
                    match File::create(&temp_path) {
                        Ok(file) => {
                            let mut writer = BufWriter::new(file);
                            match copy(&mut resp.into_reader(), &mut writer) {
                                Ok(bytes_written) => {
                                    drop(writer);
                                    if let Err(e) = fs::rename(&temp_path, &job.out_path) {
                                        eprintln!("  [ERROR] Failed to save {:?}: {}", job.out_path, e);
                                        let _ = fs::remove_file(&temp_path);
                                    } else {
                                        let elapsed = t0.elapsed();
                                        println!(
                                            "  [DONE] Tile (r:{}, c:{}) -> {:?} ({}x{} px, {:.2} KB in {:.2?})",
                                            job.row, job.col,
                                            job.out_path.file_name().unwrap(),
                                            job.width_px, job.height_px,
                                            bytes_written as f64 / 1024.0,
                                            elapsed
                                        );
                                        downloaded_count.fetch_add(1, Ordering::Relaxed);
                                        total_bytes.fetch_add(bytes_written as usize, Ordering::Relaxed);
                                    }
                                }
                                Err(e) => {
                                    eprintln!("  [ERROR] Failed during stream download of {:?}: {}", job.out_path, e);
                                    let _ = fs::remove_file(&temp_path);
                                }
                            }
                        }
                        Err(e) => eprintln!("  [ERROR] Failed to create temp file: {}", e),
                    }
                }
                Err(e) => eprintln!("  [ERROR] HTTP request failed for tile (r:{}, c:{}): {}", job.row, job.col, e),
            }
        });
    });

    let elapsed = start_time.elapsed();
    let dl = downloaded_count.load(Ordering::Relaxed);
    let sk = skipped_count.load(Ordering::Relaxed);
    let bytes = total_bytes.load(Ordering::Relaxed);

    println!("-----------------------------------------------------------");
    println!("  Mosaic Download Complete!");
    println!("  Downloaded : {} tiles", dl);
    println!("  Skipped    : {} tiles", sk);
    println!("  Total Size : {:.2} MB", bytes as f64 / (1024.0 * 1024.0));
    println!("  Total Time : {:.2?}", elapsed);
    println!("===========================================================");
    println!();
    println!("Next step: Test the mosaic engine with:");
    println!("  cargo run --release --example test_mosaic_burn_probability");
    println!();

    Ok(())
}
