//! Shared DuckDB Table Function Parameter & Binding Utilities
//!
//! Provides a safe, ergonomic abstraction over DuckDB C FFI bind and function registration APIs.
//! Centralizes parameter extraction, type conversion, bounding box resolution, and column definitions
//! across continuous, categorical, parquet, and pmtiles table functions.

pub mod bind_helper;
pub mod chunk_writer;
pub mod lifecycle;
pub mod parsing;
pub mod record_queue;
pub mod registration;

pub use bind_helper::{BindHelper, CommonRasterBindParams, CommonRasterParams, OwnedValue};
pub use chunk_writer::ChunkWriter;
pub use lifecycle::{
    delete_boxed, estimate_raster_cardinality, extract_projected_columns,
    init_table_function_local, open_mosaic_or_set_error, set_table_function_init_data,
    TableFunctionLocalData, H3_AREA_M2,
};
pub use parsing::{parse_bbox_str, parse_resolutions_str};
pub use record_queue::ConcurrentRecordQueue;
pub use registration::{
    add_named_parameter, add_positional_parameter, register_common_raster_named_parameters,
};
