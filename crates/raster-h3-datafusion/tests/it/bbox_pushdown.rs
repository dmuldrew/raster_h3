//! `FilterPredicate::Bbox` pushdown must not change any cell that intersects the box: the
//! scan may return extra cells (it is `Inexact`), but after the caller re-applies the
//! predicate every edge cell keeps the aggregates of an unfiltered scan.

use std::collections::BTreeSet;

use h3o::CellIndex;
use raster_h3_datafusion::{
    CategoricalRasterTableProvider, ContinuousRasterTableProvider, FilterPredicate,
    FilterPushdownStatus, TableProvider,
};

use crate::helpers::TestGeoTiffBuilder;
use crate::rows::{column_text, plan_table, Row};

/// 0.02° query box well inside the 0.2° raster (lon -122.45..-122.25, lat 37.60..37.80).
const QUERY: [f64; 4] = [-122.40, 37.72, -122.38, 37.74];

type Table = (Vec<String>, Vec<Row>);

/// The predicate a caller re-applies: the cell's boundary envelope intersects `QUERY`.
fn intersects_query(h3_index: &str) -> bool {
    let cell = CellIndex::try_from(h3_index.parse::<u64>().unwrap()).unwrap();
    let boundary = cell.boundary();
    let (lons, lats): (Vec<f64>, Vec<f64>) = boundary.iter().map(|p| (p.lng(), p.lat())).unzip();
    let min = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = |v: &[f64]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    min(&lons) <= QUERY[2]
        && max(&lons) >= QUERY[0]
        && min(&lats) <= QUERY[3]
        && max(&lats) >= QUERY[1]
}

/// Rows of `table` whose cell intersects `QUERY`.
fn reapply(table: &Table) -> BTreeSet<Row> {
    let cells = column_text(table, "h3_index");
    table
        .1
        .iter()
        .zip(cells)
        .filter(|(_, cell)| intersects_query(cell))
        .map(|(row, _)| row.clone())
        .collect()
}

/// Asserts the pushdown contract for one provider: `full` and `pushed` agree on every cell
/// that intersects the box, the pushed scan is smaller, and clipping to the box itself
/// (`clipped`, a table-definition bbox) changes edge-cell aggregates.
fn assert_result_preserving(full: &Table, pushed: &Table, clipped: &Table) {
    let expected = reapply(full);
    assert!(
        expected.len() > 1,
        "query box covers {} cells",
        expected.len()
    );
    assert_eq!(reapply(pushed), expected);
    assert!(
        pushed.1.len() * 2 < full.1.len(),
        "pushed scan returned {} of {} rows",
        pushed.1.len(),
        full.1.len()
    );

    // Control: the naive clip keeps the edge cells but aggregates only their pixels inside
    // the box, so their values differ. This is what the widened box avoids.
    let naive = reapply(clipped);
    assert_ne!(naive, expected);
    let expected_cells: BTreeSet<_> = column_text(full, "h3_index")
        .into_iter()
        .filter(|c| intersects_query(c))
        .collect();
    let naive_cells: BTreeSet<_> = column_text(clipped, "h3_index")
        .into_iter()
        .filter(|c| intersects_query(c))
        .collect();
    assert!(naive_cells.is_subset(&expected_cells));
    assert!(
        naive.iter().any(|row| !expected.contains(row)),
        "some edge cell is emitted with different aggregates under the naive clip"
    );
}

#[test]
fn continuous_bbox_pushdown_keeps_edge_cells_whole() {
    let (_tmp, path) = TestGeoTiffBuilder::new(200, 200)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| (c * 7 + r * 3) as f32);
    let src = path.to_str().unwrap();
    // Two resolutions: the margin must cover the coarser one.
    let provider = ContinuousRasterTableProvider::try_new(src, vec![7, 8]).unwrap();
    let bbox = [FilterPredicate::Bbox(QUERY)];
    assert_eq!(
        provider.supports_filters_pushdown(&bbox),
        vec![FilterPushdownStatus::Inexact]
    );

    let full = plan_table(provider.scan(None, &[], None).unwrap());
    let pushed = plan_table(provider.scan(None, &bbox, None).unwrap());
    let clipped = plan_table(
        ContinuousRasterTableProvider::try_new(src, vec![7, 8])
            .unwrap()
            .with_bbox(QUERY)
            .scan(None, &[], None)
            .unwrap(),
    );
    assert_result_preserving(&full, &pushed, &clipped);
}

#[test]
fn categorical_bbox_pushdown_keeps_edge_cells_whole() {
    let (_tmp, path) = TestGeoTiffBuilder::new(200, 200)
        .pixel_size(0.001)
        .create_tempfile(|c, r| ((c / 3 + r / 5) % 4) as u8);
    let src = path.to_str().unwrap();
    let provider = CategoricalRasterTableProvider::try_new(src, vec![8]).unwrap();
    let bbox = [FilterPredicate::Bbox(QUERY)];
    assert_eq!(
        provider.supports_filters_pushdown(&bbox),
        vec![FilterPushdownStatus::Inexact]
    );

    let full = plan_table(provider.scan(None, &[], None).unwrap());
    let pushed = plan_table(provider.scan(None, &bbox, None).unwrap());
    let clipped = plan_table(
        CategoricalRasterTableProvider::try_new(src, vec![8])
            .unwrap()
            .with_bbox(QUERY)
            .scan(None, &[], None)
            .unwrap(),
    );
    assert_result_preserving(&full, &pushed, &clipped);
}

#[test]
fn compacted_scan_ignores_pushed_bbox() {
    let (_tmp, path) = TestGeoTiffBuilder::new(64, 64)
        .pixel_size(0.001)
        .create_f32_tempfile(|_, _| 1.0);
    let provider = ContinuousRasterTableProvider::try_new(path.to_str().unwrap(), vec![8])
        .unwrap()
        .with_compact(true);
    let full = plan_table(provider.scan(None, &[], None).unwrap());
    let pushed = plan_table(
        provider
            .scan(
                None,
                &[FilterPredicate::Bbox([-122.44, 37.77, -122.43, 37.78])],
                None,
            )
            .unwrap(),
    );
    assert_eq!(pushed, full);
}

#[test]
fn limit_is_not_spent_on_cells_outside_a_pushed_bbox() {
    let (_tmp, path) = TestGeoTiffBuilder::new(200, 200)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| (c * 7 + r * 3) as f32);
    let src = path.to_str().unwrap();
    let bbox = [FilterPredicate::Bbox(QUERY)];
    let cont = ContinuousRasterTableProvider::try_new(src, vec![8]).unwrap();
    let cat = CategoricalRasterTableProvider::try_new(src, vec![8]).unwrap();
    for (name, provider) in [
        ("continuous", &cont as &dyn TableProvider),
        ("categorical", &cat as &dyn TableProvider),
    ] {
        // The widened box emits padding cells the caller drops, so a reader-level limit
        // could be used up before every matching cell is produced.
        let expected = reapply(&plan_table(provider.scan(None, &bbox, None).unwrap()));
        let limited = plan_table(provider.scan(None, &bbox, Some(expected.len())).unwrap());
        assert_eq!(reapply(&limited), expected, "{name}");
    }
}
