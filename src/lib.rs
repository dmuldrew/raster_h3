pub mod aggregator;
pub mod crs;
pub mod error;
pub mod ffi;
pub mod functions;
pub mod pmtiles;
pub mod raster;

use std::ffi::c_char;
use std::ptr;

use ffi::*;
use functions::{
    register_categorical_table_function, register_scalar_functions, register_table_function,
};

/// Extension entry point invoked by DuckDB upon `LOAD 'raster_h3'`
#[no_mangle]
pub unsafe extern "C" fn raster_h3_init(db: duckdb_database) -> bool {
    let mut con: duckdb_connection = ptr::null_mut();
    if duckdb_connect(db, &mut con) != DuckDBState::Success {
        return false;
    }

    // 1. Register h3_raster_continuous_aggregate Table Function (Continuous: mean, stddev, sum, min, max)
    if register_table_function(con).is_err() {
        duckdb_disconnect(&mut con);
        return false;
    }

    // 2. Register h3_raster_categorical_aggregate Table Function (Categorical: majority, histogram, long form)
    if register_categorical_table_function(con).is_err() {
        duckdb_disconnect(&mut con);
        return false;
    }

    // 3. Register Scalar Helper Functions (h3_to_string, h3_to_lat, h3_to_lng, h3_get_resolution)
    if register_scalar_functions(con).is_err() {
        duckdb_disconnect(&mut con);
        return false;
    }

    duckdb_disconnect(&mut con);
    true
}

/// Version entry point invoked by DuckDB (specifies C-API version v0.0.1)
#[no_mangle]
pub unsafe extern "C" fn raster_h3_version() -> *const c_char {
    c"v0.0.1".as_ptr()
}
