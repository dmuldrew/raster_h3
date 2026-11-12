//! Arrow C Data Interface schema and stream entry points.

use std::ffi::{c_char, c_int};
use std::ptr;

use raster_h3_core::columnar::{ArrowArrayStream, ArrowSchema};
use raster_h3_core::error::safe_panic_payload_to_string;

use crate::config::PyScanConfig;
use crate::ffi::args::{build_py_scan_config, parse_c_format};
use crate::ffi::{opt_cstr, set_c_err};
use crate::scanner::PyRasterScanner;

/// Allocate a heap-boxed `ArrowSchema` for Python `__arrow_c_schema__`.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_create_arrow_schema(
    categorical: c_int,
    format: *const c_char,
    columns: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut ArrowSchema {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_err` is null or a writable `char **` (as `set_c_err`
    // requires), and error strings are freed by the caller with `raster_h3_py_free_string`.
    // The returned box is freed by `raster_h3_py_free_arrow_schema`.
    unsafe {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let fmt = parse_c_format(format)?;
            let cols_opt = opt_cstr(columns).map(|s| {
                s.split(',')
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
                    .collect::<Vec<_>>()
            });
            let scanner = PyRasterScanner::new(PyScanConfig {
                categorical: categorical != 0,
                format: fmt,
                columns: cols_opt,
                ..Default::default()
            });
            scanner.to_arrow_c_schema()
        }));

        match res {
            Ok(Ok(schema)) => Box::into_raw(Box::new(schema)),
            Ok(Err(e)) => {
                set_c_err(out_err, &e.to_string());
                ptr::null_mut()
            }
            Err(payload) => {
                let msg = safe_panic_payload_to_string(payload);
                set_c_err(out_err, &format!("Panic in create_arrow_schema: {msg}"));
                ptr::null_mut()
            }
        }
    }
}

/// Free a heap-boxed `ArrowSchema` allocated by `raster_h3_py_create_arrow_schema`.
///
/// # Safety
/// `ptr` must be null or returned by `raster_h3_py_create_arrow_schema`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_free_arrow_schema(ptr: *mut ArrowSchema) {
    // SAFETY: non-null `ptr` came from `Box::into_raw` in
    // `raster_h3_py_create_arrow_schema` and is freed once (caller contract); dropping it
    // runs the schema's own release callback.
    unsafe {
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
    }
}

/// Allocate a heap-boxed `ArrowArrayStream` for Python `__arrow_c_stream__`.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_create_arrow_stream(
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
) -> *mut ArrowArrayStream {
    // SAFETY: forwards the caller's arguments, which meet the `_ex` contract.
    unsafe {
        raster_h3_py_create_arrow_stream_ex(
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
            out_err,
        )
    }
}

/// [`raster_h3_py_create_arrow_stream`] with `strict_transforms`: when non-zero, the
/// stream fails as soon as a valid sample point cannot be transformed to WGS84.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_create_arrow_stream_ex(
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
    out_err: *mut *mut c_char,
) -> *mut ArrowArrayStream {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_err` is null or a writable `char **` (as `set_c_err`
    // requires), and error strings are freed by the caller with `raster_h3_py_free_string`.
    // The returned box is freed by `raster_h3_py_free_arrow_stream`.
    unsafe {
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
            PyRasterScanner::new(cfg).to_arrow_c_stream()
        }));

        match res {
            Ok(Ok(stream)) => Box::into_raw(Box::new(stream)),
            Ok(Err(e)) => {
                set_c_err(out_err, &e.to_string());
                ptr::null_mut()
            }
            Err(payload) => {
                let msg = safe_panic_payload_to_string(payload);
                set_c_err(out_err, &format!("Panic in create_arrow_stream: {msg}"));
                ptr::null_mut()
            }
        }
    }
}

/// Free a heap-boxed `ArrowArrayStream` allocated by `raster_h3_py_create_arrow_stream`.
///
/// # Safety
/// `ptr` must be null or returned by `raster_h3_py_create_arrow_stream`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_free_arrow_stream(ptr: *mut ArrowArrayStream) {
    // SAFETY: non-null `ptr` came from `Box::into_raw` in
    // `raster_h3_py_create_arrow_stream` and is freed once (caller contract); dropping it
    // releases the stream unless a consumer already moved it out.
    unsafe {
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
    }
}
