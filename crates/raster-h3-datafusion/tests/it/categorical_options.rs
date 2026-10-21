//! `CategoricalRasterTableProvider` builder options compared with core
//! `CategoricalBatchReader` runs using the same `MultiResolutionConfig`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::aggregator::remap::CategoryRemapper;
use raster_h3_core::columnar::CategoricalFormat;
use raster_h3_datafusion::{
    CategoricalRasterTableProvider, FilterPredicate, FilterPushdownStatus, TableProvider,
};
use tempfile::NamedTempFile;

use crate::helpers::TestGeoTiffBuilder;
use crate::rows::{column_f64, column_text, core_categorical, plan_table};

/// 48x48 u8 classes 1..=5 in 4-pixel blocks, 0.001° pixels.
fn classes_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(48, 48)
        .pixel_size(0.001)
        .create_tempfile(|c, r| ((c / 4 + r / 4) % 5 + 1) as u8)
}

fn provider(path: &Path) -> CategoricalRasterTableProvider {
    CategoricalRasterTableProvider::try_new(path.to_str().unwrap(), vec![8]).unwrap()
}

/// Every class key appearing in the `histogram` JSON column.
fn histogram_classes(table: &(Vec<String>, Vec<Vec<String>>)) -> Vec<i64> {
    let mut out: Vec<i64> = column_text(table, "histogram")
        .iter()
        .flat_map(|h| {
            let v: serde_json::Value = serde_json::from_str(h).unwrap();
            v.as_object()
                .unwrap()
                .keys()
                .map(|k| k.parse::<i64>().unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

#[test]
fn with_nodata_removes_the_class_like_core() {
    let (_tmp, path) = classes_tif();
    let paths = vec![path.clone()];

    let plain = plan_table(provider(&path).scan(None, &[], None).unwrap());
    assert_eq!(histogram_classes(&plain), vec![1, 2, 3, 4, 5]);

    let masked = plan_table(
        provider(&path)
            .with_nodata(3.0)
            .scan(None, &[], None)
            .unwrap(),
    );
    let mut c = MultiResolutionConfig::new(vec![8]);
    c.custom_nodata = Some(3.0);
    assert_eq!(
        masked,
        core_categorical(&paths, &c, CategoricalFormat::Wide)
    );
    assert_eq!(histogram_classes(&masked), vec![1, 2, 4, 5]);
    assert!(
        column_f64(&masked, "total_count").iter().sum::<f64>()
            < column_f64(&plain, "total_count").iter().sum::<f64>()
    );
}

#[test]
fn with_remapper_matches_core_in_wide_and_long_format() {
    let (_tmp, path) = classes_tif();
    let paths = vec![path.clone()];
    let remapper = Arc::new(CategoryRemapper::parse("1..2: 10, 3: null").unwrap());

    let mut c = MultiResolutionConfig::new(vec![8]);
    c.remapper = Some(Arc::clone(&remapper));

    let wide = plan_table(
        provider(&path)
            .with_remapper(Arc::clone(&remapper))
            .scan(None, &[], None)
            .unwrap(),
    );
    assert_eq!(wide, core_categorical(&paths, &c, CategoricalFormat::Wide));
    // 1 and 2 merge into 10, 3 is dropped, 4 and 5 pass through.
    assert_eq!(histogram_classes(&wide), vec![4, 5, 10]);

    let long = plan_table(
        provider(&path)
            .with_format(CategoricalFormat::Long)
            .with_remapper(remapper)
            .scan(None, &[], None)
            .unwrap(),
    );
    assert_eq!(long, core_categorical(&paths, &c, CategoricalFormat::Long));
    let mut categories: Vec<f64> = column_f64(&long, "category");
    categories.sort_by(f64::total_cmp);
    categories.dedup();
    assert_eq!(categories, vec![4.0, 5.0, 10.0]);
}

#[test]
fn with_min_majority_fraction_intersects_scan_filter() {
    let (_tmp, path) = classes_tif();
    let paths = vec![path.clone()];

    let unfiltered = plan_table(provider(&path).scan(None, &[], None).unwrap());
    let mut fractions = column_f64(&unfiltered, "majority_fraction");
    fractions.sort_by(f64::total_cmp);
    let lo = fractions[fractions.len() / 4];
    let hi = fractions[fractions.len() * 3 / 4];
    assert!(lo < hi, "fixture needs a spread of majority fractions");

    let base = provider(&path).with_min_majority_fraction(lo);
    assert_eq!(
        base.supports_filters_pushdown(&[
            FilterPredicate::MinMajorityFraction(0.5),
            FilterPredicate::MinMean(1.0),
        ]),
        vec![
            FilterPushdownStatus::Exact,
            FilterPushdownStatus::Unsupported
        ]
    );
    let expect = |frac: f64, min_count: Option<f64>| {
        let mut c = MultiResolutionConfig::new(vec![8]);
        c.min_majority_fraction = Some(frac);
        c.min_count = min_count;
        core_categorical(&paths, &c, CategoricalFormat::Wide)
    };

    let own = plan_table(base.scan(None, &[], None).unwrap());
    assert_eq!(own, expect(lo, None));
    assert!(own.1.len() < unfiltered.1.len());
    assert!(column_f64(&own, "majority_fraction")
        .iter()
        .all(|&f| f >= lo));

    // The stricter of the builder value and the pushed filter wins.
    let tighter = plan_table(
        base.scan(None, &[FilterPredicate::MinMajorityFraction(hi)], None)
            .unwrap(),
    );
    assert_eq!(tighter, expect(hi, None));
    assert!(tighter.1.len() < own.1.len());
    let looser = plan_table(
        base.scan(None, &[FilterPredicate::MinMajorityFraction(0.0)], None)
            .unwrap(),
    );
    assert_eq!(looser, own);

    // `with_min_count` composes with it, and a pushed MinCount narrows further.
    let counts = column_f64(&own, "total_count");
    let mid = {
        let mut c = counts.clone();
        c.sort_by(f64::total_cmp);
        c[c.len() / 2]
    };
    let counted = base.clone().with_min_count(mid);
    assert_eq!(
        plan_table(counted.scan(None, &[], None).unwrap()),
        expect(lo, Some(mid))
    );
    assert_eq!(
        plan_table(
            counted
                .scan(None, &[FilterPredicate::MinCount(mid + 1.0)], None)
                .unwrap()
        ),
        expect(lo, Some(mid + 1.0))
    );
}
