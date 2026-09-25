//! Exhaustive comparisons with repeated spills, real strip boundaries and small output batches.
mod helpers;
use h3o::{LatLng, Resolution};
use raster_h3::aggregator::multi_horizon::coordinates::eviction_north_bound;
use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
    QuantileTarget,
};
use raster_h3::aggregator::{CategoricalAccumulator, H3Accumulator, SamplingPattern};
use raster_h3::crs::transformer::CrsTransformer;
use raster_h3::raster::{geotiff::GeoTiffStreamReader, geotransform::GeoTransform, RasterChunk};
use std::collections::HashMap;
use tiff::{
    encoder::{colortype, TiffEncoder},
    tags::Tag,
};

fn fixture(epsg: u16) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut encoder = TiffEncoder::new(std::fs::File::create(file.path()).unwrap()).unwrap();
    let mut image = encoder.new_image::<colortype::Gray32Float>(96, 64).unwrap();
    image.rows_per_strip(4).unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &[80.0, 80.0, 0.0][..])
        .unwrap();
    image
        .encoder()
        .write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, 500000.0, 4180000.0, 0.0][..],
        )
        .unwrap();
    image
        .encoder()
        .write_tag(
            Tag::GeoKeyDirectoryTag,
            &[1u16, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, epsg][..],
        )
        .unwrap();
    image
        .write_data(
            &(0..64)
                .flat_map(|r| (0..96).map(move |c| value(c, r)))
                .collect::<Vec<_>>(),
        )
        .unwrap();
    file
}
fn value(c: u32, r: u32) -> f32 {
    ((c * 7 + r * 3) % 31) as f32 - 15.0
}

#[test]
fn spills_preserve_continuous_quantiles_and_categories_across_projections() {
    for epsg in [32610, 5070] {
        let file = fixture(epsg);
        let reader = GeoTiffStreamReader::open(file.path()).unwrap();
        let gt = reader.metadata.geotransform;
        let crs = CrsTransformer::from_crs_or_epsg(Some(epsg as u32), None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut config = MultiResolutionConfig::new(vec![8, 9]);
        config.aggregation_budget_bytes = 128 * 1024;
        config.spill_directory = Some(dir.path().to_owned());
        config.quantiles = vec![QuantileTarget::Percentile(0.5, "p50".into())];
        config.sampling = SamplingPattern::rgss();
        let mut expected = HashMap::<(u8, u64), H3Accumulator>::new();
        let mut expected_cat = HashMap::<(u8, u64), CategoricalAccumulator>::new();
        for r in 0..64 {
            for c in 0..96 {
                for sp in &config.sampling.points {
                    let (x, y) = gt.pixel_to_coord(c as f64 + sp.dx, r as f64 + sp.dy);
                    let (lon, lat) = crs.transform_point(x, y).unwrap();
                    for res in [8u8, 9] {
                        let key = (
                            res,
                            u64::from(
                                LatLng::new(lat, lon)
                                    .unwrap()
                                    .to_cell(Resolution::try_from(res).unwrap()),
                            ),
                        );
                        expected
                            .entry(key)
                            .or_insert_with(H3Accumulator::with_quantiles)
                            .update_weighted(value(c, r) as f64, sp.weight);
                        expected_cat
                            .entry(key)
                            .or_default()
                            .update_weighted(value(c, r) as i64, sp.weight);
                    }
                }
            }
        }
        let mut stream = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        let mut actual = HashMap::new();
        loop {
            let batch = stream.fetch_next_batch(7).unwrap();
            assert!(batch.len() <= 7);
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                assert!(actual
                    .insert((rec.resolution, rec.h3_index), rec.accumulator)
                    .is_none());
            }
        }
        assert!(stream.spill_run_count() > 4);
        assert!(stream.peak_active_bytes() <= config.aggregation_budget_bytes);
        assert_eq!(actual.len(), expected.len());
        for (key, a) in actual {
            let e = &expected[&key];
            assert_eq!(a.count, e.count);
            assert_eq!(a.sum, e.sum);
            assert_eq!(a.min, e.min);
            assert_eq!(a.max, e.max);
            assert!((a.m2 - e.m2).abs() < 1e-7 * e.m2.max(1.0));
            assert_eq!(a.quantiles, e.quantiles);
        }
        drop(stream);
        let reader = GeoTiffStreamReader::open(file.path()).unwrap();
        let mut stream = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
        let mut actual = HashMap::new();
        loop {
            let batch = stream.fetch_next_batch(3).unwrap();
            assert!(batch.len() <= 3);
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                assert!(actual
                    .insert((rec.resolution, rec.h3_index), rec.accumulator)
                    .is_none());
            }
        }
        assert!(stream.spill_run_count() > 4);
        assert!(stream.peak_active_bytes() <= config.aggregation_budget_bytes);
        assert_eq!(actual.len(), expected_cat.len());
        for (key, a) in actual {
            let e = &expected_cat[&key];
            assert_eq!(a.total_count, e.total_count);
            e.for_each_class(|k, v| assert_eq!(v, a.get_class_count(k)));
        }
        drop(stream);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[test]
fn affine_chunk_bounds_include_every_sample_for_either_direction() {
    let chunk = RasterChunk {
        col_offset: 13,
        row_offset: 29,
        width: 19,
        height: 17,
    };
    for epsg in [4326, 3857] {
        let crs = CrsTransformer::from_crs_or_epsg(Some(epsg), None).unwrap();
        for e in [-0.2, 0.2] {
            for d in [-0.1, 0.1] {
                let gt = GeoTransform {
                    c0: 20.0,
                    f0: 30.0,
                    a: -0.02,
                    b: 0.03,
                    d,
                    e,
                };
                let upper = eviction_north_bound(&chunk, &gt, &crs);
                assert!(upper.is_finite());
                for r in 29..46 {
                    for c in 13..32 {
                        for sp in SamplingPattern::rgss().points {
                            let (x, y) = gt.pixel_to_coord(c as f64 + sp.dx, r as f64 + sp.dy);
                            assert!(crs.transform_point(x, y).unwrap().1 <= upper);
                        }
                    }
                }
            }
        }
    }
    let crs = CrsTransformer::from_crs_or_epsg(Some(32610), None).unwrap();
    let gt = GeoTransform {
        c0: 500000.0,
        f0: 4180000.0,
        a: 80.0,
        b: 0.0,
        d: 0.0,
        e: -80.0,
    };
    let upper = eviction_north_bound(&chunk, &gt, &crs);
    assert!(upper.is_finite());
    for r in 29..46 {
        for c in 13..32 {
            for sp in SamplingPattern::rgss().points {
                let (x, y) = gt.pixel_to_coord(c as f64 + sp.dx, r as f64 + sp.dy);
                assert!(crs.transform_point(x, y).unwrap().1 <= upper);
            }
        }
    }
}

#[test]
fn spill_io_failure_is_latched() {
    let file = fixture(32610);
    let dir = tempfile::tempdir().unwrap();
    let mut config = MultiResolutionConfig::single(9);
    config.aggregation_budget_bytes = 65536;
    config.quantiles = vec![QuantileTarget::Percentile(0.5, "p50".into())];
    config.sampling = SamplingPattern::rgss();
    config.spill_directory = Some(dir.path().join("missing-directory"));
    let mut stream =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(file.path()).unwrap(), &config)
            .unwrap();
    assert!(stream.fetch_next_batch(1).is_err());
    assert!(stream.fetch_next_batch(1).is_err());
    assert!(!stream.is_finished());
}

#[test]
fn rotated_and_reversed_grids_emit_before_eof_without_duplicates() {
    use raster_h3::aggregator::multi_horizon::{
        continuous_streamer::ContinuousKernel, MultiHorizonStreamer,
    };
    for e in [-0.002, 0.002] {
        let file = fixture(32610);
        let mut reader = GeoTiffStreamReader::open(file.path()).unwrap();
        let gt = GeoTransform {
            c0: -122.0,
            f0: 37.0,
            a: 0.002,
            b: 0.0001,
            d: 0.00005,
            e,
        };
        reader.metadata.geotransform = gt;
        let mut config = MultiResolutionConfig::single(9);
        config.custom_crs = Some("EPSG:4326".into());
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
        let first = stream.fetch_next_batch(7).unwrap();
        assert!(!first.is_empty());
        assert!(stream.processed_chunk_count < stream.mosaic.chunk_refs.len());
        for rec in first {
            assert!(actual.insert(rec.h3_index, rec.accumulator).is_none());
        }
        loop {
            let batch = stream.fetch_next_batch(7).unwrap();
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                assert!(actual.insert(rec.h3_index, rec.accumulator).is_none());
            }
        }
        assert_eq!(stream.spill_run_count(), 0);
        let mut expected = HashMap::<u64, H3Accumulator>::new();
        for r in 0..64 {
            for c in 0..96 {
                let (lon, lat) = gt.pixel_center_to_coord(c as usize, r as usize);
                let key = u64::from(LatLng::new(lat, lon).unwrap().to_cell(Resolution::Nine));
                expected.entry(key).or_default().update(value(c, r) as f64);
            }
        }
        assert_eq!(actual.len(), expected.len());
        for (key, acc) in actual {
            assert_eq!(acc.count, expected[&key].count);
            assert_eq!(acc.sum, expected[&key].sum);
        }
    }
}

#[test]
fn sorted_compaction_agrees_with_and_without_spilling() {
    let file = fixture(32610);
    let mut reference = None;
    for budget in [8 * 1024 * 1024, 65536] {
        let mut config = MultiResolutionConfig::single(9);
        config.aggregation_budget_bytes = budget;
        config.compact_h3_children = true;
        let mut stream =
            MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(file.path()).unwrap(), &config)
                .unwrap();
        let mut actual = HashMap::new();
        loop {
            let batch = stream.fetch_next_batch(1).unwrap();
            assert!(batch.len() <= 1);
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                assert!(actual
                    .insert((rec.resolution, rec.h3_index), rec.accumulator)
                    .is_none());
            }
        }
        if budget == 65536 {
            assert!(stream.spill_run_count() > 0);
        }
        assert_eq!(actual.values().map(|a| a.count).sum::<f64>(), 96.0 * 64.0);
        if let Some(expected) = &reference {
            let expected: &HashMap<(u8, u64), H3Accumulator> = expected;
            assert_eq!(actual.len(), expected.len());
            for (key, acc) in actual {
                let e = &expected[&key];
                assert_eq!(acc.count, e.count);
                assert_eq!(acc.sum, e.sum);
            }
        } else {
            reference = Some(actual);
        }
    }
}
