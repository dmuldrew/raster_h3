//! # `raster_h3`
//!
//! Unified workspace facade re-exporting:
//! - [`raster_h3_core`] (`core`): Pure-Rust GeoTIFF/COG reader, CRS reprojection, H3 scanline aggregator,
//!   Arrow C Data/Stream interface (`ArrowArrayStream`), Parquet/GeoParquet, and PMTiles v3 export.
//! - [`raster_h3_cli`] (`cli`): Command-line streaming (`ipc`, `jsonl`, `parquet`, `pmtiles`).
//! - [`raster_h3_duckdb`] (`duckdb`): DuckDB C-API loadable extension (`LOAD 'raster_h3'`).
//!
//! Behind optional features (off by default, both enabled by `full`):
//! - `raster_h3_python` (`python`, feature `python`): Python / Polars IO plugin and Arrow PyCapsule
//!   (`__arrow_c_stream__`) bindings.
//! - `raster_h3_pg` (`pg`, feature `pg`): PostgreSQL V1 extension (`Set-Returning Functions` and H3
//!   scalar functions).
//!
//! These are opt-in because this crate's `cdylib` is packaged as the DuckDB extension: linking the
//! Python or PostgreSQL adapters would make it export their C symbols as well.

pub use raster_h3_cli as cli;
pub use raster_h3_core as core;
pub use raster_h3_duckdb as duckdb;
#[cfg(feature = "pg")]
pub use raster_h3_pg as pg;
#[cfg(feature = "python")]
pub use raster_h3_python as python;

pub use raster_h3_duckdb::*;

/// Ensure the linker retains DuckDB C-API extension entry points in `libraster_h3.{so,dylib,dll}`.
#[doc(hidden)]
pub fn _keep_duckdb_c_api_symbols() -> (usize, usize, usize) {
    (
        raster_h3_init as *const () as usize,
        raster_h3_init_c_api as *const () as usize,
        raster_h3_version as *const () as usize,
    )
}
