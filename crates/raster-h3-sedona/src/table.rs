//! [`RasterH3Table`]: a DataFusion `TableProvider` over the `raster-h3-datafusion` providers.

use std::fmt;
use std::sync::Arc;

use arrow::array::{RecordBatch, RecordBatchOptions};
use arrow::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use async_trait::async_trait;
use datafusion::catalog::{Session, TableProvider};
use datafusion::common::stats::Precision;
use datafusion::common::Statistics;
use datafusion::datasource::TableType;
use datafusion::error::Result;
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown};
use datafusion::physical_plan::stream::RecordBatchReceiverStream;
use datafusion::physical_plan::streaming::{PartitionStream, StreamingTableExec};
use datafusion::physical_plan::ExecutionPlan;
use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::ArrowArrayStream;
use raster_h3_datafusion::{
    CategoricalRasterTableProvider, ContinuousRasterTableProvider, FilterPredicate,
};

use crate::convert::{to_arrow_schema, to_df_error};
use crate::filters::translate;
use crate::spatial::cell_margin_km;

/// Which aggregation a table runs; selects the columns filters may be pushed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Continuous,
    Categorical,
}

/// A raster aggregated to H3 cells, exposed as a DataFusion table.
///
/// Projection and `LIMIT` are pushed into the scanline aggregator, and so are filters on
/// `count` / `total_count`, `mean`, `majority_fraction` and `resolution`, and spatial
/// predicates on `wkb` against a constant lon/lat geometry (see the `filters` and
/// `spatial` modules). Aggregation runs on DataFusion's blocking thread pool and streams
/// batches back through the Arrow C Stream Interface without copying.
pub struct RasterH3Table {
    inner: Arc<dyn raster_h3_datafusion::TableProvider>,
    kind: TableKind,
    schema: arrow::datatypes::SchemaRef,
    /// Bound on cell size for spatial pushdown; `None` disables it.
    bbox_margin_km: Option<f64>,
}

impl RasterH3Table {
    /// Wrap a configured continuous provider (CRS, nodata, quantiles, ...).
    pub fn continuous(provider: ContinuousRasterTableProvider) -> Self {
        let margin = bbox_margin_km(&provider.config);
        Self::new(Arc::new(provider), TableKind::Continuous, margin)
    }

    /// Wrap a configured categorical provider (format, remapper, ...).
    pub fn categorical(provider: CategoricalRasterTableProvider) -> Self {
        let margin = bbox_margin_km(&provider.config);
        Self::new(Arc::new(provider), TableKind::Categorical, margin)
    }

    fn new(
        inner: Arc<dyn raster_h3_datafusion::TableProvider>,
        kind: TableKind,
        bbox_margin_km: Option<f64>,
    ) -> Self {
        let schema = to_arrow_schema(&inner.schema());
        Self {
            inner,
            kind,
            schema,
            bbox_margin_km,
        }
    }
}

/// Margin covering the largest cell the scan can emit: the coarsest configured resolution.
/// Compaction merges children into coarser parents, so it gets no margin.
fn bbox_margin_km(config: &MultiResolutionConfig) -> Option<f64> {
    if config.compact || config.compact_h3_children {
        return None;
    }
    cell_margin_km(*config.resolutions.iter().min()?)
}

impl fmt::Debug for RasterH3Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RasterH3Table")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TableProvider for RasterH3Table {
    fn schema(&self) -> arrow::datatypes::SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| {
                translate(f, self.kind, self.bbox_margin_km)
                    .map_or(TableProviderFilterPushDown::Unsupported, |t| t.pushdown)
            })
            .collect())
    }

    fn statistics(&self) -> Option<Statistics> {
        let stats = self.inner.statistics();
        let num_rows = if stats.is_exact {
            Precision::Exact(stats.num_rows as usize)
        } else {
            Precision::Inexact(stats.num_rows as usize)
        };
        Some(Statistics::new_unknown(&self.schema).with_num_rows(num_rows))
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let predicates: Vec<FilterPredicate> = filters
            .iter()
            .filter_map(|f| translate(f, self.kind, self.bbox_margin_km))
            .flat_map(|t| t.predicates)
            .collect();

        // An empty projection (e.g. `COUNT(*)`) still needs a column to count rows from.
        let inner_projection = match projection {
            Some(p) if p.is_empty() => Some(vec![0]),
            other => other.cloned(),
        };
        let plan = self
            .inner
            .scan(inner_projection.as_deref(), &predicates, limit)
            .map_err(to_df_error)?;

        let output_schema = match projection {
            Some(p) => Arc::new(self.schema.project(p)?),
            None => Arc::clone(&self.schema),
        };
        let partition = Arc::new(RasterH3Partition {
            plan: Arc::from(plan),
            schema: Arc::clone(&output_schema),
        });
        Ok(Arc::new(StreamingTableExec::try_new(
            output_schema,
            vec![partition],
            None,
            [],
            false,
            limit,
        )?))
    }
}

/// One aggregation run, streamed as a single DataFusion partition.
struct RasterH3Partition {
    plan: Arc<dyn raster_h3_datafusion::ExecutionPlan>,
    schema: arrow::datatypes::SchemaRef,
}

impl fmt::Debug for RasterH3Partition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RasterH3Partition")
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

impl PartitionStream for RasterH3Partition {
    fn schema(&self) -> &arrow::datatypes::SchemaRef {
        &self.schema
    }

    fn execute(&self, _ctx: Arc<TaskContext>) -> SendableRecordBatchStream {
        let mut builder = RecordBatchReceiverStream::builder(Arc::clone(&self.schema), 2);
        let tx = builder.tx();
        let plan = Arc::clone(&self.plan);
        let schema = Arc::clone(&self.schema);
        builder.spawn_blocking(move || {
            let mut stream: ArrowArrayStream =
                plan.execute_arrow_c_stream().map_err(to_df_error)?;
            // SAFETY: core's `ArrowArrayStream` is the `#[repr(C)]` Arrow C Stream struct, the
            // same layout as `FFI_ArrowArrayStream`. `from_raw` moves the live stream out and
            // leaves `stream` released, so core's `Drop` does not release it a second time.
            let ffi = unsafe {
                FFI_ArrowArrayStream::from_raw(
                    (&mut stream as *mut ArrowArrayStream).cast::<FFI_ArrowArrayStream>(),
                )
            };
            let reader = ArrowArrayStreamReader::try_new(ffi)?;
            for batch in reader {
                let batch = batch?;
                // Re-attach the declared schema: the C stream carries no field metadata, so
                // this restores the `geoarrow.wkb` tag. For an empty projection, drop the
                // placeholder column but keep the row count.
                let columns = if schema.fields().is_empty() {
                    Vec::new()
                } else {
                    batch.columns().to_vec()
                };
                let options = RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
                let batch =
                    RecordBatch::try_new_with_options(Arc::clone(&schema), columns, &options)?;
                if tx.blocking_send(Ok(batch)).is_err() {
                    break; // The consumer hung up (LIMIT satisfied or query cancelled).
                }
            }
            Ok(())
        });
        builder.build()
    }
}
