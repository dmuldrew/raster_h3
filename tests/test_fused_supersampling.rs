mod helpers;
use helpers::TestGeoTiffBuilder;
use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::raster::GeoTiffStreamReader;

#[test]
#[ignore = "manual release throughput comparison"]
fn benchmark_supersampling() {
    for (epsg, x, y, size) in [
        (4326, -122.4, 37.8, 0.0001),
        (32610, 500000.0, 4180000.0, 10.0),
    ] {
        let (_file, path) = TestGeoTiffBuilder::new(512, 128)
            .origin(x, y)
            .pixel_size(size)
            .epsg(epsg)
            .create_f32_tempfile(|c, r| (1 + (c + r) % 17) as f32);
        for pattern in [SamplingPattern::rgss(), SamplingPattern::sixteen_point()] {
            let mut times = Vec::new();
            for _ in 0..5 {
                let mut config = MultiResolutionConfig::single(8);
                config.sampling = pattern.clone();
                let start = std::time::Instant::now();
                let mut stream = MultiScanHorizonStreamer::new(
                    GeoTiffStreamReader::open(&path).unwrap(),
                    &config,
                )
                .unwrap();
                let mut count = 0.0;
                while !stream.is_finished() {
                    for record in stream.fetch_next_batch(1024).unwrap() {
                        count += record.accumulator.count;
                    }
                }
                assert_eq!(count, 512.0 * 128.0);
                times.push(start.elapsed().as_secs_f64());
            }
            times.sort_by(f64::total_cmp);
            println!(
                "EPSG {epsg}, {} samples: median {:.6}s",
                pattern.points.len(),
                times[2]
            );
        }
    }
}

#[test]
fn fused_samples_match_reference_statistics_histograms_and_quantiles() {
    use fxhash::FxBuildHasher;
    use h3o::{LatLng, Resolution};
    use raster_h3::aggregator::multi_horizon::{
        categorical::process_categorical_chunk_payload_into,
        continuous::process_continuous_chunk_payload_into,
    };
    use raster_h3::aggregator::sampling::SamplePoint;
    use raster_h3::aggregator::{accumulator::H3Accumulator, categorical::CategoricalAccumulator};
    use raster_h3::crs::CrsTransformer;
    use raster_h3::raster::{geotransform::GeoTransform, RasterChunk};
    use std::collections::HashMap;
    use tiff::decoder::DecodingResult;
    let chunk = RasterChunk {
        col_offset: 7,
        row_offset: 3,
        width: 24,
        height: 8,
    };
    let values: Vec<f32> = (0..192)
        .map(|i| {
            if i % 19 == 0 {
                f32::NAN
            } else {
                (i % 17) as f32 - 8.0
            }
        })
        .collect();
    let decoded = DecodingResult::F32(values.clone());
    let resolutions = [Resolution::Seven, Resolution::Nine];
    for (epsg, x, y, size, rotation) in [
        (4326, -122.4, 37.8, 0.001, 0.0),
        (32610, 500000.0, 4180000.0, 100.0, 30.0),
        (5070, -100000.0, 1500000.0, 100.0, 0.0),
    ] {
        let crs = CrsTransformer::from_crs_or_epsg(Some(epsg), None).unwrap();
        let gt = GeoTransform {
            c0: x,
            f0: y,
            a: size,
            e: -size,
            b: rotation,
            d: rotation,
        };
        let (lon, lat) = crs
            .transform_point(
                gt.pixel_center_to_coord(19, 7).0,
                gt.pixel_center_to_coord(19, 7).1,
            )
            .unwrap();
        for bbox in [None, Some([lon, lat - 1.0, lon + 1.0, lat + 1.0])] {
            for pattern in [
                SamplingPattern::rgss(),
                SamplingPattern::sixteen_point(),
                SamplingPattern {
                    points: vec![
                        SamplePoint {
                            dx: 0.1,
                            dy: 0.9,
                            weight: 0.2,
                        },
                        SamplePoint {
                            dx: 0.5,
                            dy: 0.5,
                            weight: 0.3,
                        },
                        SamplePoint {
                            dx: 0.9,
                            dy: 0.1,
                            weight: 0.7,
                        },
                    ],
                },
            ] {
                let mut expected = vec![HashMap::<u64, H3Accumulator>::new(); 2];
                let mut classes = vec![HashMap::<(u64, i64), f64>::new(); 2];
                let mut retained = 0;
                for (idx, &value) in values.iter().enumerate() {
                    if !value.is_finite() {
                        continue;
                    }
                    for sp in &pattern.points {
                        let (x, y) = gt.pixel_to_coord(
                            (chunk.col_offset as usize + idx % 24) as f64 + sp.dx,
                            (chunk.row_offset as usize + idx / 24) as f64 + sp.dy,
                        );
                        let (lon, lat) = crs.transform_point(x, y).unwrap();
                        if let Some([w, s, e, n]) = bbox {
                            if lon < w || lon > e || lat < s || lat > n {
                                continue;
                            }
                        }
                        retained += 1;
                        for (i, &res) in resolutions.iter().enumerate() {
                            let cell = u64::from(LatLng::new(lat, lon).unwrap().to_cell(res));
                            expected[i]
                                .entry(cell)
                                .or_insert_with(H3Accumulator::with_quantiles)
                                .update_weighted(value as f64, sp.weight);
                            *classes[i].entry((cell, value as i64)).or_default() += sp.weight;
                        }
                    }
                }
                for quantiles in [false, true] {
                    let mut actual =
                        vec![HashMap::<u64, H3Accumulator, FxBuildHasher>::default(); 2];
                    let scope = raster_h3::aggregator::multi_horizon::profile::WorkerScope::new();
                    process_continuous_chunk_payload_into(
                        &chunk,
                        &decoded,
                        &resolutions,
                        &crs,
                        &gt,
                        &pattern,
                        bbox,
                        24,
                        None,
                        1,
                        1,
                        None,
                        None,
                        quantiles,
                        &mut actual,
                    );
                    let metrics = scope.snapshot();
                    drop(scope);
                    if cfg!(feature = "stream-profile") {
                        assert_eq!(
                            metrics.transform_calls,
                            (values.iter().filter(|v| v.is_finite()).count() * pattern.points.len())
                                as u64
                        );
                        assert_eq!(
                            metrics.h3_calls,
                            retained * 2,
                            "Every retained sample is indexed exactly once per resolution"
                        );
                    }
                    for i in 0..2 {
                        assert_eq!(actual[i].len(), expected[i].len());
                        for (cell, oracle) in &expected[i] {
                            let got = &actual[i][cell];
                            for (a, b) in [
                                (got.count, oracle.count),
                                (got.sum, oracle.sum),
                                (got.m2, oracle.m2),
                            ] {
                                assert!((a - b).abs() < 1e-8, "{a} != {b}");
                            }
                            assert_eq!(got.min, oracle.min);
                            assert_eq!(got.max, oracle.max);
                            if quantiles {
                                for q in [0.1, 0.5, 0.9] {
                                    // Sketch merging can change the last few floating-point bits.
                                    let expected = oracle.quantile(q);
                                    assert!(
                                        (got.quantile(q) - expected).abs()
                                            <= 1e-12 * expected.abs().max(1.0)
                                    );
                                }
                            }
                        }
                    }
                }
                let mut actual =
                    vec![HashMap::<u64, CategoricalAccumulator, FxBuildHasher>::default(); 2];
                process_categorical_chunk_payload_into(
                    &chunk,
                    &decoded,
                    &resolutions,
                    &crs,
                    &gt,
                    &pattern,
                    bbox,
                    24,
                    None,
                    1,
                    1,
                    None,
                    None,
                    &mut actual,
                );
                for i in 0..2 {
                    assert_eq!(actual[i].len(), expected[i].len());
                    let mut got_classes = HashMap::new();
                    for (&cell, acc) in &actual[i] {
                        acc.for_each_class(|class, count| {
                            got_classes.insert((cell, class), count);
                        });
                    }
                    assert_eq!(got_classes.len(), classes[i].len());
                    for (key, count) in &classes[i] {
                        assert!((got_classes[key] - count).abs() < 1e-8);
                    }
                }
            }
        }
    }
}
