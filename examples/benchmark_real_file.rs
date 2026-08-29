use std::env;
use std::time::Instant;
use raster_h3::aggregator::{aggregate_raster_stream, AggregationConfig};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: benchmark_real_file <path_to_geotiff>");
        std::process::exit(1);
    }
    let path = &args[1];

    println!("=================================================================");
    println!("  Pure Rust Raster-to-H3 Speedup Benchmark (No DuckDB)");
    println!("  File: {}", path);
    println!("=================================================================");

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open file");
    
    let config = AggregationConfig {
        resolution: 8,
        ..Default::default()
    };

    let threads = [1, 2, 4, 8];
    for &t in &threads {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(t)
            .build()
            .unwrap();

        let start = Instant::now();
        let map = pool.install(|| {
            aggregate_raster_stream(&reader, &config).expect("Failed to aggregate")
        });
        let duration = start.elapsed();
        
        println!("  Threads: {}  |  Elapsed: {} ms  |  Hexagons: {}", 
                 t, duration.as_millis(), map.len());
    }
    println!("=================================================================");
}
