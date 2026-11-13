//! Integration tests for the `scan` module (formerly the `raster-h3-datafusion` crate),
//! linked into a single test binary.

#[path = "../../../raster-h3-core/tests/it/helpers.rs"]
mod helpers;

mod bbox_pushdown;
mod categorical_options;
mod compaction_pushdown;
mod continuous_options;
mod partitions;
mod rows;
mod scalar_udfs;
mod statistics;
mod transform_failures;
