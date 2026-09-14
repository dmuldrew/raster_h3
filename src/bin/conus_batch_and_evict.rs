//! Full CONUS Wildfire Burn Probability Batch & Evict Streaming Pipeline
//!
//! Downloads CONUS GeoTIFF tiles in latitudinal bands from the USFS GeoPlatform ImageServer,
//! aggregates them into multi-resolution H3 hexagonal indices (e.g. Res 8 and 9),
//! streams them into Parquet partitions and/or PMTiles v3 archives using the unified
//! `H3ParquetWriter` and `H3PmtilesTiler` engines, and immediately evicts raw GeoTIFF tiles
//! to keep total disk usage strictly bounded under 2.5 GB.
//!
//! Usage:
//!   cargo run --release --bin conus_batch_and_evict -- --start-band 0 --end-band 4
//!   cargo run --release --bin conus_batch_and_evict -- --dry-run
//!   cargo run --release --bin conus_batch_and_evict -- --format both --compression zstd

use std::env;
use std::fs::{self, File};
use std::io::{copy, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use parquet::basic::Compression;
use rayon::prelude::*;

use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3::pmtiles::H3PmtilesTiler;
use raster_h3::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};

const BASE_URL: &str = "https://imagery.geoplatform.gov/iipp/rest/services/Fire_Aviation/USFS_EDW_RMRS_WRC_BurnProbability/ImageServer/exportImage";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Parquet,
    Pmtiles,
    Both,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "parquet" => Some(Self::Parquet),
            "pmtiles" => Some(Self::Pmtiles),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

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

/// Query current process resident set size (RSS) in megabytes
fn get_process_rss_mb() -> Option<f64> {
    #[cfg(unix)]
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            #[cfg(target_os = "macos")]
            {
                Some(usage.ru_maxrss as f64 / (1024.0 * 1024.0))
            }
            #[cfg(not(target_os = "macos"))]
            {
                Some(usage.ru_maxrss as f64 / 1024.0)
            }
        } else {
            None
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Validate whether a file begins with valid TIFF magic bytes ('II*\0' or 'MM\0*')
fn is_valid_tiff(path: &Path) -> bool {
    if let Ok(mut f) = File::open(path) {
        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_ok() {
            return (magic == [0x49, 0x49, 0x2A, 0x00]) || (magic == [0x4D, 0x4D, 0x00, 0x2A]);
        }
    }
    false
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
    println!("  --format <fmt>        Output format: parquet, pmtiles, or both (default: parquet)");
    println!("  --compression <type>  Parquet compression: snappy, zstd, gzip, or none (default: snappy)");
    println!("  --compact <bool>      Compact Parquet format without lat/lng/hex strings (default: true)");
    println!("  --geoparquet          Emit OGC GeoParquet 1.1 geometry and metadata (default: false)");
    println!("  --row-group-size <n>  Parquet row group row count (default: 131072)");
    println!("  --overlap-rule <rule> Mosaic overlap resolution: cutline, first, last, min, max, mean (default: cutline)");
    println!("  --sampling <pattern>  Subpixel sampling: center, 5point, 7point, 9point (default: center)");
    println!("  --workers <n>         Concurrent download threads (default: 8)");
    println!("  --output-dir <path>   Directory for final Parquet/PMTiles files (default: data/conus_bands)");
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
            // Check if already downloaded and valid TIFF
            if job.out_path.exists() {
                if let Ok(meta) = fs::metadata(&job.out_path) {
                    if meta.len() > 1024 && is_valid_tiff(&job.out_path) {
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
                .timeout(Duration::from_secs(90))
                .build();

            let mut backoff = Duration::from_secs(1);
            let max_attempts = 3;

            for attempt in 1..=max_attempts {
                let temp_path = job.out_path.with_extension("tif.tmp");
                let mut success = false;

                match agent.get(&url).call() {
                    Ok(resp) => {
                        if let Ok(file) = File::create(&temp_path) {
                            let mut writer = BufWriter::new(file);
                            if let Ok(bytes_written) = copy(&mut resp.into_reader(), &mut writer) {
                                drop(writer);
                                if is_valid_tiff(&temp_path) {
                                    if fs::rename(&temp_path, &job.out_path).is_ok() {
                                        downloaded_count.fetch_add(1, Ordering::Relaxed);
                                        total_bytes.fetch_add(bytes_written as usize, Ordering::Relaxed);
                                        success = true;
                                    }
                                } else {
                                    eprintln!(
                                        "    [WARN] Non-TIFF payload received (r:{}, c:{}), discarding...",
                                        job.row, job.col
                                    );
                                    let _ = fs::remove_file(&temp_path);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if attempt == max_attempts {
                            eprintln!(
                                "    [WARN] Tile fetch failed after {} attempts (r:{}, c:{}): {}",
                                max_attempts, job.row, job.col, e
                            );
                        }
                    }
                }

                if success {
                    break;
                }

                if attempt < max_attempts {
                    sleep(backoff);
                    backoff *= 2;
                }
            }
        });
    });

    let dl = downloaded_count.load(Ordering::Relaxed);
    let sk = skipped_count.load(Ordering::Relaxed);
    let mb = total_bytes.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
    println!(
        "  Band {}: Download complete in {:.1?} ({} downloaded, {} cached, {:.1} MB staged)",
        band.band_index, t0.elapsed(), dl, sk, mb
    );

    let pattern = format!("{}/tile_*.tif", raw_band_dir.to_str().unwrap());
    let sources = resolve_raster_sources(&pattern)?;
    let valid_sources: Vec<PathBuf> = sources.into_iter().filter(|p| is_valid_tiff(p)).collect();

    Ok(valid_sources)
}

fn process_band_streaming(
    band_index: usize,
    paths: &[PathBuf],
    resolutions: &[u8],
    sampling: SamplingPattern,
    overlap_rule: OverlapRule,
    format: OutputFormat,
    parquet_config: &ParquetExportConfig,
    output_dir: &Path,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    println!("  Band {}: Opening MosaicReader across {} tiles (rule: {:?})...",
        band_index, paths.len(), overlap_rule);
    let t_init = Instant::now();
    let mosaic = Arc::new(MosaicReader::open(paths, None, None, overlap_rule)?);
    println!("  Band {}: Mosaic opened in {:.2?} ({} total chunks across mosaic)",
        band_index, t_init.elapsed(), mosaic.chunk_refs.len());

    let mut config = MultiResolutionConfig::new(resolutions.to_vec());
    config.overlap_rule = overlap_rule;
    config.sampling = sampling;

    let mut total_hexagons_emitted = 0usize;

    // 1. Export Parquet if requested
    if format == OutputFormat::Parquet || format == OutputFormat::Both {
        let out_parquet = output_dir.join(format!("conus_bp_band_{}.parquet", band_index));
        if out_parquet.exists() {
            println!("  [RESUME] Band {} Parquet already exists ({:?}), skipping.",
                band_index, out_parquet);
        } else {
            let t_stream = Instant::now();
            let streamer = MultiScanHorizonStreamer::new_mosaic(Arc::clone(&mosaic), &config)?;

            let mut next_log = 2_000_000usize;
            let total_written = H3ParquetWriter::write_continuous_streamer_to_parquet_with_progress(
                streamer,
                &out_parquet,
                parquet_config.clone(),
                |count| {
                    if count >= next_log {
                        let elapsed = t_stream.elapsed().as_secs_f64();
                        let rss_str = get_process_rss_mb()
                            .map(|r| format!(", RSS: {:.1} MB", r))
                            .unwrap_or_default();
                        println!(
                            "    Band {}: Streamed {:>5.1}M hexagons in {:>5.1}s ({:.1} khex/s{})...",
                            band_index,
                            count as f64 / 1_000_000.0,
                            elapsed,
                            (count as f64 / 1000.0) / elapsed.max(0.001),
                            rss_str
                        );
                        next_log += 2_000_000;
                    }
                },
            )?;

            let p_size_mb = fs::metadata(&out_parquet)
                .map(|m| m.len() as f64 / (1024.0 * 1024.0))
                .unwrap_or(0.0);

            println!(
                "  Band {}: Hexified to Parquet in {:.1?} | Total: {} hexes | Output: {:.1} MB",
                band_index, t_stream.elapsed(), total_written, p_size_mb
            );
            total_hexagons_emitted = total_written;
        }
    }

    // 2. Export PMTiles if requested
    if format == OutputFormat::Pmtiles || format == OutputFormat::Both {
        let out_pmtiles = output_dir.join(format!("conus_bp_band_{}.pmtiles", band_index));
        if out_pmtiles.exists() {
            println!("  [RESUME] Band {} PMTiles already exists ({:?}), skipping.",
                band_index, out_pmtiles);
        } else {
            let t_pmtiles = Instant::now();
            let streamer = MultiScanHorizonStreamer::new_mosaic(Arc::clone(&mosaic), &config)?;

            let total_pmtiles_hex = H3PmtilesTiler::generate_from_continuous_streamer(
                streamer,
                &out_pmtiles,
            )?;

            let p_size_mb = fs::metadata(&out_pmtiles)
                .map(|m| m.len() as f64 / (1024.0 * 1024.0))
                .unwrap_or(0.0);

            println!(
                "  Band {}: Tiled to PMTiles in {:.1?} | Total: {} hexes | Output: {:.1} MB",
                band_index, t_pmtiles.elapsed(), total_pmtiles_hex, p_size_mb
            );
            if total_hexagons_emitted == 0 {
                total_hexagons_emitted = total_pmtiles_hex;
            }
        }
    }

    Ok(total_hexagons_emitted)
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = env::args().collect();

    let mut start_band = 0usize;
    let mut end_band = 4usize;
    let mut resolutions = vec![8u8, 9u8];
    let mut format = OutputFormat::Parquet;
    let mut compression = Compression::SNAPPY;
    let mut compact = true;
    let mut geoparquet = false;
    let mut row_group_size = 131_072usize;
    let mut overlap_rule = OverlapRule::Cutline;
    let mut sampling = SamplingPattern::center();
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
            "--format" | "-f" => {
                if i + 1 < args.len() {
                    if let Some(fmt) = OutputFormat::parse(&args[i + 1]) {
                        format = fmt;
                    }
                    i += 1;
                }
            }
            "--compression" => {
                if i + 1 < args.len() {
                    match args[i + 1].trim().to_lowercase().as_str() {
                        "snappy" => compression = Compression::SNAPPY,
                        "zstd" => compression = Compression::ZSTD(Default::default()),
                        "gzip" => compression = Compression::GZIP(Default::default()),
                        "none" | "uncompressed" => compression = Compression::UNCOMPRESSED,
                        _ => {}
                    }
                    i += 1;
                }
            }
            "--compact" => {
                if i + 1 < args.len() {
                    compact = args[i + 1].trim().parse::<bool>().unwrap_or(true);
                    i += 1;
                }
            }
            "--row-group-size" => {
                if i + 1 < args.len() {
                    row_group_size = args[i + 1].trim().parse::<usize>().unwrap_or(131_072);
                    i += 1;
                }
            }
            "--overlap-rule" => {
                if i + 1 < args.len() {
                    overlap_rule = OverlapRule::parse(&args[i + 1]);
                    i += 1;
                }
            }
            "--sampling" => {
                if i + 1 < args.len() {
                    match args[i + 1].trim().to_lowercase().as_str() {
                        "center" => sampling = SamplingPattern::center(),
                        "rgss" => sampling = SamplingPattern::rgss(),
                        "5point" => sampling = SamplingPattern::five_point(),
                        "7point" | "hex_seven_point" => sampling = SamplingPattern::hex_seven_point(),
                        "9point" => sampling = SamplingPattern::nine_point(),
                        _ => {}
                    }
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
            "--geoparquet" => {
                geoparquet = true;
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

    let parquet_config = ParquetExportConfig {
        compact,
        compression,
        row_group_size,
        is_categorical: false,
        geoparquet,
    };

    println!("=========================================================================================");
    println!("     CONUS WILDFIRE BURN PROBABILITY: BATCH & EVICT HEXIFICATION PIPELINE                ");
    println!("=========================================================================================");
    println!("  Target Resolutions : {:?}", resolutions);
    println!("  Output Format      : {:?}", format);
    println!("  Parquet Config     : compression={:?}, compact={}, geoparquet={}, row_group_size={}", compression, compact, geoparquet, row_group_size);
    println!("  Sampling Pattern   : {:?}", sampling);
    println!("  Overlap Rule       : {:?}", overlap_rule);
    println!("  Active Bands       : Band {} to Band {}", start_band, end_band);
    println!("  Download Workers   : {}", num_workers);
    println!("  Evict Raw GeoTIFFs : {}", evict_raw);
    println!("  Output Directory   : {:?}", output_dir);
    println!("  Raw Staging Dir    : {:?}", raw_dir);
    if let Some(rss) = get_process_rss_mb() {
        println!("  Initial Memory RSS : {:.1} MB", rss);
    }
    println!("-----------------------------------------------------------------------------------------\n");

    if dry_run {
        println!("Dry run mode enabled. Band plans:");
        for b in &bands {
            if b.band_index >= start_band && b.band_index <= end_band {
                println!(
                    "  Band {}: Lat [{:.2}°N to {:.2}°N], Lon [{:.2}°W to {:.2}°W] (Grid: {}x{}, ~{} potential tiles)",
                    b.band_index, b.min_lat, b.max_lat, b.min_lon, b.max_lon, b.cols, b.rows, b.cols * b.rows
                );
            }
        }
        return Ok(());
    }

    let global_start = Instant::now();
    let mut total_conus_hexagons = 0usize;

    for band in &bands {
        if band.band_index < start_band || band.band_index > end_band {
            continue;
        }

        println!("\n>>> STARTING BAND {} [Lat: {:.2}°N to {:.2}°N] <<<",
            band.band_index, band.min_lat, band.max_lat);

        let raw_band_dir = raw_dir.join(format!("band_{}", band.band_index));

        // Step 1: Download tiles for this band
        let tile_paths = download_band_tiles(band, &raw_band_dir, num_workers)?;

        if tile_paths.is_empty() {
            println!("  [WARN] No valid GeoTIFF tiles available for Band {}. Skipping processing.", band.band_index);
            continue;
        }

        // Step 2: Stream and hexify to Parquet / PMTiles
        let hex_count = process_band_streaming(
            band.band_index,
            &tile_paths,
            &resolutions,
            sampling.clone(),
            overlap_rule,
            format,
            &parquet_config,
            &output_dir,
        )?;

        total_conus_hexagons += hex_count;

        // Step 3: Evict raw GeoTIFF files immediately to free disk space
        if evict_raw {
            println!("  Band {}: Evicting raw GeoTIFF tiles from {:?}...", band.band_index, raw_band_dir);
            if let Err(e) = fs::remove_dir_all(&raw_band_dir) {
                eprintln!("    [WARN] Failed to purge raw directory {:?}: {}", raw_band_dir, e);
            } else {
                let rss_str = get_process_rss_mb()
                    .map(|r| format!(" (Current RSS: {:.1} MB)", r))
                    .unwrap_or_default();
                println!("  Band {}: Eviction complete. Disk space reclaimed successfully{}.",
                    band.band_index, rss_str);
            }
        }
    }

    println!("\n=========================================================================================");
    println!("                     FULL CONUS BATCH & EVICT PIPELINE COMPLETE!                         ");
    println!("=========================================================================================");
    println!("  Total Time Elapsed  : {:.1?}", global_start.elapsed());
    println!("  Total Hexagons      : {}", total_conus_hexagons);
    println!("  Artifacts Stored In : {:?}", output_dir);
    if let Some(rss) = get_process_rss_mb() {
        println!("  Final Memory RSS    : {:.1} MB", rss);
    }
    println!("=========================================================================================\n");

    Ok(())
}
