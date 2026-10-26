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
            // SAFETY: the guard is only built from a connection `duckdb_connect`
            // just opened, and it is the sole owner, so this disconnects once.
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
        // SAFETY: `db` is the live database DuckDB is loading us into (caller
        // contract); `raw_con` is a local out-parameter.
        if unsafe { duckdb_connect(db, &mut raw_con) } != DuckDBState::Success {
            return false;
        }
        let guard = ConnectionGuard(raw_con);
        let con = guard.0;

        // SAFETY: `con` was just opened and stays open until `guard` drops at the
        // end of this closure, after every registration has returned.
        unsafe {
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
        }

        true
    }));

    match result {
        Ok(success) => success,
        Err(payload) => {
            let msg = safe_panic_payload_to_string(payload);
            safe_eprintln("[raster_h3] raster_h3_init panicked", &msg);
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
    info: duckdb_extension_info,
    access: *const duckdb_extension_access,
) -> bool {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if access.is_null() {
            return false;
        }
        // SAFETY: `access` is non-null and points to DuckDB's
        // `duckdb_extension_access` table (caller contract), whose layout is
        // pinned by the const assertions in `duckdb_c`; `info` is the matching
        // extension handle each callback expects.
        let access = unsafe { &*access };
        if let Some(get_api) = access.get_api {
            // SAFETY: see above; the version string is a NUL-terminated literal.
            let api = unsafe { get_api(info, DUCKDB_C_API_VERSION.as_ptr()) };
            if api.is_null() {
                // DuckDB has already reported the version mismatch via set_error.
                return false;
            }
            // SAFETY: a non-null result of `get_api` for `DUCKDB_C_API_VERSION` is a
            // `duckdb_ext_api_v1` of at least that version, i.e. `API_TABLE_LEN`
            // slots; it is copied before init returns.
            unsafe { install_api_table(api) };
        }
        let get_db = match access.get_database {
            Some(f) => f,
            None => return false,
        };
        // SAFETY: DuckDB's own callback, called with its own `info`.
        let db_ptr = unsafe { get_db(info) };
        if db_ptr.is_null() {
            return false;
        }
        // SAFETY: non-null `get_database` result points to a live
        // `duckdb_database` handle owned by DuckDB.
        let db = unsafe { *db_ptr };
        // SAFETY: `db` is the database DuckDB is loading the extension into.
        unsafe { raster_h3_init(db) }
    }));

    match result {
        Ok(success) => success,
        Err(payload) => {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let msg = safe_panic_payload_to_string(payload);
                safe_eprintln("[raster_h3] raster_h3_init_c_api panicked", &msg);
                if !access.is_null() {
                    // SAFETY: non-null `access` is DuckDB's access table (caller
                    // contract); `set_error` copies the NUL-terminated message.
                    unsafe {
                        if let Some(set_err) = (*access).set_error {
                            let err_c =
                                to_c_string(&format!("raster_h3 extension init panicked: {msg}"));
                            set_err(info, err_c.as_ptr());
                        }
                    }
                }
            }))
            .map_err(|secondary| {
                std::mem::forget(secondary);
                if !access.is_null() {
                    // SAFETY: as above, with a static C string literal.
                    unsafe {
                        if let Some(set_err) = (*access).set_error {
                            set_err(info, c"raster_h3 extension init panicked".as_ptr());
                        }
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
