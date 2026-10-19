//! Cross-engine and packaging tests that need the root `raster_h3` cdylib or bins.

#[path = "../../crates/raster-h3-core/tests/it/helpers.rs"]
mod helpers;

mod duckdb_sql_smoke;
mod multi_engine_e2e;
mod package_extension;
mod pg_extension_e2e;
