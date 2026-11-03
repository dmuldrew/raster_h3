//! raster-h3-duckdb integration tests, linked into a single test binary.

#[path = "../../../raster-h3-core/tests/it/helpers.rs"]
mod helpers;

mod cell_bbox_pushdown;
// The ffi_sql harness emulates a v1.2.0-shaped API table; the v1.5.6 build
// targets the newer table, so these mock-driven tests only run in the
// default build. Feature-path coverage lives in the real-CLI smoke tests
// (`duckdb_sql_smoke::real_sql_v156_features`).
#[cfg(not(feature = "duckdb-api-v1_5_6"))]
mod ffi_sql;
mod prefetch_resilience;
mod record_queue;
mod spatial_geometry;
