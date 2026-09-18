//! Regression tests verifying rotated, sheared, and South-to-North raster grid correctness.
//!
//! Compares streaming results against exhaustive per-sample aggregation with zero early eviction.
//! Covers:
//! - b != 0, d == 0 (horizontal shear with row-constant latitude)
//! - b == 0, d != 0 (d-induced latitude variation across columns)
//! - b != 0, d != 0 (general affine rotation and shear)
//! - b == 0, d == 0, e > 0 (South-to-North traversal)
//!
//! Checks:
//! - Aggregate values (count, sum, min, max, variance)
//! - Output uniqueness (zero duplicate / prematurely evicted split cells)

mod helpers;

use h3o::{LatLng, Resolution};
use helpers::TestGeoTiffBuilder;
use raster_h3::aggregator::accumulator::H3Accumulator;
use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use std::collections::HashMap;

fn pattern_value(col: u32, row: u32) -> f32 {
    if (col + row * 16).is_multiple_of(13) {
        f32::NAN
    } else {
        (10.0 + (col as f32) * 1.5 - (row as f32) * 0.8).abs()
    }
}

#[test]
fn test_rotated_and_south_to_north_grids_match_exhaustive_reference() {
    let width = 32u32;
    let height = 32u32;

    // Test matrix covering all 4 affine edge cases:
    let cases = [
        // Name, b, d, e_sign
        ("shear_only_b_nonzero", 0.0003, 0.0, -1.0),
        ("latitude_tilt_d_nonzero", 0.0, 0.0002, -1.0),
        ("general_rotation_both_nonzero", 0.0002, 0.0002, -1.0),
        ("south_to_north_e_positive", 0.0, 0.0, 1.0),
    ];

    let (_temp_file, path) = TestGeoTiffBuilder::new(width, height)
        .origin(-122.45, 37.85)
        .epsg(4326)
        .pixel_size(0.0004)
        .create_f32_tempfile(pattern_value);

    for (name, b_val, d_val, e_sign) in cases {
        for pattern in [SamplingPattern::center(), SamplingPattern::rgss()] {
            let mut reader = GeoTiffStreamReader::open(&path).unwrap();
            reader.metadata.geotransform.b = b_val;
            reader.metadata.geotransform.d = d_val;
            reader.metadata.geotransform.e = reader.metadata.geotransform.e.abs() * e_sign;
            let gt = reader.metadata.geotransform;

            let res = Resolution::try_from(9).unwrap();

            // 1. Compute exhaustive per-sample reference with zero early eviction
            let mut expected_continuous: HashMap<u64, H3Accumulator> = HashMap::new();
            let mut expected_categorical: HashMap<u64, HashMap<i64, f64>> = HashMap::new();

            for r in 0..height {
                for c in 0..width {
                    let val = pattern_value(c, r);
                    if !val.is_finite() {
                        continue;
                    }
                    for sp in &pattern.points {
                        let (lon, lat) = gt.pixel_to_coord(c as f64 + sp.dx, r as f64 + sp.dy);
                        if let Ok(ll) = LatLng::new(lat, lon) {
                            let cell_u64: u64 = ll.to_cell(res).into();
                            expected_continuous
                                .entry(cell_u64)
                                .or_default()
                                .update_weighted(val as f64, sp.weight);
                            *expected_categorical
                                .entry(cell_u64)
                                .or_default()
                                .entry(val.round() as i64)
                                .or_default() += sp.weight;
                        }
                    }
                }
            }

            // 2. Stream through MultiScanHorizonStreamer with small batch sizes
            // Small batch size (7) forces multiple batch boundaries and eviction passes
            let mut config = MultiResolutionConfig::single(9);
            config.sampling = pattern.clone();

            let mut continuous_streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

            let mut streamed_continuous: HashMap<u64, H3Accumulator> = HashMap::new();
            let mut emitted_order: Vec<u64> = Vec::new();

            loop {
                let batch = continuous_streamer.fetch_next_batch(7).unwrap();
                if batch.is_empty() {
                    break;
                }
                for rec in batch {
                    // Check output uniqueness: each cell must be emitted at most once!
                    // A prematurely evicted cell would be recreated, producing duplicate emissions.
                    assert!(
                        streamed_continuous
                            .insert(rec.h3_index, rec.accumulator)
                            .is_none(),
                        "Duplicate cell emitted in {name} with pattern {:?}: {:x}",
                        pattern,
                        rec.h3_index
                    );
                    emitted_order.push(rec.h3_index);
                }
            }

            // Verify cell set equality
            assert_eq!(
                streamed_continuous.len(),
                expected_continuous.len(),
                "Cell count mismatch for {name}"
            );

            // Verify statistics conservation for every cell
            for (cell, exp_acc) in &expected_continuous {
                let act_acc = streamed_continuous.get(cell).unwrap_or_else(|| {
                    panic!("Missing cell {:x} in streamed output for {name}", cell)
                });
                assert!(
                    (act_acc.count - exp_acc.count).abs() < 1e-6,
                    "Count mismatch in {name} for cell {:x}: {} vs {}",
                    cell,
                    act_acc.count,
                    exp_acc.count
                );
                assert!(
                    (act_acc.sum - exp_acc.sum).abs() < 1e-4,
                    "Sum mismatch in {name} for cell {:x}: {} vs {}",
                    cell,
                    act_acc.sum,
                    exp_acc.sum
                );
                assert_eq!(act_acc.min, exp_acc.min, "Min mismatch in {name}");
                assert_eq!(act_acc.max, exp_acc.max, "Max mismatch in {name}");
            }

            // 3. Stream through MultiCategoricalHorizonStreamer
            let mut cat_reader = GeoTiffStreamReader::open(&path).unwrap();
            cat_reader.metadata.geotransform = gt;
            let mut cat_streamer =
                MultiCategoricalHorizonStreamer::new(cat_reader, &config).unwrap();

            let mut streamed_categorical: HashMap<u64, HashMap<i64, f64>> = HashMap::new();
            loop {
                let batch = cat_streamer.fetch_next_batch(7).unwrap();
                if batch.is_empty() {
                    break;
                }
                for rec in batch {
                    let mut classes = HashMap::new();
                    rec.accumulator.for_each_class(|cls, cnt| {
                        classes.insert(cls, cnt);
                    });
                    assert!(
                        streamed_categorical.insert(rec.h3_index, classes).is_none(),
                        "Duplicate categorical cell emitted in {name}"
                    );
                }
            }

            assert_eq!(
                streamed_categorical.len(),
                expected_categorical.len(),
                "Categorical cell count mismatch for {name}"
            );

            for (cell, exp_classes) in &expected_categorical {
                let act_classes = streamed_categorical.get(cell).unwrap();
                assert_eq!(act_classes.len(), exp_classes.len());
                for (cls, exp_cnt) in exp_classes {
                    let act_cnt = act_classes.get(cls).unwrap();
                    assert!(
                        (act_cnt - exp_cnt).abs() < 1e-6,
                        "Class count mismatch in {name} for cell {:x}, class {cls}: act {} vs exp {}",
                        cell,
                        act_cnt,
                        exp_cnt
                    );
                }
            }
        }
    }
}
