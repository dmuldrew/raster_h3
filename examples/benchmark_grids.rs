//! Time and peak memory of a full Hawaii scan on each grid (H3, A5, S2, HEALPix).
//!
//! Each case runs in its own child process, so its peak resident memory is its own and
//! not the high-water mark of an earlier case. Resolutions are matched by cell area:
//! for every H3 resolution given, each other grid uses the level whose mean cell area
//! is closest to it.
//!
//! Run with: `cargo run --release --example benchmark_grids -- [--h3-res 8,10] [--runs 3]`
//!
//! Needs `data/CFL_HI.tif` (continuous) and `data/LF2024_FBFM40_HI.tif` (categorical).

use raster_h3::core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::core::grid::GridKind;
use raster_h3::core::raster::geotiff::GeoTiffStreamReader;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

const DATASETS: [(&str, &str, bool); 2] = [
    ("CFL (continuous)", "data/CFL_HI.tif", false),
    ("FBFM40 (categorical)", "data/LF2024_FBFM40_HI.tif", true),
];

/// The resolution of `grid` whose mean cell area is closest (in ratio) to `area_m2`.
fn area_matched_resolution(grid: GridKind, area_m2: f64) -> u8 {
    (0..=grid.max_resolution())
        .min_by(|&a, &b| {
            let off = |r: u8| (grid.area_m2(r) / area_m2).ln().abs();
            off(a).total_cmp(&off(b))
        })
        .unwrap()
}

/// Peak resident set size of this process in bytes.
fn peak_rss_bytes() -> u64 {
    // SAFETY: `getrusage` fills the zeroed struct it is given and has no other effect.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    // Bytes on macOS, kilobytes elsewhere.
    let scale = if cfg!(target_os = "macos") { 1 } else { 1024 };
    usage.ru_maxrss as u64 * scale
}

/// One scan in this process: prints `seconds cells pixels peak_rss_bytes`.
fn run_case(grid: GridKind, path: &str, categorical: bool, res: u8) {
    let config = MultiResolutionConfig::new(vec![res]).with_grid(grid);
    let reader = GeoTiffStreamReader::open(path).expect("open raster");
    let start = Instant::now();
    let (mut cells, mut pixels) = (0usize, 0.0f64);
    if categorical {
        let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
        while streamer
            .drain_completed_into(2048, |_, rec| {
                cells += 1;
                pixels += rec.accumulator.total_count;
            })
            .unwrap()
            > 0
        {}
    } else {
        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        while streamer
            .drain_completed_into(2048, |_, rec| {
                cells += 1;
                pixels += rec.accumulator.count;
            })
            .unwrap()
            > 0
        {}
    }
    println!(
        "{} {} {} {}",
        start.elapsed().as_secs_f64(),
        cells,
        pixels,
        peak_rss_bytes()
    );
}

struct Outcome {
    seconds: f64,
    cells: usize,
    pixels: f64,
    peak_rss: u64,
}

/// Run one case `runs` times in child processes; the fastest time, the largest peak.
fn measure(grid: GridKind, path: &str, categorical: bool, res: u8, runs: usize) -> Outcome {
    let exe = std::env::current_exe().unwrap();
    let mut best: Option<Outcome> = None;
    for _ in 0..runs {
        let output = Command::new(&exe)
            .args(["--case", grid.as_str(), path])
            .arg(categorical.to_string())
            .arg(res.to_string())
            .output()
            .expect("spawn case");
        assert!(
            output.status.success(),
            "{grid} res {res} on {path} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let fields: Vec<&str> = text.split_whitespace().collect();
        let run = Outcome {
            seconds: fields[0].parse().unwrap(),
            cells: fields[1].parse().unwrap(),
            pixels: fields[2].parse().unwrap(),
            peak_rss: fields[3].parse().unwrap(),
        };
        best = Some(match best {
            None => run,
            Some(prev) => Outcome {
                seconds: prev.seconds.min(run.seconds),
                peak_rss: prev.peak_rss.max(run.peak_rss),
                ..run
            },
        });
    }
    best.unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--case") {
        let grid = GridKind::parse(&args[1]).expect("grid");
        run_case(
            grid,
            &args[2],
            args[3].parse().unwrap(),
            args[4].parse().unwrap(),
        );
        return;
    }

    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    let h3_resolutions: Vec<u8> = value("--h3-res")
        .map(|v| v.split(',').map(|r| r.parse().expect("H3 resolution")).collect())
        .unwrap_or_else(|| vec![8, 10]);
    let runs: usize = value("--runs").map_or(3, |v| v.parse().expect("runs"));

    for (_, path, _) in DATASETS {
        if !Path::new(path).exists() {
            eprintln!("Error: {path} not found; download the Hawaii datasets into data/.");
            std::process::exit(1);
        }
    }

    println!(
        "Hawaii scans per grid: fastest of {runs} runs, largest peak RSS, {} threads\n",
        std::thread::available_parallelism().map_or(1, usize::from)
    );
    for (label, path, categorical) in DATASETS {
        let reader = GeoTiffStreamReader::open(path).unwrap();
        println!(
            "{label}: {path}, {} x {} pixels",
            reader.metadata.width, reader.metadata.height
        );
        drop(reader);
        println!(
            "  {:<8} {:>4} {:>12} {:>10} {:>9} {:>9} {:>10}",
            "grid", "res", "cell km2", "cells", "time s", "Mpx/s", "peak MB"
        );
        for &h3_res in &h3_resolutions {
            let target_area = GridKind::H3.area_m2(h3_res);
            for grid in GridKind::ALL {
                let res = if grid == GridKind::H3 {
                    h3_res
                } else {
                    area_matched_resolution(grid, target_area)
                };
                let run = measure(grid, path, categorical, res, runs);
                println!(
                    "  {:<8} {:>4} {:>12.4} {:>10} {:>9.3} {:>9.1} {:>10.1}",
                    grid.label(),
                    res,
                    grid.area_m2(res) / 1e6,
                    run.cells,
                    run.seconds,
                    run.pixels / 1e6 / run.seconds,
                    run.peak_rss as f64 / (1024.0 * 1024.0)
                );
            }
            println!();
        }
    }
}
