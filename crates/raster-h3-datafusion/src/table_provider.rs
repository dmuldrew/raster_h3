//! DataFusion `TableProvider`, `ExecutionPlan`, and `TableFunctionImpl` implementations
//! for continuous and categorical GeoTIFF/COG to H3 aggregation.

use std::path::PathBuf;
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiResolutionConfig, QuantileTarget, SpectralFormula,
};
use raster_h3_core::aggregator::remap::CategoryRemapper;
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::columnar::{
    ArrowArrayStream, CategoricalBatchReader, CategoricalFormat, ColumnSchema,
    ContinuousBatchReader, RecordBatch,
};
use raster_h3_core::error::Result;
use raster_h3_core::raster::mosaic::{resolve_raster_sources, OverlapRule};
use raster_h3_core::scalar_ops::estimate_raster_cardinality;

/// Cardinality statistics exposed to query planners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableStatistics {
    pub num_rows: u64,
    pub is_exact: bool,
}

/// Filter predicates pushable into the raster H3 scanline engine.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterPredicate {
    MinCount(f64),
    MinMean(f64),
    MaxMean(f64),
    MinMajorityFraction(f64),
    Bbox([f64; 4]),
    Resolution(u8),
    Other(String),
}

/// Filter pushdown support classification (`Exact`, `Inexact`, `Unsupported`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPushdownStatus {
    Exact,
    Inexact,
    Unsupported,
}

/// Physical execution plan producing columnar [`RecordBatch`] streams or [`ArrowArrayStream`].
pub trait ExecutionPlan: Send + Sync {
    /// Output schema of this physical plan (after projection pushdown).
    fn schema(&self) -> Arc<ColumnSchema>;

    /// Execute the plan and return a streaming iterator of [`RecordBatch`]es.
    fn execute(&self) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + Send>>;

    /// Execute the plan and export an ABI-stable [`ArrowArrayStream`].
    fn execute_arrow_c_stream(&self) -> Result<ArrowArrayStream> {
        let schema = self.schema();
        let iter = self.execute()?;
        Ok(ArrowArrayStream::from_iterator(schema, iter))
    }
}

/// DataFusion-compatible `TableProvider` trait for scanning GeoTIFF/COG rasters as H3 tables.
pub trait TableProvider: Send + Sync {
    /// Table schema prior to projection.
    fn schema(&self) -> Arc<ColumnSchema>;

    /// Estimated table cardinality for query optimization.
    fn statistics(&self) -> TableStatistics;

    /// Classify which filter predicates can be pushed down into the scanline aggregator.
    fn supports_filters_pushdown(&self, filters: &[FilterPredicate]) -> Vec<FilterPushdownStatus>;

    /// Create a physical [`ExecutionPlan`] with projection, filter, and limit pushdown.
    fn scan(
        &self,
        projection: Option<&[usize]>,
        filters: &[FilterPredicate],
        limit: Option<usize>,
    ) -> Result<Box<dyn ExecutionPlan>>;
}

/// Physical execution plan for continuous H3 raster aggregation.
pub struct ContinuousRasterExec {
    paths: Vec<PathBuf>,
    config: MultiResolutionConfig,
    projection: Option<Vec<usize>>,
    schema: Arc<ColumnSchema>,
    batch_size: usize,
    limit: Option<usize>,
}

impl ExecutionPlan for ContinuousRasterExec {
    fn schema(&self) -> Arc<ColumnSchema> {
        Arc::clone(&self.schema)
    }

    fn execute(&self) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + Send>> {
        let reader = ContinuousBatchReader::open(
            &self.paths,
            &self.config,
            self.projection.clone(),
            self.batch_size,
            self.limit,
        )?;
        Ok(Box::new(reader))
    }
}

/// `TableProvider` for `h3_raster_continuous_aggregate`.
#[derive(Clone)]
pub struct ContinuousRasterTableProvider {
    pub paths: Vec<PathBuf>,
    pub config: MultiResolutionConfig,
    pub batch_size: usize,
}

impl ContinuousRasterTableProvider {
    pub fn try_new(source: &str, resolutions: Vec<u8>) -> Result<Self> {
        let paths = resolve_raster_sources(source)?;
        let res = if resolutions.is_empty() {
            vec![8]
        } else {
            resolutions
        };
        Ok(Self {
            paths,
            config: MultiResolutionConfig::new(res),
            batch_size: 2048,
        })
    }

    pub fn with_crs(mut self, crs: impl Into<String>) -> Self {
        self.config.custom_crs = Some(crs.into());
        self
    }

    pub fn with_nodata(mut self, nodata: f64) -> Self {
        self.config.custom_nodata = Some(nodata);
        self
    }

    pub fn with_bbox(mut self, bbox: [f64; 4]) -> Self {
        self.config.bbox = Some(bbox);
        self
    }

    pub fn with_sampling(mut self, sampling: SamplingPattern) -> Self {
        self.config.sampling = sampling;
        self
    }

    pub fn with_band(mut self, band: usize) -> Self {
        self.config.band = band.max(1);
        self
    }

    pub fn with_spectral_formula(mut self, formula: SpectralFormula) -> Self {
        self.config.spectral_formula = Some(formula);
        self
    }

    pub fn with_quantiles(mut self, quantiles: Vec<QuantileTarget>) -> Self {
        self.config.quantiles = quantiles;
        self
    }

    pub fn with_min_count(mut self, min_count: f64) -> Self {
        self.config.min_count = Some(min_count);
        self
    }

    pub fn with_min_mean(mut self, min_mean: f64) -> Self {
        self.config.min_mean = Some(min_mean);
        self
    }

    pub fn with_max_mean(mut self, max_mean: f64) -> Self {
        self.config.max_mean = Some(max_mean);
        self
    }

    pub fn with_compact(mut self, compact: bool) -> Self {
        self.config.compact = compact;
        self.config.compact_h3_children = compact;
        self
    }

    pub fn with_overlap_rule(mut self, rule: OverlapRule) -> Self {
        self.config.overlap_rule = rule;
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }
}

impl TableProvider for ContinuousRasterTableProvider {
    fn schema(&self) -> Arc<ColumnSchema> {
        Arc::new(ColumnSchema::continuous(&self.config.quantiles))
    }

    fn statistics(&self) -> TableStatistics {
        let est = estimate_raster_cardinality(&self.paths, &self.config.resolutions);
        TableStatistics {
            num_rows: est,
            is_exact: false,
        }
    }

    fn supports_filters_pushdown(&self, filters: &[FilterPredicate]) -> Vec<FilterPushdownStatus> {
        filters
            .iter()
            .map(|f| match f {
                FilterPredicate::MinCount(_)
                | FilterPredicate::MinMean(_)
                | FilterPredicate::MaxMean(_)
                | FilterPredicate::Resolution(_) => FilterPushdownStatus::Exact,
                FilterPredicate::Bbox(_) => FilterPushdownStatus::Inexact,
                _ => FilterPushdownStatus::Unsupported,
            })
            .collect()
    }

    fn scan(
        &self,
        projection: Option<&[usize]>,
        filters: &[FilterPredicate],
        limit: Option<usize>,
    ) -> Result<Box<dyn ExecutionPlan>> {
        let mut config = self.config.clone();
        for f in filters {
            match *f {
                FilterPredicate::MinCount(v) => {
                    config.min_count = Some(config.min_count.map_or(v, |c| c.max(v)));
                }
                FilterPredicate::MinMean(v) => {
                    config.min_mean = Some(config.min_mean.map_or(v, |c| c.max(v)));
                }
                FilterPredicate::MaxMean(v) => {
                    config.max_mean = Some(config.max_mean.map_or(v, |c| c.min(v)));
                }
                FilterPredicate::Bbox(bbox) => {
                    config.bbox = Some(bbox);
                }
                FilterPredicate::Resolution(r) if r <= 15 => {
                    config.resolutions = vec![r];
                }
                _ => {}
            }
        }

        let base_schema = self.schema();
        let schema = match projection {
            Some(indices) => Arc::new(base_schema.project(indices)?),
            None => base_schema,
        };

        Ok(Box::new(ContinuousRasterExec {
            paths: self.paths.clone(),
            config,
            projection: projection.map(|p| p.to_vec()),
            schema,
            batch_size: self.batch_size,
            limit,
        }))
    }
}

/// Physical execution plan for categorical H3 raster aggregation.
pub struct CategoricalRasterExec {
    paths: Vec<PathBuf>,
    config: MultiResolutionConfig,
    format: CategoricalFormat,
    projection: Option<Vec<usize>>,
    schema: Arc<ColumnSchema>,
    batch_size: usize,
    limit: Option<usize>,
}

impl ExecutionPlan for CategoricalRasterExec {
    fn schema(&self) -> Arc<ColumnSchema> {
        Arc::clone(&self.schema)
    }

    fn execute(&self) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + Send>> {
        let reader = CategoricalBatchReader::open(
            &self.paths,
            &self.config,
            self.format,
            self.projection.clone(),
            self.batch_size,
            self.limit,
        )?;
        Ok(Box::new(reader))
    }
}

/// `TableProvider` for `h3_raster_categorical_aggregate`.
#[derive(Clone)]
pub struct CategoricalRasterTableProvider {
    pub paths: Vec<PathBuf>,
    pub config: MultiResolutionConfig,
    pub format: CategoricalFormat,
    pub batch_size: usize,
}

impl CategoricalRasterTableProvider {
    pub fn try_new(source: &str, resolutions: Vec<u8>) -> Result<Self> {
        let paths = resolve_raster_sources(source)?;
        let res = if resolutions.is_empty() {
            vec![8]
        } else {
            resolutions
        };
        Ok(Self {
            paths,
            config: MultiResolutionConfig::new(res),
            format: CategoricalFormat::Wide,
            batch_size: 2048,
        })
    }

    pub fn with_format(mut self, format: CategoricalFormat) -> Self {
        self.format = format;
        self
    }

    pub fn with_crs(mut self, crs: impl Into<String>) -> Self {
        self.config.custom_crs = Some(crs.into());
        self
    }

    pub fn with_nodata(mut self, nodata: f64) -> Self {
        self.config.custom_nodata = Some(nodata);
        self
    }

    pub fn with_bbox(mut self, bbox: [f64; 4]) -> Self {
        self.config.bbox = Some(bbox);
        self
    }

    pub fn with_remapper(mut self, remapper: Arc<CategoryRemapper>) -> Self {
        self.config.remapper = Some(remapper);
        self
    }

    pub fn with_min_count(mut self, min_count: f64) -> Self {
        self.config.min_count = Some(min_count);
        self
    }

    pub fn with_min_majority_fraction(mut self, frac: f64) -> Self {
        self.config.min_majority_fraction = Some(frac);
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }
}

impl TableProvider for CategoricalRasterTableProvider {
    fn schema(&self) -> Arc<ColumnSchema> {
        match self.format {
            CategoricalFormat::Wide => Arc::new(ColumnSchema::categorical_wide()),
            CategoricalFormat::Long => Arc::new(ColumnSchema::categorical_long()),
        }
    }

    fn statistics(&self) -> TableStatistics {
        let est = estimate_raster_cardinality(&self.paths, &self.config.resolutions);
        TableStatistics {
            num_rows: est,
            is_exact: false,
        }
    }

    fn supports_filters_pushdown(&self, filters: &[FilterPredicate]) -> Vec<FilterPushdownStatus> {
        filters
            .iter()
            .map(|f| match f {
                FilterPredicate::MinCount(_)
                | FilterPredicate::MinMajorityFraction(_)
                | FilterPredicate::Resolution(_) => FilterPushdownStatus::Exact,
                FilterPredicate::Bbox(_) => FilterPushdownStatus::Inexact,
                _ => FilterPushdownStatus::Unsupported,
            })
            .collect()
    }

    fn scan(
        &self,
        projection: Option<&[usize]>,
        filters: &[FilterPredicate],
        limit: Option<usize>,
    ) -> Result<Box<dyn ExecutionPlan>> {
        let mut config = self.config.clone();
        for f in filters {
            match *f {
                FilterPredicate::MinCount(v) => {
                    config.min_count = Some(config.min_count.map_or(v, |c| c.max(v)));
                }
                FilterPredicate::MinMajorityFraction(v) => {
                    config.min_majority_fraction =
                        Some(config.min_majority_fraction.map_or(v, |c| c.max(v)));
                }
                FilterPredicate::Bbox(bbox) => {
                    config.bbox = Some(bbox);
                }
                FilterPredicate::Resolution(r) if r <= 15 => {
                    config.resolutions = vec![r];
                }
                _ => {}
            }
        }

        let base_schema = self.schema();
        let schema = match projection {
            Some(indices) => Arc::new(base_schema.project(indices)?),
            None => base_schema,
        };

        Ok(Box::new(CategoricalRasterExec {
            paths: self.paths.clone(),
            config,
            format: self.format,
            projection: projection.map(|p| p.to_vec()),
            schema,
            batch_size: self.batch_size,
            limit,
        }))
    }
}
