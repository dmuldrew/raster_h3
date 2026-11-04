//! # `raster-h3-sedona`
//!
//! Apache DataFusion adapter for `raster_h3`, version-matched to
//! [SedonaDB](https://github.com/apache/sedona-db) so the same objects register into a
//! plain DataFusion `SessionContext` or into `SedonaContext::ctx`:
//! - [`RasterH3Table`]: a real `datafusion::catalog::TableProvider` wrapping the
//!   `raster-h3-datafusion` providers, with projection, limit and `Expr` filter pushdown
//! - Table functions `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate`
//! - The `raster_h3` H3 scalar functions as DataFusion `ScalarUDF`s, including
//!   `h3_polygon_to_cells(geometry, resolution [, containment])`
//! - A `pmtiles` output format: `COPY (query) TO 'cells.pmtiles'` writes any query's rows as
//!   H3 hexagon features in a PMTiles v3 archive ([`pmtiles`])
//!
//! Geometry columns (`wkb`, and the output of `h3_to_wkb`) are tagged with the
//! `geoarrow.wkb` extension type and CRS `OGC:CRS84`, so Sedona's `ST_*` functions accept
//! them directly.
//!
//! ```no_run
//! # async fn demo() -> datafusion::error::Result<()> {
//! let ctx = datafusion::prelude::SessionContext::new();
//! raster_h3_sedona::register_all(&ctx);
//! ctx.sql("SELECT h3_index, mean FROM h3_raster_continuous_aggregate('dem.tif', 8) WHERE count >= 4")
//!     .await?
//!     .show()
//!     .await?;
//! # Ok(())
//! # }
//! ```

mod convert;
mod filters;
pub mod merge;
pub mod pmtiles;
mod polyfill;
mod spatial;
pub mod table;
pub mod udf;
pub mod udtf;

use std::sync::Arc;

use datafusion::prelude::SessionContext;

pub use convert::{geometry_field, GEOARROW_WKB_EXTENSION, GEOARROW_WKB_METADATA};
pub use merge::{merge_udafs, MergePartials};
pub use pmtiles::{
    register_pmtiles_format, write_pmtiles, write_record_batches, PmtilesFormatFactory,
    PmtilesWriteOptions,
};
pub use polyfill::H3PolygonToCells;
pub use table::{RasterH3Table, TableKind};
pub use udf::{scalar_udfs, RasterH3ScalarUdf};
pub use udtf::{CategoricalAggregateFunction, ContinuousAggregateFunction, PlanPartitionsFunction};

/// Register the `raster_h3` table functions, scalar UDFs, merge aggregates and the
/// `pmtiles` output format (`COPY ... TO 'cells.pmtiles'`, see [`pmtiles`]) into `ctx`.
///
/// For SedonaDB, pass `&sedona_context.ctx`.
pub fn register_all(ctx: &SessionContext) {
    ctx.register_udtf(
        "h3_raster_continuous_aggregate",
        Arc::new(ContinuousAggregateFunction),
    );
    ctx.register_udtf(
        "h3_raster_categorical_aggregate",
        Arc::new(CategoricalAggregateFunction),
    );
    ctx.register_udtf(
        "h3_raster_plan_partitions",
        Arc::new(PlanPartitionsFunction),
    );
    for udf in scalar_udfs() {
        ctx.register_udf(udf);
    }
    for udaf in merge_udafs() {
        ctx.register_udaf(udaf);
    }
    // Replacing an existing `pmtiles` registration cannot fail.
    let _ = register_pmtiles_format(ctx);
}
