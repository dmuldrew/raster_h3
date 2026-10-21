//! raster-h3-datafusion integration tests, linked into a single test binary.

#[path = "../../../raster-h3-core/tests/it/helpers.rs"]
mod helpers;

mod categorical_options;
mod continuous_options;
mod rows;
mod scalar_udfs;
mod statistics;
