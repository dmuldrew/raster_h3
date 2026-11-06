//! A/B harness for exact row indexing (`h3o::LatLngIndexer::index_row`).
//!
//! Streams WGS84, Web Mercator and Albers rasters (north-up, and rotated by
//! [`ROTATION`] radians through the geotransform) and prints the median wall
//! time per case, plus a fingerprint of every output record (resolution, cell,
//! count, sum, min, max). Pixel values are small integers, so the sums are
//! exact whatever the merge order and fingerprints must match bit for bit.
//!
//! ```text
//! RASTER_H3_EXACT_ROWS=0 cargo run --release -p raster-h3-core --example bench_exact_rows -- off.txt
//! RASTER_H3_EXACT_ROWS=1 cargo run --release -p raster-h3-core --example bench_exact_rows -- on.txt
//! diff off.txt on.txt
//! ```
//!
//! `REPS` sets the positive repetitions per case (default 3) and `CASES` keeps only
//! the cases whose label (e.g. `albers5070 30m rotated res [8]`) contains it.
//! `PROFILE=1` prints where each continuous run's wall time went.

#[path = "../tests/it/helpers.rs"]
mod helpers;

use helpers::TestGeoTiffBuilder;
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

fn value(col: u32, row: u32) -> f32 {
    if (col + row * 31).is_multiple_of(97) {
        f32::NAN
    } else {
        (1 + (col / 7 + row / 5) % 9) as f32
    }
}

/// Angle of the rotated cases (about 17°).
const ROTATION: f64 = 0.3;

fn open(path: &Path, rotated: bool) -> GeoTiffStreamReader {
    let mut reader = GeoTiffStreamReader::open(path).unwrap();
    if rotated {
        let gt = &mut reader.metadata.geotransform;
        let size = gt.a;
        let (sin, cos) = ROTATION.sin_cos();
        (gt.a, gt.b, gt.d, gt.e) = (size * cos, size * sin, size * sin, -size * cos);
    }
    reader
}

fn continuous(path: &Path, res: &[u8], rotated: bool, out: &mut String) -> f64 {
    let reader = open(path, rotated);
    let config = MultiResolutionConfig::new(res.to_vec());
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let start = Instant::now();
    let mut rows = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(4096).unwrap();
        if batch.is_empty() {
            break;
        }
        for r in batch {
            let a = &r.accumulator;
            rows.push((r.resolution, r.h3_index, a.count, a.sum, a.min, a.max));
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    if std::env::var_os("PROFILE").is_some() {
        let m = streamer.metrics();
        let ms = |ns: u64| ns as f64 * 1e-6;
        eprintln!(
            "  wall {:.0} ms: kernel {:.0} ms (worker cpu {:.0} ms), merge {:.0} ms, \
             prefetch wait {:.0} ms; {} waves, {} jobs",
            elapsed * 1e3,
            ms(m.kernel_wall_ns),
            ms(m.worker_ns),
            ms(m.merge_ns),
            ms(m.prefetch_wait_ns),
            m.waves,
            m.jobs
        );
    }
    rows.sort_by_key(|r| (r.0, r.1));
    for (res, cell, count, sum, min, max) in rows {
        writeln!(out, "{res} {cell:x} {count} {sum} {min} {max}").unwrap();
    }
    elapsed
}

fn categorical(path: &Path, res: &[u8], rotated: bool, out: &mut String) -> f64 {
    let reader = open(path, rotated);
    let config = MultiResolutionConfig::new(res.to_vec());
    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
    let start = Instant::now();
    let mut rows = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(4096).unwrap();
        if batch.is_empty() {
            break;
        }
        for r in batch {
            rows.push((
                r.resolution,
                r.h3_index,
                format!("{:?}", r.accumulator.total_count),
            ));
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    rows.sort();
    for (res, cell, total) in rows {
        writeln!(out, "cat {res} {cell:x} {total}").unwrap();
    }
    elapsed
}

fn parse_repetitions(value: &str) -> Result<usize, String> {
    value
        .parse::<std::num::NonZeroUsize>()
        .map(std::num::NonZeroUsize::get)
        .map_err(|_| format!("REPS must be a positive integer, got {value:?}"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "fingerprint.txt".into());
    let reps = match std::env::var("REPS") {
        Ok(value) => parse_repetitions(&value)?,
        Err(std::env::VarError::NotPresent) => 3,
        Err(error) => return Err(error.into()),
    };
    // Only run cases whose label ("name res [..]") contains this.
    let filter = std::env::var("CASES").unwrap_or_default();
    let mode = std::env::var("RASTER_H3_EXACT_ROWS").unwrap_or_else(|_| "1".into());
    let size = 3000;

    let wgs84 = TestGeoTiffBuilder::new(size, size)
        .origin(-122.5, 37.85)
        .epsg(4326)
        .pixel_size(0.00025) // ~27 m
        .create_f32_tempfile(value);
    let mercator = TestGeoTiffBuilder::new(size, size)
        .origin(-11_700_000.0, 4_830_000.0) // Denver
        .epsg(3857)
        .pixel_size(30.0)
        .create_f32_tempfile(value);
    let albers = TestGeoTiffBuilder::new(size, size)
        .origin(-300_000.0, 2_000_000.0) // central CONUS, EPSG:5070 metres
        .epsg(5070)
        .pixel_size(30.0)
        .create_f32_tempfile(value);
    let albers_classes = TestGeoTiffBuilder::new(size, size)
        .origin(-300_000.0, 2_000_000.0)
        .epsg(5070)
        .pixel_size(30.0)
        .create_tempfile(|col, row| ((col / 11 + row / 13) % 6) as u8);
    let classes = TestGeoTiffBuilder::new(size, size)
        .origin(-122.5, 37.85)
        .epsg(4326)
        .pixel_size(0.00025)
        .create_tempfile(|col, row| ((col / 11 + row / 13) % 6) as u8);

    let mut fingerprint = String::new();
    println!("mode RASTER_H3_EXACT_ROWS={mode}, {size}x{size} px, median of {reps}");
    let cases: [(&str, &Path, &[u8], bool, bool); 20] = [
        ("wgs84 27m", &wgs84.1, &[6], false, false),
        ("wgs84 27m", &wgs84.1, &[8], false, false),
        ("wgs84 27m", &wgs84.1, &[9], false, false),
        ("wgs84 27m", &wgs84.1, &[10], false, false),
        ("wgs84 27m", &wgs84.1, &[6, 8, 10], false, false),
        ("mercator 30m", &mercator.1, &[8], false, false),
        ("mercator 30m", &mercator.1, &[6, 8, 10], false, false),
        ("wgs84 27m categorical", &classes.1, &[8], true, false),
        (
            "wgs84 27m categorical",
            &classes.1,
            &[6, 8, 10],
            true,
            false,
        ),
        ("albers5070 30m", &albers.1, &[8], false, false),
        ("albers5070 30m", &albers.1, &[9], false, false),
        ("albers5070 30m", &albers.1, &[10], false, false),
        ("albers5070 30m", &albers.1, &[6, 8, 10], false, false),
        (
            "albers5070 30m categorical",
            &albers_classes.1,
            &[6, 8, 10],
            true,
            false,
        ),
        ("wgs84 27m rotated", &wgs84.1, &[8], false, true),
        ("wgs84 27m rotated", &wgs84.1, &[10], false, true),
        ("mercator 30m rotated", &mercator.1, &[8], false, true),
        ("albers5070 30m rotated", &albers.1, &[8], false, true),
        ("albers5070 30m rotated", &albers.1, &[10], false, true),
        (
            "albers5070 30m rotated",
            &albers.1,
            &[6, 8, 10],
            false,
            true,
        ),
    ];
    for (name, path, res, is_cat, rotated) in cases {
        if !format!("{name} res {res:?}").contains(filter.as_str()) {
            continue;
        }
        let mut times = Vec::new();
        let mut out = String::new();
        for _ in 0..reps {
            out.clear();
            times.push(if is_cat {
                categorical(path, res, rotated, &mut out)
            } else {
                continuous(path, res, rotated, &mut out)
            });
        }
        times.sort_by(f64::total_cmp);
        let lines = out.lines().count();
        println!(
            "{name:<26} res {res:?}: {:>8.3} s  ({lines} records)",
            times[reps / 2]
        );
        writeln!(fingerprint, "# {name} {res:?}").unwrap();
        fingerprint.push_str(&out);
    }
    std::fs::write(&out_path, fingerprint).unwrap();
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn repetitions_must_be_positive() {
        for value in ["0", "-1", "", "invalid"] {
            assert!(super::parse_repetitions(value).is_err());
        }
        assert_eq!(super::parse_repetitions("1").unwrap(), 1);
        assert_eq!(super::parse_repetitions("3").unwrap(), 3);
    }
}
