//! Profiling harness for two deferred optimizations:
//!
//! 1. **Mosaic tile scans.** `MosaicReader::open` checks every chunk against
//!    every other tile while planning overlaps, and `is_point_owned_by` scans
//!    all tiles for each overlapping sample. This measures how planning,
//!    per-call ownership cost and end-to-end streaming scale with tile count.
//! 2. **Nested Rayon waves.** When a streamer with a dedicated aggregation
//!    pool is driven from a worker of another Rayon pool, every kernel wave is
//!    handed over through a freshly spawned OS thread. This measures how many
//!    waves take that path and what it costs relative to a direct call.
//!
//! Allocation counts and approximate peak extra live bytes come from a counting
//! global allocator (not RSS). Concurrent scheduling can change these numbers;
//! wall times are medians of several runs and depend on the machine. Full-stream
//! comparisons do not isolate the cost of a helper-thread hand-off.
//!
//! ```text
//! cargo run --release -p raster-h3-core --example profile_hotspots -- [--quick] [--json out.json]
//! ```
//!
//! Results print as Markdown tables. Under GitHub Actions they are also
//! appended to the job summary and emitted as `::notice` annotations.

#[path = "../tests/memory/alloc.rs"]
mod alloc;
#[path = "../tests/it/helpers.rs"]
mod helpers;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::mosaic::{MosaicReader, OverlapRule};
use serde_json::{json, Value};

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

/// Wall time, allocations and approximate peak extra live bytes for one closure call.
#[derive(Clone, Copy, Debug)]
struct Sample {
    ms: f64,
    allocs: u64,
    alloc_mib: f64,
    peak_mib: f64,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, Sample) {
    let start = Instant::now();
    let (out, region) = alloc::region(f);
    let ms = start.elapsed().as_secs_f64() * 1e3;
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let sample = Sample {
        ms,
        allocs: region.allocs,
        alloc_mib: mib(region.alloc_bytes),
        peak_mib: mib(region.peak_extra as u64),
    };
    (out, sample)
}

/// Run `f` `reps` times; return the output and measurements of the median wall-time run.
fn median_of<T>(reps: usize, mut f: impl FnMut() -> T) -> (T, Sample) {
    let mut runs: Vec<(T, Sample)> = (0..reps.max(1)).map(|_| measure(&mut f)).collect();
    runs.sort_by(|a, b| a.1.ms.total_cmp(&b.1.ms));
    let mid = runs.len() / 2;
    runs.swap_remove(mid)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PIXEL_DEG: f64 = 0.001;
const ORIGIN: (f64, f64) = (-100.0, 40.0);

/// An `n_side × n_side` grid of `tile_px` square f32 tiles whose neighbours
/// overlap by `overlap_px` pixels, written in row-major tile order.
struct Grid {
    _dir: tempfile::TempDir,
    paths: Vec<PathBuf>,
    n_side: usize,
    tile_px: u32,
    stride_deg: f64,
}

impl Grid {
    fn write(n_side: usize, tile_px: u32, overlap_px: u32) -> Grid {
        let dir = tempfile::tempdir().expect("temp dir");
        let stride_deg = f64::from(tile_px - overlap_px) * PIXEL_DEG;
        let mut paths = Vec::with_capacity(n_side * n_side);
        for j in 0..n_side {
            for i in 0..n_side {
                let path = dir.path().join(format!("tile_{j:03}_{i:03}.tif"));
                helpers::TestGeoTiffBuilder::new(tile_px, tile_px)
                    .origin(
                        ORIGIN.0 + i as f64 * stride_deg,
                        ORIGIN.1 - j as f64 * stride_deg,
                    )
                    .pixel_size(PIXEL_DEG)
                    .write_fn(&path, |c, r| {
                        ((c * 31 + r * 17 + (i + j) as u32) % 97) as f32
                    })
                    .expect("write tile");
                paths.push(path);
            }
        }
        Grid {
            _dir: dir,
            paths,
            n_side,
            tile_px,
            stride_deg,
        }
    }

    /// Sample each tile equally at pixel centres across its entire footprint.
    fn ownership_points(&self, count: usize) -> Vec<(usize, f64, f64)> {
        let mut rng = Rng(0x5EED);
        (0..count)
            .map(|i| {
                let tile = i % self.paths.len();
                let col = (rng.next_f64() * f64::from(self.tile_px)).floor();
                let row = (rng.next_f64() * f64::from(self.tile_px)).floor();
                let lon = ORIGIN.0
                    + (tile % self.n_side) as f64 * self.stride_deg
                    + (col + 0.5) * PIXEL_DEG;
                let lat = ORIGIN.1
                    - (tile / self.n_side) as f64 * self.stride_deg
                    - (row + 0.5) * PIXEL_DEG;
                (tile, lon, lat)
            })
            .collect()
    }
}

/// Deterministic uniform values in `[0, 1)` (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn drain(streamer: &mut MultiScanHorizonStreamer) -> usize {
    let mut cells = 0;
    loop {
        let batch = streamer.fetch_next_batch(4096).expect("stream batch");
        if batch.is_empty() {
            return cells;
        }
        cells += batch.len();
    }
}

// ---------------------------------------------------------------------------
// Experiment 1: mosaic planning and ownership scaling
// ---------------------------------------------------------------------------

fn mosaic_scaling(sides: &[usize], reps: usize, report: &mut Report) {
    const TILE_PX: u32 = 64;
    const OVERLAP_PX: u32 = 4;
    const POINTS: usize = 200_000;

    let mut table = String::from(
        "| tiles | chunks | overlap chunks | open files ms | open + plan ms | plan ms (est.) | open + plan allocs | Cutline ns/owner call | First ns/owner call |\n\
         |---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    let mut rows = Vec::new();

    for &side in sides {
        let grid = Grid::write(side, TILE_PX, OVERLAP_PX);
        let tiles = side * side;

        let (_, files) = median_of(reps, || {
            let readers: Vec<GeoTiffStreamReader> = grid
                .paths
                .iter()
                .map(|p| GeoTiffStreamReader::open(p).expect("open tile"))
                .collect();
            readers.len()
        });
        let (plan, opened) = median_of(reps, || {
            MosaicReader::open(&grid.paths, None, None, OverlapRule::Cutline).expect("open mosaic")
        });
        let chunks = plan.chunk_refs.len();
        let overlaps = plan.chunk_refs.iter().filter(|c| c.has_overlap).count();

        let points = grid.ownership_points(POINTS);
        let owner_ns = |mosaic: &MosaicReader| -> f64 {
            let start = Instant::now();
            let owned = std::hint::black_box(
                points
                    .iter()
                    .filter(|&&(t, lon, lat)| mosaic.is_point_owned_by(t, lon, lat))
                    .count(),
            );
            let ns = start.elapsed().as_nanos() as f64 / POINTS as f64;
            assert!(
                owned > 0 && owned <= POINTS,
                "every point lies in some tile"
            );
            ns
        };
        let cutline_ns = owner_ns(&plan);
        let first = MosaicReader::open(&grid.paths, None, None, OverlapRule::First).expect("open");
        let first_ns = owner_ns(&first);

        let plan_ms = (opened.ms - files.ms).max(0.0);
        let _ = writeln!(
            table,
            "| {tiles} | {chunks} | {overlaps} | {:.1} | {:.1} | {plan_ms:.1} | {} | {cutline_ns:.0} | {first_ns:.0} |",
            files.ms, opened.ms, opened.allocs
        );
        rows.push(json!({
            "tiles": tiles, "chunks": chunks, "overlap_chunks": overlaps,
            "open_files_ms": files.ms, "open_and_plan_ms": opened.ms, "plan_ms_est": plan_ms,
            "open_and_plan_allocs": opened.allocs, "open_and_plan_peak_mib": opened.peak_mib,
            "cutline_owner_ns": cutline_ns, "first_owner_ns": first_ns,
        }));
    }
    report.section(
        "Mosaic planning and ownership scaling",
        &format!(
            "{TILE_PX}×{TILE_PX} px tiles overlapping by {OVERLAP_PX} px. `plan ms (est.)` = mosaic open minus opening the files alone; allocations include both opening and planning. Owner calls are `is_point_owned_by` at {POINTS} pixel centres sampled equally from every tile's full footprint, including non-overlap samples. Overlapping pixels can be sampled from either tile. Peak bytes are approximate, scheduling-dependent allocator accounting, not RSS."
        ),
        table,
        json!(rows),
    );
}

// ---------------------------------------------------------------------------
// Experiment 2: end-to-end mosaic streaming by overlap rule
// ---------------------------------------------------------------------------

fn mosaic_streaming(sides: &[usize], reps: usize, report: &mut Report) {
    const TILE_PX: u32 = 128;
    const OVERLAP_PX: u32 = 8;

    let mut table = String::from(
        "| tiles | Mpx | rule | wall ms | Mpx/s | kernel ms | allocs | peak MiB | cells |\n\
         |---:|---:|:---|---:|---:|---:|---:|---:|---:|\n",
    );
    let mut rows = Vec::new();
    for &side in sides {
        let grid = Grid::write(side, TILE_PX, OVERLAP_PX);
        let mpx = (side * side) as f64 * f64::from(TILE_PX * TILE_PX) / 1e6;
        for (name, rule) in [
            ("Average", OverlapRule::Average),
            ("First", OverlapRule::First),
            ("Cutline", OverlapRule::Cutline),
        ] {
            let mut config = MultiResolutionConfig::single(9);
            config.overlap_rule = rule;
            let ((cells, kernel_ms), s) = median_of(reps, || {
                let mosaic =
                    Arc::new(MosaicReader::open(&grid.paths, None, None, rule).expect("open"));
                let mut streamer =
                    MultiScanHorizonStreamer::new_mosaic(mosaic, &config).expect("streamer");
                let cells = drain(&mut streamer);
                (cells, streamer.metrics().kernel_wall_ns as f64 / 1e6)
            });
            let _ = writeln!(
                table,
                "| {} | {mpx:.1} | {name} | {:.1} | {:.1} | {kernel_ms:.1} | {} | {:.1} | {cells} |",
                side * side,
                s.ms,
                mpx / (s.ms / 1e3),
                s.allocs,
                s.peak_mib
            );
            rows.push(json!({
                "tiles": side * side, "mpx": mpx, "rule": name, "wall_ms": s.ms,
                "kernel_ms": kernel_ms, "allocs": s.allocs, "alloc_mib": s.alloc_mib,
                "peak_mib": s.peak_mib, "cells": cells,
            }));
        }
    }
    report.section(
        "End-to-end mosaic streaming by overlap rule",
        &format!(
            "{TILE_PX}×{TILE_PX} px tiles overlapping by {OVERLAP_PX} px, resolution 9, includes opening and planning. `Average` skips ownership checks, so `First`/`Cutline` minus `Average` bounds what a tile index could save (rules also differ in which samples count)."
        ),
        table,
        json!(rows),
    );
}

// ---------------------------------------------------------------------------
// Experiment 3: nested Rayon wave dispatch
// ---------------------------------------------------------------------------

fn rayon_nesting(side_px: u32, budgets_kib: &[usize], reps: usize, report: &mut Report) {
    const WORKERS: usize = 4;
    let (_file, path) = helpers::TestGeoTiffBuilder::new(side_px, side_px)
        .origin(ORIGIN.0, ORIGIN.1)
        .pixel_size(PIXEL_DEG / 4.0)
        .create_f32_tempfile(|c, r| ((c * 13 + r * 7) % 101) as f32);
    let outer = rayon::ThreadPoolBuilder::new()
        .num_threads(WORKERS)
        .build()
        .expect("outer pool");

    // Isolated spawn + join baseline, not the cost of dispatching a real wave.
    let spawns = 2000;
    let (_, spawn) = measure(|| {
        for _ in 0..spawns {
            std::thread::scope(|s| s.spawn(|| std::hint::black_box(1)).join().unwrap());
        }
    });
    let spawn_us = spawn.ms * 1e3 / f64::from(spawns);

    let mut table = String::from(
        "| budget KiB | caller | pool | wall ms | waves | bridged | serial | jobs | kernel ms | allocs |\n\
         |---:|:---|:---|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    let mut rows = Vec::new();
    for &budget_kib in budgets_kib {
        for (caller, nested, dedicated) in [
            ("main thread", false, true),
            ("Rayon worker", true, true),
            ("main thread", false, false),
            ("Rayon worker", true, false),
        ] {
            let mut config = MultiResolutionConfig::single(9);
            config.aggregation_budget_bytes = budget_kib * 1024;
            config.aggregation_workers = dedicated.then_some(WORKERS);
            let run = || -> Result<[u64; 5], String> {
                let mosaic = Arc::new(
                    MosaicReader::open(
                        std::slice::from_ref(&path),
                        None,
                        None,
                        OverlapRule::Cutline,
                    )
                    .map_err(|e| e.to_string())?,
                );
                let mut streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)
                    .map_err(|e| e.to_string())?;
                drain(&mut streamer);
                let m = streamer.metrics();
                Ok([
                    m.waves,
                    m.bridged_waves,
                    m.serial_waves,
                    m.jobs,
                    m.kernel_wall_ns,
                ])
            };
            let (result, s) = median_of(reps, || if nested { outer.install(run) } else { run() });
            let pool = if dedicated { "dedicated" } else { "ambient" };
            let [waves, bridged, serial, jobs, kernel_ns] = match result {
                Ok(v) => v,
                Err(e) => {
                    let _ = writeln!(
                        table,
                        "| {budget_kib} | {caller} | {pool} | error: {e} |||||||"
                    );
                    continue;
                }
            };
            let _ = writeln!(
                table,
                "| {budget_kib} | {caller} | {pool} | {:.1} | {waves} | {bridged} | {serial} | {jobs} | {:.1} | {} |",
                s.ms,
                kernel_ns as f64 / 1e6,
                s.allocs
            );
            rows.push(json!({
                "budget_kib": budget_kib, "caller": caller, "pool": pool, "wall_ms": s.ms,
                "waves": waves, "bridged_waves": bridged, "serial_waves": serial, "jobs": jobs,
                "kernel_ms": kernel_ns as f64 / 1e6, "allocs": s.allocs, "peak_mib": s.peak_mib,
            }));
        }
    }
    report.section(
        "Nested Rayon wave dispatch",
        &format!(
            "{side_px}×{side_px} px raster, resolution 9, {WORKERS} aggregation workers when dedicated, called directly or from a {WORKERS}-thread outer Rayon pool. Smaller budgets mean smaller windows and more waves. Bare `thread::scope` spawn + join on this machine: {spawn_us:.1} µs (isolated baseline only). Full-stream wall-time differences include opening, decoding and scheduling, not just hand-off overhead. `ambient` from a Rayon worker runs every wave serially."
        ),
        table,
        json!({ "spawn_join_us": spawn_us, "runs": rows }),
    );
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Report {
    markdown: String,
    json: serde_json::Map<String, Value>,
}

impl Report {
    fn section(&mut self, title: &str, note: &str, table: String, data: Value) {
        let body = format!("### {title}\n\n{note}\n\n{table}\n");
        print!("{body}");
        if std::env::var_os("GITHUB_ACTIONS").is_some() {
            // Annotations survive where raw logs may not be reachable.
            let encoded = format!("{note}\n\n{table}")
                .replace('%', "%25")
                .replace('\r', "%0D")
                .replace('\n', "%0A");
            println!("::notice title={title}::{encoded}");
        }
        self.markdown.push_str(&body);
        self.json.insert(title.to_string(), data);
    }

    fn finish(self, json_path: Option<&Path>) {
        if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(summary)
                .expect("step summary");
            let _ = writeln!(file, "## Hotspot profile\n\n{}", self.markdown);
        }
        if let Some(path) = json_path {
            let text = serde_json::to_string_pretty(&Value::Object(self.json)).expect("json");
            std::fs::write(path, text).expect("write json");
            eprintln!("wrote {}", path.display());
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    let json_path = args
        .iter()
        .position(|a| a == "--json")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    let (sides, stream_sides, reps): (&[usize], &[usize], usize) = if quick {
        (&[1, 4, 8], &[2, 4], 1)
    } else {
        (&[1, 4, 8, 16, 32], &[2, 8, 16], 3)
    };
    let (rayon_px, budgets): (u32, &[usize]) = if quick {
        (512, &[256, 4096])
    } else {
        (2048, &[256, 2048, 16384])
    };

    let mut report = Report::default();
    mosaic_scaling(sides, reps, &mut report);
    mosaic_streaming(stream_sides, reps, &mut report);
    rayon_nesting(rayon_px, budgets, reps.max(3), &mut report);
    report.finish(json_path.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn ownership_samples_cover_both_tiles_in_overlaps() {
        let grid = Grid::write(2, 4, 2);
        let points = grid.ownership_points(4096);
        let mut per_tile = [0; 4];
        let mut pixels = BTreeMap::<(i32, i32), BTreeSet<usize>>::new();
        for &(tile, lon, lat) in &points {
            per_tile[tile] += 1;
            let col = ((lon - ORIGIN.0) / PIXEL_DEG).floor() as i32;
            let row = ((ORIGIN.1 - lat) / PIXEL_DEG).floor() as i32;
            let tile_col = (tile % 2) as i32 * 2;
            let tile_row = (tile / 2) as i32 * 2;
            assert!((tile_col..tile_col + 4).contains(&col));
            assert!((tile_row..tile_row + 4).contains(&row));
            pixels.entry((col, row)).or_default().insert(tile);
        }
        assert_eq!(per_tile, [1024; 4]);
        assert_eq!(pixels.len(), 36);
        assert_eq!(pixels[&(2, 0)], BTreeSet::from([0, 1]));
        assert_eq!(pixels[&(0, 2)], BTreeSet::from([0, 2]));
        assert_eq!(pixels[&(2, 2)], BTreeSet::from([0, 1, 2, 3]));

        let first = MosaicReader::open(&grid.paths, None, None, OverlapRule::First).unwrap();
        let lon = ORIGIN.0 + 2.5 * PIXEL_DEG;
        let lat = ORIGIN.1 - 0.5 * PIXEL_DEG;
        for tile in [0, 1] {
            assert!(points.iter().any(|&(t, x, y)| {
                t == tile && (x - lon).abs() < 1e-10 && (y - lat).abs() < 1e-10
            }));
        }
        assert!(first.is_point_owned_by(0, lon, lat));
        assert!(!first.is_point_owned_by(1, lon, lat));
    }
}
