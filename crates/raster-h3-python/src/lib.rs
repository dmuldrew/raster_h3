//! # `raster-h3-python`
//!
//! Python, Polars, and Arrow PyCapsule (`__arrow_c_stream__` / `__arrow_c_schema__`) bindings
//! for [`raster_h3_core`].
//!
//! Exposes both:
//! 1. High-level Rust types ([`PyRasterScanner`]) and Arrow C Stream generators.
//! 2. C-ABI entry points (`raster_h3_py_*`) and `PyCapsule` destructors so Python (`ctypes` / `cffi`
//!    or C-API `PyCapsule_New`) can construct zero-copy `"arrow_array_stream"` and `"arrow_schema"`
//!    PyCapsules for Polars (`polars.from_arrow`, `register_io_source`), PyArrow (`pyarrow.table`),
//!    DuckDB, Daft, GeoPandas, and Lonboard.

mod config;
mod ffi;
mod scanner;

#[cfg(test)]
mod tests;

pub use config::PyScanConfig;
pub use ffi::*;
pub use scanner::{BatchIter, PyBatchReader, PyRasterScanner};
