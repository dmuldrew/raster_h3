//! Tests the category remapping engine for categorical raster pipelines.
//!
//! Validates exact, range, list, and wildcard remapping syntax rules, NoData handling,
//! unmapped passthrough fallbacks, and integration with categorical horizon streaming.

mod helpers;

use std::collections::HashMap;
use std::sync::Arc;
use tempfile::NamedTempFile;

use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig,
};
use raster_h3::aggregator::remap::CategoryRemapper;
use raster_h3::raster::geotiff::GeoTiffStreamReader;

#[test]
fn test_remapper_syntax_parsing() {
    // 1. Basic exact mappings
    let r1 = CategoryRemapper::parse("1: 10, 2: 20").unwrap();
    assert_eq!(r1.remap(1), Some(10));
    assert_eq!(r1.remap(2), Some(20));
    assert_eq!(r1.remap(3), Some(3)); // PassThrough default

    // 2. Range mappings with .., ..=, and -
    let r2 = CategoryRemapper::parse("{101..105: 1, 121..=124: 2, 141-143: 3}").unwrap();
    assert_eq!(r2.remap(101), Some(1));
    assert_eq!(r2.remap(105), Some(1));
    assert_eq!(r2.remap(106), Some(106)); // PassThrough
    assert_eq!(r2.remap(121), Some(2));
    assert_eq!(r2.remap(124), Some(2));
    assert_eq!(r2.remap(141), Some(3));
    assert_eq!(r2.remap(143), Some(3));

    // 3. Bracketed lists and arrows => and ->
    let r3 = CategoryRemapper::parse("[10, 20, 30] => 1; 40 -> 2").unwrap();
    assert_eq!(r3.remap(10), Some(1));
    assert_eq!(r3.remap(20), Some(1));
    assert_eq!(r3.remap(30), Some(1));
    assert_eq!(r3.remap(40), Some(2));
    assert_eq!(r3.remap(50), Some(50));

    // 4. Null / Nodata dropping
    let r4 = CategoryRemapper::parse("99: null, 98: nodata, 97: none").unwrap();
    assert_eq!(r4.remap(99), None);
    assert_eq!(r4.remap(98), None);
    assert_eq!(r4.remap(97), None);
    assert_eq!(r4.remap(10), Some(10));

    // 5. Fallback unmapped actions
    let r5 = CategoryRemapper::parse("1..5: 1, else: null").unwrap();
    assert_eq!(r5.remap(3), Some(1));
    assert_eq!(r5.remap(100), None);

    let r6 = CategoryRemapper::parse("10: 1, default: 0").unwrap();
    assert_eq!(r6.remap(10), Some(1));
    assert_eq!(r6.remap(999), Some(0));

    // 6. JSON dictionary syntax
    let r7 = CategoryRemapper::parse(r#"{"101": 1, "102": 1, "99": null}"#).unwrap();
    assert_eq!(r7.remap(101), Some(1));
    assert_eq!(r7.remap(102), Some(1));
    assert_eq!(r7.remap(99), None);
}

#[test]
fn test_remapper_sparse_and_negative_categories() {
    let r = CategoryRemapper::parse("-1: 0, 100000..100005: 99, else: null").unwrap();
    assert_eq!(r.remap(-1), Some(0));
    assert_eq!(r.remap(100000), Some(99));
    assert_eq!(r.remap(100003), Some(99));
    assert_eq!(r.remap(5), None);
}

#[test]
fn test_remapper_invalid_syntax() {
    assert!(CategoryRemapper::parse("").is_err());
    assert!(CategoryRemapper::parse("   ").is_err());
    assert!(CategoryRemapper::parse("invalid_no_colon").is_err());
    assert!(CategoryRemapper::parse("10: abc").is_err());
}

fn create_categorical_test_raster(width: usize, height: usize) -> NamedTempFile {
    let (temp_file, _path) = helpers::TestGeoTiffBuilder::new(width as u32, height as u32)
        .origin(-122.45, 37.85)
        .pixel_size(0.001)
        .create_tempfile(|_c, r| {
            if r < 25 {
                101 + (r % 3) as u8
            } else if r < 50 {
                121 + (r % 2) as u8
            } else if r < 75 {
                99
            } else {
                42
            }
        });
    temp_file
}

#[test]
fn test_categorical_streamer_with_remapping() {
    let raster_file = create_categorical_test_raster(50, 100);
    let reader = GeoTiffStreamReader::open(raster_file.path()).unwrap();

    // Remapping spec:
    // 101..103 -> 1 (Shrub)
    // 121..122 -> 2 (Timber)
    // 99 -> null (Dropped)
    // 42 -> passes through unchanged
    let remapper =
        Arc::new(CategoryRemapper::parse("{101..103: 1, 121..122: 2, 99: null}").unwrap());

    let mut config = MultiResolutionConfig::single(8);
    config.remapper = Some(remapper);

    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();
    let mut total_pixels = 0.0;
    let mut class_counts: HashMap<i64, f64> = HashMap::new();

    loop {
        let batch = streamer.fetch_next_batch(64).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let acc = rec.accumulator;
            total_pixels += acc.total_count;
            acc.for_each_class(|cat, cnt| {
                *class_counts.entry(cat).or_insert(0.0) += cnt;
            });
        }
    }

    // Class 99 was 25 * 50 = 1250 pixels and should be DROPPED entirely
    // Remaining pixels: 75 * 50 = 3750
    assert_eq!(total_pixels, 3750.0);

    // Class 1 should have 25 * 50 = 1250 pixels
    assert_eq!(class_counts.get(&1).copied().unwrap_or(0.0), 1250.0);

    // Class 2 should have 25 * 50 = 1250 pixels
    assert_eq!(class_counts.get(&2).copied().unwrap_or(0.0), 1250.0);

    // Class 42 (unmapped, PassThrough) should have 25 * 50 = 1250 pixels
    assert_eq!(class_counts.get(&42).copied().unwrap_or(0.0), 1250.0);

    // None of 101, 102, 103, 121, 122, or 99 should exist in the accumulator!
    assert_eq!(class_counts.get(&101), None);
    assert_eq!(class_counts.get(&102), None);
    assert_eq!(class_counts.get(&103), None);
    assert_eq!(class_counts.get(&121), None);
    assert_eq!(class_counts.get(&122), None);
    assert_eq!(class_counts.get(&99), None);
}

#[test]
fn test_multi_categorical_horizon_streamer_with_remapping() {
    let raster_file = create_categorical_test_raster(50, 100);
    let reader = GeoTiffStreamReader::open(raster_file.path()).unwrap();

    // Remapping spec with fallback:
    // 101..103 -> 1
    // 121..122 -> 2
    // else -> null (drops 99 and 42!)
    let remapper =
        Arc::new(CategoryRemapper::parse("{101..103: 1, 121..122: 2, else: null}").unwrap());

    let mut multi_config = MultiResolutionConfig::new(vec![7, 8]);
    multi_config.remapper = Some(remapper);

    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &multi_config).unwrap();
    let mut counts_by_res: HashMap<u8, HashMap<i64, f64>> = HashMap::new();
    let mut total_pixels_by_res: HashMap<u8, f64> = HashMap::new();

    loop {
        let batch = streamer.fetch_next_batch(64).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            *total_pixels_by_res.entry(rec.resolution).or_insert(0.0) +=
                rec.accumulator.total_count;
            let class_map = counts_by_res
                .entry(rec.resolution)
                .or_insert_with(HashMap::new);
            rec.accumulator.for_each_class(|cat, cnt| {
                *class_map.entry(cat).or_insert(0.0) += cnt;
            });
        }
    }

    // Both resolutions 7 and 8 should have exact pixel conservation for classes 1 and 2
    for &res in &[7u8, 8u8] {
        let total = total_pixels_by_res.get(&res).copied().unwrap_or(0.0);
        // 50 * 50 = 2500 pixels kept (101..103 and 121..122), 50 * 50 = 2500 dropped (99 and 42)
        assert_eq!(total, 2500.0, "Resolution {} total pixel mismatch", res);

        let classes = counts_by_res.get(&res).unwrap();
        assert_eq!(classes.get(&1).copied().unwrap_or(0.0), 1250.0);
        assert_eq!(classes.get(&2).copied().unwrap_or(0.0), 1250.0);
        assert_eq!(classes.len(), 2);
    }
}
