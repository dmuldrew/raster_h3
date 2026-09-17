//! Tests sub-pixel super-sampling pattern conservation and scanline lookahead.
//!
//! Validates total pixel mass conservation across center, RGSS, quincunx, Gaussian, hexagonal,
//! and grid patterns under geographic (EPSG:4326) and projected (UTM) coordinate reference systems.

mod helpers;

#[test]
fn web_mercator_samples_match_exact_projection() {
    use h3o::{LatLng, Resolution};
    use raster_h3::crs::transformer::CrsTransformer;
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

use helpers::{create_fn_f32_geotiff as create_test_geotiff, TestGeoTiffBuilder};
use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::raster::GeoTiffStreamReader;

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
                assert!(maj_class >= 1 && maj_class <= 4);
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
    use raster_h3::crs::transformer::CrsTransformer;
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
