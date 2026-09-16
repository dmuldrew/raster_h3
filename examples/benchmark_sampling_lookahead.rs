//! Evaluates sub-pixel super-sampling patterns and scanline lookahead acceleration.
//!
//! Benchmarks center, RGSS, quincunx, Gaussian, hexagonal, and grid sampling patterns.
//!
//! Run with: `cargo run --example benchmark_sampling_lookahead`

use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use std::time::Instant;

fn main() {
    println!("=========================================================================");
    println!("     raster_h3 Benchmarking: Super-Sampling Lookahead Acceleration      ");
    println!("=========================================================================");

    let cfl_path = "data/CFL_HI.tif";
    let reader_probe = match GeoTiffStreamReader::open(cfl_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Could not open {}: {}", cfl_path, e);
            return;
        }
    };
    let width = reader_probe.metadata.width;
    let height = reader_probe.metadata.height;
    let total_pixels = (width as f64) * (height as f64);
    println!(
        "Test Dataset: {} ({} x {} = {:.2} Mpx)",
        cfl_path,
        width,
        height,
        total_pixels / 1_000_000.0
    );

    let patterns = [
        ("Center (1-point, baseline)", SamplingPattern::center(), 1),
        ("RGSS (4-point)", SamplingPattern::rgss(), 4),
        ("5-Point Quincunx", SamplingPattern::five_point(), 5),
        (
            "Gaussian 5-Point",
            SamplingPattern::gaussian_five_point(),
            5,
        ),
        ("Hex 7-Point", SamplingPattern::hex_seven_point(), 7),
        ("16-Point Grid", SamplingPattern::sixteen_point(), 16),
    ];

    println!(
        "\n{:<30} | {:<10} | {:<10} | {:<12} | {:<15}",
        "Sampling Pattern", "Points/Px", "Time", "Throughput", "Efficiency vs Center"
    );
    println!(
        "{:-<30}-|-{:-<10}-|-{:-<10}-|-{:-<12}-|-{:-<15}",
        "", "", "", "", ""
    );

    let mut center_duration = 0.0;

    for (name, pattern, spp) in &patterns {
        let reader = GeoTiffStreamReader::open(cfl_path).unwrap();
        let mut cfg = MultiResolutionConfig::new(vec![8]);
        cfg.sampling = pattern.clone();
        let spp = *spp;

        let t0 = Instant::now();
        let mut streamer = MultiScanHorizonStreamer::new(reader, &cfg).unwrap();
        let mut total_hexes = 0;
        let mut total_weight = 0.0;
        loop {
            let n = streamer.drain_completed_into(4096, |_, rec| {
                total_hexes += 1;
                total_weight += rec.accumulator.count;
            });
            if n == 0 {
                break;
            }
        }
        let elapsed = t0.elapsed();
        let secs = elapsed.as_secs_f64();
        let mpx_sec = (total_pixels / secs) / 1_000_000.0;

        if spp == 1 {
            center_duration = secs;
            println!(
                "{:<30} | {:<10} | {:<10.2?} | {:<8.2} Mpx/s | {:<15}",
                name, spp, elapsed, mpx_sec, "100.0% (Baseline)"
            );
        } else {
            let pct = (center_duration / secs) * 100.0;
            // Without lookahead, naive 16-point would be ~6% of center throughput (1/16th)
            println!(
                "{:<30} | {:<10} | {:<10.2?} | {:<8.2} Mpx/s | {:<5.1}% of Center",
                name, spp, elapsed, mpx_sec, pct
            );
        }
    }

    println!("\n-------------------------------------------------------------------------");
    println!(" [2] Categorical Streamer: LF2024_FBFM40_HI.tif (256.54 Mpx)");
    println!("-------------------------------------------------------------------------");
    let lf_path = "data/LF2024_FBFM40_HI.tif";
    if let Ok(reader_probe_lf) = GeoTiffStreamReader::open(lf_path) {
        let lf_pixels =
            (reader_probe_lf.metadata.width as f64) * (reader_probe_lf.metadata.height as f64);
        println!(
            "{:<30} | {:<10} | {:<10} | {:<12} | {:<15}",
            "Sampling Pattern", "Points/Px", "Time", "Throughput", "Efficiency vs Center"
        );
        println!(
            "{:-<30}-|-{:-<10}-|-{:-<10}-|-{:-<12}-|-{:-<15}",
            "", "", "", "", ""
        );

        let mut center_lf_duration = 0.0;
        for (name, pattern, spp) in &patterns {
            let reader = GeoTiffStreamReader::open(lf_path).unwrap();
            let mut cfg = MultiResolutionConfig::new(vec![8]);
            cfg.sampling = pattern.clone();
            let spp = *spp;

            let t0 = Instant::now();
            let mut streamer =
                raster_h3::aggregator::multi_horizon::MultiCategoricalHorizonStreamer::new(
                    reader, &cfg,
                )
                .unwrap();
            let mut total_hexes = 0;
            loop {
                let n = streamer.drain_completed_into(4096, |_, _| total_hexes += 1);
                if n == 0 {
                    break;
                }
            }
            let elapsed = t0.elapsed();
            let secs = elapsed.as_secs_f64();
            let mpx_sec = (lf_pixels / secs) / 1_000_000.0;

            if spp == 1 {
                center_lf_duration = secs;
                println!(
                    "{:<30} | {:<10} | {:<10.2?} | {:<8.2} Mpx/s | {:<15}",
                    name, spp, elapsed, mpx_sec, "100.0% (Baseline)"
                );
            } else {
                let pct = (center_lf_duration / secs) * 100.0;
                println!(
                    "{:<30} | {:<10} | {:<10.2?} | {:<8.2} Mpx/s | {:<5.1}% of Center",
                    name, spp, elapsed, mpx_sec, pct
                );
            }
        }
    }

    println!(
        "\nNote: Without Core Lookahead, N-point super-sampling throughput drops linearly by 1/N."
    );
    println!("With Core Lookahead, interior pixels run at single-sample SIMD speed, maintaining near-baseline throughput!");
}
