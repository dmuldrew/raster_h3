use crate::ffi::{
    duckdb_create_logical_type, duckdb_destroy_logical_type, duckdb_table_function,
    duckdb_table_function_add_named_parameter, duckdb_table_function_add_parameter, to_c_string,
    DuckDBType,
};

/// Add a positional parameter with automated logical type lifecycle management
pub unsafe fn add_positional_parameter(func: duckdb_table_function, duckdb_type: DuckDBType) {
    let mut logical_type = duckdb_create_logical_type(duckdb_type);
    duckdb_table_function_add_parameter(func, logical_type);
    duckdb_destroy_logical_type(&mut logical_type);
}

/// Add a named parameter with automated logical type lifecycle management
pub unsafe fn add_named_parameter(
    func: duckdb_table_function,
    name: &str,
    duckdb_type: DuckDBType,
) {
    let param_name = to_c_string(name);
    let mut logical_type = duckdb_create_logical_type(duckdb_type);
    duckdb_table_function_add_named_parameter(func, param_name.as_ptr(), logical_type);
    duckdb_destroy_logical_type(&mut logical_type);
}

/// Register standard named parameters shared across all raster aggregation functions
pub unsafe fn register_common_raster_named_parameters(func: duckdb_table_function) {
    add_named_parameter(func, "resolution", DuckDBType::BigInt);
    add_named_parameter(func, "resolutions", DuckDBType::Varchar);
    add_named_parameter(func, "min_resolution", DuckDBType::BigInt);
    add_named_parameter(func, "max_resolution", DuckDBType::BigInt);
    add_named_parameter(func, "sampling", DuckDBType::Varchar);
    add_named_parameter(func, "band", DuckDBType::BigInt);
    add_named_parameter(func, "nodata", DuckDBType::Double);
    add_named_parameter(func, "source_crs", DuckDBType::Varchar);
    add_named_parameter(func, "crs", DuckDBType::Varchar);
    add_named_parameter(func, "min_lon", DuckDBType::Double);
    add_named_parameter(func, "min_lat", DuckDBType::Double);
    add_named_parameter(func, "max_lon", DuckDBType::Double);
    add_named_parameter(func, "max_lat", DuckDBType::Double);
    add_named_parameter(func, "bbox", DuckDBType::Varchar);
    add_named_parameter(func, "h3_cell", DuckDBType::BigInt);
    add_named_parameter(func, "h3_hex", DuckDBType::Varchar);
    add_named_parameter(func, "compact", DuckDBType::Boolean);
    add_named_parameter(func, "overlap_rule", DuckDBType::Varchar);
    add_named_parameter(func, "workers", DuckDBType::BigInt);
    add_named_parameter(func, "threads", DuckDBType::BigInt);
}
