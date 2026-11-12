//! [`RasterH3Table`]: a DataFusion `TableProvider` over the `raster-h3-datafusion` providers.

use std::fmt;
use std::sync::Arc;

use crate::scan::{
    CategoricalRasterTableProvider, ContinuousRasterTableProvider, FilterPredicate,
    FilterPushdownStatus,
};
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
use raster_h3_core::columnar::ArrowArrayStream;

use crate::convert::{to_arrow_schema, to_df_error};
use crate::filters::translate;
use crate::spatial::Untagged;

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
/// predicates (`ST_Intersects`-style and `ST_DWithin`) between `wkb` or
/// `h3_to_wkb(h3_index)` and a constant lon/lat geometry (see the `filters` and `spatial`
/// modules). `LIMIT` is deferred whenever a filter still needs caller-side evaluation.
/// Aggregation runs on DataFusion's blocking thread pool and streams batches back through
/// the Arrow C Stream Interface without copying.
pub struct RasterH3Table {
    inner: Arc<dyn crate::scan::TableProvider>,
    kind: TableKind,
    schema: arrow::datatypes::SchemaRef,
    untagged: Untagged,
}

impl RasterH3Table {
    /// Wrap a configured continuous provider (CRS, nodata, quantiles, ...).
    pub fn continuous(provider: ContinuousRasterTableProvider) -> Self {
        Self::new(Arc::new(provider), TableKind::Continuous)
    }

    /// Wrap a configured categorical provider (format, remapper, ...).
    pub fn categorical(provider: CategoricalRasterTableProvider) -> Self {
        Self::new(Arc::new(provider), TableKind::Categorical)
    }

    fn new(inner: Arc<dyn crate::scan::TableProvider>, kind: TableKind) -> Self {
        let core_schema = inner.schema();
        let schema = to_arrow_schema(&core_schema);
        Self {
            inner,
            kind,
            schema,
            untagged: Untagged::Reject,
        }
    }

    /// Treat `Binary` literals without `geoarrow.wkb` metadata in spatial predicates as
    /// geometries in the cells' CRS (`OGC:CRS84`), so they push down too.
    ///
    /// Enable this only when filters come from SedonaDB's planner across its extension ABI
    /// (as in the Python bindings): `datafusion-proto` strips literal metadata there, and
    /// SedonaDB has already rejected predicates whose geometries differ in CRS. In a plain
    /// DataFusion context an untagged literal could be in any CRS, so it is not pushed.
    pub fn with_untagged_wkb_as_lnglat(mut self, enabled: bool) -> Self {
        self.untagged = if enabled {
            Untagged::LngLat
        } else {
            Untagged::Reject
        };
        self
    }
}

impl RasterH3Table {
    /// Translate a filter for pushdown, capped by what the wrapped provider accepts.
    ///
    /// The provider may refuse predicates its scan would ignore or misapply: a partitioned
    /// scan keeps partial rows whole and pushes only `resolution`, and a compacted scan
    /// pushes nothing (its rows and aggregates depend on unfiltered children). Advertising
    /// such a filter as `Exact` would drop it from DataFusion's plan while the scan ignores
    /// it, so the filter is not pushed and DataFusion evaluates it on the completed rows.
    fn translate(&self, filter: &Expr) -> Option<crate::filters::Translated> {
        let mut translated = translate(filter, self.kind, self.untagged)?;
        let inner = self.inner.supports_filters_pushdown(&translated.predicates);
        if inner.contains(&FilterPushdownStatus::Unsupported) {
            return None;
        }
        if inner.contains(&FilterPushdownStatus::Inexact) {
            translated.pushdown = TableProviderFilterPushDown::Inexact;
        }
        Some(translated)
    }
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
                self.translate(f)
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
        let translated: Vec<_> = filters.iter().map(|f| self.translate(f)).collect();
        let limit = limit.filter(|_| {
            translated.iter().all(|t| {
                t.as_ref()
                    .is_some_and(|t| t.pushdown == TableProviderFilterPushDown::Exact)
            })
        });
        let predicates: Vec<FilterPredicate> = translated
            .into_iter()
            .flatten()
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
    plan: Arc<dyn crate::scan::ExecutionPlan>,
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
