// -----------------------------------------------------------------------------
// `_v2` scan entry points: the positional arguments plus an `options` JSON object
// -----------------------------------------------------------------------------

use std::ffi::{c_char, c_int, CString};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{plan_partitions, QuantileTarget};
use raster_h3_core::columnar::{ArrowArrayStream, ArrowSchema};
use raster_h3_core::error::RasterH3Error;
use raster_h3_core::raster::mosaic::{resolve_raster_sources, MosaicReader};

use crate::ffi::args::{
    build_py_scan_config, build_py_scan_config_v2, parse_c_opt_partition,
    parse_overlap_rule_strict, parse_quantiles, placeholder_partition, PyScanOptions,
};
use crate::ffi::{ffi_call, opt_cstr};
use crate::scanner::{PyBatchReader, PyRasterScanner};

/// [`raster_h3_py_create_arrow_stream_ex`](crate::raster_h3_py_create_arrow_stream_ex) that also takes `partition` (NULL for a whole
/// scan, else a JSON object from `raster_h3_py_plan_partitions_v2`) and `options`, a JSON
/// object of further scan options (NULL for none): `band`, `sampling`, `overlap_rule`,
/// `min_majority_fraction`, `spectral_formula` and `nir_band` / `red_band` /
/// `green_band` / `blue_band` / `swir_band`.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL; `out_err` must be
/// NULL or a writable `char **`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_create_arrow_stream_v2(
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
    options: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut ArrowArrayStream {
    // SAFETY: the C string arguments are null or NUL-terminated and `out_err` is null or a
    // writable `char **` (caller contract). The returned box is freed by
    // `raster_h3_py_free_arrow_stream`.
    unsafe {
        ffi_call("create_arrow_stream_v2", out_err, || {
            let mut cfg = build_py_scan_config_v2(
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
                options,
            )?;
            cfg.strict_transforms = strict_transforms != 0;
            cfg.partition = parse_c_opt_partition(partition)?;
            let stream = PyRasterScanner::new(cfg).to_arrow_c_stream()?;
            Ok(Box::into_raw(Box::new(stream)))
        })
        .unwrap_or(ptr::null_mut())
    }
}

/// [`raster_h3_py_open_batch_reader_ex`](crate::raster_h3_py_open_batch_reader_ex) with the `partition` and `options` arguments of
/// [`raster_h3_py_create_arrow_stream_v2`].
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL; `out_err` must be
/// NULL or a writable `char **`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_open_batch_reader_v2(
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
    options: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut PyBatchReader {
    // SAFETY: as for `raster_h3_py_create_arrow_stream_v2`; the returned box is freed by
    // `raster_h3_py_free_batch_reader`.
    unsafe {
        ffi_call("open_batch_reader_v2", out_err, || {
            let mut cfg = build_py_scan_config_v2(
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
                options,
            )?;
            if batch_size > 0 {
                cfg.batch_size = batch_size as usize;
            }
            let transform_failures = Arc::new(AtomicU64::new(0));
            cfg.strict_transforms = strict_transforms != 0;
            cfg.transform_failure_counter = Some(Arc::clone(&transform_failures));
            cfg.partition = parse_c_opt_partition(partition)?;
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

/// [`raster_h3_py_scan_json_ex`](crate::raster_h3_py_scan_json_ex) with the `options` argument of
/// [`raster_h3_py_create_arrow_stream_v2`].
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL;
/// `out_transform_failures` must be NULL or point to a writable `u64`; `out_err` must be
/// NULL or a writable `char **`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_scan_json_v2(
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
    options: *const c_char,
    out_transform_failures: *mut u64,
    out_err: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: as for `raster_h3_py_scan_json_ex`; the returned JSON string is freed with
    // `raster_h3_py_free_string`.
    unsafe {
        let failures = Arc::new(AtomicU64::new(0));
        let out = ffi_call("scan_json_v2", out_err, || {
            let mut cfg = build_py_scan_config_v2(
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
                options,
            )?;
            cfg.strict_transforms = strict_transforms != 0;
            cfg.transform_failure_counter = Some(Arc::clone(&failures));
            let json = PyRasterScanner::new(cfg).to_columnar_json()?;
            Ok(CString::new(json).unwrap_or_default().into_raw())
        });
        if !out_transform_failures.is_null() {
            *out_transform_failures = failures.load(Ordering::Relaxed);
        }
        out.unwrap_or(ptr::null_mut())
    }
}

/// [`raster_h3_py_plan_partitions`](crate::raster_h3_py_plan_partitions) with an `options` JSON object (NULL for none). Only
/// `overlap_rule` affects the plan, but the object must match the scans' so the partition
/// fingerprints agree; the other options are validated and otherwise ignored.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL; `out_err` must be
/// NULL or a writable `char **`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_plan_partitions_v2(
    path: *const c_char,
    crs: *const c_char,
    bbox: *const c_char,
    target_chunks: i64,
    options: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: the C string arguments are null or NUL-terminated and `out_err` is null or a
    // writable `char **` (caller contract), as `build_py_scan_config` and `ffi_call` require.
    unsafe {
        ffi_call("plan_partitions", out_err, || {
            let mut cfg = build_py_scan_config(
                path,
                ptr::null(),
                0,
                ptr::null(),
                crs,
                0,
                0.0,
                bbox,
                ptr::null(),
                -1,
            )?;
            // Category- or value-specific options are checked against the scan, not here.
            if let Some(json) = opt_cstr(options) {
                let opts: PyScanOptions = serde_json::from_str(&json).map_err(|e| {
                    RasterH3Error::InvalidParameter(format!("Invalid scan options: {e}"))
                })?;
                if let Some(name) = &opts.overlap_rule {
                    cfg.overlap_rule = parse_overlap_rule_strict(name)?;
                }
            }
            let target = usize::try_from(target_chunks)
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| {
                    RasterH3Error::InvalidParameter("target_chunks must be at least 1".into())
                })?;
            // Open the mosaic exactly as the scans do, so the fingerprints match.
            let paths = resolve_raster_sources(&cfg.path)?;
            let mosaic =
                MosaicReader::open(&paths, cfg.bbox, cfg.crs.as_deref(), cfg.overlap_rule)?;
            let plan = plan_partitions(&mosaic, target)?;
            let json = serde_json::to_string(&plan)
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
            Ok(CString::new(json).unwrap_or_default().into_raw())
        })
        .unwrap_or(ptr::null_mut())
    }
}

/// [`raster_h3_py_create_arrow_schema`](crate::raster_h3_py_create_arrow_schema) honouring the `options` JSON object (quantile
/// columns change the schema); `partitioned` non-zero adds the `partial_state` column.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL; `out_err` must be
/// NULL or a writable `char **`. Free the result with `raster_h3_py_free_arrow_schema`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_create_arrow_schema_v2(
    categorical: c_int,
    format: *const c_char,
    columns: *const c_char,
    partitioned: c_int,
    options: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut ArrowSchema {
    // SAFETY: the C string arguments are null or NUL-terminated and `out_err` is null or a
    // writable `char **` (caller contract).
    unsafe {
        ffi_call("create_arrow_schema_v2", out_err, || {
            let mut cfg = build_py_scan_config_v2(
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
                options,
            )?;
            if partitioned != 0 {
                // The schema does not depend on which partition is scanned.
                cfg.partition = Some(placeholder_partition());
            }
            Ok(Box::into_raw(Box::new(ArrowSchema::from_column_schema(
                &*PyRasterScanner::new(cfg).schema()?,
            ))))
        })
        .unwrap_or(ptr::null_mut())
    }
}

/// The output column names of a `quantiles` specification, as a JSON array of strings, in
/// output order. Free the string with `raster_h3_py_free_string`.
///
/// # Safety
/// `spec` must be NULL or NUL-terminated; `out_err` must be NULL or a writable `char **`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_quantile_columns(
    spec: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: `spec` is null or NUL-terminated and `out_err` null or writable (caller contract).
    unsafe {
        ffi_call("quantile_columns", out_err, || {
            let targets = match opt_cstr(spec) {
                Some(spec) => parse_quantiles(&spec)?,
                None => Vec::new(),
            };
            let names: Vec<&str> = targets.iter().map(QuantileTarget::column_name).collect();
            let json = serde_json::to_string(&names)
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
            Ok(CString::new(json).unwrap_or_default().into_raw())
        })
        .unwrap_or(ptr::null_mut())
    }
}
