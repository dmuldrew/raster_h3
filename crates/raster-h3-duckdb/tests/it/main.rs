//! raster-h3-duckdb integration tests, linked into a single test binary.

#[path = "../../../raster-h3-core/tests/it/helpers.rs"]
mod helpers;

mod cell_bbox_pushdown;
mod ffi_sql;
mod prefetch_resilience;
mod record_queue;
mod spatial_geometry;
