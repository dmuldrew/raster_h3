//! Generated invariants complement fixed geospatial regression fixtures.

use std::collections::HashSet;

use h3o::{LatLng, Resolution};
use proptest::prelude::*;
use raster_h3_core::aggregator::accumulator::H3Accumulator;
use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::crs::transformer::CrsTransformer;
use raster_h3_core::encoding::{cell_to_wkb, wkb_reader::wkb_to_multipolygon, WKB_BUF_LEN};
use raster_h3_core::raster::{geotiff::GeoTiffStreamReader, geotransform::GeoTransform};

use crate::helpers::TestGeoTiffBuilder;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn weighted_merge_matches_single_pass(
        values in prop::collection::vec((-10_000i32..10_000, 1u8..5), 1..128),
        split in 0usize..128,
    ) {
        let split = split % (values.len() + 1);
        let mut whole = H3Accumulator::default();
        let mut left = H3Accumulator::default();
        let mut right = H3Accumulator::default();
        for (i, &(value, weight)) in values.iter().enumerate() {
            let (value, weight) = (f64::from(value), f64::from(weight) / 4.0);
            whole.update_weighted(value, weight);
            if i < split { left.update_weighted(value, weight); }
            else { right.update_weighted(value, weight); }
        }
        left.merge(&right);
        prop_assert_eq!(left.count, whole.count);
        prop_assert_eq!(left.sum, whole.sum);
        prop_assert_eq!(left.min, whole.min);
        prop_assert_eq!(left.max, whole.max);
        prop_assert!((left.m2 - whole.m2).abs() <= whole.m2.abs().max(1.0) * 1e-10);
    }

    #[test]
    fn generated_wkb_roundtrips_and_rejects_truncation(
        lat in -89.0f64..89.0, lon in -180.0f64..180.0, res in 0u8..16, cut in any::<u16>(),
    ) {
        let cell = LatLng::new(lat, lon).unwrap().to_cell(Resolution::try_from(res).unwrap());
        let mut buf = [0; WKB_BUF_LEN];
        let len = cell_to_wkb(cell, &mut buf);
        let mut bytes = buf[..len].to_vec();
        prop_assert!(wkb_to_multipolygon(&bytes).is_ok());
        let truncated_len = usize::from(cut) % bytes.len();
        prop_assert!(wkb_to_multipolygon(&bytes[..truncated_len]).is_err());
        bytes.push(0);
        prop_assert!(wkb_to_multipolygon(&bytes).is_err());
    }

    #[test]
    fn affine_inverse_roundtrips(
        lon in -180.0f64..180.0, lat in -85.0f64..85.0,
        scale in 0.001f64..1.0, b in -0.0001f64..0.0001, d in -0.0001f64..0.0001,
        col in -1024.0f64..1024.0, row in -1024.0f64..1024.0,
    ) {
        let gt = GeoTransform::from_gdal_array([lon, scale, b, lat, d, -scale]);
        let (x, y) = gt.pixel_to_coord(col, row);
        let (back_col, back_row) = gt.coord_to_pixel(x, y).unwrap();
        prop_assert!((back_col - col).abs() < 1e-7);
        prop_assert!((back_row - row).abs() < 1e-7);
    }

    #[test]
    fn mercator_inverse_roundtrips(lat in -85.0f64..85.0, lon in -180.0f64..180.0) {
        let tf = CrsTransformer::from_crs_or_epsg(Some(3857), None).unwrap();
        let (x, y) = tf.wgs84_to_source(lon, lat).unwrap();
        let (back_lon, back_lat) = tf.transform_point(x, y).unwrap();
        prop_assert!((back_lon - lon).abs() < 1e-8);
        prop_assert!((back_lat - lat).abs() < 1e-8);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn streaming_conserves_pixels_across_antimeridian(
        width in 1u32..24, height in 1u32..24,
        lon in prop_oneof![179.5f64..180.5, -180.5f64..-179.5], lat in -70.0f64..70.0,
        res in 2u8..10, supersample in any::<bool>(),
    ) {
        let value = |col: u32, row: u32| {
            if (col + row).is_multiple_of(7) { f32::NAN } else { (col + row) as f32 }
        };
        let (_file, path) = TestGeoTiffBuilder::new(width, height)
            .origin(lon, lat).pixel_size(0.1).create_f32_tempfile(value);
        let mut config = MultiResolutionConfig::single(res);
        if supersample { config.sampling = SamplingPattern::rgss(); }
        let mut stream = MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config).unwrap();
        let (mut count, mut sum) = (0.0, 0.0);
        let mut seen = HashSet::new();
        loop {
            let batch = stream.fetch_next_batch(3).unwrap();
            if batch.is_empty() { break; }
            for rec in batch {
                prop_assert!(seen.insert(rec.h3_index));
                count += rec.accumulator.count;
                sum += rec.accumulator.sum;
            }
        }
        let expected: Vec<f64> = (0..height).flat_map(|r| (0..width).map(move |c| value(c, r)))
            .filter(|v| v.is_finite()).map(f64::from).collect();
        prop_assert!((count - expected.len() as f64).abs() < 1e-6);
        prop_assert!((sum - expected.iter().sum::<f64>()).abs() < 1e-6);
    }
}
