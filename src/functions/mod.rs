//! DuckDB C-FFI table function and scalar function registrations.
//!
//! This module implements the DuckDB C-FFI table function and scalar function
//! registrations. Each table function follows DuckDB's execution lifecycle:
//!
//! ```text
//! bind (parameter parsing) -> init (state setup) -> function (row emission) -> cleanup
//! ```
//!
//! The [`bind_utils`](crate::functions::bind_utils) submodule centralizes shared bind state management, parameter
//! parsing, chunk writing, and lifecycle management across all table functions.
//!
//! # Submodules
//!
//! - [`table_function`](crate::functions::table_function): Continuous raster aggregation table function (`raster_h3`).
//! - [`categorical_table_function`](crate::functions::categorical_table_function): Categorical raster aggregation table function (`raster_h3_categorical`).
//! - [`pmtiles_table_function`](crate::functions::pmtiles_table_function): Direct streaming export of H3 aggregations to PMTiles archives (`raster_h3_pmtiles`).
//! - [`parquet_table_function`](crate::functions::parquet_table_function): Direct streaming export of H3 aggregations to Parquet files (`raster_h3_parquet`).
//! - [`scalar`](crate::functions::scalar): Helper scalar functions (such as `h3_to_string`).
//! - [`bind_utils`](crate::functions::bind_utils): Shared bind infrastructure, parameter parsing, chunk writing, and lifecycle management.
//! - [`fast_hex`](crate::functions::fast_hex): Fast hexadecimal formatting and parsing for 64-bit H3 cell indices.
//! - [`wkb`](crate::functions::wkb): Well-Known Binary (WKB) geometry serialization for H3 cells.

/// Shared bind state management, parameter parsing, chunk writing, and lifecycle management.
pub mod bind_utils;
/// Categorical raster aggregation table function (`raster_h3_categorical`).
pub mod categorical_table_function;
/// Fast zero-allocation hexadecimal formatting and parsing for 64-bit H3 cell indices (re-exported from crate::encoding).
pub use crate::encoding::fast_hex;
/// Direct streaming export of H3 aggregations to Parquet files (`raster_h3_parquet`).
pub mod parquet_table_function;
/// Direct streaming export of H3 aggregations to PMTiles archives (`raster_h3_pmtiles`).
pub mod pmtiles_table_function;
/// Helper scalar functions such as `h3_to_string` and coordinate extractors.
pub mod scalar;
/// Continuous raster aggregation table function (`raster_h3`).
pub mod table_function;
/// Well-Known Binary (WKB) geometry serialization for H3 cells (re-exported from crate::encoding).
pub use crate::encoding::wkb;

/// Well-Known Binary (WKB) geometry encoding utilities for H3 cells.
pub use crate::encoding::{cell_to_wkb, h3_index_to_wkb};
/// Fast hexadecimal encoding and decoding utilities for H3 cell index `u64` values.
pub use crate::encoding::{fast_hex_u64, parse_hex_u64};
/// Shared parameter parsing, bind state, chunk writing, and lifecycle utilities.
pub use bind_utils::{
    add_named_parameter, add_positional_parameter, delete_boxed, estimate_raster_cardinality,
    extract_projected_columns, register_common_raster_named_parameters, BindHelper, ChunkWriter,
    CommonRasterBindParams, CommonRasterParams, TableFunctionLocalData,
};
/// Registers the `raster_h3_categorical` DuckDB table function for categorical raster aggregation.
pub use categorical_table_function::register_categorical_table_function;
/// Registers the `raster_h3_parquet` DuckDB table function for direct Parquet export.
pub use parquet_table_function::register_parquet_table_function;
/// Registers the `raster_h3_pmtiles` DuckDB table function for direct PMTiles export.
pub use pmtiles_table_function::register_pmtiles_table_function;
/// Registers helper scalar functions (e.g. `h3_to_string`) with DuckDB.
pub use scalar::register_scalar_functions;
/// Registers the `raster_h3` DuckDB table function for continuous raster aggregation.
pub use table_function::register_table_function;
