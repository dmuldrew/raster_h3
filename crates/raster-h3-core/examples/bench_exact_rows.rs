//! A/B harness for exact row indexing (`h3o::LatLngIndexer::index_row`).
//!
//! Streams north-up WGS84 and Web Mercator rasters and prints the median wall
//! time per case, plus a fingerprint of every output record (resolution, cell,
//! count, sum, min, max). Pixel values are small integers, so the sums are
//! exact whatever the merge order and fingerprints must match bit for bit.
//!
//! ```text
//! RASTER_H3_EXACT_ROWS=0 cargo run --release -p raster-h3-core --example bench_exact_rows -- off.txt
//! RASTER_H3_EXACT_ROWS=1 cargo run --release -p raster-h3-core --example bench_exact_rows -- on.txt
//! diff off.txt on.txt
//! ```

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

fn continuous(path: &Path, res: &[u8], out: &mut String) -> f64 {
    let reader = GeoTiffStreamReader::open(path).unwrap();
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
    rows.sort_by_key(|r| (r.0, r.1));
    for (res, cell, count, sum, min, max) in rows {
        writeln!(out, "{res} {cell:x} {count} {sum} {min} {max}").unwrap();
    }
    elapsed
}

fn categorical(path: &Path, res: &[u8], out: &mut String) -> f64 {
    let reader = GeoTiffStreamReader::open(path).unwrap();
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

fn main() {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "fingerprint.txt".into());
    let reps: usize = std::env::var("REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
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
    let classes = TestGeoTiffBuilder::new(size, size)
        .origin(-122.5, 37.85)
        .epsg(4326)
        .pixel_size(0.00025)
        .create_tempfile(|col, row| ((col / 11 + row / 13) % 6) as u8);

    let mut fingerprint = String::new();
    println!("mode RASTER_H3_EXACT_ROWS={mode}, {size}x{size} px, median of {reps}");
    let cases: [(&str, &Path, &[u8], bool); 9] = [
        ("wgs84 27m", &wgs84.1, &[6], false),
        ("wgs84 27m", &wgs84.1, &[8], false),
        ("wgs84 27m", &wgs84.1, &[9], false),
        ("wgs84 27m", &wgs84.1, &[10], false),
        ("wgs84 27m", &wgs84.1, &[6, 8, 10], false),
        ("mercator 30m", &mercator.1, &[8], false),
        ("mercator 30m", &mercator.1, &[6, 8, 10], false),
        ("wgs84 27m categorical", &classes.1, &[8], true),
        ("wgs84 27m categorical", &classes.1, &[6, 8, 10], true),
    ];
    for (name, path, res, is_cat) in cases {
        let mut times = Vec::new();
        let mut out = String::new();
        for _ in 0..reps {
            out.clear();
            times.push(if is_cat {
                categorical(path, res, &mut out)
            } else {
                continuous(path, res, &mut out)
            });
        }
        times.sort_by(f64::total_cmp);
        let lines = out.lines().count();
        println!(
            "{name:<24} res {res:?}: {:>8.3} s  ({lines} records)",
            times[reps / 2]
        );
        writeln!(fingerprint, "# {name} {res:?}").unwrap();
        fingerprint.push_str(&out);
    }
    std::fs::write(&out_path, fingerprint).unwrap();
}
