//! Benchmarks multi-band spectral indices, H3 hierarchical compaction, and predicate pushdown filtering.
//!
//! Evaluates continuous and categorical raster pipelines, measuring row reduction, throughput, and pruned cells.
//!
//! Run with: `cargo run --example benchmark_features`

use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use std::time::Instant;

fn main() {
    println!("=========================================================================");
    println!("     raster_h3 Benchmarking: Multi-Band, Compaction & Pushdown Features  ");
    println!("=========================================================================");

    let cfl_path = "data/CFL_HI.tif";
    let lf_path = "data/LF2024_FBFM40_HI.tif";

    // 1. Continuous Baseline vs Compaction
    println!("\n[1] Continuous (CFL_HI.tif, 28.4M pixels)");

    // Baseline
    let r1 = GeoTiffStreamReader::open(cfl_path).unwrap();
    let cfg_base = MultiResolutionConfig::new(vec![8]);
    let t0 = Instant::now();
    let mut s1 = MultiScanHorizonStreamer::new(r1, &cfg_base).unwrap();
    let mut hexes_base = 0;
    loop {
        let n = s1
            .drain_completed_into(4096, |_, _| hexes_base += 1)
            .unwrap();
        if n == 0 {
            break;
        }
    }
    let d_base = t0.elapsed();
    println!(
        "  • Baseline (compact = false) : {:.2?} | {} hexagons output",
        d_base, hexes_base
    );

    // With Compaction
    let r2 = GeoTiffStreamReader::open(cfl_path).unwrap();
    let mut cfg_compact = MultiResolutionConfig::new(vec![8]);
    cfg_compact.compact = true;
    let t1 = Instant::now();
    let mut s2 = MultiScanHorizonStreamer::new(r2, &cfg_compact).unwrap();
    let mut hexes_compact = 0;
    let mut res7_hexes = 0;
    loop {
        let n = s2
            .drain_completed_into(4096, |_, rec| {
                hexes_compact += 1;
                if rec.resolution == 7 {
                    res7_hexes += 1;
                }
            })
            .unwrap();
        if n == 0 {
            break;
        }
    }
    let d_compact = t1.elapsed();
    let reduction = (1.0 - (hexes_compact as f64 / hexes_base as f64)) * 100.0;
    println!("  • Compacted (compact = true) : {:.2?} | {} hexagons output ({:.1}% row reduction, {} parent res-7 cells)",
        d_compact, hexes_compact, reduction, res7_hexes);

    // With Predicate Pushdown (min_mean = 5.0)
    let r3 = GeoTiffStreamReader::open(cfl_path).unwrap();
    let mut cfg_pushdown = MultiResolutionConfig::new(vec![8]);
    cfg_pushdown.min_mean = Some(5.0);
    let t2 = Instant::now();
    let mut s3 = MultiScanHorizonStreamer::new(r3, &cfg_pushdown).unwrap();
    let mut hexes_pushdown = 0;
    loop {
        let n = s3
            .drain_completed_into(4096, |_, _| hexes_pushdown += 1)
            .unwrap();
        if n == 0 {
            break;
        }
    }
    let d_pushdown = t2.elapsed();
    println!(
        "  • Pushdown (min_mean >= 5.0) : {:.2?} | {} hexagons output ({:.1}% rows dropped)",
        d_pushdown,
        hexes_pushdown,
        (1.0 - (hexes_pushdown as f64 / hexes_base as f64)) * 100.0
    );

    // 2. Categorical Baseline vs Compaction
    println!("\n[2] Categorical (LF2024_FBFM40_HI.tif, 256.5M pixels)");

    // Baseline
    let r_lf1 = GeoTiffStreamReader::open(lf_path).unwrap();
    let cfg_lf_base = MultiResolutionConfig::new(vec![8]);
    let t_lf0 = Instant::now();
    let mut s_lf1 = MultiCategoricalHorizonStreamer::new(r_lf1, &cfg_lf_base).unwrap();
    let mut hexes_lf_base = 0;
    loop {
        let n = s_lf1
            .drain_completed_into(4096, |_, _| hexes_lf_base += 1)
            .unwrap();
        if n == 0 {
            break;
        }
    }
    let d_lf_base = t_lf0.elapsed();
    println!(
        "  • Baseline (compact = false) : {:.2?} | {} hexagons output ({:.2} Mpx/sec)",
        d_lf_base,
        hexes_lf_base,
        256.54 / d_lf_base.as_secs_f64()
    );

    // With Compaction
    let r_lf2 = GeoTiffStreamReader::open(lf_path).unwrap();
    let mut cfg_lf_compact = MultiResolutionConfig::new(vec![8]);
    cfg_lf_compact.compact = true;
    let t_lf1 = Instant::now();
    let mut s_lf2 = MultiCategoricalHorizonStreamer::new(r_lf2, &cfg_lf_compact).unwrap();
    let mut hexes_lf_compact = 0;
    let mut res7_lf = 0;
    loop {
        let n = s_lf2
            .drain_completed_into(4096, |_, rec| {
                hexes_lf_compact += 1;
                if rec.resolution == 7 {
                    res7_lf += 1;
                }
            })
            .unwrap();
        if n == 0 {
            break;
        }
    }
    let d_lf_compact = t_lf1.elapsed();
    let lf_reduction = (1.0 - (hexes_lf_compact as f64 / hexes_lf_base as f64)) * 100.0;
    println!("  • Compacted (compact = true) : {:.2?} | {} hexagons output ({:.1}% row reduction, {} parent res-7 cells)",
        d_lf_compact, hexes_lf_compact, lf_reduction, res7_lf);

    println!("\n=========================================================================");
}
