use std::fs::File;
use std::io::BufWriter;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::{Gray8, RGBA8};
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
    SpectralFormula,
};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

/// Create a test 4-band RGBA8 GeoTIFF where each band has distinctive deterministic values
fn create_test_rgba_geotiff(
    width: usize,
    height: usize,
    r_val: u8,
    g_val: u8,
    b_val: u8,
    a_val: u8,
) -> NamedTempFile {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let mut data = Vec::with_capacity(width * height * 4);
    for _ in 0..height {
        for _ in 0..width {
            data.push(r_val);
            data.push(g_val);
            data.push(b_val);
            data.push(a_val);
        }
    }

    let file = File::create(&path).unwrap();
    let writer = BufWriter::new(file);
    let mut encoder = TiffEncoder::new(writer).unwrap();
    let mut image = encoder
        .new_image::<RGBA8>(width as u32, height as u32)
        .unwrap();

    // Tie point: SF Bay (-122.45, 37.80)
    image
        .encoder()
        .write_tag(
            Tag::Unknown(33922),
            &[-0.0f64, 0.0, 0.0, -122.45, 37.80, 0.0][..],
        )
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[0.001f64, 0.001, 0.0][..])
        .unwrap();

    let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])
        .unwrap();
    image.write_data(&data).unwrap();

    temp_file
}

/// Create a test 1-band Gray8 GeoTIFF
fn create_test_gray8_geotiff(
    width: usize,
    height: usize,
    val_fn: impl Fn(usize, usize) -> u8,
) -> NamedTempFile {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    let mut data = Vec::with_capacity(width * height);
    for row in 0..height {
        for col in 0..width {
            data.push(val_fn(col, row));
        }
    }

    let file = File::create(&path).unwrap();
    let writer = BufWriter::new(file);
    let mut encoder = TiffEncoder::new(writer).unwrap();
    let mut image = encoder
        .new_image::<Gray8>(width as u32, height as u32)
        .unwrap();

    image
        .encoder()
        .write_tag(
            Tag::Unknown(33922),
            &[-0.0f64, 0.0, 0.0, -122.45, 37.80, 0.0][..],
        )
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[0.001f64, 0.001, 0.0][..])
        .unwrap();

    let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])
        .unwrap();
    image.write_data(&data).unwrap();

    temp_file
}

#[test]
fn test_multiband_selection_and_samples_per_pixel() {
    // Create 4-band image: R=50, G=100, B=150, NIR/A=200
    let temp_raster = create_test_rgba_geotiff(64, 64, 50, 100, 150, 200);
    let raster_path = temp_raster.path();

    // Verify metadata detects samples_per_pixel = 4
    let reader = GeoTiffStreamReader::open(raster_path).unwrap();
    assert_eq!(reader.metadata.samples_per_pixel, 4);

    // Test Band 1 (R)
    let config_b1 = MultiResolutionConfig {
        resolutions: vec![8],
        band: 1,
        ..Default::default()
    };
    let mut streamer_b1 = MultiScanHorizonStreamer::new(reader.clone(), &config_b1).unwrap();
    let records_b1 = streamer_b1.fetch_next_batch(1000);
    assert!(!records_b1.is_empty());
    for rec in &records_b1 {
        assert_eq!(rec.accumulator.mean(), 50.0);
    }

    // Test Band 2 (G)
    let config_b2 = MultiResolutionConfig {
        resolutions: vec![8],
        band: 2,
        ..Default::default()
    };
    let mut streamer_b2 = MultiScanHorizonStreamer::new(reader.clone(), &config_b2).unwrap();
    let records_b2 = streamer_b2.fetch_next_batch(1000);
    assert!(!records_b2.is_empty());
    for rec in &records_b2 {
        assert_eq!(rec.accumulator.mean(), 100.0);
    }

    // Test Band 4 (NIR/A)
    let config_b4 = MultiResolutionConfig {
        resolutions: vec![8],
        band: 4,
        ..Default::default()
    };
    let mut streamer_b4 = MultiScanHorizonStreamer::new(reader, &config_b4).unwrap();
    let records_b4 = streamer_b4.fetch_next_batch(1000);
    assert!(!records_b4.is_empty());
    for rec in &records_b4 {
        assert_eq!(rec.accumulator.mean(), 200.0);
    }
}

#[test]
fn test_spectral_formula_ndvi_on_the_fly() {
    // R=50, G=100, B=150, NIR/A=200
    // Expected NDVI = (NIR - Red) / (NIR + Red) = (200 - 50) / (200 + 50) = 150 / 250 = 0.60
    let temp_raster = create_test_rgba_geotiff(64, 64, 50, 100, 150, 200);
    let raster_path = temp_raster.path();

    let reader = GeoTiffStreamReader::open(raster_path).unwrap();

    let config = MultiResolutionConfig {
        resolutions: vec![8],
        spectral_formula: Some(SpectralFormula::Ndvi {
            nir_band: 4,
            red_band: 1,
        }),
        ..Default::default()
    };

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let records = streamer.fetch_next_batch(1000);
    assert!(!records.is_empty());
    for rec in &records {
        assert!((rec.accumulator.mean() - 0.60).abs() < 1e-6);
        assert!((rec.accumulator.min - 0.60).abs() < 1e-6);
        assert!((rec.accumulator.max - 0.60).abs() < 1e-6);
        assert!(rec.accumulator.count > 0.0);
    }
}

#[test]
fn test_spectral_formula_ndwi_on_the_fly() {
    // G=100, NIR=200
    // Expected NDWI = (Green - NIR) / (Green + NIR) = (100 - 200) / (100 + 200) = -100 / 300 = -0.3333333...
    let temp_raster = create_test_rgba_geotiff(64, 64, 50, 100, 150, 200);
    let raster_path = temp_raster.path();

    let reader = GeoTiffStreamReader::open(raster_path).unwrap();

    let config = MultiResolutionConfig {
        resolutions: vec![8],
        spectral_formula: Some(SpectralFormula::Ndwi {
            green_band: 2,
            nir_band: 4,
        }),
        ..Default::default()
    };

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let records = streamer.fetch_next_batch(1000);
    assert!(!records.is_empty());
    for rec in &records {
        assert!((rec.accumulator.mean() - (-1.0 / 3.0)).abs() < 1e-6);
    }
}

#[test]
fn test_continuous_predicate_pushdown_min_count_and_mean() {
    // Values gradient: col + row
    let temp_raster = create_test_gray8_geotiff(64, 64, |c, r| ((c + r) % 200) as u8);
    let raster_path = temp_raster.path();

    // Baseline: unconstrained
    let reader_base = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_base = MultiResolutionConfig {
        resolutions: vec![8],
        ..Default::default()
    };
    let mut streamer_base = MultiScanHorizonStreamer::new(reader_base, &config_base).unwrap();
    let mut baseline_records = Vec::new();
    loop {
        let b = streamer_base.fetch_next_batch(200);
        if b.is_empty() {
            break;
        }
        baseline_records.extend(b);
    }
    assert!(!baseline_records.is_empty());

    // 1. Predicate pushdown: min_count = 10.0
    let reader_count = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_count = MultiResolutionConfig {
        resolutions: vec![8],
        min_count: Some(10.0),
        ..Default::default()
    };
    let mut streamer_count = MultiScanHorizonStreamer::new(reader_count, &config_count).unwrap();
    let mut count_filtered = Vec::new();
    loop {
        let b = streamer_count.fetch_next_batch(200);
        if b.is_empty() {
            break;
        }
        count_filtered.extend(b);
    }
    for rec in &count_filtered {
        assert!(rec.accumulator.count >= 10.0);
    }
    assert!(count_filtered.len() <= baseline_records.len());

    // 2. Predicate pushdown: min_mean = 50.0, max_mean = 100.0
    let reader_mean = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_mean = MultiResolutionConfig {
        resolutions: vec![8],
        min_mean: Some(50.0),
        max_mean: Some(100.0),
        ..Default::default()
    };
    let mut streamer_mean = MultiScanHorizonStreamer::new(reader_mean, &config_mean).unwrap();
    let mut mean_filtered = Vec::new();
    loop {
        let b = streamer_mean.fetch_next_batch(200);
        if b.is_empty() {
            break;
        }
        mean_filtered.extend(b);
    }
    for rec in &mean_filtered {
        assert!(rec.accumulator.mean() >= 50.0);
        assert!(rec.accumulator.mean() <= 100.0);
    }
    assert!(mean_filtered.len() < baseline_records.len());
}

#[test]
fn test_categorical_predicate_pushdown_majority_fraction() {
    // Raster where left half is class 1 (uniform), right half alternates 2 and 3 at pixel level
    let temp_raster =
        create_test_gray8_geotiff(
            64,
            64,
            |c, r| {
                if c < 32 {
                    1
                } else {
                    ((c + r) % 2 + 2) as u8
                }
            },
        );
    let raster_path = temp_raster.path();

    // 1. Baseline unfiltered
    let reader_all = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_all = MultiResolutionConfig {
        resolutions: vec![8],
        ..Default::default()
    };
    let mut streamer_all = MultiCategoricalHorizonStreamer::new(reader_all, &config_all).unwrap();
    let mut all_records = Vec::new();
    loop {
        let b = streamer_all.fetch_next_batch(200);
        if b.is_empty() {
            break;
        }
        all_records.extend(b);
    }
    let mixed_cells = all_records
        .iter()
        .filter(|r| {
            let (_, _, frac) = r.accumulator.majority();
            frac < 0.9
        })
        .count();
    assert!(mixed_cells > 0, "Must have mixed cells in the right half");

    // 2. Filtered: min_majority_fraction = 0.9
    let reader = GeoTiffStreamReader::open(raster_path).unwrap();
    let config = MultiResolutionConfig {
        resolutions: vec![8],
        min_majority_fraction: Some(0.9),
        ..Default::default()
    };
    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut filtered = Vec::new();
    loop {
        let b = streamer.fetch_next_batch(200);
        if b.is_empty() {
            break;
        }
        filtered.extend(b);
    }
    assert!(!filtered.is_empty());
    assert!(filtered.len() < all_records.len());
    for rec in &filtered {
        let (_, _, frac) = rec.accumulator.majority();
        assert!(frac >= 0.9);
    }
}

#[test]
fn test_hierarchical_compaction_continuous_conservation() {
    // Large enough raster so many complete groups of 7 child hexagons exist at resolution 8
    let temp_raster = create_test_gray8_geotiff(128, 128, |c, r| ((c * 2 + r) % 150 + 20) as u8);
    let raster_path = temp_raster.path();

    // 1. Run uncompacted
    let reader_raw = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_raw = MultiResolutionConfig {
        resolutions: vec![8],
        compact: false,
        ..Default::default()
    };
    let mut streamer_raw = MultiScanHorizonStreamer::new(reader_raw, &config_raw).unwrap();
    let mut uncompacted_records = Vec::new();
    loop {
        let b = streamer_raw.fetch_next_batch(500);
        if b.is_empty() {
            break;
        }
        uncompacted_records.extend(b);
    }

    let uncompacted_count: f64 = uncompacted_records
        .iter()
        .map(|r| r.accumulator.count)
        .sum();
    let uncompacted_sum: f64 = uncompacted_records.iter().map(|r| r.accumulator.sum).sum();

    // 2. Run with compaction enabled (compact := true)
    let reader_comp = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_comp = MultiResolutionConfig {
        resolutions: vec![8],
        compact: true,
        ..Default::default()
    };
    let mut streamer_comp = MultiScanHorizonStreamer::new(reader_comp, &config_comp).unwrap();
    let mut compacted_records = Vec::new();
    loop {
        let b = streamer_comp.fetch_next_batch(500);
        if b.is_empty() {
            break;
        }
        compacted_records.extend(b);
    }

    // Number of records should be strictly less due to 7:1 compaction
    assert!(compacted_records.len() < uncompacted_records.len());

    // Check that some records were compacted to resolution 7!
    let compacted_to_res_7 = compacted_records
        .iter()
        .filter(|r| r.resolution == 7)
        .count();
    assert!(
        compacted_to_res_7 > 0,
        "Expected at least some complete parent cells at res 7"
    );

    // Total pixel count and pixel sum MUST BE 100% CONSERVED!
    let compacted_count: f64 = compacted_records.iter().map(|r| r.accumulator.count).sum();
    let compacted_sum: f64 = compacted_records.iter().map(|r| r.accumulator.sum).sum();

    assert!(
        (compacted_count - uncompacted_count).abs() < 1e-5,
        "Count conservation failed"
    );
    assert!(
        (compacted_sum - uncompacted_sum).abs() < 1e-5,
        "Sum conservation failed"
    );
}

#[test]
fn test_hierarchical_compaction_categorical_conservation() {
    let temp_raster = create_test_gray8_geotiff(128, 128, |c, r| ((c / 8 + r / 8) % 5 + 1) as u8);
    let raster_path = temp_raster.path();

    // 1. Uncompacted
    let reader_raw = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_raw = MultiResolutionConfig {
        resolutions: vec![8],
        compact: false,
        ..Default::default()
    };
    let mut streamer_raw = MultiCategoricalHorizonStreamer::new(reader_raw, &config_raw).unwrap();
    let mut uncompacted = Vec::new();
    loop {
        let b = streamer_raw.fetch_next_batch(500);
        if b.is_empty() {
            break;
        }
        uncompacted.extend(b);
    }

    let raw_total_pixels: f64 = uncompacted.iter().map(|r| r.accumulator.total_count).sum();

    // 2. Compacted
    let reader_comp = GeoTiffStreamReader::open(raster_path).unwrap();
    let config_comp = MultiResolutionConfig {
        resolutions: vec![8],
        compact: true,
        ..Default::default()
    };
    let mut streamer_comp =
        MultiCategoricalHorizonStreamer::new(reader_comp, &config_comp).unwrap();
    let mut compacted = Vec::new();
    loop {
        let b = streamer_comp.fetch_next_batch(500);
        if b.is_empty() {
            break;
        }
        compacted.extend(b);
    }

    assert!(compacted.len() < uncompacted.len());
    let res_7_count = compacted.iter().filter(|r| r.resolution == 7).count();
    assert!(res_7_count > 0);

    let comp_total_pixels: f64 = compacted.iter().map(|r| r.accumulator.total_count).sum();
    assert!((comp_total_pixels - raw_total_pixels).abs() < 1e-5);
}

#[test]
fn test_spectral_formula_nbr_on_the_fly() {
    // 4-band raster: R=50, G=100, SWIR/B=60, NIR/A=140
    // Expected NBR = (NIR - SWIR) / (NIR + SWIR) = (140 - 60) / (140 + 60) = 80 / 200 = 0.40
    let temp_raster = create_test_rgba_geotiff(64, 64, 50, 100, 60, 140);
    let raster_path = temp_raster.path();

    let reader = GeoTiffStreamReader::open(raster_path).unwrap();

    let config = MultiResolutionConfig {
        resolutions: vec![8],
        spectral_formula: Some(SpectralFormula::Nbr {
            nir_band: 4,
            swir_band: 3,
        }),
        ..Default::default()
    };

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let records = streamer.fetch_next_batch(1000);
    assert!(!records.is_empty());
    for rec in &records {
        assert!((rec.accumulator.mean() - 0.40).abs() < 1e-6);
        assert!((rec.accumulator.min - 0.40).abs() < 1e-6);
        assert!((rec.accumulator.max - 0.40).abs() < 1e-6);
    }
}

#[test]
fn test_spectral_formula_evi_on_the_fly() {
    // 4-band raster: R=20, G=30, B=10, NIR=80
    // Expected EVI = 2.5 * (NIR - Red) / (NIR + 6*Red - 7.5*Blue + 1.0)
    // Numerator = 2.5 * (80 - 20) = 150.0
    // Denominator = 80 + 6*20 - 7.5*10 + 1 = 80 + 120 - 75 + 1 = 126.0
    // Expected = 150.0 / 126.0 = 1.19047619...
    let temp_raster = create_test_rgba_geotiff(64, 64, 20, 30, 10, 80);
    let raster_path = temp_raster.path();

    let reader = GeoTiffStreamReader::open(raster_path).unwrap();

    let config = MultiResolutionConfig {
        resolutions: vec![8],
        spectral_formula: Some(SpectralFormula::Evi {
            nir_band: 4,
            red_band: 1,
            blue_band: 3,
        }),
        ..Default::default()
    };

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let records = streamer.fetch_next_batch(1000);
    assert!(!records.is_empty());
    let expected_evi = 150.0 / 126.0;
    for rec in &records {
        assert!((rec.accumulator.mean() - expected_evi).abs() < 1e-5);
    }
}

#[test]
fn test_spectral_formula_zero_denominator_and_nodata_resilience() {
    // 4-band raster where NIR=0 and Red=0 everywhere (denominator NIR + Red == 0)
    let temp_raster = create_test_rgba_geotiff(64, 64, 0, 0, 0, 0);
    let raster_path = temp_raster.path();

    let reader = GeoTiffStreamReader::open(raster_path).unwrap();

    let config = MultiResolutionConfig {
        resolutions: vec![8],
        spectral_formula: Some(SpectralFormula::Ndvi {
            nir_band: 4,
            red_band: 1,
        }),
        ..Default::default()
    };

    // Streamer must handle 0/0 gracefully without NaN, Inf, or panicking
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut records = Vec::new();
    loop {
        let b = streamer.fetch_next_batch(500);
        if b.is_empty() {
            break;
        }
        records.extend(b);
    }
    // Because all pixels have denom = 0, they are all skipped, resulting in 0 valid pixel accumulations
    for rec in &records {
        assert_eq!(rec.accumulator.count, 0.0);
    }
}

#[test]
fn test_spectral_formula_out_of_bounds_band_clamping() {
    // Request Band 99 on a 4-band raster. Clamped safely to spp-1 (Band 4)
    let temp_raster = create_test_rgba_geotiff(64, 64, 50, 100, 150, 200);
    let raster_path = temp_raster.path();

    let reader = GeoTiffStreamReader::open(raster_path).unwrap();

    let config = MultiResolutionConfig {
        resolutions: vec![8],
        spectral_formula: Some(SpectralFormula::Ndvi {
            nir_band: 99,
            red_band: 1,
        }),
        ..Default::default()
    };

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let records = streamer.fetch_next_batch(1000);
    assert!(!records.is_empty());
    // Band 99 is clamped to Band 4 (NIR=200), Red is Band 1 (Red=50) -> NDVI = 0.60
    for rec in &records {
        assert!((rec.accumulator.mean() - 0.60).abs() < 1e-6);
    }
}
