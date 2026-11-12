//! Parquet and PMTiles export entry points.

use std::ffi::{c_char, c_int};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::error::{safe_panic_payload_to_string, RasterH3Error, Result};
use raster_h3_core::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3_core::pmtiles::H3PmtilesTiler;
use raster_h3_core::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};

use crate::ffi::args::parse_c_resolutions;
use crate::ffi::{ffi_call, opt_cstr, set_c_err};

/// C-ABI direct Parquet exporter (`h3_raster_to_parquet`). Returns number of hexagons written, or `-1` on error.
///
/// Kept with its original signature for callers built against earlier releases; see
/// [`raster_h3_py_export_parquet_with_stats`] for the transform-failure count.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_export_parquet(
    input_path: *const c_char,
    output_path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    crs: *const c_char,
    geoparquet: c_int,
    out_err: *mut *mut c_char,
) -> i64 {
    // SAFETY: forwards the caller's arguments unchanged (same contract), with a null
    // `out_transform_failures`, which the callee accepts.
    unsafe {
        raster_h3_py_export_parquet_with_stats(
            input_path,
            output_path,
            resolutions,
            categorical,
            crs,
            geoparquet,
            ptr::null_mut(),
            out_err,
        )
    }
}

/// [`raster_h3_py_export_parquet`] that also writes the number of valid sample points
/// dropped by failed coordinate transforms to `out_transform_failures` (when non-null).
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL;
/// `out_transform_failures` must be NULL or point to a writable `u64`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_export_parquet_with_stats(
    input_path: *const c_char,
    output_path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    crs: *const c_char,
    geoparquet: c_int,
    out_transform_failures: *mut u64,
    out_err: *mut *mut c_char,
) -> i64 {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_transform_failures` is null or a writable `u64`; `out_err`
    // is null or a writable `char **` (as `set_c_err` requires), and error strings are
    // freed by the caller with `raster_h3_py_free_string`.
    unsafe {
        let failures = Arc::new(AtomicU64::new(0));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<usize> {
            let in_str = opt_cstr(input_path)
                .ok_or_else(|| RasterH3Error::InvalidParameter("Missing input_path".into()))?;
            let out_str = opt_cstr(output_path)
                .ok_or_else(|| RasterH3Error::InvalidParameter("Missing output_path".into()))?;
            let res_vec = parse_c_resolutions(resolutions)?;
            let crs_opt = opt_cstr(crs);
            let paths = resolve_raster_sources(&in_str)?;
            let mut config = MultiResolutionConfig::new(res_vec);
            config.custom_crs = crs_opt.clone();
            config.transform_failure_counter = Some(Arc::clone(&failures));

            let mosaic = Arc::new(MosaicReader::open(
                &paths,
                None,
                crs_opt.as_deref(),
                OverlapRule::Cutline,
            )?);
            let pq_cfg = ParquetExportConfig {
                geoparquet: geoparquet != 0,
                is_categorical: categorical != 0,
                ..Default::default()
            };

            if categorical == 0 {
                let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?;
                H3ParquetWriter::write_continuous_streamer_to_parquet(
                    streamer,
                    Path::new(&out_str),
                    pq_cfg,
                )
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
            } else {
                let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)?;
                H3ParquetWriter::write_categorical_streamer_to_parquet(
                    streamer,
                    Path::new(&out_str),
                    pq_cfg,
                )
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
            }
        }));

        if !out_transform_failures.is_null() {
            *out_transform_failures = failures.load(Ordering::Relaxed);
        }
        match res {
            Ok(Ok(count)) => count as i64,
            Ok(Err(e)) => {
                set_c_err(out_err, &e.to_string());
                -1
            }
            Err(payload) => {
                let msg = safe_panic_payload_to_string(payload);
                set_c_err(out_err, &format!("Panic in export_parquet: {msg}"));
                -1
            }
        }
    }
}

/// C-ABI direct PMTiles v3 exporter (`h3_raster_to_pmtiles`). Returns number of hexagons written, or `-1` on error.
///
/// Kept with its original signature for callers built against earlier releases; see
/// [`raster_h3_py_export_pmtiles_with_stats`] for the transform-failure count.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_export_pmtiles(
    input_path: *const c_char,
    output_path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    crs: *const c_char,
    out_err: *mut *mut c_char,
) -> i64 {
    // SAFETY: forwards the caller's arguments unchanged (same contract), with a null
    // `out_transform_failures`, which the callee accepts.
    unsafe {
        raster_h3_py_export_pmtiles_with_stats(
            input_path,
            output_path,
            resolutions,
            categorical,
            crs,
            ptr::null_mut(),
            out_err,
        )
    }
}

/// [`raster_h3_py_export_pmtiles`] that also writes the number of valid sample points
/// dropped by failed coordinate transforms to `out_transform_failures` (when non-null).
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL;
/// `out_transform_failures` must be NULL or point to a writable `u64`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_export_pmtiles_with_stats(
    input_path: *const c_char,
    output_path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    crs: *const c_char,
    out_transform_failures: *mut u64,
    out_err: *mut *mut c_char,
) -> i64 {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_transform_failures` is null or a writable `u64`; `out_err`
    // is null or a writable `char **` (as `set_c_err` requires), and error strings are
    // freed by the caller with `raster_h3_py_free_string`.
    unsafe {
        let failures = Arc::new(AtomicU64::new(0));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<usize> {
            let in_str = opt_cstr(input_path)
                .ok_or_else(|| RasterH3Error::InvalidParameter("Missing input_path".into()))?;
            let out_str = opt_cstr(output_path)
                .ok_or_else(|| RasterH3Error::InvalidParameter("Missing output_path".into()))?;
            let res_vec = parse_c_resolutions(resolutions)?;
            let mut config = MultiResolutionConfig::new(res_vec);
            config.custom_crs = opt_cstr(crs);
            config.transform_failure_counter = Some(Arc::clone(&failures));

            if categorical == 0 {
                H3PmtilesTiler::process_raster_source_to_pmtiles(
                    &in_str,
                    Path::new(&out_str),
                    config,
                )
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
            } else {
                H3PmtilesTiler::process_categorical_source_to_pmtiles(
                    &in_str,
                    Path::new(&out_str),
                    config,
                )
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
            }
        }));

        if !out_transform_failures.is_null() {
            *out_transform_failures = failures.load(Ordering::Relaxed);
        }
        match res {
            Ok(Ok(count)) => count as i64,
            Ok(Err(e)) => {
                set_c_err(out_err, &e.to_string());
                -1
            }
            Err(payload) => {
                let msg = safe_panic_payload_to_string(payload);
                set_c_err(out_err, &format!("Panic in export_pmtiles: {msg}"));
                -1
            }
        }
    }
}

/// Transcode an H3-indexed Parquet file into a PMTiles v3 archive (DuckDB's
/// `h3_parquet_to_pmtiles`). `h3_column` names the H3 column (integer or hex string,
/// matched case-insensitively); NULL auto-detects `h3_index`, `h3_hex`, `h3`, `cell` or
/// `hex`. Returns the number of hexagon features written, or `-1` with `out_err` set.
///
/// # Safety
/// Pointer arguments must be valid NUL-terminated C strings or NULL; `out_err` must be
/// NULL or a writable `char **`.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_parquet_to_pmtiles(
    parquet_path: *const c_char,
    pmtiles_path: *const c_char,
    h3_column: *const c_char,
    out_err: *mut *mut c_char,
) -> i64 {
    // SAFETY: the C string arguments are null or NUL-terminated and `out_err` is null or a
    // writable `char **` (caller contract), as `opt_cstr` and `ffi_call` require.
    unsafe {
        ffi_call("parquet_to_pmtiles", out_err, || {
            let in_str = opt_cstr(parquet_path)
                .ok_or_else(|| RasterH3Error::InvalidParameter("Missing parquet_path".into()))?;
            let out_str = opt_cstr(pmtiles_path)
                .ok_or_else(|| RasterH3Error::InvalidParameter("Missing pmtiles_path".into()))?;
            let column = opt_cstr(h3_column);
            let summary = H3PmtilesTiler::process_parquet_to_pmtiles(
                Path::new(&in_str),
                Path::new(&out_str),
                column.as_deref(),
            )
            .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
            Ok(summary.valid_features as i64)
        })
        .unwrap_or(-1)
    }
}
