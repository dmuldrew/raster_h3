use std::path::Path;
use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use raster_h3::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3::aggregator::SamplingPattern;

fn print_help() {
    println!("raster_h3 PMTiles v3 Converter (GeoTIFF & Parquet)");
    println!("Usage:");
    println!("  convert_to_pmtiles --input <in.tif|in.parquet> --output <out.pmtiles> [options]");
    println!("  convert_to_pmtiles <in.tif> <out.pmtiles> [resolutions] [categorical] [nodata]");
    println!("\nOptions:");
    println!("  -i, --input <PATH>         Input GeoTIFF raster path, COG URL, or Parquet file");
    println!("  -o, --output <PATH>        Output PMTiles v3 archive path");
    println!("  -r, --resolutions <LIST>   Target H3 resolutions (comma-separated, default: 5,6,7,8,9,10)");
    println!("  -c, --categorical          Enable categorical mode for raster inputs");
    println!("  -s, --sampling <PATTERN>   Sampling pattern: center, rgss, hex, 16point (default: center)");
    println!("      --nodata <FLOAT>       Custom nodata override value");
    println!("  -p, --properties <LIST>    Comma-separated list of properties to include in vector tiles");
    println!("      --h3-column <NAME>     H3 cell column name for Parquet input (default: auto-detect)");
    println!("      --bbox <BBOX>          Bounding box crop in WGS84: min_lon,min_lat,max_lon,max_lat");
    println!("      --parquet              Force Parquet ingestion mode");
    println!("  -h, --help                 Display this help menu");
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        if args.len() < 2 {
            std::process::exit(1);
        }
        return Ok(());
    }

    let mut input_path = String::new();
    let mut output_path = String::new();
    let mut resolutions: Vec<u8> = vec![5, 6, 7, 8, 9, 10];
    let mut is_categorical = false;
    let mut custom_nodata = None;
    let mut sampling = SamplingPattern::center();
    let mut properties = None;
    let mut h3_column = None;
    let mut bbox = None;
    let mut force_parquet = false;

    // Check if using named flags
    let is_flag_mode = args.iter().any(|a| a.starts_with('-'));
    if is_flag_mode {
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--input" | "-i" => {
                    if i + 1 < args.len() { input_path = args[i + 1].clone(); i += 1; }
                }
                "--output" | "-o" => {
                    if i + 1 < args.len() { output_path = args[i + 1].clone(); i += 1; }
                }
                "--resolutions" | "-r" => {
                    if i + 1 < args.len() {
                        resolutions = args[i + 1].split(',').filter_map(|s| s.trim().parse().ok()).collect();
                        i += 1;
                    }
                }
                "--categorical" | "-c" => {
                    is_categorical = true;
                }
                "--sampling" | "-s" => {
                    if i + 1 < args.len() {
                        sampling = match args[i + 1].to_ascii_lowercase().as_str() {
                            "rgss" => SamplingPattern::rgss(),
                            "hex" | "hex_seven_point" => SamplingPattern::hex_seven_point(),
                            "16point" | "sixteen_point" => SamplingPattern::sixteen_point(),
                            _ => SamplingPattern::center(),
                        };
                        i += 1;
                    }
                }
                "--nodata" => {
                    if i + 1 < args.len() { custom_nodata = args[i + 1].parse().ok(); i += 1; }
                }
                "--properties" | "-p" => {
                    if i + 1 < args.len() { properties = Some(args[i + 1].clone()); i += 1; }
                }
                "--h3-column" => {
                    if i + 1 < args.len() { h3_column = Some(args[i + 1].clone()); i += 1; }
                }
                "--bbox" => {
                    if i + 1 < args.len() {
                        let parts: Vec<f64> = args[i + 1].split(',').filter_map(|s| s.trim().parse().ok()).collect();
                        if parts.len() == 4 {
                            bbox = Some([parts[0], parts[1], parts[2], parts[3]]);
                        } else {
                            eprintln!("Error: --bbox requires min_lon,min_lat,max_lon,max_lat");
                            std::process::exit(1);
                        }
                        i += 1;
                    }
                }
                "--parquet" => {
                    force_parquet = true;
                }
                _ => {}
            }
            i += 1;
        }
    } else {
        // Positional arguments fallback
        input_path = args[1].clone();
        output_path = args[2].clone();
        if args.len() > 3 {
            resolutions = args[3].split(',').filter_map(|s| s.trim().parse().ok()).collect();
        }
        if args.len() > 4 {
            is_categorical = args[4].parse().unwrap_or(true);
        }
        if args.len() > 5 {
            custom_nodata = args[5].parse().ok();
        }
    }

    if input_path.is_empty() || output_path.is_empty() {
        eprintln!("Error: Both input and output paths must be specified.");
        std::process::exit(1);
    }

    let is_parquet = force_parquet || input_path.ends_with(".parquet");

    if is_parquet {
        println!("================================================================================");
        println!("raster_h3 PMTiles v3 Converter: Parquet Mode");
        println!("================================================================================");
        println!("  Input:     {}", input_path);
        println!("  Output:    {}", output_path);
        if let Some(ref col) = h3_column {
            println!("  H3 Column: {}", col);
        } else {
            println!("  H3 Column: Auto-detect (h3_index, h3_hex, h3, cell, hex)");
        }
        println!("--------------------------------------------------------------------------------");

        let start = std::time::Instant::now();
        let summary = H3PmtilesTiler::process_parquet_to_pmtiles(
            &input_path,
            &output_path,
            h3_column.as_deref(),
        )?;
        let elapsed = start.elapsed();

        println!("Success! Streamed Parquet into PMTiles in {:.2?}", elapsed);
        println!("  Features Processed: {}", summary.total_features);
        println!("  Valid Features:     {}", summary.valid_features);
        if summary.invalid_features_dropped > 0 {
            println!("  Invalid Dropped:    {}", summary.invalid_features_dropped);
        }
        println!("  Total Tiles:        {}", summary.total_tiles);
        println!("  Zoom Range:         {} to {}", summary.min_zoom, summary.max_zoom);
    } else {
        println!("================================================================================");
        println!("raster_h3 PMTiles v3 Converter: GeoTIFF Raster Mode");
        println!("================================================================================");
        println!("  Input:        {}", input_path);
        println!("  Output:       {}", output_path);
        println!("  Resolutions:  {:?}", resolutions);
        println!("  Categorical:  {}", is_categorical);
        if let Some(nd) = custom_nodata {
            println!("  Custom NoData: {}", nd);
        }
        if let Some(ref b) = bbox {
            println!("  BBox Crop:    [{:.4}, {:.4}, {:.4}, {:.4}]", b[0], b[1], b[2], b[3]);
        }
        if let Some(ref p) = properties {
            println!("  Properties:   {}", p);
        }
        println!("--------------------------------------------------------------------------------");

        let mut config = MultiResolutionConfig::new(resolutions);
        config.custom_nodata = custom_nodata;
        config.sampling = sampling;
        config.properties = properties;
        config.bbox = bbox;

        let start = std::time::Instant::now();
        let total_hexagons = if is_categorical {
            H3PmtilesTiler::process_categorical_source_to_pmtiles(&input_path, &output_path, config)?
        } else {
            H3PmtilesTiler::process_raster_source_to_pmtiles(&input_path, &output_path, config)?
        };
        let elapsed = start.elapsed();

        println!("Success! Generated {} hexagons in {:.2?}", total_hexagons, elapsed);
    }

    if let Ok(meta) = Path::new(&output_path).metadata() {
        println!("  Output Size:  {} bytes ({:.2} MB)", meta.len(), meta.len() as f64 / (1024.0 * 1024.0));
    }
    println!("================================================================================");

    Ok(())
}
