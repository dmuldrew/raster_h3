//! Tests sub-pixel super-sampling pattern conservation and exact sample assignment.
//!
//! Validates total pixel mass conservation across center, RGSS, quincunx, Gaussian, hexagonal,
//! and grid patterns under geographic (EPSG:4326) and projected (UTM) coordinate reference systems.

use crate::helpers;
use helpers::{create_fn_f32_geotiff as create_test_geotiff, TestGeoTiffBuilder};
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::raster::GeoTiffStreamReader;

#[test]
fn web_mercator_samples_match_exact_projection() {
    use h3o::{LatLng, Resolution};
    use raster_h3_core::crs::transformer::CrsTransformer;
    use std::collections::HashMap;

    // Large high-latitude pixels expose nonlinear latitude errors; small
    // pixels at a coarser H3 resolution exercise shared-cell core spans.
    for (size, resolution) in [(100_000.0, 12), (100.0, 5)] {
        let (_file, path) = TestGeoTiffBuilder::new(32, 2)
            .origin(0.0, 15_000_000.0)
            .pixel_size(size)
            .epsg(3857)
            .create_f32_tempfile(|_, _| 3.0);
        for pattern in [SamplingPattern::five_point(), SamplingPattern::rgss()] {
            for bbox in [None, Some([0.0, 78.0, 15.0, 81.0])] {
                let reader = GeoTiffStreamReader::open(&path).unwrap();
                let gt = reader.metadata.geotransform;
                let crs = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
                let mut expected = HashMap::<u64, f64>::new();
                for row in 0..2 {
                    for col in 0..32 {
                        for sample in &pattern.points {
                            let (x, y) =
                                gt.pixel_to_coord(col as f64 + sample.dx, row as f64 + sample.dy);
                            let (lon, lat) = crs.transform_point(x, y).unwrap();
                            if let Some([west, south, east, north]) = bbox {
                                if lon < west || lon > east || lat < south || lat > north {
                                    continue;
                                }
                            }
                            let cell = LatLng::new(lat, lon)
                                .unwrap()
                                .to_cell(Resolution::try_from(resolution).unwrap());
                            *expected.entry(cell.into()).or_default() += sample.weight;
                        }
                    }
                }
                let mut config = MultiResolutionConfig::single(resolution);
                config.sampling = pattern.clone();
                config.bbox = bbox;
                let mut continuous = MultiScanHorizonStreamer::new(reader, &config).unwrap();
                let mut actual = HashMap::new();
                loop {
                    let batch = continuous.fetch_next_batch(17).unwrap();
                    if batch.is_empty() {
                        break;
                    }
                    for record in batch {
                        assert!(actual
                            .insert(record.h3_index, record.accumulator.count)
                            .is_none());
                        assert!(
                            (record.accumulator.sum - 3.0 * record.accumulator.count).abs() < 1e-8
                        );
                    }
                }
                let mut categorical = MultiCategoricalHorizonStreamer::new(
                    GeoTiffStreamReader::open(&path).unwrap(),
                    &config,
                )
                .unwrap();
                let mut categories = HashMap::new();
                loop {
                    let batch = categorical.fetch_next_batch(17).unwrap();
                    if batch.is_empty() {
                        break;
                    }
                    for record in batch {
                        assert!(categories
                            .insert(record.h3_index, record.accumulator.total_count)
                            .is_none());
                    }
                }
                for output in [actual, categories] {
                    assert_eq!(output.len(), expected.len());
                    for (cell, count) in &expected {
                        assert!((output[cell] - count).abs() < 1e-8, "cell {cell}");
                    }
                }
            }
        }
    }
}

#[test]
fn test_continuous_supersampling_all_patterns_conservation() {
    let width = 128u32;
    let height = 128u32;
    let expected_pixels = (width * height) as f64;
    let constant_val = 42.5f32;

    let (_tmp, path) = create_test_geotiff(width, height, |_c, _r| constant_val);

    let patterns = [
        ("center", SamplingPattern::center()),
        ("rgss", SamplingPattern::rgss()),
        ("five_point", SamplingPattern::five_point()),
        (
            "gaussian_five_point",
            SamplingPattern::gaussian_five_point(),
        ),
        ("hex_seven_point", SamplingPattern::hex_seven_point()),
        ("sixteen_point", SamplingPattern::sixteen_point()),
    ];

    for (name, pattern) in patterns {
        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let mut config = MultiResolutionConfig::single(8);
        config.sampling = pattern;

        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        let mut total_count = 0.0;
        let mut total_sum = 0.0;
        let mut cell_count = 0;

        loop {
            let batch = streamer.fetch_next_batch(64).unwrap();
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                let acc = rec.accumulator;
                cell_count += 1;
                total_count += acc.count;
                total_sum += acc.sum;
                // For a constant raster, every cell mean must be exactly 42.5
                assert!(
                    (acc.mean() - constant_val as f64).abs() < 1e-4,
                    "Pattern {} cell mean {} != expected {}",
                    name,
                    acc.mean(),
                    constant_val
                );
                // Variance must be near 0
                assert!(
                    acc.variance() < 1e-4,
                    "Pattern {} cell variance {} != 0",
                    name,
                    acc.variance()
                );
            }
        }

        assert!(cell_count > 0);
        // Conservation of total pixel count across sub-pixel weights
        assert!(
            (total_count - expected_pixels).abs() < 1e-5,
            "Pattern {} total count {} != expected {}",
            name,
            total_count,
            expected_pixels
        );
        // Conservation of total integral / sum
        let expected_sum = expected_pixels * constant_val as f64;
        assert!(
            (total_sum - expected_sum).abs() < 1e-3,
            "Pattern {} total sum {} != expected {}",
            name,
            total_sum,
            expected_sum
        );
    }
}

#[test]
fn test_continuous_gradient_supersampling_consistency() {
    let width = 128u32;
    let height = 128u32;
    let expected_pixels = (width * height) as f64;

    // Linear diagonal gradient: f(x, y) = 10.0 + x * 0.5 + y * 0.5
    let (_tmp, path) = create_test_geotiff(width, height, |c, r| {
        10.0 + (c as f32) * 0.5 + (r as f32) * 0.5
    });

    let patterns = [
        ("center", SamplingPattern::center()),
        ("rgss", SamplingPattern::rgss()),
        ("five_point", SamplingPattern::five_point()),
        (
            "gaussian_five_point",
            SamplingPattern::gaussian_five_point(),
        ),
        ("hex_seven_point", SamplingPattern::hex_seven_point()),
        ("sixteen_point", SamplingPattern::sixteen_point()),
    ];

    let mut pattern_sums = Vec::new();

    for (name, pattern) in patterns {
        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let mut config = MultiResolutionConfig::single(8);
        config.sampling = pattern;

        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        let mut total_count = 0.0;
        let mut total_sum = 0.0;
        let mut cell_count = 0;

        loop {
            let batch = streamer.fetch_next_batch(64).unwrap();
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                let acc = rec.accumulator;
                cell_count += 1;
                total_count += acc.count;
                total_sum += acc.sum;
                assert!(acc.min <= acc.mean() + 1e-5);
                assert!(acc.mean() <= acc.max + 1e-5);
            }
        }

        assert!(cell_count > 0);
        assert!(
            (total_count - expected_pixels).abs() < 1e-5,
            "Pattern {} count {} != expected {}",
            name,
            total_count,
            expected_pixels
        );
        pattern_sums.push((name, total_sum));
    }

    // In a linear gradient, the total integral of all pixel values over the raster
    // should be closely conserved across all sampling schemes (within sub-pixel boundary shift tolerance)
    let baseline_sum = pattern_sums[0].1;
    for (name, sum) in &pattern_sums[1..] {
        let rel_diff = (*sum - baseline_sum).abs() / baseline_sum;
        assert!(
            rel_diff < 0.005,
            "Pattern {} sum {} deviated from baseline {} by {:.4}%",
            name,
            sum,
            baseline_sum,
            rel_diff * 100.0
        );
    }
}

#[test]
fn test_categorical_supersampling_all_patterns_conservation() {
    let width = 128u32;
    let height = 128u32;
    let expected_pixels = (width * height) as f64;

    // Checkerboard / banded categories 1, 2, 3, 4
    let (_tmp, path) = create_test_geotiff(width, height, |c, r| {
        let cat = ((c / 16) % 2) + 2 * ((r / 16) % 2) + 1;
        cat as f32
    });

    let patterns = [
        ("center", SamplingPattern::center()),
        ("rgss", SamplingPattern::rgss()),
        ("five_point", SamplingPattern::five_point()),
        (
            "gaussian_five_point",
            SamplingPattern::gaussian_five_point(),
        ),
        ("hex_seven_point", SamplingPattern::hex_seven_point()),
        ("sixteen_point", SamplingPattern::sixteen_point()),
    ];

    for (name, pattern) in patterns {
        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let mut config = MultiResolutionConfig::single(8);
        config.sampling = pattern;

        let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
        let mut total_count = 0.0;
        let mut cell_count = 0;

        loop {
            let batch = streamer.fetch_next_batch(64).unwrap();
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                let acc = rec.accumulator;
                cell_count += 1;
                total_count += acc.total_count;
                let (maj_class, maj_cnt, maj_frac) = acc.majority();
                assert!((1..=4).contains(&maj_class));
                assert!(maj_cnt > 0.0);
                assert!(maj_frac > 0.0 && maj_frac <= 1.0);
            }
        }

        assert!(cell_count > 0);
        assert!(
            (total_count - expected_pixels).abs() < 1e-5,
            "Pattern {} categorical total count {} != expected {}",
            name,
            total_count,
            expected_pixels
        );
    }
}

#[test]
fn test_projected_utm_jacobian_supersampling_conservation() {
    let width = 128u32;
    let height = 128u32;
    let expected_pixels = (width * height) as f64;
    let constant_val = 17.25f32;

    let (_temp_file, path) = TestGeoTiffBuilder::new(width, height)
        .origin(500000.0, 4180000.0)
        .pixel_size(30.0)
        .epsg(32610)
        .create_f32_tempfile(|_, _| constant_val);

    let patterns = [
        ("center", SamplingPattern::center()),
        ("rgss", SamplingPattern::rgss()),
        ("five_point", SamplingPattern::five_point()),
        (
            "gaussian_five_point",
            SamplingPattern::gaussian_five_point(),
        ),
        ("hex_seven_point", SamplingPattern::hex_seven_point()),
        ("sixteen_point", SamplingPattern::sixteen_point()),
    ];

    for (name, pattern) in patterns {
        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let mut config = MultiResolutionConfig::single(9);
        config.sampling = pattern;

        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        let mut total_count = 0.0;
        let mut total_sum = 0.0;
        let mut cell_count = 0;

        loop {
            let batch = streamer.fetch_next_batch(64).unwrap();
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                let acc = rec.accumulator;
                cell_count += 1;
                total_count += acc.count;
                total_sum += acc.sum;
                assert!(
                    (acc.mean() - constant_val as f64).abs() < 1e-4,
                    "Pattern {} UTM cell mean {} != expected {}",
                    name,
                    acc.mean(),
                    constant_val
                );
            }
        }

        assert!(cell_count > 0);
        assert!(
            (total_count - expected_pixels).abs() < 1e-5,
            "Pattern {} UTM total count {} != expected {}",
            name,
            total_count,
            expected_pixels
        );
        let expected_sum = expected_pixels * constant_val as f64;
        assert!(
            (total_sum - expected_sum).abs() < 1e-3,
            "Pattern {} UTM total sum {} != expected {}",
            name,
            total_sum,
            expected_sum
        );
    }
}

#[test]
fn test_projected_utm_gradient_exact_cell_assignment() {
    use h3o::{LatLng, Resolution};
    use raster_h3_core::crs::transformer::CrsTransformer;
    use std::collections::HashMap;

    let width = 32u32;
    let height = 32u32;
    let (_temp_file, path) = TestGeoTiffBuilder::new(width, height)
        .origin(500000.0, 4180000.0)
        .pixel_size(30.0)
        .epsg(32610)
        .create_f32_tempfile(|c, r| (c * 2 + r * 3) as f32);

    let pattern = SamplingPattern::rgss();
    let res = Resolution::try_from(10).unwrap();

    // 1. Compute ground-truth reference by direct per-sample transformation
    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let gt = reader.metadata.geotransform;
    let crs_trans = CrsTransformer::from_crs_or_epsg(Some(32610), None).unwrap();

    let mut expected_cells: HashMap<u64, (f64, f64)> = HashMap::new();
    for r in 0..height {
        for c in 0..width {
            let val = (c * 2 + r * 3) as f64;
            for sp in &pattern.points {
                let (x, y) = gt.pixel_to_coord(c as f64 + sp.dx, r as f64 + sp.dy);
                let (lon, lat) = crs_trans.transform_point(x, y).unwrap();
                let cell: u64 = LatLng::new(lat, lon).unwrap().to_cell(res).into();
                let entry = expected_cells.entry(cell).or_insert((0.0, 0.0));
                entry.0 += sp.weight;
                entry.1 += sp.weight * val;
            }
        }
    }

    // 2. Stream through MultiScanHorizonStreamer
    let mut config = MultiResolutionConfig::single(10);
    config.sampling = pattern;
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let mut streamed_cells: HashMap<u64, (f64, f64)> = HashMap::new();
    loop {
        let batch = streamer.fetch_next_batch(64).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            streamed_cells.insert(rec.h3_index, (rec.accumulator.count, rec.accumulator.sum));
        }
    }

    assert_eq!(
        streamed_cells.len(),
        expected_cells.len(),
        "Number of cells must match ground truth reference"
    );

    for (cell, (exp_count, exp_sum)) in &expected_cells {
        let (act_count, act_sum) = streamed_cells
            .get(cell)
            .unwrap_or_else(|| panic!("Cell {} missing from streamer output", cell));
        assert!(
            (act_count - exp_count).abs() < 1e-5,
            "Cell {} count mismatch: {} vs {}",
            cell,
            act_count,
            exp_count
        );
        assert!(
            (act_sum - exp_sum).abs() < 1e-3,
            "Cell {} sum mismatch: {} vs {}",
            cell,
            act_sum,
            exp_sum
        );
    }
}

#[test]
fn custom_single_samples_preserve_offsets_and_weights() {
    use h3o::{LatLng, Resolution};
    use raster_h3_core::aggregator::sampling::SamplePoint;
    use std::collections::HashMap;
    let (_file, path) = TestGeoTiffBuilder::new(16, 4)
        .origin(-122.0, 38.0)
        .pixel_size(0.01)
        .epsg(4326)
        .create_f32_tempfile(|_, _| 3.0);
    for (dx, dy, weight) in [(0.1, 0.9, 1.0), (0.5, 0.5, 0.25), (0.1, 0.9, 0.25)] {
        for resolutions in [vec![9], vec![8, 9]] {
            let reader = GeoTiffStreamReader::open(&path).unwrap();
            let gt = reader.metadata.geotransform;
            let mut expected = HashMap::<u64, f64>::new();
            for &res in &resolutions {
                for row in 0..4 {
                    for col in 0..16 {
                        let (lon, lat) = gt.pixel_to_coord(col as f64 + dx, row as f64 + dy);
                        let cell = LatLng::new(lat, lon)
                            .unwrap()
                            .to_cell(Resolution::try_from(res).unwrap());
                        *expected.entry(cell.into()).or_default() += weight;
                    }
                }
            }
            let mut config = MultiResolutionConfig::new(resolutions);
            config.sampling = SamplingPattern {
                points: vec![SamplePoint { dx, dy, weight }],
            };
            let mut stream = MultiScanHorizonStreamer::new(reader.clone(), &config).unwrap();
            let mut actual = HashMap::new();
            while !stream.is_finished() {
                for record in stream.fetch_next_batch(17).unwrap() {
                    assert!(actual
                        .insert(record.h3_index, record.accumulator.count)
                        .is_none());
                    assert_eq!(record.accumulator.sum, record.accumulator.count * 3.0);
                }
            }
            assert_eq!(actual, expected);
            let mut stream = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
            let mut actual = HashMap::new();
            while !stream.is_finished() {
                for record in stream.fetch_next_batch(17).unwrap() {
                    assert!(actual
                        .insert(record.h3_index, record.accumulator.total_count)
                        .is_none());
                }
            }
            assert_eq!(actual, expected);
        }
    }
}

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
    use h3o::{LatLng, Resolution};
    use raster_h3_core::aggregator::cell_hash::H3BuildHasher;
    use raster_h3_core::aggregator::multi_horizon::{
        categorical::process_categorical_chunk_payload_into,
        continuous::{process_continuous_chunk_payload_into, ContinuousOptions},
        WalkContext,
    };
    use raster_h3_core::aggregator::sampling::SamplePoint;
    use raster_h3_core::aggregator::{
        accumulator::H3Accumulator, categorical::CategoricalAccumulator,
    };
    use raster_h3_core::crs::CrsTransformer;
    use raster_h3_core::raster::{geotransform::GeoTransform, RasterChunk};
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
                SamplingPattern::center(),
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
                let window = WalkContext {
                    chunk: &chunk,
                    resolutions: &resolutions,
                    crs: &crs,
                    gt: &gt,
                    sampling: &pattern,
                    bbox,
                    stride: 24,
                    samples_per_pixel: 1,
                    owner: None,
                };
                for quantiles in [false, true] {
                    let mut actual =
                        vec![HashMap::<u64, H3Accumulator, H3BuildHasher>::default(); 2];
                    let scope =
                        raster_h3_core::aggregator::multi_horizon::profile::WorkerScope::new();
                    process_continuous_chunk_payload_into(
                        &window,
                        &decoded,
                        None,
                        ContinuousOptions {
                            band: 1,
                            spectral_formula: None,
                            track_quantiles: quantiles,
                        },
                        &mut actual,
                    );
                    let metrics = scope.snapshot();
                    drop(scope);
                    if cfg!(feature = "stream-profile") {
                        let unpruned = (values.iter().filter(|v| v.is_finite()).count()
                            * pattern.points.len()) as u64;
                        if bbox.is_none() {
                            assert_eq!(metrics.transform_calls, unpruned);
                        } else {
                            // Conservative row pruning may skip rejected samples before projection.
                            assert!((retained..=unpruned).contains(&metrics.transform_calls));
                        }
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
                    vec![HashMap::<u64, CategoricalAccumulator, H3BuildHasher>::default(); 2];
                process_categorical_chunk_payload_into(
                    &window,
                    &decoded,
                    None,
                    1,
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

#[test]
fn test_subpixel_sampling_patterns() {
    let center = SamplingPattern::parse("center");
    assert!(center.is_single_point());

    let presets = [
        "rgss", "5point", "gaussian", "hex", "8rooks", "9point", "16point",
    ];
    for name in presets {
        let pattern = SamplingPattern::parse(name);
        assert!(!pattern.is_single_point());
        let sum_w: f64 = pattern.points.iter().map(|p| p.weight).sum();
        assert!(
            (sum_w - 1.0).abs() < 1e-9,
            "Weights did not sum to 1.0 for {}",
            name
        );
    }
}

#[test]
fn test_subpixel_rgss_streaming() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(40, 40)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_constant_tempfile(25.0f32);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut config = MultiResolutionConfig::single(9);
    config.sampling = SamplingPattern::rgss();

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    let mut has_fractional_cell = false;

    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = rec.accumulator;
            total_pixels += acc.count;
            assert!((acc.mean() - 25.0).abs() < 1e-6);
            if (acc.count.fract() - 0.0).abs() > 1e-4 {
                has_fractional_cell = true;
            }
        }
    }

    // RGSS 4-point weights (0.25 each) must sum to the exact total pixels (1600.0)
    assert!(
        (total_pixels - 1600.0).abs() < 1e-3,
        "Total RGSS area weight mismatch: {}",
        total_pixels
    );
    // Boundary hexagons should have fractional pixel area contributions
    assert!(
        has_fractional_cell,
        "Expected boundary hexagons to have fractional counts under RGSS super-sampling"
    );
}

#[test]
fn test_subpixel_hex_streaming() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(30, 30)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_constant_tempfile(42.0f32);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut config = MultiResolutionConfig::single(9);
    config.sampling = SamplingPattern::hex_seven_point();

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;

    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            total_pixels += rec.accumulator.count;
            assert!((rec.accumulator.mean() - 42.0).abs() < 1e-6);
        }
    }

    assert!(
        (total_pixels - 900.0).abs() < 1e-3,
        "Total Hex 7-point area weight mismatch: {}",
        total_pixels
    );
}

#[test]
fn test_sampling_pattern_fallback_to_center() {
    let default_pattern = SamplingPattern::parse("unknown_nonexistent_mode");
    assert!(default_pattern.is_single_point());
    assert_eq!(default_pattern.points.len(), 1);
    assert_eq!(default_pattern.points[0].weight, 1.0);
    assert_eq!(default_pattern.points[0].dx, 0.5);
    assert_eq!(default_pattern.points[0].dy, 0.5);
}
