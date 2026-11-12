//! Columnar JSON scan entry points.

use std::ffi::{c_char, c_int, CString};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use raster_h3_core::error::safe_panic_payload_to_string;

use crate::ffi::args::build_py_scan_config;
use crate::ffi::set_c_err;
use crate::scanner::PyRasterScanner;

/// Execute a scan and return a NUL-terminated JSON string containing columnar arrays.
/// Free the returned string with `raster_h3_py_free_string`.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_scan_json(
    path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    format: *const c_char,
    crs: *const c_char,
    has_nodata: c_int,
    nodata: f64,
    bbox: *const c_char,
    columns: *const c_char,
    limit: i64,
    out_err: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: forwards the caller's arguments, which meet the `_ex` contract.
    unsafe {
        raster_h3_py_scan_json_ex(
            path,
            resolutions,
            categorical,
            format,
            crs,
            has_nodata,
            nodata,
            bbox,
            columns,
            limit,
            0,
            ptr::null_mut(),
            out_err,
        )
    }
}

/// [`raster_h3_py_scan_json`] with `strict_transforms` (non-zero fails the scan as
/// soon as a valid sample point cannot be transformed to WGS84) and
/// `out_transform_failures`, which receives the number of such points dropped.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL;
/// `out_transform_failures` must be NULL or point to a writable `u64`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_scan_json_ex(
    path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    format: *const c_char,
    crs: *const c_char,
    has_nodata: c_int,
    nodata: f64,
    bbox: *const c_char,
    columns: *const c_char,
    limit: i64,
    strict_transforms: c_int,
    out_transform_failures: *mut u64,
    out_err: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_transform_failures` is null or a writable `u64`; `out_err`
    // is null or a writable `char **` (as `set_c_err` requires), and error strings are
    // freed by the caller with `raster_h3_py_free_string`. The returned JSON string is
    // freed with `raster_h3_py_free_string`.
    unsafe {
        let failures = Arc::new(AtomicU64::new(0));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut cfg = build_py_scan_config(
                path,
                resolutions,
                categorical,
                format,
                crs,
                has_nodata,
                nodata,
                bbox,
                columns,
                limit,
            )?;
            cfg.strict_transforms = strict_transforms != 0;
            cfg.transform_failure_counter = Some(Arc::clone(&failures));
            PyRasterScanner::new(cfg).to_columnar_json()
        }));
        if !out_transform_failures.is_null() {
            *out_transform_failures = failures.load(Ordering::Relaxed);
        }

        match res {
            Ok(Ok(json_str)) => CString::new(json_str).unwrap_or_default().into_raw(),
            Ok(Err(e)) => {
                set_c_err(out_err, &e.to_string());
                ptr::null_mut()
            }
            Err(payload) => {
                let msg = safe_panic_payload_to_string(payload);
                set_c_err(out_err, &format!("Panic in scan_json: {msg}"));
                ptr::null_mut()
            }
        }
    }
}
