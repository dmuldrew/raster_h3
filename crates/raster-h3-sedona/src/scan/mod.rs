//! Engine-neutral raster scans and H3 scalar functions over `raster_h3_core`'s columnar types.
//!
//! [`crate::table::RasterH3Table`] and [`crate::udf::RasterH3ScalarUdf`] wrap these in
//! DataFusion's real `TableProvider` and `ScalarUDFImpl`. The traits here are deliberately
//! named differently from DataFusion's:
//! - [`ContinuousRasterTableProvider`] and [`CategoricalRasterTableProvider`] implement
//!   [`RasterScan`] (`h3_raster_continuous_aggregate`, `h3_raster_categorical_aggregate`):
//!   schema, statistics, filter classification and [`RasterScan::scan`].
//! - [`RasterScanExec`] plans with column projection, filter (`min_count`, `min_mean`,
//!   `max_mean`, `min_majority_fraction`, `bbox`, `resolution`) and row-limit pushdown, run as
//!   [`RecordBatch`] iterators or an Arrow C stream.
//! - [`H3ScalarFn`] implementations for the H3 scalar functions, listed by
//!   [`default_scalar_udfs`] and found by name or alias with [`find_scalar_udf`].
//!
//! This was the separate `raster-h3-datafusion` crate until it moved here, its only user.

pub mod provider;
pub mod scalar;

use std::sync::Arc;

pub use provider::{
    CategoricalRasterExec, CategoricalRasterTableProvider, ContinuousRasterExec,
    ContinuousRasterTableProvider, FilterPredicate, FilterPushdownStatus, PartitionedRasterExec,
    RasterScan, RasterScanExec, TableStatistics,
};
pub use raster_h3_core::aggregator::multi_horizon::ScanPartition;
pub use raster_h3_core::columnar::{
    merge_partial_batches, ArrowArray, ArrowArrayStream, ArrowDataType, ArrowSchema,
    CategoricalFormat, ColumnArray, ColumnField, ColumnSchema, RecordBatch, PARTIAL_STATE_COLUMN,
};
pub use scalar::{
    H3CellToParentUdf, H3GetResolutionUdf, H3IsValidUdf, H3ScalarFn, H3ToLatUdf, H3ToLngUdf,
    H3ToStringUdf, H3ToWkbUdf, StringToH3Udf,
};

/// Return all standard `raster_h3` vectorized scalar UDFs.
pub fn default_scalar_udfs() -> Vec<Arc<dyn H3ScalarFn>> {
    vec![
        Arc::new(H3ToStringUdf),
        Arc::new(StringToH3Udf),
        Arc::new(H3ToLatUdf),
        Arc::new(H3ToLngUdf),
        Arc::new(H3GetResolutionUdf),
        Arc::new(H3IsValidUdf),
        Arc::new(H3ToWkbUdf),
        Arc::new(H3CellToParentUdf),
    ]
}

/// The default scalar function named `name` or having it as an alias, ignoring ASCII case.
pub fn find_scalar_udf(name: &str) -> Option<Arc<dyn H3ScalarFn>> {
    default_scalar_udfs().into_iter().find(|udf| {
        udf.name().eq_ignore_ascii_case(name)
            || udf
                .aliases()
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use raster_h3_core::error::{RasterH3Error, Result};
    use std::fs::File;
    use tempfile::NamedTempFile;
    use tiff::encoder::colortype::Gray32Float;
    use tiff::encoder::TiffEncoder;

    fn invoke_scalar(name: &str, args: &[ColumnArray]) -> Result<ColumnArray> {
        find_scalar_udf(name)
            .ok_or_else(|| RasterH3Error::InvalidParameter(format!("no scalar function '{name}'")))?
            .invoke(args)
    }

    fn write_test_tif(width: u32, height: u32, val: f32) -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let file = File::create(tmp.path()).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut img = encoder.new_image::<Gray32Float>(width, height).unwrap();
        img.encoder()
            .write_tag(
                tiff::tags::Tag::Unknown(33550),
                &[0.001f64, 0.001f64, 0.0f64][..],
            )
            .unwrap();
        img.encoder()
            .write_tag(
                tiff::tags::Tag::Unknown(33922),
                &[0.0f64, 0.0f64, 0.0f64, -122.45f64, 37.80f64, 0.0f64][..],
            )
            .unwrap();
        let data = vec![val; (width * height) as usize];
        img.write_data(&data).unwrap();
        tmp
    }

    #[test]
    fn test_scan_table_provider_projection_filter_limit_and_arrow_stream() {
        let tif = write_test_tif(24, 24, 15.0);

        let provider =
            ContinuousRasterTableProvider::try_new(&tif.path().to_string_lossy(), vec![8])
                .unwrap()
                .with_crs("EPSG:4326")
                .with_batch_size(4);

        let stats = provider.statistics();
        assert!(stats.num_rows > 0);

        let filters = vec![
            FilterPredicate::MinCount(1.0),
            FilterPredicate::MinMean(10.0),
        ];
        let pushdown = provider.supports_filters_pushdown(&filters);
        assert_eq!(
            pushdown,
            vec![FilterPushdownStatus::Exact, FilterPushdownStatus::Exact]
        );

        // Project only h3_index (0) and mean (2), with limit = 3
        let plan = provider.scan(Some(&[0, 2]), &filters, Some(3)).unwrap();
        assert_eq!(plan.schema().fields.len(), 2);
        assert_eq!(plan.schema().fields[0].name, "h3_index");
        assert_eq!(plan.schema().fields[1].name, "mean");

        let mut stream = plan.execute_arrow_c_stream().unwrap();
        // SAFETY: `execute_arrow_c_stream` returns a freshly built, unreleased stream owned here.
        let (schema, batches) = unsafe { stream.collect_batches().unwrap() };
        assert_eq!(schema.fields.len(), 2);
        let total_rows: usize = batches.iter().map(|b| b.num_rows).sum();
        assert!(total_rows > 0 && total_rows <= 3);

        // Test Categorical RasterScan in Long format
        let cat_provider =
            CategoricalRasterTableProvider::try_new(&tif.path().to_string_lossy(), vec![8])
                .unwrap()
                .with_crs("EPSG:4326")
                .with_format(CategoricalFormat::Long);
        let cat_plan = cat_provider.scan(Some(&[0, 2, 3]), &[], Some(5)).unwrap();
        let cat_batches: Vec<RecordBatch> = cat_plan
            .execute()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(!cat_batches.is_empty());
        assert_eq!(cat_batches[0].schema.fields[1].name, "category");
    }

    fn count_rows(plan: Box<dyn RasterScanExec>) -> usize {
        plan.execute().unwrap().map(|b| b.unwrap().num_rows).sum()
    }

    #[test]
    fn test_scan_pushdown_filters_only_narrow_the_table() {
        let tif = write_test_tif(24, 24, 15.0);
        let path = tif.path().to_string_lossy().to_string();
        let provider = ContinuousRasterTableProvider::try_new(&path, vec![7, 8])
            .unwrap()
            .with_crs("EPSG:4326");

        let all = count_rows(provider.scan(None, &[], None).unwrap());
        let res8 = count_rows(
            provider
                .scan(None, &[FilterPredicate::Resolution(8)], None)
                .unwrap(),
        );
        assert!(res8 > 0 && res8 < all);

        // Resolutions outside the configured set (including > 15) match nothing.
        for r in [10, 20] {
            let plan = provider
                .scan(None, &[FilterPredicate::Resolution(r)], None)
                .unwrap();
            assert_eq!(count_rows(plan), 0, "resolution {r}");
        }
        let cat = CategoricalRasterTableProvider::try_new(&path, vec![8])
            .unwrap()
            .with_crs("EPSG:4326");
        let plan = cat
            .scan(None, &[FilterPredicate::Resolution(9)], None)
            .unwrap();
        assert_eq!(count_rows(plan), 0);

        // A pushed bbox intersects the table's own bbox instead of replacing it.
        let narrow = [-122.45, 37.79, -122.44, 37.80];
        let bounded = ContinuousRasterTableProvider::try_new(&path, vec![8])
            .unwrap()
            .with_crs("EPSG:4326")
            .with_bbox(narrow);
        let own = count_rows(bounded.scan(None, &[], None).unwrap());
        let wider = count_rows(
            bounded
                .scan(
                    None,
                    &[FilterPredicate::Bbox([-123.0, 37.0, -122.0, 38.0])],
                    None,
                )
                .unwrap(),
        );
        assert_eq!(wider, own);
        let disjoint = bounded
            .scan(None, &[FilterPredicate::Bbox([0.0, 0.0, 1.0, 1.0])], None)
            .unwrap();
        assert_eq!(count_rows(disjoint), 0);
    }

    #[test]
    fn test_scan_vectorized_scalar_udfs() {
        let cell = 0x8828308281fffffu64;
        let col_u64 = ColumnArray::UInt64 {
            values: vec![cell, 0],
            validity: Some(vec![true, false]),
        };

        let hex_col = invoke_scalar("h3_to_string", std::slice::from_ref(&col_u64)).unwrap();
        let ColumnArray::Utf8 { values, validity } = &hex_col else {
            panic!("expected Utf8");
        };
        assert_eq!(values[0], "8828308281fffff");
        assert_eq!(validity.as_ref().unwrap(), &vec![true, false]);

        let back_u64 = invoke_scalar("string_to_h3", std::slice::from_ref(&hex_col)).unwrap();
        if let ColumnArray::UInt64 { values, .. } = back_u64 {
            assert_eq!(values[0], cell);
        } else {
            panic!("expected UInt64");
        }

        let res_col = invoke_scalar("h3_get_resolution", std::slice::from_ref(&col_u64)).unwrap();
        if let ColumnArray::Int64 { values, .. } = res_col {
            assert_eq!(values[0], 8);
        }

        let valid_col = invoke_scalar("h3_is_valid", &[hex_col]).unwrap();
        if let ColumnArray::Boolean { values, .. } = valid_col {
            assert_eq!(values, vec![true, false]);
        }

        let geom_col =
            invoke_scalar("h3_cell_to_geometry", std::slice::from_ref(&col_u64)).unwrap();
        if let ColumnArray::Binary { values, .. } = geom_col {
            assert!(values[0].len() >= 109);
        }

        let parent_col = invoke_scalar(
            "h3_cell_to_parent",
            &[
                col_u64,
                ColumnArray::Int64 {
                    values: vec![5, 5],
                    validity: None,
                },
            ],
        )
        .unwrap();
        if let ColumnArray::UInt64 { values, .. } = parent_col {
            assert_eq!(raster_h3_core::scalar_ops::h3_get_resolution(values[0]), 5);
        }
    }
}
