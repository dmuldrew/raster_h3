//! # `raster_h3`
//!
//! A high-performance DuckDB loadable extension written in pure Rust for aggregating
//! geospatial raster data (GeoTIFF) into Uber H3 hexagonal grid cells.
//!
//! The crate operates as both a `cdylib` (DuckDB loadable extension loaded at runtime
//! via `LOAD 'raster_h3'`) and an `rlib` (Rust library crate).
//!
//! ## Pipeline Overview
//!
//! The processing pipeline follows a multi-stage flow:
//!
//! `raster` (GeoTIFF I/O) &rarr; `crs` (coordinate projection) &rarr; `aggregator` (scanline H3 accumulation) &rarr; `functions` (DuckDB C-FFI table functions) &rarr; SQL output.
//!
//! - [`raster`]: GeoTIFF reading, metadata decoding, chunked raster processing, and memory-mapped file I/O.
//! - [`crs`]: Coordinate reference system detection and reprojection from raster space to WGS 84 coordinates.
//! - [`aggregator`]: Parallel scanline traversal accumulating raster pixel values into H3 hexagonal cells.
//! - [`functions`]: DuckDB C-FFI scalar and table functions exposing the aggregated data to SQL queries.
//!
//! ## Direct Export Modules
//!
//! In addition to returning query results directly in DuckDB:
//!
//! - The [`pmtiles`] module provides direct PMTiles v3 vector tile export with MVT encoding.
//! - The [`parquet`] module provides native Parquet/GeoParquet streaming export.

/// Scanline accumulation and statistical aggregation of raster pixels into H3 hexagonal cells.
pub mod aggregator;
/// Coordinate reference system (CRS) detection, parsing, and reprojection.
pub mod crs;
/// Shared zero-allocation hexadecimal formatting and WKB geometry encoding utilities.
pub mod encoding;
/// Crate-wide error handling types and result alias.
pub mod error;
/// DuckDB C-API foreign function interface (FFI) bindings and wrappers.
pub mod ffi;
/// DuckDB scalar and table function implementations exposed to SQL.
pub mod functions;
/// Native Parquet and GeoParquet streaming export.
pub mod parquet;
/// Direct PMTiles v3 vector tile export with MVT encoding.
pub mod pmtiles;
/// GeoTIFF file decoding, metadata extraction, and chunked raster I/O.
pub mod raster;
/// Cross-format dataset transcoding pipelines (e.g. Parquet to PMTiles).
pub mod transcode;

use std::ffi::c_char;
use std::ptr;

use ffi::*;
use functions::{
    register_categorical_table_function, register_scalar_functions, register_table_function,
};

/// RAII guard ensuring DuckDB connection is disconnected on success, error, or unwind
struct ConnectionGuard(duckdb_connection);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                duckdb_disconnect(&mut self.0);
            }
        }
    }
}

/// Extension entry point invoked by DuckDB upon `LOAD 'raster_h3'`
#[no_mangle]
pub unsafe extern "C" fn raster_h3_init(db: duckdb_database) -> bool {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut raw_con: duckdb_connection = ptr::null_mut();
        if duckdb_connect(db, &mut raw_con) != DuckDBState::Success {
            return false;
        }
        let guard = ConnectionGuard(raw_con);
        let con = guard.0;

        // 1. Register h3_raster_continuous_aggregate Table Function (Continuous: mean, stddev, sum, min, max)
        if register_table_function(con).is_err() {
            return false;
        }

        // 2. Register h3_raster_categorical_aggregate Table Function (Categorical: majority, histogram, long form)
        if register_categorical_table_function(con).is_err() {
            return false;
        }

        // 3. Register Scalar Helper Functions (h3_to_string, h3_to_lat, h3_to_lng, h3_get_resolution)
        if register_scalar_functions(con).is_err() {
            return false;
        }

        // 4. Register h3_raster_to_pmtiles Table Function (Direct PMTiles v3 export)
        if functions::register_pmtiles_table_function(con).is_err() {
            return false;
        }

        // 5. Register h3_raster_to_parquet Table Function (Direct native Parquet export - Option 5)
        if functions::register_parquet_table_function(con).is_err() {
            return false;
        }

        true
    }));

    match result {
        Ok(success) => success,
        Err(payload) => {
            let msg = crate::ffi::safe_panic_payload_to_string(payload);
            crate::ffi::safe_eprintln("[raster_h3] raster_h3_init panicked", &msg);
            false
        }
    }
}

/// C API extension entry point invoked by DuckDB v1.2+
#[no_mangle]
pub unsafe extern "C" fn raster_h3_init_c_api(
    info: crate::ffi::duckdb_c::duckdb_extension_info,
    access: *const crate::ffi::duckdb_c::duckdb_extension_access,
) -> bool {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if access.is_null() {
            return false;
        }
        let get_db = match (*access).get_database {
            Some(f) => f,
            None => return false,
        };
        let db_ptr = get_db(info);
        if db_ptr.is_null() {
            return false;
        }
        let db = *db_ptr;
        raster_h3_init(db)
    }));

    match result {
        Ok(success) => success,
        Err(payload) => {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let msg = crate::ffi::safe_panic_payload_to_string(payload);
                crate::ffi::safe_eprintln("[raster_h3] raster_h3_init_c_api panicked", &msg);
                if !access.is_null() {
                    if let Some(set_err) = (*access).set_error {
                        let err_c = to_c_string(&format!("raster_h3 extension init panicked: {msg}"));
                        set_err(info, err_c.as_ptr());
                    }
                }
            }))
            .map_err(|secondary| {
                std::mem::forget(secondary);
                if !access.is_null() {
                    if let Some(set_err) = (*access).set_error {
                        set_err(info, c"raster_h3 extension init panicked".as_ptr());
                    }
                }
            });
            false
        }
    }
}

/// Version entry point invoked by DuckDB (specifies C-API version v0.0.1)
#[no_mangle]
pub unsafe extern "C" fn raster_h3_version() -> *const c_char {
    c"v0.0.1".as_ptr()
}
