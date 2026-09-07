//! Full CONUS Wildfire Burn Probability Batch & Evict Streaming Pipeline
//!
//! Downloads CONUS GeoTIFF tiles in latitudinal bands from the USFS GeoPlatform ImageServer,
//! aggregates them into H3 Resolution 8 and 9 hexagons, writes each band to Parquet,
//! and immediately purges raw GeoTIFF tiles to keep disk usage strictly bounded under 2.5 GB.
//!
//! Usage:
//!   cargo run --release --bin conus_batch_and_evict -- --start-band 0 --end-band 4

use std::env;
use std::fs::{self, File};
use std::io::{copy, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parquet::data_type::{DoubleType, Int32Type, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use rayon::prelude::*;

use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};

const BASE_URL: &str = "https://imagery.geoplatform.gov/iipp/rest/services/Fire_Aviation/USFS_EDW_RMRS_WRC_BurnProbability/ImageServer/exportImage";

#[derive(Debug, Clone)]
struct BandConfig {
    band_index: usize,
    min_lon: f64,
    max_lon: f64,
    min_lat: f64,
    max_lat: f64,
    cols: usize,
    rows: usize,
}

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

/// Approximate CONUS land filter to avoid downloading tiles deep in ocean
fn intersects_conus_rough(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> bool {
    // Pacific ocean cutoff
    if max_lon < -124.8 && max_lat < 48.5 && min_lat > 32.5 {
        return false;
    }
    // North Atlantic ocean cutoff
    if min_lon > -67.0 {
        return false;
    }
    // Gulf of Mexico deep water
    if max_lat < 27.5 && min_lon > -96.0 && max_lon < -83.0 {
        return false;
    }
    // South of Florida deep Atlantic
    if max_lat < 24.5 {
        return false;
    }
    // Far northwest off coast of WA/OR
    if max_lon < -125.0 && min_lat < 46.0 {
        return false;
    }
    true
}

fn print_help() {
    println!("CONUS Wildfire Burn Probability: Batch & Evict Hexification Pipeline");
    println!("Usage:");
    println!("  conus_batch_and_evict [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --start-band <n>      Starting band index (0 to 4, default: 0)");
    println!("  --end-band <n>        Ending band index inclusive (0 to 4, default: 4)");
    println!("  --resolutions <r..>   H3 resolutions separated by commas (default: 8,9)");
    println!("  --workers <n>         Concurrent download threads (default: 8)");
    println!("  --output-dir <path>   Directory for final Parquet files (default: data/conus_bands)");
    println!("  --raw-dir <path>      Directory for temporary GeoTIFF tiles (default: data/conus_bands_raw)");
    println!("  --keep-raw            Do not delete raw GeoTIFF tiles after processing (default: false)");
    println!("  --dry-run             Display plan and tile coordinates without downloading");
    println!("  --help, -h            Show this help menu");
}

fn download_band_tiles(
    band: &BandConfig,
    raw_band_dir: &Path,
    num_workers: usize,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error + Send + Sync>> {
    fs::create_dir_all(raw_band_dir)?;

    let col_step = (band.max_lon - band.min_lon) / band.cols as f64;
    let row_step = (band.max_lat - band.min_lat) / band.rows as f64;

    let mut jobs = Vec::new();

    for r in 0..band.rows {
        let max_lat = band.max_lat - (r as f64 * row_step);
        let min_lat = max_lat - row_step;

        let center_lat = (min_lat + max_lat) * 0.5;
        let meters_per_deg_lon = 111_120.0 * center_lat.to_radians().cos().abs();
        let meters_per_deg_lat = 111_120.0;

        let height_px = ((max_lat - min_lat) * meters_per_deg_lat / 30.0).round() as u32;

        for c in 0..band.cols {
            let min_lon = band.min_lon + (c as f64 * col_step);
            let max_lon = min_lon + col_step;

            if !intersects_conus_rough(min_lon, min_lat, max_lon, max_lat) {
                continue;
            }

            let width_px = ((max_lon - min_lon) * meters_per_deg_lon / 30.0).round() as u32;
            let file_name = format!("tile_r{:02}_c{:02}.tif", r, c);
            let out_path = raw_band_dir.join(file_name);

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

    println!("  Band {}: Identified {} active land tiles to fetch", band.band_index, jobs.len());

    let pool = rayon::ThreadPoolBuilder::new().num_threads(num_workers).build()?;
    let downloaded_count = Arc::new(AtomicUsize::new(0));
    let skipped_count = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicUsize::new(0));

    let t0 = Instant::now();

    pool.install(|| {
        jobs.par_iter().for_each(|job| {
            if job.out_path.exists() {
                if let Ok(meta) = fs::metadata(&job.out_path) {
                    if meta.len() > 1024 {
                        skipped_count.fetch_add(1, Ordering::Relaxed);
                        total_bytes.fetch_add(meta.len() as usize, Ordering::Relaxed);
                        return;
                    }
                }
            }

            let url = format!(
                "{}?bbox={:.6},{:.6},{:.6},{:.6}&bboxSR=4326&imageSR=4326&size={},{}&format=tiff&compression=LZW&f=image",
                BASE_URL,
                job.min_lon, job.min_lat, job.max_lon, job.max_lat,
                job.width_px, job.height_px
            );

            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(90))
                .build();

            match agent.get(&url).call() {
                Ok(resp) => {
                    let temp_path = job.out_path.with_extension("tif.tmp");
                    if let Ok(file) = File::create(&temp_path) {
                        let mut writer = BufWriter::new(file);
                        if let Ok(bytes_written) = copy(&mut resp.into_reader(), &mut writer) {
                            drop(writer);
                            if fs::rename(&temp_path, &job.out_path).is_ok() {
                                downloaded_count.fetch_add(1, Ordering::Relaxed);
                                total_bytes.fetch_add(bytes_written as usize, Ordering::Relaxed);
                            } else {
                                let _ = fs::remove_file(&temp_path);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("    [WARN] Tile fetch failed (r:{}, c:{}): {}", job.row, job.col, e);
                }
            }
        });
    });

    let dl = downloaded_count.load(Ordering::Relaxed);
    let sk = skipped_count.load(Ordering::Relaxed);
    let mb = total_bytes.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
    println!("  Band {}: Download completed in {:.1?} ({} downloaded, {} cached, {:.1} MB total)",
        band.band_index, t0.elapsed(), dl, sk, mb);

    let pattern = format!("{}/tile_*.tif", raw_band_dir.to_str().unwrap());
    Ok(resolve_raster_sources(&pattern)?)
}

fn process_band_to_parquet(
    band_index: usize,
    paths: &[PathBuf],
    resolutions: &[u8],
    out_parquet: &Path,
) -> Result<(usize, usize), Box<dyn std::error::Error + Send + Sync>> {
    println!("  Band {}: Initializing MosaicReader across {} tiles...", band_index, paths.len());
    let t_init = Instant::now();
    let mosaic = Arc::new(MosaicReader::open(paths, None, None, OverlapRule::Cutline)?);
    println!("  Band {}: Mosaic opened in {:.2?} ({} total chunks across mosaic)",
        band_index, t_init.elapsed(), mosaic.chunk_refs.len());

    let mut config = MultiResolutionConfig::new(resolutions.to_vec());
    config.overlap_rule = OverlapRule::Cutline;

    let t_stream = Instant::now();
    let mut streamer = MultiScanHorizonStreamer::new_mosaic(Arc::clone(&mosaic), &config)?;

    // Parquet Schema (omitting h3_hex to save CPU & allocations; can be formatted on-the-fly via printf('%x', h3_index))
    let message_type = "
        message conus_burn_probability {
            REQUIRED INT64 h3_index;
            REQUIRED INT32 resolution;
            REQUIRED DOUBLE pixel_count;
            REQUIRED DOUBLE mean_burn_probability;
            REQUIRED DOUBLE max_burn_probability;
        }
    ";
    let schema = Arc::new(parse_message_type(message_type)?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(parquet::basic::Compression::SNAPPY)
            .build(),
    );

    let file = File::create(out_parquet)?;
    let mut writer = SerializedFileWriter::new(file, schema, props)?;

    let mut total_res8 = 0usize;
    let mut total_res9 = 0usize;

    // Buffer for batch writing row groups
    let mut batch_h3_indices = Vec::with_capacity(100_000);
    let mut batch_resolutions = Vec::with_capacity(100_000);
    let mut batch_pixel_counts = Vec::with_capacity(100_000);
    let mut batch_means = Vec::with_capacity(100_000);
    let mut batch_maxs = Vec::with_capacity(100_000);

    let flush_batch = |
        writer: &mut SerializedFileWriter<File>,
        indices: &mut Vec<i64>,
        resols: &mut Vec<i32>,
        pixels: &mut Vec<f64>,
        means: &mut Vec<f64>,
        maxs: &mut Vec<f64>,
    | -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if indices.is_empty() {
            return Ok(());
        }
        let mut row_group = writer.next_row_group()?;

        // Col 0: h3_index
        let mut col_writer = row_group.next_column()?.unwrap();
        col_writer.typed::<Int64Type>().write_batch(indices, None, None)?;
        col_writer.close()?;

        // Col 1: resolution
        let mut col_writer = row_group.next_column()?.unwrap();
        col_writer.typed::<Int32Type>().write_batch(resols, None, None)?;
        col_writer.close()?;

        // Col 2: pixel_count
        let mut col_writer = row_group.next_column()?.unwrap();
        col_writer.typed::<DoubleType>().write_batch(pixels, None, None)?;
        col_writer.close()?;

        // Col 3: mean_burn_probability
        let mut col_writer = row_group.next_column()?.unwrap();
        col_writer.typed::<DoubleType>().write_batch(means, None, None)?;
        col_writer.close()?;

        // Col 4: max_burn_probability
        let mut col_writer = row_group.next_column()?.unwrap();
        col_writer.typed::<DoubleType>().write_batch(maxs, None, None)?;
        col_writer.close()?;

        row_group.close()?;

        indices.clear();
        resols.clear();
        pixels.clear();
        means.clear();
        maxs.clear();

        Ok(())
    };

    let mut next_progress = 2_000_000usize;

    loop {
        let n = streamer.drain_completed_into(4096, |_hex_id, rec| {
            let res = rec.resolution;
            if res == 8 {
                total_res8 += 1;
            } else if res == 9 {
                total_res9 += 1;
            }

            batch_h3_indices.push(rec.h3_index as i64);
            batch_resolutions.push(res as i32);
            batch_pixel_counts.push(rec.accumulator.count);
            batch_means.push(rec.accumulator.mean());
            batch_maxs.push(rec.accumulator.max);
        });

        let total_hex = total_res8 + total_res9;
        if total_hex >= next_progress {
            let elapsed = t_stream.elapsed().as_secs_f64();
            println!(
                "    Band {}: Streamed {:>5.1}M hexagons in {:>5.1}s ({:.1} khex/s, lat horizon: {:.2}°N)...",
                band_index,
                total_hex as f64 / 1_000_000.0,
                elapsed,
                (total_hex as f64 / 1000.0) / elapsed,
                streamer.current_lat_horizon()
            );
            next_progress += 2_000_000;
        }

        if batch_h3_indices.len() >= 100_000 {
            flush_batch(
                &mut writer,
                &mut batch_h3_indices,
                &mut batch_resolutions,
                &mut batch_pixel_counts,
                &mut batch_means,
                &mut batch_maxs,
            )?;
        }

        if n == 0 {
            break;
        }
    }

    // Flush remaining records
    flush_batch(
        &mut writer,
        &mut batch_h3_indices,
        &mut batch_resolutions,
        &mut batch_pixel_counts,
        &mut batch_means,
        &mut batch_maxs,
    )?;

    writer.close()?;

    let parquet_meta = fs::metadata(out_parquet)?;
    let p_size_mb = parquet_meta.len() as f64 / (1024.0 * 1024.0);

    println!("  Band {}: Hexified in {:.1?} | Res 8: {} hexes | Res 9: {} hexes | Output: {:.1} MB",
        band_index, t_stream.elapsed(), total_res8, total_res9, p_size_mb);

    Ok((total_res8, total_res9))
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = env::args().collect();

    let mut start_band = 0usize;
    let mut end_band = 4usize;
    let mut resolutions = vec![8u8, 9u8];
    let mut num_workers = 8usize;
    let mut output_dir = PathBuf::from("data/conus_bands");
    let mut raw_dir = PathBuf::from("data/conus_bands_raw");
    let mut evict_raw = true;
    let mut dry_run = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--start-band" => {
                if i + 1 < args.len() {
                    start_band = args[i + 1].trim().parse::<usize>().unwrap_or(0);
                    i += 1;
                }
            }
            "--end-band" => {
                if i + 1 < args.len() {
                    end_band = args[i + 1].trim().parse::<usize>().unwrap_or(4);
                    i += 1;
                }
            }
            "--resolutions" | "-r" => {
                if i + 1 < args.len() {
                    resolutions = args[i + 1]
                        .split(',')
                        .filter_map(|s| s.trim().parse::<u8>().ok())
                        .collect();
                    i += 1;
                }
            }
            "--workers" | "-w" => {
                if i + 1 < args.len() {
                    num_workers = args[i + 1].trim().parse::<usize>().unwrap_or(8).max(1);
                    i += 1;
                }
            }
            "--output-dir" => {
                if i + 1 < args.len() {
                    output_dir = PathBuf::from(&args[i + 1]);
                    i += 1;
                }
            }
            "--raw-dir" => {
                if i + 1 < args.len() {
                    raw_dir = PathBuf::from(&args[i + 1]);
                    i += 1;
                }
            }
            "--keep-raw" => {
                evict_raw = false;
            }
            "--dry-run" => {
                dry_run = true;
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

    // Define the 5 latitudinal bands covering CONUS
    // 0.05° seam overlap ensures boundary H3 cells are completely covered
    let bands = vec![
        // Band 0: Northern Tier (WA, ID, MT, ND, MN, Upper Great Lakes)
        BandConfig { band_index: 0, min_lon: -125.0, max_lon: -66.5, min_lat: 44.50, max_lat: 49.50, cols: 39, rows: 5 },
        // Band 1: Upper Central (OR, WY, SD, NE, IA, IL, MI, NY, New England)
        BandConfig { band_index: 1, min_lon: -125.0, max_lon: -66.5, min_lat: 39.50, max_lat: 44.55, cols: 39, rows: 5 },
        // Band 2: Mid Tier (CA, NV, UT, CO, KS, MO, IN, OH, PA, Mid-Atlantic)
        BandConfig { band_index: 2, min_lon: -125.0, max_lon: -66.5, min_lat: 34.50, max_lat: 39.55, cols: 39, rows: 5 },
        // Band 3: Southern Tier (AZ, NM, TX, OK, AR, TN, NC, SC, GA, AL, MS)
        BandConfig { band_index: 3, min_lon: -125.0, max_lon: -66.5, min_lat: 29.50, max_lat: 34.55, cols: 39, rows: 5 },
        // Band 4: Deep South & Florida (South TX, LA Coast, FL Peninsula)
        BandConfig { band_index: 4, min_lon: -125.0, max_lon: -66.5, min_lat: 24.50, max_lat: 29.55, cols: 39, rows: 5 },
    ];

    println!("=========================================================================================");
    println!("     CONUS WILDFIRE BURN PROBABILITY: BATCH & EVICT HEXIFICATION PIPELINE                ");
    println!("=========================================================================================");
    println!("  Target Resolutions : {:?}", resolutions);
    println!("  Active Bands       : Band {} to Band {}", start_band, end_band);
    println!("  Download Workers   : {}", num_workers);
    println!("  Evict Raw GeoTIFFs : {}", evict_raw);
    println!("  Output Directory   : {:?}", output_dir);
    println!("  Raw Staging Dir    : {:?}", raw_dir);
    println!("-----------------------------------------------------------------------------------------\n");

    if dry_run {
        println!("Dry run mode enabled. Band plans:");
        for b in &bands {
            if b.band_index >= start_band && b.band_index <= end_band {
                println!("  Band {}: Lat [{:.2}°N to {:.2}°N], Lon [{:.2}°W to {:.2}°W] (Grid: {}x{})",
                    b.band_index, b.min_lat, b.max_lat, b.min_lon, b.max_lon, b.cols, b.rows);
            }
        }
        return Ok(());
    }

    let global_start = Instant::now();
    let mut total_conus_res8 = 0usize;
    let mut total_conus_res9 = 0usize;

    for band in &bands {
        if band.band_index < start_band || band.band_index > end_band {
            continue;
        }

        println!("\n>>> STARTING BAND {} [Lat: {:.2}°N to {:.2}°N] <<<",
            band.band_index, band.min_lat, band.max_lat);

        let out_parquet = output_dir.join(format!("conus_bp_band_{}.parquet", band.band_index));
        if out_parquet.exists() {
            println!("  [RESUME] Band {} Parquet already exists ({:?}), skipping.",
                band.band_index, out_parquet);
            continue;
        }

        let raw_band_dir = raw_dir.join(format!("band_{}", band.band_index));

        // Step 1: Download tiles for this band
        let tile_paths = download_band_tiles(band, &raw_band_dir, num_workers)?;

        if tile_paths.is_empty() {
            println!("  [WARN] No tiles downloaded for Band {}. Skipping processing.", band.band_index);
            continue;
        }

        // Step 2: Stream and hexify to Parquet
        let (res8_count, res9_count) = process_band_to_parquet(
            band.band_index,
            &tile_paths,
            &resolutions,
            &out_parquet,
        )?;

        total_conus_res8 += res8_count;
        total_conus_res9 += res9_count;

        // Step 3: Evict raw GeoTIFF files immediately to free disk space
        if evict_raw {
            println!("  Band {}: Evicting raw GeoTIFF tiles from {:?}...", band.band_index, raw_band_dir);
            if let Err(e) = fs::remove_dir_all(&raw_band_dir) {
                eprintln!("    [WARN] Failed to purge raw directory {:?}: {}", raw_band_dir, e);
            } else {
                println!("  Band {}: Eviction complete. Disk space reclaimed successfully.", band.band_index);
            }
        }
    }

    println!("\n=========================================================================================");
    println!("                     FULL CONUS BATCH & EVICT PIPELINE COMPLETE!                         ");
    println!("=========================================================================================");
    println!("  Total Time Elapsed  : {:.1?}", global_start.elapsed());
    println!("  Total Res 8 Hexagons: {}", total_conus_res8);
    println!("  Total Res 9 Hexagons: {}", total_conus_res9);
    println!("  Parquet Partitions  : {:?}", output_dir);
    println!("=========================================================================================\n");

    Ok(())
}
