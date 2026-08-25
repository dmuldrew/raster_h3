pub mod duckdb_c;

pub use duckdb_c::*;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// Helper to convert a Rust string slice to a CString raw pointer
pub fn to_c_string(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// Helper to convert a DuckDB allocated C string to a Rust String and free the C string
pub unsafe fn from_duckdb_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    duckdb_free(ptr as *mut std::ffi::c_void);
    Some(s)
}
