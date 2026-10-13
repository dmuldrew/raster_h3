//! Decoded-chunk and compaction byte budgets bound memory without changing
//! results beyond what each budget documents.

use std::collections::HashMap;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

use h3o::CellIndex;
use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

const NODATA: f32 = -9999.0;
const WIDTH: u32 = 300;
const ROWS_PER_STRIP: u32 = 16;
const STRIP_BYTES: usize = (WIDTH * ROWS_PER_STRIP) as usize * 4;

/// North-up WGS84 Float32 strips with a nodata band so some sibling groups
/// are incomplete.
fn fixture() -> tempfile::NamedTempFile {
    let height = 1200u32;
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut encoder = TiffEncoder::new(std::fs::File::create(file.path()).unwrap()).unwrap();
    let mut image = encoder
        .new_image::<colortype::Gray32Float>(WIDTH, height)
        .unwrap();
    image.rows_per_strip(ROWS_PER_STRIP).unwrap();
    let enc = image.encoder();
    enc.write_tag(Tag::ModelPixelScaleTag, &[0.0005, 0.0005, 0.0][..])
        .unwrap();
    enc.write_tag(
        Tag::ModelTiepointTag,
        &[0.0, 0.0, 0.0, -120.0, 45.0, 0.0][..],
    )
    .unwrap();
    let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    enc.write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..])
        .unwrap();
    enc.write_tag(Tag::Unknown(42113), "-9999").unwrap();
    let data: Vec<f32> = (0..height)
        .flat_map(|r| {
            (0..WIDTH).map(move |c| {
                if (c + r / 3) % 97 < 9 {
                    NODATA
                } else {
                    1.0 + (c % 5) as f32
                }
            })
        })
        .collect();
    image.write_data(&data).unwrap();
    file
}

struct Run {
    cells: HashMap<(u8, u64), (f64, f64)>,
    peak_decoded_bytes: usize,
    peak_compaction_bytes: usize,
    forced_compaction_flushes: u64,
    spill_runs: u64,
}

fn run(path: &std::path::Path, config: &MultiResolutionConfig) -> Run {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiScanHorizonStreamer::new(reader, config).unwrap();
    let mut cells = HashMap::new();
    loop {
        let batch = streamer.fetch_next_batch(64).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let previous = cells.insert(
                (rec.resolution, rec.h3_index),
                (rec.accumulator.count, rec.accumulator.sum),
            );
            assert!(previous.is_none(), "duplicate record {:x}", rec.h3_index);
        }
    }
    let metrics = streamer.metrics();
    Run {
        cells,
        peak_decoded_bytes: metrics.peak_decoded_bytes,
        peak_compaction_bytes: metrics.peak_compaction_bytes,
        forced_compaction_flushes: metrics.forced_compaction_flushes,
        spill_runs: streamer.spill_run_count(),
    }
}

fn assert_same(expected: &HashMap<(u8, u64), (f64, f64)>, actual: &HashMap<(u8, u64), (f64, f64)>) {
    assert_eq!(expected.len(), actual.len());
    for (key, (count, sum)) in expected {
        let (c, s) = actual[key];
        assert_eq!(c, *count, "{key:?}");
        assert!((s - sum).abs() <= 1e-9 * sum.abs().max(1.0), "{key:?}");
    }
}

#[test]
fn decode_budget_bounds_held_chunks_without_changing_results() {
    let file = fixture();
    let mut config = MultiResolutionConfig::new(vec![8, 9]);
    config.decode_workers = 4;
    let reference = run(file.path(), &config);
    assert!(
        reference.peak_decoded_bytes > 3 * STRIP_BYTES,
        "the default budget should let several strips be held: {}",
        reference.peak_decoded_bytes
    );

    // Three strips at a time, and a budget below one strip, which admits
    // each strip alone. Both must finish with four decode workers.
    for budget in [3 * STRIP_BYTES, 1] {
        config.decode_budget_bytes = budget;
        let bounded = run(file.path(), &config);
        assert_same(&reference.cells, &bounded.cells);
        assert!(
            bounded.peak_decoded_bytes <= budget.max(STRIP_BYTES),
            "budget {budget}: peak {} bytes",
            bounded.peak_decoded_bytes
        );
    }
}

#[test]
fn compaction_limit_decomposes_groups_without_losing_or_duplicating_samples() {
    let file = fixture();
    let plain = run(file.path(), &MultiResolutionConfig::new(vec![8])).cells;

    let mut config = MultiResolutionConfig::new(vec![8]);
    config.compact_h3_children = true;
    let unbounded = run(file.path(), &config);
    // The limit is opt-in; by default compaction is never cut short.
    assert_eq!(unbounded.forced_compaction_flushes, 0);

    let limit = 16 * 1024;
    assert!(unbounded.peak_compaction_bytes > limit);
    config.compaction_budget_bytes = Some(limit);
    let bounded = run(file.path(), &config);
    assert_eq!(
        bounded.spill_runs, 0,
        "spilling would mask the compaction limit"
    );
    assert!(bounded.forced_compaction_flushes > 0);
    // The peak is sampled before enforcement, so it includes one push and
    // possibly one table growth beyond the limit.
    assert!(
        bounded.peak_compaction_bytes <= 2 * limit,
        "bounded peak {} bytes, limit {limit}",
        bounded.peak_compaction_bytes
    );

    // Every plain cell is represented exactly once: directly, or through a
    // parent record whose complete sibling group sums to it.
    let mut covered = HashMap::new();
    let mut parents = 0;
    for (&(res, cell), &(count, sum)) in &bounded.cells {
        if res == 8 {
            assert_eq!(plain.get(&(8, cell)), Some(&(count, sum)), "{cell:x}");
            *covered.entry(cell).or_insert(0) += 1;
            continue;
        }
        parents += 1;
        let parent = CellIndex::try_from(cell).unwrap();
        let children: Vec<u64> = parent
            .children(h3o::Resolution::Eight)
            .map(u64::from)
            .collect();
        let (mut c, mut s) = (0.0, 0.0);
        for child in &children {
            let (cc, cs) = plain[&(8, *child)];
            c += cc;
            s += cs;
            *covered.entry(*child).or_insert(0) += 1;
        }
        assert_eq!(count, c, "{cell:x}");
        assert!((sum - s).abs() <= 1e-9 * s.abs().max(1.0), "{cell:x}");
    }
    assert!(parents > 0, "some groups should still compact");
    assert_eq!(covered.len(), plain.len());
    assert!(covered.values().all(|&n| n == 1));
}
