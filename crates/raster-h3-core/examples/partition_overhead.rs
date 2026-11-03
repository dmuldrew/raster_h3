//! CPU cost of a partitioned scan (`MultiResolutionConfig::partition`) relative to one
//! scan of the same raster.
//!
//! A partitioned run plans the partitions, scans each one in turn (each with the full
//! thread pool, like one task per executor), passes every partial cell through its byte
//! encoding and merges them. Its total process CPU time (user + sys, all threads, from
//! `getrusage`) is compared with a single scan's. Runs are interleaved and the medians of
//! several repetitions are reported; the file is in the page cache after a warm-up run.
//!
//! ```text
//! cargo run --release -p raster-h3-core --example partition_overhead -- [--quick] [--json out.json]
//! ```
//!
//! Partitions here are small (tens to hundreds of 256 px tiles), so boundary fractions
//! are higher than for production-sized partitions of large rasters.

#[path = "../tests/it/helpers.rs"]
#[allow(dead_code)]
mod helpers;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use raster_h3_core::aggregator::multi_horizon::{
    encode_partial_state, plan_partitions, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::raster::mosaic::{MosaicReader, OverlapRule};
use serde_json::{json, Value};

/// Process CPU seconds (user + system, all threads).
fn cpu_seconds() -> f64 {
    // SAFETY: `getrusage` only writes the zero-initialized struct we pass it.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

#[derive(Clone, Copy, Default)]
struct Cost {
    cpu_s: f64,
    wall_s: f64,
}

fn measure(f: impl FnOnce()) -> Cost {
    let (cpu, wall) = (cpu_seconds(), Instant::now());
    f();
    Cost {
        cpu_s: cpu_seconds() - cpu,
        wall_s: wall.elapsed().as_secs_f64(),
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn open(path: &Path) -> Arc<MosaicReader> {
    Arc::new(
        MosaicReader::open(
            std::slice::from_ref(&path.to_path_buf()),
            None,
            None,
            OverlapRule::Cutline,
        )
        .expect("open mosaic"),
    )
}

fn single_scan(path: &Path, config: &MultiResolutionConfig) -> usize {
    let mut s = MultiScanHorizonStreamer::new_mosaic(open(path), config).expect("streamer");
    let mut cells = 0;
    while s
        .drain_completed_into(4096, |_, _| cells += 1)
        .expect("drain")
        > 0
    {}
    cells
}

#[derive(Default)]
struct PartitionedRun {
    partitions: usize,
    cells: usize,
    interior: u64,
    boundary: u64,
    merge: Cost,
}

fn partitioned_scan(
    path: &Path,
    config: &MultiResolutionConfig,
    target_chunks: usize,
) -> PartitionedRun {
    let plan = plan_partitions(&open(path), target_chunks).expect("plan");
    let mut run = PartitionedRun {
        partitions: plan.len(),
        ..Default::default()
    };
    let mut partials: Vec<(u8, u64, Vec<u8>)> = Vec::new();
    for partition in plan {
        let mut cfg = config.clone();
        cfg.partition = Some(partition);
        let mut s = MultiScanHorizonStreamer::new_mosaic(open(path), &cfg).expect("streamer");
        while s
            .drain_partitioned_into(
                4096,
                |_, _| run.cells += 1,
                |p| {
                    partials.push((
                        p.resolution,
                        p.h3_index,
                        encode_partial_state(&p.accumulator),
                    ))
                },
            )
            .expect("drain")
            > 0
        {}
        let (interior, boundary) = s.partition_cell_counts().expect("partitioned");
        run.interior += interior;
        run.boundary += boundary;
    }
    run.merge = measure(|| {
        let mut merger = MultiScanHorizonStreamer::partial_merger(config);
        for (res, h3, state) in &partials {
            merger
                .push_encoded(*res, *h3, state, 64 << 20)
                .expect("merge");
        }
        run.cells += merger.finish().len();
    });
    run
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    let json_path = args
        .iter()
        .position(|a| a == "--json")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let (side, reps): (u32, usize) = if quick { (2048, 3) } else { (6144, 5) };
    const TILE: u32 = 256;
    let chunks = (side / TILE).pow(2) as usize;
    let targets: Vec<usize> = [chunks, chunks / 4, chunks / 16]
        .into_iter()
        .filter(|&t| t >= 1)
        .collect();

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("raster.tif");
    helpers::write_tiled(
        &path,
        (side, side),
        (TILE, TILE),
        helpers::Crs::Geographic {
            lon: -100.0,
            lat: 40.0,
            pixel_deg: 0.0002,
        },
        "-9999",
        |c, r| 100.0 + ((c * 31 + r * 17) % 97) as f32 + (c as f32 / 211.0).sin() * 20.0,
    );
    let mpx = f64::from(side) * f64::from(side) / 1e6;
    println!(
        "### Partitioned scan CPU overhead\n\n{side}×{side} px f32 raster ({mpx:.1} Mpx, {chunks} tiles of {TILE} px, ~20 m pixels), {reps} interleaved repetitions, medians, {} threads. \
         `CPU ratio` = partitioned CPU (all partitions + merge) / single-scan CPU.\n",
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
    let mut table = String::from(
        "| resolutions | partitions | tiles/partition | cells | boundary % | single CPU s | partitioned CPU s | merge CPU s | CPU ratio | single wall s | partitioned wall s |\n\
         |:---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    let mut rows = Vec::new();

    for resolutions in [vec![8u8], vec![10], vec![7, 8, 9]] {
        let config = MultiResolutionConfig::new(resolutions.clone());
        let expected = single_scan(&path, &config); // warm-up, and the reference cell count
        let mut single = (Vec::new(), Vec::new());
        let mut parts: Vec<(Vec<f64>, Vec<f64>, Vec<f64>, PartitionedRun)> =
            targets.iter().map(|_| Default::default()).collect();
        for _ in 0..reps {
            let c = measure(|| assert_eq!(single_scan(&path, &config), expected));
            single.0.push(c.cpu_s);
            single.1.push(c.wall_s);
            for (i, &target) in targets.iter().enumerate() {
                let mut run = PartitionedRun::default();
                let c = measure(|| run = partitioned_scan(&path, &config, target));
                assert_eq!(
                    run.cells, expected,
                    "partitioned scan lost or duplicated cells"
                );
                parts[i].0.push(c.cpu_s);
                parts[i].1.push(c.wall_s);
                parts[i].2.push(run.merge.cpu_s);
                parts[i].3 = run;
            }
        }
        let (single_cpu, single_wall) = (median(single.0), median(single.1));
        let label = format!("{resolutions:?}");
        for ((cpu, wall, merge, run), &target) in parts.into_iter().zip(&targets) {
            let (cpu, wall, merge) = (median(cpu), median(wall), median(merge));
            let boundary_pct =
                100.0 * run.boundary as f64 / (run.interior + run.boundary).max(1) as f64;
            let ratio = cpu / single_cpu;
            let _ = writeln!(
                table,
                "| {label} | {} | {} | {expected} | {boundary_pct:.2} | {single_cpu:.3} | {cpu:.3} | {merge:.4} | {ratio:.3} | {single_wall:.3} | {wall:.3} |",
                run.partitions,
                chunks / run.partitions,
            );
            rows.push(json!({
                "resolutions": resolutions, "partitions": run.partitions, "target_chunks": target,
                "cells": expected, "boundary_pct": boundary_pct, "single_cpu_s": single_cpu,
                "partitioned_cpu_s": cpu, "merge_cpu_s": merge, "cpu_ratio": ratio,
                "single_wall_s": single_wall, "partitioned_wall_s": wall,
            }));
        }
    }
    println!("{table}");
    if let Some(path) = json_path {
        let text = serde_json::to_string_pretty(&Value::from(rows)).expect("json");
        std::fs::write(&path, text).expect("write json");
        eprintln!("wrote {}", path.display());
    }
}
