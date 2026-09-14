mod helpers;

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
            let batch = streamer.fetch_next_batch(64);
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
            let batch = streamer.fetch_next_batch(64);
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
            let batch = streamer.fetch_next_batch(64);
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
            let batch = streamer.fetch_next_batch(64);
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
