//! Projected rasters aggregate exactly while uncertified bounds disable early eviction.

use std::collections::HashMap;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

use h3o::{LatLng, Resolution};
use raster_h3::aggregator::multi_horizon::coordinates::eviction_north_bound;
use raster_h3::aggregator::multi_horizon::{
    continuous_streamer::ContinuousKernel, MultiHorizonStreamer, MultiResolutionConfig,
};
use raster_h3::aggregator::{H3Accumulator, SamplingPattern};
use raster_h3::crs::transformer::CrsTransformer;
use raster_h3::raster::{geotiff::GeoTiffStreamReader, geotransform::GeoTransform, RasterChunk};

fn create_projected_fixture(
    epsg: u16,
    width: u32,
    height: u32,
    rows_per_strip: u32,
) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut encoder = TiffEncoder::new(std::fs::File::create(file.path()).unwrap()).unwrap();
    let mut image = encoder
        .new_image::<colortype::Gray32Float>(width, height)
        .unwrap();
    image.rows_per_strip(rows_per_strip).unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &[30.0, 30.0, 0.0][..])
        .unwrap();

    let tiepoint = if epsg == 5070 {
        // CONUS Albers coordinates in meters: pixel (0, 0) at (-100000, 1500000)
        [0.0, 0.0, 0.0, -100000.0, 1500000.0, 0.0]
    } else {
        // UTM Zone 10N coordinates in meters: pixel (0, 0) at (500000, 4180000)
        [0.0, 0.0, 0.0, 500000.0, 4180000.0, 0.0]
    };
    image
        .encoder()
        .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
        .unwrap();
    image
        .encoder()
        .write_tag(
            Tag::GeoKeyDirectoryTag,
            &[1u16, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, epsg][..],
        )
        .unwrap();

    let data: Vec<f32> = (0..height)
        .flat_map(|r| (0..width).map(move |c| ((c * 11 + r * 5) % 37) as f32 + 1.0))
        .collect();
    image.write_data(&data).unwrap();
    file
}

#[test]
fn utm_raster_retains_cells_without_a_certificate() {
    let file = create_projected_fixture(32610, 128, 96, 8); // 12 strips
    let reader = GeoTiffStreamReader::open(file.path()).unwrap();
    let gt = reader.metadata.geotransform;
    let crs = CrsTransformer::from_crs_or_epsg(Some(32610), None).unwrap();

    let mut config = MultiResolutionConfig::single(9);
    config.aggregation_budget_bytes = 64 * 1024 * 1024; // 64 MiB default
    let kernel = ContinuousKernel {
        band: 1,
        spectral_formula: None,
        min_count: None,
        min_mean: None,
        max_mean: None,
        track_quantiles: false,
    };

    let mut stream = MultiHorizonStreamer::new(reader, &config, kernel).unwrap();
    assert!(!stream.can_evict_early);

    let mut actual = HashMap::new();
    let first_batch = stream.fetch_next_batch(10).unwrap();
    assert!(
        !first_batch.is_empty(),
        "First batch should yield completed records"
    );
    assert!(
        stream.processed_chunk_count == stream.mosaic.chunk_refs.len(),
        "Uncertified projected output must wait until EOF"
    );

    for rec in first_batch {
        assert!(
            actual.insert(rec.h3_index, rec.accumulator).is_none(),
            "Duplicate cell emitted"
        );
    }

    loop {
        let batch = stream.fetch_next_batch(10).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            assert!(
                actual.insert(rec.h3_index, rec.accumulator).is_none(),
                "Duplicate cell emitted"
            );
        }
    }

    assert_eq!(
        stream.spill_run_count(),
        0,
        "No disk spills should occur on streaming UTM raster"
    );
    assert!(stream.is_finished());

    // Compute ground-truth reference in memory
    let mut expected = HashMap::<u64, H3Accumulator>::new();
    for r in 0..96 {
        for c in 0..128 {
            let val = ((c * 11 + r * 5) % 37) as f64 + 1.0;
            let (x, y) = gt.pixel_center_to_coord(c as usize, r as usize);
            let (lon, lat) = crs.transform_point(x, y).unwrap();
            let key = u64::from(LatLng::new(lat, lon).unwrap().to_cell(Resolution::Nine));
            expected.entry(key).or_default().update(val);
        }
    }

    assert_eq!(
        actual.len(),
        expected.len(),
        "Cell counts must match exactly"
    );
    let actual_sum: f64 = actual.values().map(|a| a.sum).sum();
    let expected_sum: f64 = expected.values().map(|a| a.sum).sum();
    assert!(
        (actual_sum - expected_sum).abs() < 1e-4,
        "Total sums must match"
    );

    for (key, actual_acc) in &actual {
        let expected_acc = &expected[key];
        assert_eq!(actual_acc.count, expected_acc.count);
        assert!((actual_acc.sum - expected_acc.sum).abs() < 1e-6);
        assert_eq!(actual_acc.min, expected_acc.min);
        assert_eq!(actual_acc.max, expected_acc.max);
    }
}

#[test]
fn albers_raster_evicts_early_soundly() {
    let file = create_projected_fixture(5070, 128, 96, 8); // 12 strips
    let reader = GeoTiffStreamReader::open(file.path()).unwrap();
    let gt = reader.metadata.geotransform;
    let crs = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();

    let mut config = MultiResolutionConfig::single(9);
    config.aggregation_budget_bytes = 64 * 1024 * 1024;
    let kernel = ContinuousKernel {
        band: 1,
        spectral_formula: None,
        min_count: None,
        min_mean: None,
        max_mean: None,
        track_quantiles: false,
    };

    let mut stream = MultiHorizonStreamer::new(reader, &config, kernel).unwrap();
    assert!(stream.can_evict_early);

    let mut actual = HashMap::new();
    let first_batch = stream.fetch_next_batch(10).unwrap();
    assert!(
        !first_batch.is_empty(),
        "First batch should yield completed records"
    );
    assert!(
        stream.processed_chunk_count < stream.mosaic.chunk_refs.len(),
        "Certified projected output should evict early before EOF"
    );

    for rec in first_batch {
        assert!(
            actual.insert(rec.h3_index, rec.accumulator).is_none(),
            "Duplicate cell emitted"
        );
    }

    loop {
        let batch = stream.fetch_next_batch(10).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            assert!(
                actual.insert(rec.h3_index, rec.accumulator).is_none(),
                "Duplicate cell emitted"
            );
        }
    }

    assert_eq!(
        stream.spill_run_count(),
        0,
        "No disk spills should occur on streaming Albers raster"
    );
    assert!(stream.is_finished());

    // Compute ground-truth reference in memory
    let mut expected = HashMap::<u64, H3Accumulator>::new();
    for r in 0..96 {
        for c in 0..128 {
            let val = ((c * 11 + r * 5) % 37) as f64 + 1.0;
            let (x, y) = gt.pixel_center_to_coord(c as usize, r as usize);
            let (lon, lat) = crs.transform_point(x, y).unwrap();
            let key = u64::from(LatLng::new(lat, lon).unwrap().to_cell(Resolution::Nine));
            expected.entry(key).or_default().update(val);
        }
    }

    assert_eq!(
        actual.len(),
        expected.len(),
        "Cell counts must match exactly"
    );
    let actual_sum: f64 = actual.values().map(|a| a.sum).sum();
    let expected_sum: f64 = expected.values().map(|a| a.sum).sum();
    assert!(
        (actual_sum - expected_sum).abs() < 1e-4,
        "Total sums must match"
    );

    for (key, actual_acc) in &actual {
        let expected_acc = &expected[key];
        assert_eq!(actual_acc.count, expected_acc.count);
        assert!((actual_acc.sum - expected_acc.sum).abs() < 1e-6);
        assert_eq!(actual_acc.min, expected_acc.min);
        assert_eq!(actual_acc.max, expected_acc.max);
    }
}

#[test]
fn eviction_north_bound_is_sound_over_curvature() {
    let chunk = RasterChunk {
        col_offset: 10,
        row_offset: 20,
        width: 64,
        height: 48,
    };
    for epsg in [32610, 5070] {
        let crs = CrsTransformer::from_crs_or_epsg(Some(epsg), None).unwrap();
        let gt = if epsg == 5070 {
            GeoTransform {
                c0: -100000.0,
                f0: 1500000.0,
                a: 30.0,
                b: 0.0,
                d: 0.0,
                e: -30.0,
            }
        } else {
            GeoTransform {
                c0: 500000.0,
                f0: 4180000.0,
                a: 30.0,
                b: 0.0,
                d: 0.0,
                e: -30.0,
            }
        };

        let upper = eviction_north_bound(&chunk, &gt, &crs);
        if epsg == 32610 {
            assert_eq!(upper, f64::INFINITY);
        } else {
            assert!(upper.is_finite(), "Analytical Albers bound must be finite");
        }

        // Dense sampling over the entire chunk area and perimeter
        for r in 20..68 {
            for c in 10..74 {
                for sp in SamplingPattern::rgss().points {
                    let (x, y) = gt.pixel_to_coord(c as f64 + sp.dx, r as f64 + sp.dy);
                    let (_, lat) = crs.transform_point(x, y).unwrap();
                    assert!(
                        lat <= upper,
                        "Sample latitude {} exceeded certified upper bound {} for EPSG {}",
                        lat,
                        upper,
                        epsg
                    );
                }
            }
        }
    }
}

#[test]
fn off_grid_pole_cannot_supply_a_finite_eviction_certificate() {
    let crs = CrsTransformer::from_crs_or_epsg(
        None,
        Some("+proj=stere +lat_0=90 +lat_ts=70 +lon_0=0 +datum=WGS84 +units=m"),
    )
    .unwrap();
    let gt = GeoTransform {
        c0: -550000.0,
        f0: 550000.0,
        a: 10000.0,
        e: -10000.0,
        b: 0.0,
        d: 0.0,
    };
    let chunk = RasterChunk {
        col_offset: 0,
        row_offset: 0,
        width: 120,
        height: 120,
    };
    // Even the former combined padding misses the pole between interior probes.
    assert!(crs.transform_rect_bounds(&gt, 0.0, 0.0, 120.0, 120.0)[3] + 0.005 < 90.0);
    assert_eq!(eviction_north_bound(&chunk, &gt, &crs), f64::INFINITY);
}

#[test]
fn large_polar_chunk_cannot_hide_a_pole_inside_its_boundary() {
    let crs = CrsTransformer::from_crs_or_epsg(
        None,
        Some("+proj=stere +lat_0=90 +lat_ts=70 +lon_0=0 +datum=WGS84 +units=m"),
    )
    .unwrap();
    let gt = GeoTransform {
        c0: -2000000.0,
        f0: 2000000.0,
        a: 10000.0,
        e: -10000.0,
        b: 0.0,
        d: 0.0,
    };
    let chunk = RasterChunk {
        col_offset: 0,
        row_offset: 0,
        width: 400,
        height: 400,
    };
    // Boundary latitudes are below 80 degrees, but the interior contains the pole.
    let upper = eviction_north_bound(&chunk, &gt, &crs);
    let (_, lat) = crs.transform_point(5000.0, -5000.0).unwrap();
    assert!(lat > 89.0);
    assert!(
        lat <= upper,
        "Interior latitude {lat} exceeds bound {upper}"
    );
    assert_eq!(upper, f64::INFINITY);
}

#[test]
fn albers_apex_inside_chunk_disables_boundary_certificate() {
    use raster_h3::crs::transformer::AlbersConicFast;
    let albers = AlbersConicFast::epsg_5070();
    let crs = CrsTransformer::AlbersConic(albers);
    let chunk = RasterChunk {
        col_offset: 0,
        row_offset: 0,
        width: 1000,
        height: 1000,
    };
    for rotation in [0.0, 2000.0] {
        let gt = GeoTransform {
            a: 10000.0,
            b: rotation,
            c0: -5000000.0 - rotation * 500.0,
            d: rotation,
            e: -10000.0,
            f0: albers.rho0 + 5000000.0 - rotation * 500.0,
        };
        let upper = eviction_north_bound(&chunk, &gt, &crs);
        // The unrotated case previously returned 69.43 for this 76.15 degree pixel.
        let (x, y) = gt.pixel_center_to_coord(500, 950);
        let (_, lat) = crs.transform_point(x, y).unwrap();
        assert!(lat > 70.0 && lat < 90.0);
        assert!(lat <= upper);
        assert_eq!(upper, f64::INFINITY);
    }
}
