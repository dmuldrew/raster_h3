//! Residual predicates must see completed compacted rows, with LIMIT applied last.

use raster_h3_core::columnar::CategoricalFormat;
use raster_h3_datafusion::{
    CategoricalRasterTableProvider, ContinuousRasterTableProvider, FilterPredicate,
    FilterPushdownStatus, TableProvider,
};

use crate::helpers::TestGeoTiffBuilder;
use crate::rows::{column_f64, plan_table, Row};

type Table = (Vec<String>, Vec<Row>);

fn residual(table: &Table, filters: &[FilterPredicate]) -> Vec<Row> {
    table
        .1
        .iter()
        .filter(|row| {
            let value = |name: &str| {
                let index = table.0.iter().position(|n| n == name).unwrap();
                row[index].parse::<f64>().unwrap()
            };
            filters.iter().all(|f| match *f {
                FilterPredicate::Resolution(r) => value("resolution") == f64::from(r),
                FilterPredicate::MinCount(n) => {
                    let column = if table.0.iter().any(|n| n == "total_count") {
                        "total_count"
                    } else {
                        "count"
                    };
                    value(column) >= n
                }
                FilterPredicate::MinMean(n) => value("mean") >= n,
                FilterPredicate::MaxMean(n) => value("mean") <= n,
                FilterPredicate::MinMajorityFraction(n) => value("majority_fraction") >= n,
                _ => panic!("unsupported test predicate"),
            })
        })
        .cloned()
        .collect()
}

fn assert_residual_scan(
    provider: &dyn TableProvider,
    projection: &[usize],
    filters: &[FilterPredicate],
    full: &Table,
) {
    assert_eq!(
        provider.supports_filters_pushdown(filters),
        vec![FilterPushdownStatus::Unsupported; filters.len()]
    );
    for limit in [None, Some(1), Some(0)] {
        let scanned = plan_table(provider.scan(Some(projection), filters, limit).unwrap());
        // In particular, don't spend the limit on rows the residual predicate rejects.
        assert_eq!(&scanned, full, "filters {filters:?}, limit {limit:?}");
        let expected = residual(full, filters);
        let actual = residual(&scanned, filters);
        let n = limit.unwrap_or(usize::MAX);
        assert_eq!(
            actual.into_iter().take(n).collect::<Vec<_>>(),
            expected.into_iter().take(n).collect::<Vec<_>>()
        );
    }
}

#[test]
fn continuous_compaction_defers_resolution_and_aggregate_filters() {
    let (_file, path) = TestGeoTiffBuilder::new(128, 128)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| ((c * 2 + r) % 150 + 20) as f32);
    // Include every predicate column but omit expensive geometry/string encodings.
    let projection = [0, 2, 4, 8]; // h3_index, mean, count, resolution
    for (compact, children) in [(true, false), (false, true), (true, true)] {
        let mut provider =
            ContinuousRasterTableProvider::try_new(path.to_str().unwrap(), vec![8]).unwrap();
        provider.config.compact = compact;
        provider.config.compact_h3_children = children;
        provider = provider.with_batch_size(3);
        let full = plan_table(provider.scan(Some(&projection), &[], None).unwrap());
        assert!(column_f64(&full, "resolution").contains(&7.0));
        assert!(column_f64(&full, "resolution").contains(&8.0));
        assert!(!residual(&full, &[FilterPredicate::MinCount(150.0)]).is_empty());
        for filters in [
            vec![FilterPredicate::Resolution(7)],
            vec![FilterPredicate::Resolution(8)],
            vec![FilterPredicate::Resolution(15)],
            vec![FilterPredicate::MinCount(150.0)],
            vec![FilterPredicate::MinMean(95.0)],
            vec![FilterPredicate::MaxMean(95.0)],
            vec![
                FilterPredicate::Resolution(7),
                FilterPredicate::MinCount(150.0),
            ],
            vec![
                FilterPredicate::MinMean(85.0),
                FilterPredicate::MaxMean(105.0),
            ],
            vec![
                FilterPredicate::Resolution(7),
                FilterPredicate::Resolution(8),
            ],
        ] {
            assert_residual_scan(&provider, &projection, &filters, &full);
        }
        // Without residual predicates, ordinary projection and limit pushdown still work.
        let limited = plan_table(provider.scan(Some(&projection), &[], Some(1)).unwrap());
        assert_eq!(limited.1.len(), 1);
        assert!(full.1.contains(&limited.1[0]));
        assert!(
            plan_table(provider.scan(Some(&projection), &[], Some(0)).unwrap())
                .1
                .is_empty()
        );
    }
}

#[test]
fn categorical_compaction_defers_filters_in_both_formats() {
    let (_file, path) = TestGeoTiffBuilder::new(128, 128)
        .pixel_size(0.001)
        .create_tempfile(|c, r| ((c + r) % 3) as u8);
    for format in [CategoricalFormat::Wide, CategoricalFormat::Long] {
        for (compact, children) in [(true, false), (false, true)] {
            let mut provider =
                CategoricalRasterTableProvider::try_new(path.to_str().unwrap(), vec![8])
                    .unwrap()
                    .with_format(format)
                    .with_batch_size(3);
            provider.config.compact = compact;
            provider.config.compact_h3_children = children;
            let schema = provider.schema();
            let projection: Vec<_> = schema
                .fields
                .iter()
                .enumerate()
                .filter(|(_, field)| field.name != "wkb" && field.name != "h3_hex")
                .map(|(i, _)| i)
                .collect();
            let full = plan_table(provider.scan(Some(&projection), &[], None).unwrap());
            assert!(column_f64(&full, "resolution").contains(&7.0));
            for filters in [
                vec![FilterPredicate::Resolution(7)],
                vec![FilterPredicate::Resolution(8)],
                vec![FilterPredicate::MinCount(150.0)],
            ] {
                assert_residual_scan(&provider, &projection, &filters, &full);
            }
            if matches!(format, CategoricalFormat::Wide) {
                assert_residual_scan(
                    &provider,
                    &projection,
                    &[FilterPredicate::MinMajorityFraction(0.35)],
                    &full,
                );
            }
        }
    }
}
