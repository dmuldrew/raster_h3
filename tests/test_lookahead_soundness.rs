//! Lookahead soundness and precondition validation tests.
//!
//! Verifies that:
//! 1. H3ScanlineLookahead and H3SpanOptimizer fall back to exact per-sample lookup
//!    outside certified regimes:
//!    - Coarse H3 resolutions (res < 4)
//!    - High latitude (|lat| >= 70°)
//!    - Antimeridian proximity (|lon| > 175°)
//!    - Projected CRSs (UTM, Albers)
//! 2. Streaming aggregation matches exhaustive direct per-sample H3 indexing across all these regimes.
//! 3. H3ScanlineLookahead::find_core_span certifies every intermediate pixel and rejects the core
//!    span if any corner of an intermediate pixel belongs to another H3 cell.

mod helpers;

use h3o::{LatLng, Resolution};
use helpers::TestGeoTiffBuilder;
use raster_h3::aggregator::accumulator::H3Accumulator;
use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::aggregator::H3ScanlineLookahead;
use raster_h3::crs::transformer::CrsTransformer;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use std::collections::HashMap;

#[test]
fn test_core_span_certifies_all_intermediate_pixels() {
    let res = Resolution::Eight;
    let lookahead = H3ScanlineLookahead::for_resolution(res);

    // 1. All pixels in [0, 10) have all corners in cell:
    // Should certify core span [1, 9).
    let (core_s, core_e) =
        lookahead.find_core_span(0, 10, (-0.5, 0.5), (-0.5, 0.5), |_px, _py| true);
    assert_eq!(core_s, 1);
    assert_eq!(core_e, 9);

    // 2. Endpoints (pixel 1 and pixel 8) are core, but intermediate pixel 5 fails:
    // With intermediate pixel failing, certification must fail and fall back to (10, 10).
    let (rejected_s, rejected_e) =
        lookahead.find_core_span(0, 10, (-0.5, 0.5), (-0.5, 0.5), |px, _py| {
            // Pixel 5 fails corner check
            !(4.5..=5.5).contains(&px)
        });
    assert_eq!(rejected_s, 10);
    assert_eq!(rejected_e, 10);
}

#[test]
fn test_coarse_resolutions_match_exhaustive_reference() {
    // Res 0 to 3 must fall back to exact per-sample indexing without span shortcuts.
    for res_val in [0, 1, 2, 3] {
        let width = 16u32;
        let height = 16u32;
        let (_file, path) = TestGeoTiffBuilder::new(width, height)
            .origin(-122.5, 38.0)
            .pixel_size(0.1) // 0.1 degree pixels
            .epsg(4326)
            .create_f32_tempfile(|c, r| (c * 3 + r * 5 + 1) as f32);

        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let gt = reader.metadata.geotransform;
        let h3_res = Resolution::try_from(res_val).unwrap();

        // Ground truth via exact per-pixel transformation
        let mut expected: HashMap<u64, H3Accumulator> = HashMap::new();
        for r in 0..height {
            for c in 0..width {
                let val = (c * 3 + r * 5 + 1) as f64;
                let (lon, lat) = gt.pixel_to_coord(c as f64 + 0.5, r as f64 + 0.5);
                let cell: u64 = LatLng::new(lat, lon).unwrap().to_cell(h3_res).into();
                expected.entry(cell).or_default().update(val);
            }
        }

        let mut config = MultiResolutionConfig::single(res_val);
        config.sampling = SamplingPattern::center();
        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

        let mut actual: HashMap<u64, H3Accumulator> = HashMap::new();
        while !streamer.is_finished() {
            let batch = streamer.fetch_next_batch(16).unwrap();
            for rec in batch {
                actual.insert(rec.h3_index, rec.accumulator);
            }
        }

        assert_eq!(
            actual.len(),
            expected.len(),
            "Res {res_val} cell count mismatch"
        );
        for (cell, exp_acc) in &expected {
            let act_acc = actual
                .get(cell)
                .unwrap_or_else(|| panic!("Res {res_val}: missing cell {cell:#x}"));
            assert!(
                (act_acc.count - exp_acc.count).abs() < 1e-6,
                "Res {res_val}: count mismatch on cell {cell:#x}"
            );
            assert!(
                (act_acc.sum - exp_acc.sum).abs() < 1e-4,
                "Res {res_val}: sum mismatch on cell {cell:#x}"
            );
        }
    }
}

#[test]
fn test_high_latitude_matches_exhaustive_reference() {
    // At lat >= 70°, span shortcuts must be disabled and fall back to exact per-sample lookup.
    for lat_origin in [75.0, 82.0] {
        let width = 24u32;
        let height = 24u32;
        let (_file, path) = TestGeoTiffBuilder::new(width, height)
            .origin(15.0, lat_origin)
            .pixel_size(0.02)
            .epsg(4326)
            .create_f32_tempfile(|c, r| (c + r * 2 + 10) as f32);

        let reader = GeoTiffStreamReader::open(&path).unwrap();
        let gt = reader.metadata.geotransform;
        let h3_res = Resolution::Eight;

        let mut expected: HashMap<u64, H3Accumulator> = HashMap::new();
        for r in 0..height {
            for c in 0..width {
                let val = (c + r * 2 + 10) as f64;
                let (lon, lat) = gt.pixel_to_coord(c as f64 + 0.5, r as f64 + 0.5);
                let cell: u64 = LatLng::new(lat, lon).unwrap().to_cell(h3_res).into();
                expected.entry(cell).or_default().update(val);
            }
        }

        let mut config = MultiResolutionConfig::single(8);
        config.sampling = SamplingPattern::center();
        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

        let mut actual: HashMap<u64, H3Accumulator> = HashMap::new();
        while !streamer.is_finished() {
            let batch = streamer.fetch_next_batch(16).unwrap();
            for rec in batch {
                actual.insert(rec.h3_index, rec.accumulator);
            }
        }

        assert_eq!(
            actual.len(),
            expected.len(),
            "High-lat {lat_origin} cell count mismatch"
        );
        for (cell, exp_acc) in &expected {
            let act_acc = actual
                .get(cell)
                .unwrap_or_else(|| panic!("High-lat {lat_origin}: missing cell {cell:#x}"));
            assert!((act_acc.count - exp_acc.count).abs() < 1e-6);
            assert!((act_acc.sum - exp_acc.sum).abs() < 1e-4);
        }
    }
}

#[test]
fn test_antimeridian_proximity_matches_exhaustive_reference() {
    // Rasters close to or crossing +/- 180° longitude must fall back to exact per-sample lookup.
    let width = 32u32;
    let height = 16u32;
    // Origin at 179.5°, step 0.03° -> spans from 179.5° to 180.46° (crossing antimeridian)
    let (_file, path) = TestGeoTiffBuilder::new(width, height)
        .origin(179.5, 20.0)
        .pixel_size(0.03)
        .epsg(4326)
        .create_f32_tempfile(|c, r| (c * 2 + r + 1) as f32);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let gt = reader.metadata.geotransform;
    let h3_res = Resolution::Seven;

    let mut expected: HashMap<u64, H3Accumulator> = HashMap::new();
    for r in 0..height {
        for c in 0..width {
            let val = (c * 2 + r + 1) as f64;
            let (mut lon, lat) = gt.pixel_to_coord(c as f64 + 0.5, r as f64 + 0.5);
            if lon > 180.0 {
                lon -= 360.0;
            } else if lon < -180.0 {
                lon += 360.0;
            }
            let cell: u64 = LatLng::new(lat, lon).unwrap().to_cell(h3_res).into();
            expected.entry(cell).or_default().update(val);
        }
    }

    let mut config = MultiResolutionConfig::single(7);
    config.sampling = SamplingPattern::center();
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let mut actual: HashMap<u64, H3Accumulator> = HashMap::new();
    while !streamer.is_finished() {
        let batch = streamer.fetch_next_batch(16).unwrap();
        for rec in batch {
            actual.insert(rec.h3_index, rec.accumulator);
        }
    }

    assert_eq!(
        actual.len(),
        expected.len(),
        "Antimeridian cell count mismatch"
    );
    for (cell, exp_acc) in &expected {
        let act_acc = actual
            .get(cell)
            .unwrap_or_else(|| panic!("Antimeridian: missing cell {cell:#x}"));
        assert!((act_acc.count - exp_acc.count).abs() < 1e-6);
        assert!((act_acc.sum - exp_acc.sum).abs() < 1e-4);
    }
}

#[test]
fn test_projected_utm_with_supersampling_matches_exhaustive_reference() {
    // In projected CRSs (UTM zone 10 EPSG 32610), span shortcuts are bypassed.
    let width = 20u32;
    let height = 20u32;
    let (_file, path) = TestGeoTiffBuilder::new(width, height)
        .origin(500000.0, 4180000.0)
        .pixel_size(30.0)
        .epsg(32610)
        .create_f32_tempfile(|c, r| (c * 3 + r * 2 + 5) as f32);

    let pattern = SamplingPattern::five_point();
    let res = Resolution::Nine;

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let gt = reader.metadata.geotransform;
    let crs_trans = CrsTransformer::from_crs_or_epsg(Some(32610), None).unwrap();

    let mut expected: HashMap<u64, H3Accumulator> = HashMap::new();
    for r in 0..height {
        for c in 0..width {
            let val = (c * 3 + r * 2 + 5) as f64;
            for sp in &pattern.points {
                let (x, y) = gt.pixel_to_coord(c as f64 + sp.dx, r as f64 + sp.dy);
                let (lon, lat) = crs_trans.transform_point(x, y).unwrap();
                let cell: u64 = LatLng::new(lat, lon).unwrap().to_cell(res).into();
                expected
                    .entry(cell)
                    .or_default()
                    .update_weighted(val, sp.weight);
            }
        }
    }

    let mut config = MultiResolutionConfig::single(9);
    config.sampling = pattern;
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let mut actual: HashMap<u64, H3Accumulator> = HashMap::new();
    while !streamer.is_finished() {
        let batch = streamer.fetch_next_batch(16).unwrap();
        for rec in batch {
            actual.insert(rec.h3_index, rec.accumulator);
        }
    }

    assert_eq!(actual.len(), expected.len(), "UTM cell count mismatch");
    for (cell, exp_acc) in &expected {
        let act_acc = actual
            .get(cell)
            .unwrap_or_else(|| panic!("UTM: missing cell {cell:#x}"));
        assert!((act_acc.count - exp_acc.count).abs() < 1e-5);
        assert!((act_acc.sum - exp_acc.sum).abs() < 1e-3);
    }
}
