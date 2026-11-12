//! Pull-based batch reader entry points.

use std::ffi::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use raster_h3_core::columnar::ArrowArrayStream;
use raster_h3_core::error::{catch_unwind_or, safe_panic_payload_to_string, Result};

use crate::ffi::args::build_py_scan_config;
use crate::ffi::set_c_err;
use crate::scanner::{PyBatchReader, PyRasterScanner};

/// Open a [`PyBatchReader`] for incremental consumption (`batch_size` <= 0 uses the default).
/// Free with `raster_h3_py_free_batch_reader`.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_open_batch_reader(
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
    batch_size: i64,
    out_err: *mut *mut c_char,
) -> *mut PyBatchReader {
    // SAFETY: forwards the caller's arguments, which meet the `_ex` contract.
    unsafe {
        raster_h3_py_open_batch_reader_ex(
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
            batch_size,
            0,
            out_err,
        )
    }
}

/// [`raster_h3_py_open_batch_reader`] with `strict_transforms`: when non-zero, the
/// reader fails as soon as a valid sample point cannot be transformed to WGS84.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_open_batch_reader_ex(
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
    batch_size: i64,
    strict_transforms: c_int,
    out_err: *mut *mut c_char,
) -> *mut PyBatchReader {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_err` is null or a writable `char **` (as `set_c_err`
    // requires), and error strings are freed by the caller with `raster_h3_py_free_string`.
    // The returned box is freed by `raster_h3_py_free_batch_reader`.
    unsafe {
        let res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<PyBatchReader> {
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
                if batch_size > 0 {
                    cfg.batch_size = batch_size as usize;
                }
                let transform_failures = Arc::new(AtomicU64::new(0));
                cfg.strict_transforms = strict_transforms != 0;
                cfg.transform_failure_counter = Some(Arc::clone(&transform_failures));
                let (schema, iter) = PyRasterScanner::new(cfg).batch_iter()?;
                Ok(PyBatchReader {
                    schema,
                    iter,
                    transform_failures,
                })
            }));

        match res {
            Ok(Ok(reader)) => Box::into_raw(Box::new(reader)),
            Ok(Err(e)) => {
                set_c_err(out_err, &e.to_string());
                ptr::null_mut()
            }
            Err(payload) => {
                let msg = safe_panic_payload_to_string(payload);
                set_c_err(out_err, &format!("Panic in open_batch_reader: {msg}"));
                ptr::null_mut()
            }
        }
    }
}

/// Pull the next batch as a single-batch `ArrowArrayStream`. Returns NULL at end of
/// stream, or NULL with `out_err` set on failure.
///
/// # Safety
/// `reader` must be a live pointer returned by `raster_h3_py_open_batch_reader`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_batch_reader_next(
    reader: *mut PyBatchReader,
    out_err: *mut *mut c_char,
) -> *mut ArrowArrayStream {
    // SAFETY: non-null `reader` is a live box from `raster_h3_py_open_batch_reader`, not
    // used concurrently (caller contract), so a unique borrow is valid for this call;
    // `out_err` is null or a writable `char **` (as `set_c_err` requires), and error
    // strings are freed by the caller with `raster_h3_py_free_string`.
    unsafe {
        if reader.is_null() {
            set_c_err(out_err, "batch reader is NULL");
            return ptr::null_mut();
        }
        let reader = &mut *reader;
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.iter.next()));
        match res {
            Ok(Some(Ok(batch))) => Box::into_raw(Box::new(ArrowArrayStream::from_iterator(
                Arc::clone(&reader.schema),
                std::iter::once(Ok(batch)),
            ))),
            Ok(None) => ptr::null_mut(),
            Ok(Some(Err(e))) => {
                set_c_err(out_err, &e.to_string());
                ptr::null_mut()
            }
            Err(payload) => {
                let msg = safe_panic_payload_to_string(payload);
                set_c_err(out_err, &format!("Panic in batch_reader_next: {msg}"));
                ptr::null_mut()
            }
        }
    }
}

/// Valid sample points the reader has dropped so far because their coordinate
/// transform failed (0 for a null reader).
///
/// # Safety
/// `reader` must be null or a live pointer returned by `raster_h3_py_open_batch_reader`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_batch_reader_transform_failures(
    reader: *const PyBatchReader,
) -> u64 {
    // SAFETY: non-null `reader` is a live box from `raster_h3_py_open_batch_reader`
    // (caller contract); only its atomic counter is read.
    unsafe { reader.as_ref() }.map_or(0, |r| r.transform_failures.load(Ordering::Relaxed))
}

/// Free a reader returned by `raster_h3_py_open_batch_reader`.
///
/// # Safety
/// `reader` must be null or returned by `raster_h3_py_open_batch_reader`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_free_batch_reader(reader: *mut PyBatchReader) {
    // SAFETY: non-null `reader` came from `Box::into_raw` in
    // `raster_h3_py_open_batch_reader` and is freed once (caller contract).
    unsafe {
        if !reader.is_null() {
            let reader = Box::from_raw(reader);
            catch_unwind_or((), || drop(reader));
        }
    }
}
