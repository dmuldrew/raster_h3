use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use raster_h3::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3::aggregator::SamplingPattern;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: convert_to_pmtiles <input.tif> <output.pmtiles> [resolutions, e.g. 5,6,7,8,9,10] [categorical: true/false] [nodata]");
        std::process::exit(1);
    }

    let input_path = &args[1];
    let output_path = &args[2];
    let resolutions: Vec<u8> = if args.len() > 3 {
        args[3].split(',').filter_map(|s| s.trim().parse().ok()).collect()
    } else {
        vec![5, 6, 7, 8, 9, 10]
    };

    let is_categorical = if args.len() > 4 {
        args[4].parse().unwrap_or(true)
    } else {
        true
    };

    let custom_nodata = if args.len() > 5 {
        args[5].parse().ok()
    } else {
        None
    };

    println!("Converting GeoTIFF -> PMTiles v3");
    println!("  Input: {}", input_path);
    println!("  Output: {}", output_path);
    println!("  Resolutions: {:?}", resolutions);
    println!("  Categorical: {}", is_categorical);
    println!("  Custom NoData: {:?}", custom_nodata);

    let mut config = MultiResolutionConfig::new(resolutions);
    config.custom_nodata = custom_nodata;
    config.sampling = SamplingPattern::center();

    let start = std::time::Instant::now();
    let total_hexagons = if is_categorical {
        H3PmtilesTiler::process_categorical_geotiff_to_pmtiles(input_path, output_path, config)?
    } else {
        H3PmtilesTiler::process_geotiff_to_pmtiles(input_path, output_path, config)?
    };
    let elapsed = start.elapsed();

    println!("Success! Generated {} hexagons in {:.2?}", total_hexagons, elapsed);
    if let Ok(meta) = std::fs::metadata(output_path) {
        println!("Output PMTiles size: {} bytes ({:.2} MB)", meta.len(), meta.len() as f64 / (1024.0 * 1024.0));
    }

    Ok(())
}
