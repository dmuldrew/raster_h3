//! Independent reference: index every sample directly, without scanline or cache helpers.
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

fn pixel_value(col: u32, row: u32) -> f32 {
    if (col + row * 32) % 17 == 0 {
        f32::NAN
    } else {
        (1 + (col / 3 + row / 2) % 5) as f32
    }
}

#[test]
fn supersampling_matches_direct_h3_per_cell_and_per_class() {
    // Includes the neighborhood of a known nearest-centroid mismatch, high latitudes,
    // and a raster crossing +180 degrees longitude.
    for (lon, lat) in [
        (-153.6792, -74.8768),
        (-122.45, 37.85),
        (25.0, 80.0),
        (179.999, 10.0),
    ] {
        let (_file, path) = TestGeoTiffBuilder::new(32, 32)
            .origin(lon, lat)
            .pixel_size(0.0004)
            .create_f32_tempfile(pixel_value);
        for pattern in [
            SamplingPattern::rgss(),
            SamplingPattern::five_point(),
            SamplingPattern::gaussian_five_point(),
            SamplingPattern::hex_seven_point(),
            SamplingPattern::sixteen_point(),
        ] {
            for resolutions in [vec![8], vec![7, 8, 9]] {
                let reader = GeoTiffStreamReader::open(&path).unwrap();
                let gt = reader.metadata.geotransform;
                let mut expected: HashMap<(u8, u64), H3Accumulator> = HashMap::new();
                let mut classes: HashMap<(u8, u64, i64), f64> = HashMap::new();
                for row in 0..32 {
                    for col in 0..32 {
                        let value = pixel_value(col, row);
                        if !value.is_finite() {
                            continue;
                        }
                        for sample in &pattern.points {
                            let (x, y) =
                                gt.pixel_to_coord(col as f64 + sample.dx, row as f64 + sample.dy);
                            for &res in &resolutions {
                                let cell: u64 = LatLng::new(y, x)
                                    .unwrap()
                                    .to_cell(Resolution::try_from(res).unwrap())
                                    .into();
                                expected
                                    .entry((res, cell))
                                    .or_default()
                                    .update_weighted(value as f64, sample.weight);
                                *classes.entry((res, cell, value as i64)).or_default() +=
                                    sample.weight;
                            }
                        }
                    }
                }
                let mut config = MultiResolutionConfig::new(resolutions);
                config.sampling = pattern.clone();
                let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
                let mut actual = HashMap::new();
                loop {
                    let batch = streamer.fetch_next_batch(7).unwrap();
                    if batch.is_empty() {
                        break;
                    }
                    for rec in batch {
                        assert!(
                            actual
                                .insert((rec.resolution, rec.h3_index), rec.accumulator)
                                .is_none(),
                            "duplicate cell emitted"
                        );
                    }
                }
                assert_eq!(
                    actual.len(),
                    expected.len(),
                    "cell set differs at {lon},{lat}, pattern {pattern:?}"
                );
                for (key, reference) in &expected {
                    let got = actual.get(key).expect("missing reference cell");
                    assert!(
                        (got.count - reference.count).abs() < 1e-8,
                        "count mismatch for {key:?} at {lon},{lat}: {} vs {}",
                        got.count,
                        reference.count
                    );
                    assert!((got.sum - reference.sum).abs() < 1e-7);
                    assert!((got.m2 - reference.m2).abs() < 1e-7);
                    assert_eq!(got.min, reference.min);
                    assert_eq!(got.max, reference.max);
                }
                let mut streamer = MultiCategoricalHorizonStreamer::new(
                    GeoTiffStreamReader::open(&path).unwrap(),
                    &config,
                )
                .unwrap();
                let mut actual_classes = HashMap::new();
                let mut actual_cells = HashMap::new();
                loop {
                    let batch = streamer.fetch_next_batch(7).unwrap();
                    if batch.is_empty() {
                        break;
                    }
                    for rec in batch {
                        assert!(actual_cells
                            .insert((rec.resolution, rec.h3_index), rec.accumulator.total_count)
                            .is_none());
                        rec.accumulator.for_each_class(|class, count| {
                            assert!(actual_classes
                                .insert((rec.resolution, rec.h3_index, class), count)
                                .is_none());
                        });
                    }
                }
                assert_eq!(actual_cells.len(), expected.len());
                assert_eq!(actual_classes.len(), classes.len());
                for (key, count) in classes {
                    assert!(
                        (actual_classes.get(&key).expect("missing reference class") - count).abs()
                            < 1e-8
                    );
                }
            }
        }
    }
}
