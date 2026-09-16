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
    for (lon, lat, epsg, pixel_size, b, d) in [
        (-153.6792, -74.8768, 4326, 0.0004, 0.0, 0.0),
        (-122.45, 37.85, 4326, 0.0004, 0.0, 0.0),
        (25.0, 80.0, 4326, 0.0004, 0.0, 0.0),
        (179.999, 10.0, 4326, 0.0004, 0.0, 0.0),
        (-122.45, 37.85, 4326, 0.0004, 0.0003, 0.005),
        (-122.45, 37.85, 4326, 0.0004, -0.0007, 0.0),
        (-13631000.0, 4551000.0, 3857, 40.0, 30.0, 500.0),
        (-13631000.0, 4551000.0, 3857, 40.0, -70.0, 0.0),
    ] {
        let (_file, path) = TestGeoTiffBuilder::new(32, 32)
            .origin(lon, lat)
            .epsg(epsg)
            .pixel_size(pixel_size)
            .create_f32_tempfile(pixel_value);
        for pattern in [
            SamplingPattern::center(),
            SamplingPattern::rgss(),
            SamplingPattern::five_point(),
            SamplingPattern::gaussian_five_point(),
            SamplingPattern::hex_seven_point(),
            SamplingPattern::sixteen_point(),
        ] {
            for bbox in [None, Some([-122.46, 37.87, -122.40, 37.96])] {
                for resolutions in [vec![8], vec![7, 8, 9]] {
                    let mut reader = GeoTiffStreamReader::open(&path).unwrap();
                    // Override the affine geometry to isolate walker behavior from TIFF tags.
                    reader.metadata.geotransform.b = b;
                    reader.metadata.geotransform.d = d;
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
                                let (x, y) = gt
                                    .pixel_to_coord(col as f64 + sample.dx, row as f64 + sample.dy);
                                let (x, y) = if epsg == 3857 {
                                    (
                                        (x / 6378137.0).to_degrees(),
                                        (2.0 * (y / 6378137.0).exp().atan()
                                            - std::f64::consts::FRAC_PI_2)
                                            .to_degrees(),
                                    )
                                } else {
                                    (x, y)
                                };
                                if let Some([min_x, min_y, max_x, max_y]) = bbox {
                                    if x < min_x || x > max_x || y < min_y || y > max_y {
                                        continue;
                                    }
                                }
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
                    config.bbox = bbox;
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
                    let mut reader = GeoTiffStreamReader::open(&path).unwrap();
                    reader.metadata.geotransform = gt;
                    let mut streamer =
                        MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
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
                            (actual_classes.get(&key).expect("missing reference class") - count)
                                .abs()
                                < 1e-8
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn nan_nodata_preserves_finite_raster_values() {
    for marker in ["nan", "NaN"] {
        let (_file, path) = TestGeoTiffBuilder::new(32, 32)
            .origin(-122.45, 37.85)
            .pixel_size(0.00001)
            .gdal_nodata(marker)
            .create_f32_tempfile(pixel_value);
        for pattern in [SamplingPattern::center(), SamplingPattern::five_point()] {
            let mut config = MultiResolutionConfig::new(vec![7, 8]);
            config.sampling = pattern;
            let mut streamer =
                MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config)
                    .unwrap();
            let mut totals: HashMap<u8, (f64, f64)> = HashMap::new();
            loop {
                let batch = streamer.fetch_next_batch(7).unwrap();
                if batch.is_empty() {
                    break;
                }
                for rec in batch {
                    let total = totals.entry(rec.resolution).or_default();
                    total.0 += rec.accumulator.count;
                    total.1 += rec.accumulator.sum;
                }
            }
            let finite: Vec<_> = (0..32)
                .flat_map(|r| (0..32).map(move |c| pixel_value(c, r)))
                .filter(|v| v.is_finite())
                .collect();
            for res in [7, 8] {
                let (count, sum) = totals[&res];
                assert!((count - finite.len() as f64).abs() < 1e-8);
                assert!((sum - finite.iter().map(|v| *v as f64).sum::<f64>()).abs() < 1e-8);
            }
        }
    }
}

#[test]
fn compact_adjacent_resolutions_are_rejected() {
    let (_file, path) = TestGeoTiffBuilder::new(8, 8)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_f32_tempfile(|_, _| 1.0);
    let mut config = MultiResolutionConfig::new(vec![7, 8]);
    config.compact = true;

    let err =
        match MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config) {
            Ok(_) => panic!("adjacent compact resolutions should be rejected"),
            Err(err) => err,
        };
    assert!(err.to_string().contains("duplicate parent cells"));

    config.resolutions = vec![7, 9];
    assert!(
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config).is_ok()
    );
}

#[test]
fn bbox_keeps_subpixel_samples_when_center_is_outside() {
    let (_file, path) = TestGeoTiffBuilder::new(1, 1)
        .origin(0.0, 1.0)
        .pixel_size(1.0)
        .create_f32_tempfile(|_, _| 7.0);
    let mut config = MultiResolutionConfig::single(5);
    config.sampling = SamplingPattern::five_point();
    config.bbox = Some([0.1, 0.7, 0.3, 0.9]);

    let mut continuous =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config).unwrap();
    let mut categorical =
        MultiCategoricalHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config)
            .unwrap();
    let records = continuous.fetch_next_batch(10).unwrap();
    let categories = categorical.fetch_next_batch(10).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(categories.len(), 1);
    assert!((records[0].accumulator.count - 0.2).abs() < 1e-10);
    assert!((records[0].accumulator.sum - 1.4).abs() < 1e-10);
    assert!((categories[0].accumulator.total_count - 0.2).abs() < 1e-10);
}
