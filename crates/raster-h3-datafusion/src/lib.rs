//! # `raster-h3-datafusion`
//!
//! Apache DataFusion integration for [`raster_h3_core`], providing:
//! - [`ContinuousRasterTableProvider`] (`h3_raster_continuous_aggregate`)
//! - [`CategoricalRasterTableProvider`] (`h3_raster_categorical_aggregate`)
//! - [`ExecutionPlan`] implementations with column projection, filter (`min_count`, `min_mean`,
//!   `max_mean`, `min_majority_fraction`, `bbox`, `resolution`), and row-limit pushdown
//! - Vectorized [`ScalarUDFImpl`] implementations for all H3 scalar functions
//! - [`RasterH3SessionCatalog`] for registering and invoking table functions and scalar UDFs

pub mod scalar_udf;
pub mod table_provider;

use std::collections::HashMap;
use std::sync::Arc;

pub use raster_h3_core::columnar::{
    ArrowArray, ArrowArrayStream, ArrowDataType, ArrowSchema, CategoricalFormat, ColumnArray,
    ColumnField, ColumnSchema, RecordBatch,
};
use raster_h3_core::error::{RasterH3Error, Result};
pub use scalar_udf::{
    H3CellToParentUdf, H3GetResolutionUdf, H3IsValidUdf, H3ToLatUdf, H3ToLngUdf, H3ToStringUdf,
    H3ToWkbUdf, ScalarUDFImpl, StringToH3Udf,
};
pub use table_provider::{
    CategoricalRasterExec, CategoricalRasterTableProvider, ContinuousRasterExec,
    ContinuousRasterTableProvider, ExecutionPlan, FilterPredicate, FilterPushdownStatus,
    TableProvider, TableStatistics,
};

/// Return all standard `raster_h3` vectorized scalar UDFs.
pub fn default_scalar_udfs() -> Vec<Arc<dyn ScalarUDFImpl>> {
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

/// Session catalog registry managing registered `TableProvider`s and vectorized `ScalarUDFImpl`s.
#[derive(Default)]
pub struct RasterH3SessionCatalog {
    tables: HashMap<String, Arc<dyn TableProvider>>,
    scalar_udfs: HashMap<String, Arc<dyn ScalarUDFImpl>>,
}

impl RasterH3SessionCatalog {
    /// Create a new catalog with all `raster_h3` scalar UDFs registered.
    pub fn new() -> Self {
        let mut cat = Self::default();
        register_all(&mut cat);
        cat
    }

    /// Register a vectorized scalar UDF (along with its aliases).
    pub fn register_udf(&mut self, udf: Arc<dyn ScalarUDFImpl>) {
        self.scalar_udfs
            .insert(udf.name().to_ascii_lowercase(), Arc::clone(&udf));
        for &alias in udf.aliases() {
            self.scalar_udfs
                .insert(alias.to_ascii_lowercase(), Arc::clone(&udf));
        }
    }

    /// Register a named `TableProvider`.
    pub fn register_table(&mut self, name: impl Into<String>, provider: Arc<dyn TableProvider>) {
        self.tables.insert(name.into().to_ascii_lowercase(), provider);
    }

    /// Look up a registered `TableProvider` by name.
    pub fn table(&self, name: &str) -> Option<Arc<dyn TableProvider>> {
        self.tables.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Look up a registered `ScalarUDFImpl` by name or alias.
    pub fn udf(&self, name: &str) -> Option<Arc<dyn ScalarUDFImpl>> {
        self.scalar_udfs.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Invoke a registered scalar UDF by name on columnar arguments.
    pub fn invoke_scalar(&self, name: &str, args: &[ColumnArray]) -> Result<ColumnArray> {
        let udf = self.udf(name).ok_or_else(|| {
            RasterH3Error::InvalidParameter(format!("Unregistered scalar UDF: '{}'", name))
        })?;
        udf.invoke(args)
    }

    /// Create and scan a continuous raster table (`h3_raster_continuous_aggregate`).
    pub fn h3_raster_continuous_aggregate(
        &self,
        source: &str,
        resolutions: Vec<u8>,
    ) -> Result<ContinuousRasterTableProvider> {
        ContinuousRasterTableProvider::try_new(source, resolutions)
    }

    /// Create and scan a categorical raster table (`h3_raster_categorical_aggregate`).
    pub fn h3_raster_categorical_aggregate(
        &self,
        source: &str,
        resolutions: Vec<u8>,
    ) -> Result<CategoricalRasterTableProvider> {
        CategoricalRasterTableProvider::try_new(source, resolutions)
    }
}

/// Register all `raster_h3` scalar UDFs into a [`RasterH3SessionCatalog`].
pub fn register_all(catalog: &mut RasterH3SessionCatalog) {
    for udf in default_scalar_udfs() {
        catalog.register_udf(udf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::NamedTempFile;
    use tiff::encoder::colortype::Gray32Float;
    use tiff::encoder::TiffEncoder;

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
    fn test_datafusion_table_provider_projection_filter_limit_and_arrow_stream() {
        let tif = write_test_tif(24, 24, 15.0);
        let ctx = RasterH3SessionCatalog::new();

        let provider = ctx
            .h3_raster_continuous_aggregate(&tif.path().to_string_lossy(), vec![8])
            .unwrap()
            .with_crs("EPSG:4326")
            .with_batch_size(4);

        let stats = provider.statistics();
        assert!(stats.num_rows > 0);

        let filters = vec![FilterPredicate::MinCount(1.0), FilterPredicate::MinMean(10.0)];
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
        let (schema, batches) = unsafe { stream.collect_batches().unwrap() };
        assert_eq!(schema.fields.len(), 2);
        let total_rows: usize = batches.iter().map(|b| b.num_rows).sum();
        assert!(total_rows > 0 && total_rows <= 3);

        // Test Categorical TableProvider in Long format
        let cat_provider = ctx
            .h3_raster_categorical_aggregate(&tif.path().to_string_lossy(), vec![8])
            .unwrap()
            .with_crs("EPSG:4326")
            .with_format(CategoricalFormat::Long);
        let cat_plan = cat_provider.scan(Some(&[0, 2, 3]), &[], Some(5)).unwrap();
        let cat_batches: Vec<RecordBatch> =
            cat_plan.execute().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert!(!cat_batches.is_empty());
        assert_eq!(cat_batches[0].schema.fields[1].name, "category");
    }

    #[test]
    fn test_datafusion_vectorized_scalar_udfs() {
        let ctx = RasterH3SessionCatalog::new();
        let cell = 0x8828308281fffffu64;
        let col_u64 = ColumnArray::UInt64 {
            values: vec![cell, 0],
            validity: Some(vec![true, false]),
        };

        let hex_col = ctx.invoke_scalar("h3_to_string", &[col_u64.clone()]).unwrap();
        let ColumnArray::Utf8 { values, validity } = &hex_col else {
            panic!("expected Utf8");
        };
        assert_eq!(values[0], "8828308281fffff");
        assert_eq!(validity.as_ref().unwrap(), &vec![true, false]);

        let back_u64 = ctx.invoke_scalar("string_to_h3", &[hex_col.clone()]).unwrap();
        if let ColumnArray::UInt64 { values, .. } = back_u64 {
            assert_eq!(values[0], cell);
        } else {
            panic!("expected UInt64");
        }

        let res_col = ctx
            .invoke_scalar("h3_get_resolution", &[col_u64.clone()])
            .unwrap();
        if let ColumnArray::Int64 { values, .. } = res_col {
            assert_eq!(values[0], 8);
        }

        let valid_col = ctx.invoke_scalar("h3_is_valid", &[hex_col]).unwrap();
        if let ColumnArray::Boolean { values, .. } = valid_col {
            assert_eq!(values, vec![true, false]);
        }

        let geom_col = ctx
            .invoke_scalar("h3_cell_to_geometry", &[col_u64.clone()])
            .unwrap();
        if let ColumnArray::Binary { values, .. } = geom_col {
            assert!(values[0].len() >= 109);
        }

        let parent_col = ctx
            .invoke_scalar(
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
