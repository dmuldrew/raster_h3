//! Tests for:
//! 1. Sparse COG chunks with non-zero NoData values (verifying they are filled with NoData and skipped).
//! 2. Memory-budgeted aggregation triggering disk spilling (`spill_run_count() > 0`) and external merge sort.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use tempfile::NamedTempFile;
use tiff::decoder::DecodingResult;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

use raster_h3::aggregator::multi_horizon::{
    continuous_streamer::ContinuousKernel, MultiHorizonStreamer, MultiResolutionConfig,
    MultiScanHorizonStreamer,
};
use raster_h3::h3::{LatLng, Resolution};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

#[test]
fn test_sparse_chunk_filled_with_nonzero_nodata() {
    // Create a 2-strip 16x16 U16 TIFF with NoData = 32767
    let file = NamedTempFile::new().unwrap();
    let width = 16u32;
    let height = 16u32;
    let rows_per_strip = 8u32; // 2 strips: strip 0 (rows 0..8), strip 1 (rows 8..16)

    // Strip 0 has valid data (values = 100). Strip 1 will be zeroed out in offset table.
    let data: Vec<u16> = (0..width * height)
        .map(|idx| if idx < (width * rows_per_strip) { 100 } else { 200 })
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
        image
            .encoder()
            .write_tag(Tag::GdalNodata, "32767")
            .unwrap();
        image.write_data(&data).unwrap();
    }

    // Now modify the TIFF file so strip 1 is sparse (offset = 0, byte_count = 0)
    let mut bytes = std::fs::read(file.path()).unwrap();

    let reader_initial = GeoTiffStreamReader::open(file.path()).unwrap();
    assert_eq!(reader_initial.metadata.nodata, Some(32767.0));
    let chunk_info = reader_initial.chunk_info.as_ref().unwrap();
    let strip0_offset = chunk_info.chunk_offsets[0];
    let strip1_offset = chunk_info.chunk_offsets[1];

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
            assert_eq!(val, 32767, "Sparse chunk should be filled with NoData 32767");
        }
    } else {
        panic!("Expected U16");
    }

    // Aggregate with MultiScanHorizonStreamer: only strip 0 pixels should be aggregated
    let mut config = MultiResolutionConfig::single(10);
    config.custom_crs = Some("EPSG:4326".into());
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_count = 0.0;
    while !streamer.is_finished() {
        for record in streamer.fetch_next_batch(64).unwrap() {
            total_count += record.accumulator.count;
        }
    }

    // Strip 0 has 16 * 8 = 128 pixels. Strip 1 was sparse nodata and must be skipped entirely.
    assert_eq!(
        total_count, 128.0,
        "Only valid pixels in strip 0 should be aggregated, sparse strip 1 must be skipped"
    );
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

    // Verify bitwise exact equality with ground-truth reference
    assert_eq!(actual.len(), expected.len(), "Total cell count must match exactly");
    for (key, (exp_count, exp_sum)) in &expected {
        let (act_count, act_sum) = actual.get(key).expect("Missing cell key");
        assert_eq!(act_count, exp_count, "Pixel count mismatch on cell {:x}", key);
        assert!(
            (act_sum - exp_sum).abs() < 1e-4,
            "Sum mismatch on cell {:x}: act={} vs exp={}",
            key,
            act_sum,
            exp_sum
        );
    }
}
