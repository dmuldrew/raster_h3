//! Tests for:
//! 1. Sparse COG chunks with non-zero NoData values (verifying they are filled with NoData and skipped).
//! 2. Memory-budgeted aggregation triggering disk spilling (`spill_run_count() > 0`) and external merge sort.
//! 3. One resolution spilling while another stays in memory, with compaction,
//!    quantiles, categorical heap maps and a dedicated worker pool.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufWriter;
use tempfile::NamedTempFile;
use tiff::decoder::DecodingResult;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

use h3o::{LatLng, Resolution};
use raster_h3_core::aggregator::categorical::INLINE_CAPACITY;
use raster_h3_core::aggregator::multi_horizon::{
    continuous_streamer::ContinuousKernel, MultiCategoricalHorizonStreamer, MultiHorizonStreamer,
    MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;

use crate::helpers::TestGeoTiffBuilder;

#[test]
fn test_sparse_chunk_filled_with_nonzero_nodata() {
    // Create a 2-strip 16x16 U16 TIFF with NoData = 32767
    let file = NamedTempFile::new().unwrap();
    let width = 16u32;
    let height = 16u32;
    let rows_per_strip = 8u32; // 2 strips: strip 0 (rows 0..8), strip 1 (rows 8..16)

    // Strip 0 has valid data (values = 100). Strip 1 will be zeroed out in offset table.
    let data: Vec<u16> = (0..width * height)
        .map(|idx| {
            if idx < (width * rows_per_strip) {
                100
            } else {
                200
            }
        })
        .collect();

    {
        let tiff_file = File::create(file.path()).unwrap();
        let mut encoder = TiffEncoder::new(BufWriter::new(tiff_file)).unwrap();
        let mut image = encoder
            .new_image::<colortype::Gray16>(width, height)
            .unwrap();
        image.rows_per_strip(rows_per_strip).unwrap();
        let tiepoint = [0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0];
        let pixel_scale = [0.001f64, 0.001, 0.0];
        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &pixel_scale[..])
            .unwrap();
        image.encoder().write_tag(Tag::GdalNodata, "32767").unwrap();
        image.write_data(&data).unwrap();
    }

    // Now modify the TIFF file so strip 1 is sparse (offset = 0, byte_count = 0)
    let mut bytes = std::fs::read(file.path()).unwrap();

    // Scope the reader so its memory map is released before the file is rewritten;
    // Windows refuses to write to a file with a mapped section open.
    let (strip0_offset, strip1_offset) = {
        let reader_initial = GeoTiffStreamReader::open(file.path()).unwrap();
        assert_eq!(reader_initial.metadata.nodata, Some(32767.0));
        let chunk_info = reader_initial.chunk_info.as_ref().unwrap();
        (chunk_info.chunk_offsets[0], chunk_info.chunk_offsets[1])
    };

    let mut target_pair = [0u8; 8];
    target_pair[0..4].copy_from_slice(&(strip0_offset as u32).to_le_bytes());
    target_pair[4..8].copy_from_slice(&(strip1_offset as u32).to_le_bytes());

    let pos = bytes
        .windows(8)
        .position(|w| w == target_pair)
        .expect("StripOffsets pair must exist in TIFF file");
    // Zero out strip 1's offset
    bytes[pos + 4..pos + 8].copy_from_slice(&[0, 0, 0, 0]);
    std::fs::write(file.path(), &bytes).unwrap();

    // Reopen reader and decoder
    let reader = GeoTiffStreamReader::open(file.path()).unwrap();
    let mut decoder = reader.open_decoder().unwrap();

    // Chunk 0 is valid
    let (_b0, res0) = decoder.read_chunk(0).unwrap();
    if let DecodingResult::U16(v0) = res0 {
        assert_eq!(v0.len(), (width * rows_per_strip) as usize);
        assert_eq!(v0[0], 100);
    } else {
        panic!("Expected U16");
    }

    // Chunk 1 is sparse
    let (_b1, res1) = decoder.read_chunk(1).unwrap();
    if let DecodingResult::U16(v1) = res1 {
        assert_eq!(v1.len(), (width * rows_per_strip) as usize);
        // CRITICAL CHECK: Sparse chunk must be filled with NoData (32767), NOT 0!
        for val in v1 {
            assert_eq!(
                val, 32767,
                "Sparse chunk should be filled with NoData 32767"
            );
        }
    } else {
        panic!("Expected U16");
    }

    // Aggregate with MultiScanHorizonStreamer: only strip 0 pixels should be aggregated
    assert!(!reader.is_sparse_chunk(0));
    assert!(reader.is_sparse_chunk(1));
    for custom_nodata in [None, Some(-9999.0), Some(0.0), Some(100.0)] {
        let mut config = MultiResolutionConfig::single(10);
        config.custom_crs = Some("EPSG:4326".into());
        config.custom_nodata = custom_nodata;
        let mut streamer = MultiScanHorizonStreamer::new(reader.clone(), &config).unwrap();
        let mut total_count = 0.0;
        while !streamer.is_finished() {
            for record in streamer.fetch_next_batch(64).unwrap() {
                total_count += record.accumulator.count;
            }
        }
        let expected = if custom_nodata == Some(100.0) {
            0.0
        } else {
            128.0
        };
        assert_eq!(
            total_count, expected,
            "Sparse pixels must be absent with override {custom_nodata:?}"
        );
    }
}

#[test]
fn test_low_budget_triggers_spill_runs_and_matches_reference() {
    let file = NamedTempFile::new().unwrap();
    let width = 64u32;
    let height = 64u32; // 4096 pixels total
    let rows_per_strip = 8u32; // 8 strips

    // Create gradient data covering many cells
    let data: Vec<f32> = (0..width * height)
        .map(|idx| (idx as f32) * 0.1 + 1.0)
        .collect();

    {
        let tiff_file = File::create(file.path()).unwrap();
        let mut encoder = TiffEncoder::new(BufWriter::new(tiff_file)).unwrap();
        let mut image = encoder
            .new_image::<colortype::Gray32Float>(width, height)
            .unwrap();
        image.rows_per_strip(rows_per_strip).unwrap();
        // High resolution raster with wide coordinate extent to generate many distinct H3 cells
        let tiepoint = [0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0];
        let pixel_scale = [0.005f64, 0.005, 0.0];
        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
            .unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &pixel_scale[..])
            .unwrap();
        image.write_data(&data).unwrap();
    }

    let reader = GeoTiffStreamReader::open(file.path()).unwrap();
    let gt = reader.metadata.geotransform;

    // Ground truth in memory
    let mut expected = HashMap::<u64, (f64, f64)>::new();
    for r in 0..height {
        for c in 0..width {
            let (lon, lat) = gt.pixel_center_to_coord(c as usize, r as usize);
            let val = data[(r * width + c) as usize] as f64;
            let key = u64::from(LatLng::new(lat, lon).unwrap().to_cell(Resolution::Nine));
            let entry = expected.entry(key).or_default();
            entry.0 += 1.0;
            entry.1 += val;
        }
    }
    assert!(
        expected.len() > 500,
        "Test requires many distinct cells (got {}) to exceed shard budget",
        expected.len()
    );

    // Minimum budget of 64 KiB forces active map capacity to be tiny (~16 KiB total map budget)
    let mut config = MultiResolutionConfig::single(9);
    config.custom_crs = Some("EPSG:4326".into());
    config.aggregation_budget_bytes = 64 * 1024;
    // Compaction disabled or enabled both test merge
    let kernel = ContinuousKernel {
        band: 1,
        spectral_formula: None,
        min_count: None,
        min_mean: None,
        max_mean: None,
        track_quantiles: false,
    };

    let mut stream = MultiHorizonStreamer::new(reader, &config, kernel).unwrap();
    let mut actual = HashMap::<u64, (f64, f64)>::new();

    while !stream.is_finished() {
        for record in stream.fetch_next_batch(128).unwrap() {
            assert!(
                actual
                    .insert(
                        record.h3_index,
                        (record.accumulator.count, record.accumulator.sum)
                    )
                    .is_none(),
                "Duplicate cell emitted: {:x}",
                record.h3_index
            );
        }
    }

    // CRITICAL INVARIANT: Verify that disk spilling actually took place!
    assert!(
        stream.spill_run_count() > 0,
        "Disk spilling MUST occur when active map capacity exceeds low budget (spills={})",
        stream.spill_run_count()
    );

    // Verify exact counts and sums within the stated floating-point tolerance
    assert_eq!(
        actual.len(),
        expected.len(),
        "Total cell count must match exactly"
    );
    for (key, (exp_count, exp_sum)) in &expected {
        let (act_count, act_sum) = actual.get(key).expect("Missing cell key");
        assert_eq!(
            act_count, exp_count,
            "Pixel count mismatch on cell {:x}",
            key
        );
        assert!(
            (act_sum - exp_sum).abs() < 1e-4,
            "Sum mismatch on cell {:x}: act={} vs exp={}",
            key,
            act_sum,
            exp_sum
        );
    }
}

/// Two resolutions far apart, so a low budget spills the fine one to disk
/// (final cells come from the external merge at EOF) while the coarse one
/// stays in memory and keeps evicting early; both feed one compactor.
const MIXED_RES: [u8; 2] = [6, 8];
const MIXED_BUDGET: usize = 8 << 20;
/// Categorical states (heap class maps) are larger, so the same split needs
/// a larger budget.
const MIXED_CATEGORICAL_BUDGET: usize = 16 << 20;
const UNBOUNDED: usize = 1 << 30;

/// The coarse resolution alone, at its per-resolution share of a mixed
/// run's budget (the budget is split evenly between resolutions).
fn coarse_only_config(mixed_budget: usize) -> MultiResolutionConfig {
    let mut config = mixed_spill_config(mixed_budget / MIXED_RES.len());
    config.resolutions = vec![MIXED_RES[0]];
    config
}

fn mixed_spill_config(budget: usize) -> MultiResolutionConfig {
    let mut config = MultiResolutionConfig::new(MIXED_RES.to_vec());
    config.compact_h3_children = true;
    config.aggregation_workers = Some(4);
    config.aggregation_budget_bytes = budget;
    config
}

/// 300 x 1200 WGS84 strips at 0.002 deg: about 17k res-8 cells and a few
/// hundred res-6 cells, enough for complete sibling groups at both.
fn mixed_spill_builder() -> TestGeoTiffBuilder {
    TestGeoTiffBuilder::new(300, 1200)
        .origin(-120.0, 45.0)
        .pixel_size(0.002)
}

type ContinuousCells = HashMap<(u8, u64), (f64, f64, f64, f64, f64)>;

fn stream_continuous(
    path: &std::path::Path,
    config: &MultiResolutionConfig,
) -> (ContinuousCells, u64) {
    let mut stream =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(path).unwrap(), config).unwrap();
    let mut cells = HashMap::new();
    loop {
        let batch = stream.fetch_next_batch(97).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = &rec.accumulator;
            let row = (acc.count, acc.sum, acc.min, acc.max, acc.quantile(0.5));
            let previous = cells.insert((rec.resolution, rec.h3_index), row);
            assert!(previous.is_none(), "duplicate record {:x}", rec.h3_index);
        }
    }
    assert!(stream.is_finished());
    (cells, stream.spill_run_count())
}

#[test]
fn mixed_resolution_spill_with_compaction_and_quantiles_matches_in_memory_reference() {
    let (_file, path) =
        mixed_spill_builder().create_f32_tempfile(|c, r| 1.0 + ((c / 4 + r / 6) % 5) as f32);

    let mut reference_config = mixed_spill_config(UNBOUNDED);
    reference_config.quantiles = QuantileTarget::parse_list("p50").unwrap();
    let (reference, reference_spills) = stream_continuous(&path, &reference_config);
    assert_eq!(reference_spills, 0, "the reference must stay in memory");
    for res in MIXED_RES {
        assert!(
            reference.keys().any(|&(r, _)| r == res - 1),
            "res {res} groups should compact into parents"
        );
    }

    // The coarse resolution never reaches the spill threshold on its own.
    let mut coarse_only = coarse_only_config(MIXED_BUDGET);
    coarse_only.quantiles = reference_config.quantiles.clone();
    assert_eq!(stream_continuous(&path, &coarse_only).1, 0);

    let mut config = mixed_spill_config(MIXED_BUDGET);
    config.quantiles = reference_config.quantiles.clone();
    let (spilled, spills) = stream_continuous(&path, &config);
    assert!(
        spills > 1,
        "the fine resolution must spill repeatedly ({spills})"
    );

    assert_eq!(spilled.len(), reference.len());
    for (key, &(count, sum, min, max, p50)) in &reference {
        let &(c, s, lo, hi, q) = spilled
            .get(key)
            .unwrap_or_else(|| panic!("missing {key:?}"));
        assert_eq!((c, lo, hi), (count, min, max), "{key:?}");
        assert!(
            (s - sum).abs() <= 1e-9 * sum.abs().max(1.0),
            "{key:?}: {s} vs {sum}"
        );
        assert_eq!(q, p50, "{key:?}: sketches merge exactly");
    }
}

type CategoricalCells = HashMap<(u8, u64), BTreeMap<i64, f64>>;

fn stream_categorical(
    path: &std::path::Path,
    config: &MultiResolutionConfig,
) -> (CategoricalCells, u64) {
    let mut stream =
        MultiCategoricalHorizonStreamer::new(GeoTiffStreamReader::open(path).unwrap(), config)
            .unwrap();
    let mut cells = HashMap::new();
    loop {
        let batch = stream.fetch_next_batch(97).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let mut classes = BTreeMap::new();
            rec.accumulator.for_each_class(|class, count| {
                classes.insert(class, count);
            });
            let previous = cells.insert((rec.resolution, rec.h3_index), classes);
            assert!(previous.is_none(), "duplicate record {:x}", rec.h3_index);
        }
    }
    (cells, stream.spill_run_count())
}

#[test]
fn mixed_resolution_categorical_spill_with_compaction_matches_in_memory_reference() {
    // 40 classes in small blocks: coarse cells overflow the inline class
    // array into heap maps, which must round-trip through spill runs.
    let (_file, path) =
        mixed_spill_builder().create_tempfile(|c, r| (1 + (c / 3 * 7 + r / 3 * 13) % 40) as u8);

    let (reference, reference_spills) = stream_categorical(&path, &mixed_spill_config(UNBOUNDED));
    assert_eq!(reference_spills, 0);
    assert!(
        reference
            .values()
            .any(|classes| classes.len() > INLINE_CAPACITY),
        "some cells must use heap class maps"
    );
    assert!(
        reference.keys().any(|&(r, _)| r == MIXED_RES[1] - 1),
        "fine sibling groups should compact into parents"
    );

    let coarse_only = coarse_only_config(MIXED_CATEGORICAL_BUDGET);
    assert_eq!(stream_categorical(&path, &coarse_only).1, 0);
    let config = mixed_spill_config(MIXED_CATEGORICAL_BUDGET);
    let (spilled, spills) = stream_categorical(&path, &config);
    assert!(
        spills > 1,
        "the fine resolution must spill repeatedly ({spills})"
    );
    assert_eq!(spilled, reference);
}
