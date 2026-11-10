//! Generated invariants complement fixed geospatial regression fixtures.

use std::collections::HashSet;

use h3o::{LatLng, Resolution};
use proptest::prelude::*;
use raster_h3_core::aggregator::accumulator::H3Accumulator;
use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::aggregator::quantiles::QuantileSketch;
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn accumulator_merge_is_associative_and_commutative(
        va in prop::collection::vec((-10_000i32..10_000, 1u8..5), 0..64),
        vb in prop::collection::vec((-10_000i32..10_000, 1u8..5), 0..64),
        vc in prop::collection::vec((-10_000i32..10_000, 1u8..5), 0..64),
    ) {
        let build = |values: &[(i32, u8)]| {
            let mut acc = H3Accumulator::default();
            for &(v, w) in values {
                acc.update_weighted(f64::from(v), f64::from(w) / 4.0);
            }
            acc
        };
        let (a, b, c) = (build(&va), build(&vb), build(&vc));

        let mut ab_c = a.clone();
        ab_c.merge(&b);
        let mut ab_c2 = ab_c.clone();
        ab_c2.merge(&c);
        let mut bc = b.clone();
        bc.merge(&c);
        let mut a_bc = a.clone();
        a_bc.merge(&bc);
        let mut ba = b.clone();
        ba.merge(&a);

        for (x, y) in [(&ab_c2, &a_bc), (&ab_c, &ba)] {
            prop_assert_eq!(x.count, y.count);
            prop_assert_eq!(x.sum, y.sum);
            prop_assert_eq!(x.sum_w2, y.sum_w2);
            prop_assert_eq!(x.min, y.min);
            prop_assert_eq!(x.max, y.max);
            prop_assert!((x.m2 - y.m2).abs() <= y.m2.abs().max(1.0) * 1e-9);
        }
    }

    #[test]
    fn sketch_merge_matches_single_stream(
        values in prop::collection::vec(
            (prop_oneof![-1e6f64..1e6, -1e-20f64..1e-20, Just(0.0)], 1u8..5),
            1..200,
        ),
        split in 0usize..200,
    ) {
        let split = split % (values.len() + 1);
        let mut single = QuantileSketch::new();
        let mut left = QuantileSketch::new();
        let mut right = QuantileSketch::new();
        for (i, &(v, w)) in values.iter().enumerate() {
            let weight = f64::from(w) / 4.0;
            single.update(v, weight);
            if i < split { left.update(v, weight); }
            else { right.update(v, weight); }
        }
        left.merge(&right);
        prop_assert_eq!(left, single);
    }

    #[test]
    fn sketch_quantile_is_within_relative_error(
        values in prop::collection::vec(
            (1.0f64..10.0, -30i32..30).prop_map(|(m, e)| m * 10f64.powi(e)),
            1..200,
        ),
    ) {
        let mut sketch = QuantileSketch::new();
        for &v in &values {
            sketch.update(v, 1.0);
        }
        let mut s = values.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        let n = s.len();
        for q in [0.1, 0.5, 0.9] {
            let x = s[((q * n as f64).ceil() as usize).clamp(1, n) - 1];
            let e = sketch.quantile(q, s[0], s[n - 1]);
            prop_assert!((e - x).abs() <= 0.021 * x);
        }
    }

    #[test]
    fn quantiles_batch_matches_quantile(
        values in prop::collection::vec(
            prop_oneof![-1e6f64..1e6, -1e-20f64..1e-20, Just(0.0)],
            1..200,
        ),
        targets in prop::collection::vec(-0.1f64..1.1, 0..8),
    ) {
        let mut sketch = QuantileSketch::new();
        for &v in &values {
            sketch.update(v, 1.0);
        }
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let batched = sketch.quantiles_batch(&targets, min, max);
        prop_assert_eq!(batched.len(), targets.len());
        for (i, &q) in targets.iter().enumerate() {
            prop_assert_eq!(
                batched[i].to_bits(),
                sketch.quantile(q, min, max).to_bits()
            );
        }
    }

    #[test]
    fn ipc_stream_roundtrips(
        batches in prop::collection::vec(
            prop::collection::vec(
                (
                    any::<u64>(), any::<i64>(), any::<u8>(), -1e12f64..1e12,
                    any::<bool>(), ".{0,12}", prop::collection::vec(any::<u8>(), 0..12),
                    any::<bool>(), any::<bool>(),
                ),
                0..20,
            ),
            1..4,
        ),
    ) {
        use raster_h3_core::columnar::ipc::{read_ipc_stream, write_ipc_stream};
        use raster_h3_core::columnar::{
            ArrowDataType, ColumnArray, ColumnField, ColumnSchema, RecordBatch,
        };
        use std::sync::Arc;

        let schema = Arc::new(ColumnSchema::new(vec![
            ColumnField::new("u64", ArrowDataType::UInt64, false),
            ColumnField::new("i64", ArrowDataType::Int64, false),
            ColumnField::new("u8", ArrowDataType::UInt8, false),
            ColumnField::new("f64", ArrowDataType::Float64, true),
            ColumnField::new("flag", ArrowDataType::Boolean, false),
            ColumnField::new("name", ArrowDataType::Utf8, true),
            ColumnField::new("blob", ArrowDataType::Binary, false),
        ]));
        let batches: Vec<RecordBatch> = batches
            .iter()
            .map(|rows| {
                let mut u64s = Vec::new();
                let mut i64s = Vec::new();
                let mut u8s = Vec::new();
                let mut f64s = Vec::new();
                let mut bools = Vec::new();
                let mut names = Vec::new();
                let mut blobs = Vec::new();
                let mut f64_validity = Vec::new();
                let mut name_validity = Vec::new();
                for (u, i, b, f, flag, name, blob, fv, nv) in rows {
                    u64s.push(*u);
                    i64s.push(*i);
                    u8s.push(*b);
                    f64s.push(*f);
                    bools.push(*flag);
                    names.push(name.clone());
                    blobs.push(blob.clone());
                    f64_validity.push(*fv);
                    name_validity.push(*nv);
                }
                RecordBatch::try_new(
                    Arc::clone(&schema),
                    vec![
                        ColumnArray::UInt64 { values: u64s, validity: None },
                        ColumnArray::Int64 { values: i64s, validity: None },
                        ColumnArray::UInt8 { values: u8s, validity: None },
                        ColumnArray::Float64 {
                            values: f64s,
                            validity: Some(f64_validity),
                        },
                        ColumnArray::Boolean { values: bools, validity: None },
                        ColumnArray::Utf8 {
                            values: names,
                            validity: Some(name_validity),
                        },
                        ColumnArray::Binary { values: blobs, validity: None },
                    ],
                )
                .unwrap()
            })
            .collect();

        let mut buf = Vec::new();
        write_ipc_stream(&mut buf, &schema, batches.clone().into_iter().map(Ok)).unwrap();
        let (out_schema, out_batches) = read_ipc_stream(&mut std::io::Cursor::new(buf)).unwrap();
        // write_ipc_stream skips empty batches
        let expected: Vec<RecordBatch> = batches
            .iter()
            .filter(|b| b.num_rows > 0)
            .cloned()
            .collect();
        prop_assert!(out_schema.as_ref() == schema.as_ref());
        prop_assert_eq!(out_batches, expected);
    }

    #[test]
    fn hilbert_tile_id_roundtrips(
        z in 0u8..=24,
        x in any::<u32>(),
        y in any::<u32>(),
    ) {
        use raster_h3_core::pmtiles::writer::{tile_id_to_zxy, zxy_to_tile_id};

        let mask = (1u32 << z) - 1;
        let (x, y) = (x & mask, y & mask);
        let id = zxy_to_tile_id(z, x, y);
        prop_assert_eq!(tile_id_to_zxy(id), (z, x, y));
        let acc = |z: u8| ((1u64 << (2 * z)) - 1) / 3;
        prop_assert!(id >= acc(z) && id < acc(z + 1));
    }
}
