//! # `raster-h3-duckdb`
//!
//! DuckDB C-API loadable extension bindings (`ffi` and `functions`) built on [`raster_h3_core`].

pub use raster_h3_core::*;

/// DuckDB C-API foreign function interface (FFI) bindings and wrappers.
pub mod ffi;
/// DuckDB scalar and table function implementations exposed to SQL.
pub mod functions;

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
///
/// # Safety
/// `db` must be a valid DuckDB database handle; DuckDB calls this once
/// when loading the extension.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_init(db: duckdb_database) -> bool {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut raw_con: duckdb_connection = ptr::null_mut();
        if duckdb_connect(db, &mut raw_con) != DuckDBState::Success {
            return false;
        }
        let guard = ConnectionGuard(raw_con);
        let con = guard.0;

        // 1. Register h3_raster_continuous_aggregate Table Function
        if register_table_function(con).is_err() {
            return false;
        }

        // 2. Register h3_raster_categorical_aggregate Table Function
        if register_categorical_table_function(con).is_err() {
            return false;
        }

        // 3. Register Scalar Helper Functions
        if register_scalar_functions(con).is_err() {
            return false;
        }

        // 4. Register h3_raster_to_pmtiles Table Function
        if functions::register_pmtiles_table_function(con).is_err() {
            return false;
        }

        // 5. Register h3_raster_to_parquet Table Function
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
///
/// # Safety
/// `info` and `access` must be the extension info and access table
/// DuckDB passes to this entry point.
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
                        let err_c =
                            to_c_string(&format!("raster_h3 extension init panicked: {msg}"));
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
pub extern "C" fn raster_h3_version() -> *const c_char {
    c"v0.0.1".as_ptr()
}
