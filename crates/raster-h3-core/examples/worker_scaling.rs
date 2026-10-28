//! Aggregation-worker scaling benchmark: is the serial merge stage worth
//! parallelising?
//!
//! Each kernel wave fills one private cell map per worker in parallel; the
//! calling thread then folds every map into the shared per-resolution maps
//! (`merge_window`, timed as `StreamProfile::merge_ns`). This runs a set of
//! workloads at several `aggregation_workers` counts and reports how wall
//! time splits between the parallel kernel stage and the serial merge.
//!
//! The stage timers are always on, so this needs no `stream-profile` build.
//! Wall times are medians of several runs after one warm-up run. Every worker
//! count must produce the same cells and pixel counts, or the run aborts.
//!
//! ```text
//! cargo run --release -p raster-h3-core --example worker_scaling -- \
//!     [--quick] [--workers 1,2,4,8] [--reps N] [--side PX] [--json out.json]
//! ```
//!
//! Worker counts above the machine's cores oversubscribe it; the report
//! prints the available parallelism so results can be read accordingly.

#[path = "../tests/it/helpers.rs"]
mod helpers;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use raster_h3_core::aggregator::multi_horizon::profile::StreamProfile;
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
    QuantileTarget,
};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use serde_json::{json, Value};

const PIXEL_DEG: f64 = 0.0005;
const ORIGIN: (f64, f64) = (-100.0, 40.0);
/// Distinct classes in the categorical fixture.
const CLASSES: u32 = 40;

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Continuous,
    Categorical,
}

struct Workload {
    name: &'static str,
    why: &'static str,
    kind: Kind,
    resolutions: &'static [u8],
    quantiles: &'static str,
}

const WORKLOADS: &[Workload] = &[
    Workload {
        name: "res 9",
        why: "baseline (~30 pixels per cell)",
        kind: Kind::Continuous,
        resolutions: &[9],
        quantiles: "",
    },
    Workload {
        name: "res 7 (coarse)",
        why: "~1,500 pixels per cell: worker maps overlap, merges mostly combine",
        kind: Kind::Continuous,
        resolutions: &[7],
        quantiles: "",
    },
    Workload {
        name: "res 11 (fine)",
        why: "under one pixel per cell: merges are mostly inserts",
        kind: Kind::Continuous,
        resolutions: &[11],
        quantiles: "",
    },
    Workload {
        name: "res 7,9,11",
        why: "merge work grows with each resolution",
        kind: Kind::Continuous,
        resolutions: &[7, 9, 11],
        quantiles: "",
    },
    Workload {
        name: "res 9 + quantiles",
        why: "every merge also combines a quantile sketch",
        kind: Kind::Continuous,
        resolutions: &[9],
        quantiles: "p50,p90",
    },
    Workload {
        name: "categorical res 9",
        why: "every merge combines a per-class histogram",
        kind: Kind::Categorical,
        resolutions: &[9],
        quantiles: "",
    },
];

impl Workload {
    fn config(&self, workers: usize) -> MultiResolutionConfig {
        let mut config = MultiResolutionConfig::new(self.resolutions.to_vec());
        config.aggregation_workers = Some(workers);
        if !self.quantiles.is_empty() {
            config.quantiles = QuantileTarget::parse_list(self.quantiles).expect("quantiles");
        }
        config
    }
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// What one stream produced; must not depend on the worker count.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Checksum {
    cells: usize,
    /// Total pixel weight (integer-valued, so exact in any merge order).
    weight: f64,
    /// XOR of every (resolution, cell) key.
    keys: u64,
}

/// One timed stream: wall time plus the stage timers it reported.
#[derive(Clone, Copy, Debug)]
struct Run {
    wall_ns: u64,
    kernel_wall_ns: u64,
    worker_ns: u64,
    merge_ns: u64,
    prefetch_wait_ns: u64,
    spill_ns: u64,
    waves: u64,
    peak_concurrent_jobs: usize,
    checksum: Checksum,
}

impl Run {
    fn new(wall_ns: u64, m: &StreamProfile, checksum: Checksum) -> Self {
        Self {
            wall_ns,
            kernel_wall_ns: m.kernel_wall_ns,
            worker_ns: m.worker_ns,
            merge_ns: m.merge_ns,
            prefetch_wait_ns: m.prefetch_wait_ns,
            spill_ns: m.spill_ns,
            waves: m.waves,
            peak_concurrent_jobs: m.peak_concurrent_jobs,
            checksum,
        }
    }
}

fn stream_once(path: &Path, workload: &Workload, workers: usize) -> Run {
    let config = workload.config(workers);
    let reader = GeoTiffStreamReader::open(path).expect("open raster");
    let start = Instant::now();
    let mut sum = Checksum {
        cells: 0,
        weight: 0.0,
        keys: 0,
    };
    let mut add = |resolution: u8, h3: u64, weight: f64| {
        sum.cells += 1;
        sum.weight += weight;
        sum.keys ^= h3.rotate_left(u32::from(resolution));
    };
    match workload.kind {
        Kind::Continuous => {
            let mut s = MultiScanHorizonStreamer::new(reader, &config).expect("streamer");
            loop {
                let batch = s.fetch_next_batch(4096).expect("batch");
                if batch.is_empty() {
                    break;
                }
                for r in &batch {
                    add(r.resolution, r.h3_index, r.accumulator.count);
                }
            }
            Run::new(elapsed_ns(start), s.metrics(), sum)
        }
        Kind::Categorical => {
            let mut s = MultiCategoricalHorizonStreamer::new(reader, &config).expect("streamer");
            loop {
                let batch = s.fetch_next_batch(4096).expect("batch");
                if batch.is_empty() {
                    break;
                }
                for r in &batch {
                    add(r.resolution, r.h3_index, r.accumulator.total_count);
                }
            }
            Run::new(elapsed_ns(start), s.metrics(), sum)
        }
    }
}

fn elapsed_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

/// One warm-up run, then the median-wall-time run of `reps`.
fn median_run(path: &Path, workload: &Workload, workers: usize, reps: usize) -> Run {
    stream_once(path, workload, workers);
    let mut runs: Vec<Run> = (0..reps.max(1))
        .map(|_| stream_once(path, workload, workers))
        .collect();
    runs.sort_by_key(|r| r.wall_ns);
    let median = runs[runs.len() / 2];
    assert!(
        runs.iter().all(|r| r.checksum == median.checksum),
        "{}: output differs between runs at {workers} workers",
        workload.name
    );
    median
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

/// Recommendation from the merge share at the highest worker count.
fn verdict(merge_pct: f64) -> &'static str {
    if merge_pct > 20.0 {
        "parallelise"
    } else if merge_pct < 10.0 {
        "leave serial"
    } else {
        "borderline"
    }
}

struct Report {
    markdown: String,
    json: serde_json::Map<String, Value>,
}

impl Report {
    fn push(&mut self, text: &str) {
        print!("{text}");
        self.markdown.push_str(text);
    }

    fn finish(self, json_path: Option<&Path>) {
        if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(summary)
                .expect("step summary");
            let _ = writeln!(file, "## Worker scaling\n\n{}", self.markdown);
        }
        if let Some(path) = json_path {
            let text = serde_json::to_string_pretty(&Value::Object(self.json)).expect("json");
            std::fs::write(path, text).expect("write json");
            eprintln!("wrote {}", path.display());
        }
    }
}

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    let side: u32 = arg(&args, "--side")
        .map(|s| s.parse().expect("--side"))
        .unwrap_or(if quick { 1024 } else { 4096 });
    let reps: usize = arg(&args, "--reps")
        .map(|s| s.parse().expect("--reps"))
        .unwrap_or(if quick { 1 } else { 3 });
    let workers: Vec<usize> = arg(&args, "--workers")
        .unwrap_or("1,2,4,8")
        .split(',')
        .map(|w| w.trim().parse().expect("--workers"))
        .collect();
    let json_path = arg(&args, "--json").map(PathBuf::from);

    let dir = tempfile::tempdir().expect("temp dir");
    let builder = helpers::TestGeoTiffBuilder::new(side, side)
        .origin(ORIGIN.0, ORIGIN.1)
        .pixel_size(PIXEL_DEG);
    let continuous = dir.path().join("continuous.tif");
    builder
        .write_fn(&continuous, |c, r| ((c * 31 + r * 17) % 97) as f32)
        .expect("write continuous raster");
    let categorical = dir.path().join("categorical.tif");
    builder
        .write_fn(&categorical, |c, r| ((c / 7 + r / 5) % CLASSES) as u8)
        .expect("write categorical raster");

    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    let mpx = f64::from(side) * f64::from(side) / 1e6;
    let mut report = Report {
        markdown: String::new(),
        json: serde_json::Map::new(),
    };
    report.push(&format!(
        "{side}×{side} px rasters ({mpx:.1} Mpx, {PIXEL_DEG}° pixels), centre sampling, \
         default budgets, `decode_workers` 4. Available parallelism: {cores}. \
         Median of {reps} run(s) after one warm-up.\n\n\
         `merge %` is the serial merge's share of wall time. `kernel eff %` is summed \
         worker time over (kernel wall time × workers): how busy the parallel stage keeps \
         its workers. `decode wait` is time spent waiting on decoded chunks.\n\n"
    ));
    report.json.insert(
        "setup".into(),
        json!({ "side_px": side, "mpx": mpx, "reps": reps, "cores": cores, "workers": workers }),
    );

    let mut summary = String::from(
        "| workload | workers | speedup | kernel speedup | merge % | verdict |\n\
         |:---|---:|---:|---:|---:|:---|\n",
    );
    let mut all = Vec::new();
    for workload in WORKLOADS {
        let path = match workload.kind {
            Kind::Continuous => &continuous,
            Kind::Categorical => &categorical,
        };
        let runs: Vec<(usize, Run)> = workers
            .iter()
            .map(|&n| (n, median_run(path, workload, n, reps)))
            .collect();
        let base = runs[0].1;
        for (n, run) in &runs {
            assert_eq!(
                run.checksum, base.checksum,
                "{}: output at {n} workers differs from {} workers",
                workload.name, runs[0].0
            );
        }

        let mut table = format!(
            "### {}\n\n{}. {} cells.\n\n\
             | workers | wall ms | Mpx/s | speedup | kernel ms | merge ms | merge % | kernel eff % | decode wait ms | spill ms | waves | peak jobs |\n\
             |---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
            workload.name, workload.why, base.checksum.cells
        );
        let mut rows = Vec::new();
        for (n, r) in &runs {
            let speedup = base.wall_ns as f64 / r.wall_ns as f64;
            let eff = pct(r.worker_ns, r.kernel_wall_ns.saturating_mul(*n as u64));
            let merge_pct = pct(r.merge_ns, r.wall_ns);
            let _ = writeln!(
                table,
                "| {n} | {:.1} | {:.1} | {speedup:.2} | {:.1} | {:.1} | {merge_pct:.1} | {eff:.0} | {:.1} | {:.1} | {} | {} |",
                ms(r.wall_ns),
                mpx / (r.wall_ns as f64 / 1e9),
                ms(r.kernel_wall_ns),
                ms(r.merge_ns),
                ms(r.prefetch_wait_ns),
                ms(r.spill_ns),
                r.waves,
                r.peak_concurrent_jobs,
            );
            rows.push(json!({
                "workers": n, "wall_ms": ms(r.wall_ns), "speedup": speedup,
                "kernel_ms": ms(r.kernel_wall_ns), "worker_ms": ms(r.worker_ns),
                "merge_ms": ms(r.merge_ns), "merge_pct": merge_pct, "kernel_eff_pct": eff,
                "decode_wait_ms": ms(r.prefetch_wait_ns), "spill_ms": ms(r.spill_ns),
                "waves": r.waves, "peak_concurrent_jobs": r.peak_concurrent_jobs,
            }));
        }
        table.push('\n');
        report.push(&table);

        let (n, top) = runs[runs.len() - 1];
        let merge_pct = pct(top.merge_ns, top.wall_ns);
        let _ = writeln!(
            summary,
            "| {} | {n} | {:.2} | {:.2} | {merge_pct:.1} | {} |",
            workload.name,
            base.wall_ns as f64 / top.wall_ns as f64,
            base.kernel_wall_ns as f64 / top.kernel_wall_ns.max(1) as f64,
            verdict(merge_pct),
        );
        all.push(json!({
            "workload": workload.name, "cells": base.checksum.cells, "runs": rows,
        }));
    }
    report.json.insert("workloads".into(), Value::Array(all));
    report.push(&format!(
        "### Summary\n\nAt the highest worker count. `speedup` is end-to-end, \
         `kernel speedup` the parallel stage alone. Verdict: merge above 20% of wall time \
         → parallelise, below 10% → leave serial.\n\n{summary}\n"
    ));
    report.finish(json_path.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_counts_produce_identical_output() {
        let dir = tempfile::tempdir().unwrap();
        let builder = helpers::TestGeoTiffBuilder::new(96, 96)
            .origin(ORIGIN.0, ORIGIN.1)
            .pixel_size(PIXEL_DEG);
        let continuous = dir.path().join("c.tif");
        builder
            .write_fn(&continuous, |c, r| ((c * 31 + r * 17) % 97) as f32)
            .unwrap();
        let categorical = dir.path().join("k.tif");
        builder
            .write_fn(&categorical, |c, r| ((c / 7 + r / 5) % CLASSES) as u8)
            .unwrap();
        for workload in WORKLOADS {
            let path = match workload.kind {
                Kind::Continuous => &continuous,
                Kind::Categorical => &categorical,
            };
            let one = stream_once(path, workload, 1).checksum;
            assert_eq!(one.weight, 96.0 * 96.0 * workload.resolutions.len() as f64);
            assert_eq!(
                stream_once(path, workload, 3).checksum,
                one,
                "{}",
                workload.name
            );
        }
    }
}
