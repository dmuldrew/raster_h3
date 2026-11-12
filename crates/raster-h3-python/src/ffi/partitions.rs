// -----------------------------------------------------------------------------
// Partitioned scans
// -----------------------------------------------------------------------------

use std::ffi::{c_char, c_int};
use std::ptr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::{merge_partial_batches, ArrowArrayStream, ArrowSchema};

use crate::config::PyScanConfig;
use crate::ffi::args::{
    build_py_scan_config, build_py_scan_config_v2, parse_c_partition, placeholder_partition,
};
use crate::ffi::v2::raster_h3_py_plan_partitions_v2;
use crate::ffi::{ffi_call, set_c_err};
use crate::scanner::{PyBatchReader, PyRasterScanner};

/// Plan the partitions of a raster scan: a JSON array of partition objects, each of which
/// can be passed (re-serialized) to the `_partitioned` scan entry points. `crs` and `bbox`
/// must match the scans'. Free the string with `raster_h3_py_free_string`.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL; `out_err` must be
/// NULL or a writable `char **`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_plan_partitions(
    path: *const c_char,
    crs: *const c_char,
    bbox: *const c_char,
    target_chunks: i64,
    out_err: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: forwards the caller's arguments, which meet the `_v2` contract, with no options.
    unsafe { raster_h3_py_plan_partitions_v2(path, crs, bbox, target_chunks, ptr::null(), out_err) }
}

/// [`raster_h3_py_create_arrow_schema`](crate::raster_h3_py_create_arrow_schema) for a partitioned scan: the projected columns plus
/// `partial_state`.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_create_arrow_schema_partitioned(
    categorical: c_int,
    format: *const c_char,
    columns: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut ArrowSchema {
    // SAFETY: as for `raster_h3_py_create_arrow_schema`; the returned box is freed by
    // `raster_h3_py_free_arrow_schema`.
    unsafe {
        ffi_call("create_arrow_schema_partitioned", out_err, || {
            let cfg = build_py_scan_config(
                c"schema".as_ptr(),
                ptr::null(),
                categorical,
                format,
                ptr::null(),
                0,
                0.0,
                ptr::null(),
                columns,
                -1,
            )?;
            // The schema does not depend on which partition is scanned.
            let scanner = PyRasterScanner::new(PyScanConfig {
                partition: Some(placeholder_partition()),
                ..cfg
            });
            Ok(Box::into_raw(Box::new(ArrowSchema::from_column_schema(
                &*scanner.schema()?,
            ))))
        })
        .unwrap_or(ptr::null_mut())
    }
}

/// [`raster_h3_py_create_arrow_stream_ex`](crate::raster_h3_py_create_arrow_stream_ex) scanning one partition (`partition`: a JSON
/// object from `raster_h3_py_plan_partitions`). `limit` must be negative (none).
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_create_arrow_stream_partitioned(
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
    partition: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut ArrowArrayStream {
    // SAFETY: as for `raster_h3_py_create_arrow_stream_ex`; the returned box is freed by
    // `raster_h3_py_free_arrow_stream`.
    unsafe {
        ffi_call("create_arrow_stream_partitioned", out_err, || {
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
            cfg.partition = Some(parse_c_partition(partition)?);
            let stream = PyRasterScanner::new(cfg).to_arrow_c_stream()?;
            Ok(Box::into_raw(Box::new(stream)))
        })
        .unwrap_or(ptr::null_mut())
    }
}

/// [`raster_h3_py_open_batch_reader_ex`](crate::raster_h3_py_open_batch_reader_ex) scanning one partition; see
/// [`raster_h3_py_create_arrow_stream_partitioned`].
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_open_batch_reader_partitioned(
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
    partition: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut PyBatchReader {
    // SAFETY: as for `raster_h3_py_open_batch_reader_ex`; the returned box is freed by
    // `raster_h3_py_free_batch_reader`.
    unsafe {
        ffi_call("open_batch_reader_partitioned", out_err, || {
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
            cfg.partition = Some(parse_c_partition(partition)?);
            let (schema, iter) = PyRasterScanner::new(cfg).batch_iter()?;
            Ok(Box::into_raw(Box::new(PyBatchReader {
                schema,
                iter,
                transform_failures,
            })))
        })
        .unwrap_or(ptr::null_mut())
    }
}

/// Merge the partial rows of all partitions of a scan into final rows.
///
/// `input` is an `ArrowArrayStream` of `resolution` (UInt8), `h3_index` (UInt64) and
/// `partial_state` (Binary, no nulls) columns. It is moved: on return the caller's struct is
/// marked released, whatever the outcome. `categorical` and `columns` must match the scans'.
/// Returns a stream of the merged, filtered rows (free with `raster_h3_py_free_arrow_stream`).
///
/// # Safety
/// `input` must be null or point to a valid `ArrowArrayStream`; the other pointer arguments
/// must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_merge_partials(
    input: *mut ArrowArrayStream,
    categorical: c_int,
    columns: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut ArrowArrayStream {
    // SAFETY: forwards the caller's arguments, which meet the `_v2` contract, with no options.
    unsafe { raster_h3_py_merge_partials_v2(input, categorical, columns, ptr::null(), out_err) }
}

/// [`raster_h3_py_merge_partials`] with the scans' `options` JSON object (NULL for none), so
/// merged cells get the same filters (`min_count`, `min_mean`, `max_mean`,
/// `min_majority_fraction`) and quantile columns as the scans' final rows. Options that only
/// affect reading pixels are validated and otherwise ignored.
///
/// # Safety
/// As for [`raster_h3_py_merge_partials`]; `options` must be NULL or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_merge_partials_v2(
    input: *mut ArrowArrayStream,
    categorical: c_int,
    columns: *const c_char,
    options: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut ArrowArrayStream {
    // SAFETY: `out_err` meets `set_c_err`'s contract and `columns` / `options` are null or
    // NUL-terminated (caller contract). Non-null `input` is a valid stream that we take over
    // with move semantics: the struct is copied out and the caller's copy loses its
    // `release`, so it is released exactly once, by `owned`'s `Drop`.
    unsafe {
        if input.is_null() {
            set_c_err(out_err, "input stream is NULL");
            return ptr::null_mut();
        }
        let mut owned = ptr::read(input);
        (*input).release = None;
        ffi_call("merge_partials", out_err, move || {
            let (_, batches) = owned.collect_batches()?;
            drop(owned);
            let cfg = build_py_scan_config_v2(
                c"merge".as_ptr(),
                ptr::null(),
                categorical,
                ptr::null(),
                ptr::null(),
                0,
                0.0,
                ptr::null(),
                columns,
                -1,
                options,
            )?;
            // Resolve `columns` as the partitioned scans did, so a listed `partial_state`
            // (always appended by those scans) is accepted rather than unknown.
            let projection = PyRasterScanner::new(PyScanConfig {
                partition: Some(placeholder_partition()),
                ..cfg.clone()
            })
            .resolve_projection()?;
            let mut config = MultiResolutionConfig::new(cfg.resolutions.clone());
            cfg.apply_cell_options(&mut config);
            let (schema, merged) = merge_partial_batches(
                &batches,
                cfg.categorical,
                &config,
                projection.as_deref(),
                cfg.batch_size,
            )?;
            let stream = ArrowArrayStream::from_iterator(schema, merged.into_iter().map(Ok));
            Ok(Box::into_raw(Box::new(stream)))
        })
        .unwrap_or(ptr::null_mut())
    }
}
