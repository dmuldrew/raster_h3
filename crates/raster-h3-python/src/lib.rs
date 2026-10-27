//! # `raster-h3-python`
//!
//! Python, Polars, and Arrow PyCapsule (`__arrow_c_stream__` / `__arrow_c_schema__`) bindings
//! for [`raster_h3_core`].
//!
//! Exposes both:
//! 1. High-level Rust types ([`PyRasterScanner`]) and Arrow C Stream generators.
//! 2. C-ABI entry points (`raster_h3_py_*`) and `PyCapsule` destructors so Python (`ctypes` / `cffi`
//!    or C-API `PyCapsule_New`) can construct zero-copy `"arrow_array_stream"` and `"arrow_schema"`
//!    PyCapsules for Polars (`polars.from_arrow`, `register_io_source`), PyArrow (`pyarrow.table`),
//!    DuckDB, Daft, GeoPandas, and Lonboard.

use std::ffi::{c_char, c_int, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::columnar::{
    ArrowArrayStream, ArrowSchema, CategoricalBatchReader, CategoricalFormat, ColumnArray,
    ColumnSchema, ContinuousBatchReader, RecordBatch,
};
use raster_h3_core::error::{catch_unwind_or, safe_panic_payload_to_string, RasterH3Error, Result};
use raster_h3_core::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3_core::pmtiles::H3PmtilesTiler;
use raster_h3_core::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};
use raster_h3_core::scalar_ops::{
    h3_cell_to_parent_str, h3_cell_to_parent_u64, h3_get_resolution, h3_is_valid_str,
    h3_is_valid_u64, h3_polygon_to_cells_wkb, h3_to_lat, h3_to_lng, h3_to_string, h3_to_wkb_u64,
    parse_bbox_str, parse_containment_mode, string_to_h3, try_parse_resolutions_str,
    ContainmentMode, PolygonToCellsError,
};

/// Configuration for a Python / Polars H3 raster scan.
#[derive(Debug, Clone)]
pub struct PyScanConfig {
    pub path: String,
    pub resolutions: Vec<u8>,
    pub categorical: bool,
    pub format: CategoricalFormat,
    pub crs: Option<String>,
    pub nodata: Option<f64>,
    pub bbox: Option<[f64; 4]>,
    pub columns: Option<Vec<String>>,
    pub batch_size: usize,
    pub limit: Option<usize>,
}

impl Default for PyScanConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            resolutions: vec![8],
            categorical: false,
            format: CategoricalFormat::Wide,
            crs: None,
            nodata: None,
            bbox: None,
            columns: None,
            batch_size: 2048,
            limit: None,
        }
    }
}

/// Boxed streaming iterator of [`RecordBatch`]es.
pub type BatchIter = Box<dyn Iterator<Item = Result<RecordBatch>> + Send>;

/// Pull-based batch reader handed to Python, which wraps each batch as its own
/// single-batch Arrow stream so Polars can consume results incrementally.
pub struct PyBatchReader {
    schema: Arc<ColumnSchema>,
    iter: BatchIter,
}

/// Scanner backing the Python `RasterH3Scanner` object (`__arrow_c_schema__` and `__arrow_c_stream__`).
#[derive(Debug, Clone)]
pub struct PyRasterScanner {
    pub config: PyScanConfig,
}

impl PyRasterScanner {
    pub fn new(config: PyScanConfig) -> Self {
        Self { config }
    }

    fn base_schema(&self) -> ColumnSchema {
        if !self.config.categorical {
            ColumnSchema::continuous(&[])
        } else {
            match self.config.format {
                CategoricalFormat::Wide => ColumnSchema::categorical_wide(),
                CategoricalFormat::Long => ColumnSchema::categorical_long(),
            }
        }
    }

    fn resolve_projection(&self) -> Result<Option<Vec<usize>>> {
        let Some(cols) = &self.config.columns else {
            return Ok(None);
        };
        if cols.is_empty() {
            return Ok(None);
        }
        let base = self.base_schema();
        let mut indices = Vec::with_capacity(cols.len());
        for name in cols {
            let idx = base.index_of(name).ok_or_else(|| {
                RasterH3Error::InvalidParameter(format!(
                    "Column '{}' not found in raster_h3 schema",
                    name
                ))
            })?;
            indices.push(idx);
        }
        Ok(Some(indices))
    }

    /// Return the output [`ColumnSchema`] (respecting `columns` projection).
    pub fn schema(&self) -> Result<Arc<ColumnSchema>> {
        let base = self.base_schema();
        match self.resolve_projection()? {
            Some(indices) => Ok(Arc::new(base.project(&indices)?)),
            None => Ok(Arc::new(base)),
        }
    }

    /// Export an [`ArrowSchema`] struct for `__arrow_c_schema__`.
    pub fn to_arrow_c_schema(&self) -> Result<ArrowSchema> {
        let schema = self.schema()?;
        Ok(ArrowSchema::from_column_schema(&schema))
    }

    /// Open the streaming [`RecordBatch`] iterator for this scan.
    pub fn batch_iter(&self) -> Result<(Arc<ColumnSchema>, BatchIter)> {
        let paths = resolve_raster_sources(&self.config.path)?;
        let res = if self.config.resolutions.is_empty() {
            vec![8]
        } else {
            self.config.resolutions.clone()
        };
        let mut mr_cfg = MultiResolutionConfig::new(res);
        mr_cfg.custom_crs = self.config.crs.clone();
        mr_cfg.custom_nodata = self.config.nodata;
        mr_cfg.bbox = self.config.bbox;
        mr_cfg.overlap_rule = OverlapRule::Cutline;

        let projection = self.resolve_projection()?;
        if !self.config.categorical {
            let reader = ContinuousBatchReader::open(
                &paths,
                &mr_cfg,
                projection,
                self.config.batch_size,
                self.config.limit,
            )?;
            Ok((reader.schema(), Box::new(reader)))
        } else {
            let reader = CategoricalBatchReader::open(
                &paths,
                &mr_cfg,
                self.config.format,
                projection,
                self.config.batch_size,
                self.config.limit,
            )?;
            Ok((reader.schema(), Box::new(reader)))
        }
    }

    /// Export an [`ArrowArrayStream`] struct for `__arrow_c_stream__`.
    pub fn to_arrow_c_stream(&self) -> Result<ArrowArrayStream> {
        let (schema, iter) = self.batch_iter()?;
        Ok(ArrowArrayStream::from_iterator(schema, iter))
    }

    /// Collect all batches into a columnar JSON object `{"col_name": [v1, v2, ...], ...}`.
    pub fn to_columnar_json(&self) -> Result<String> {
        let mut stream = self.to_arrow_c_stream()?;
        // SAFETY: `stream` was just built by `ArrowArrayStream::from_iterator`, so it is a
        // valid, unreleased stream owned by this frame.
        let (schema, batches) = unsafe { stream.collect_batches()? };
        batches_to_columnar_json(&schema, &batches)
    }
}

fn batches_to_columnar_json(schema: &ColumnSchema, batches: &[RecordBatch]) -> Result<String> {
    let total_rows: usize = batches.iter().map(|b| b.num_rows).sum();
    let mut map = serde_json::Map::with_capacity(schema.fields.len());

    for (col_idx, field) in schema.fields.iter().enumerate() {
        let mut arr = Vec::with_capacity(total_rows);
        for batch in batches {
            let col = &batch.columns[col_idx];
            let validity = col.validity();
            for row in 0..batch.num_rows {
                if let Some(v) = validity {
                    if !v[row] {
                        arr.push(serde_json::Value::Null);
                        continue;
                    }
                }
                let val = match col {
                    ColumnArray::UInt64 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Int64 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::UInt8 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Float64 { values, .. } => {
                        let f = values[row];
                        if f.is_finite() {
                            serde_json::json!(f)
                        } else {
                            serde_json::Value::Null
                        }
                    }
                    ColumnArray::Boolean { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Utf8 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Binary { values, .. } => {
                        let mut hex = String::with_capacity(values[row].len() * 2);
                        for &b in &values[row] {
                            use std::fmt::Write as _;
                            let _ = write!(&mut hex, "{:02x}", b);
                        }
                        serde_json::Value::String(hex)
                    }
                };
                arr.push(val);
            }
        }
        map.insert(field.name.clone(), serde_json::Value::Array(arr));
    }

    serde_json::to_string(&serde_json::Value::Object(map))
        .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
}

// =============================================================================
// C-ABI & PyCapsule Entry Points for Python (`ctypes` / `PyCapsule_New`)
// =============================================================================

/// # Safety
/// `ptr` must be null or a valid NUL-terminated C string.
unsafe fn opt_cstr(ptr: *const c_char) -> Option<String> {
    // SAFETY: `ptr` is non-null and NUL-terminated (caller contract); the bytes are copied
    // into an owned `String` before returning.
    unsafe {
        if ptr.is_null() {
            return None;
        }
        let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// # Safety
/// `out_err` must be null or a writable `char **`.
unsafe fn set_c_err(out_err: *mut *mut c_char, msg: &str) {
    // SAFETY: non-null `out_err` is a writable `char **` (caller contract). Ownership of
    // the `CString::into_raw` string passes to the caller, who frees it with
    // `raster_h3_py_free_string`.
    unsafe {
        if !out_err.is_null() {
            let c = CString::new(msg).unwrap_or_default();
            *out_err = c.into_raw();
        }
    }
}

/// H3 resolutions from a C string; NULL or empty means the default `[8]`.
///
/// # Safety
/// `ptr` must be null or a valid NUL-terminated string.
unsafe fn parse_c_resolutions(ptr: *const c_char) -> Result<Vec<u8>> {
    // SAFETY: `ptr` is null or NUL-terminated (caller contract), as `opt_cstr` requires.
    match unsafe { opt_cstr(ptr) } {
        None => Ok(vec![8]),
        Some(s) => try_parse_resolutions_str(&s).ok_or_else(|| {
            RasterH3Error::InvalidParameter(format!(
                "Invalid resolutions '{s}': expected H3 resolutions between 0 and 15, e.g. 7,8"
            ))
        }),
    }
}

/// Categorical output format from a C string; NULL or empty means `wide`.
///
/// # Safety
/// `ptr` must be null or a valid NUL-terminated string.
unsafe fn parse_c_format(ptr: *const c_char) -> Result<CategoricalFormat> {
    // SAFETY: `ptr` is null or NUL-terminated (caller contract), as `opt_cstr` requires.
    match unsafe { opt_cstr(ptr) } {
        None => Ok(CategoricalFormat::Wide),
        Some(s) if s.eq_ignore_ascii_case("wide") => Ok(CategoricalFormat::Wide),
        Some(s) if s.eq_ignore_ascii_case("long") => Ok(CategoricalFormat::Long),
        Some(s) => Err(RasterH3Error::InvalidParameter(format!(
            "Invalid format '{s}': expected 'wide' or 'long'"
        ))),
    }
}

/// Like [`parse_bbox_str`], but also rejects non-numeric entries rather than skipping them.
fn parse_bbox_strict(s: &str) -> Option<[f64; 4]> {
    let all_numeric = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .all(|item| item.parse::<f64>().is_ok());
    if all_numeric {
        parse_bbox_str(s)
    } else {
        None
    }
}

/// # Safety
/// Every C string argument must be null or a valid NUL-terminated string.
#[allow(clippy::too_many_arguments)]
unsafe fn build_py_scan_config(
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
) -> Result<PyScanConfig> {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires.
    unsafe {
        let path_str = opt_cstr(path)
            .ok_or_else(|| RasterH3Error::InvalidParameter("Missing input raster path".into()))?;
        let res_vec = parse_c_resolutions(resolutions)?;
        let fmt = parse_c_format(format)?;
        let crs_opt = opt_cstr(crs);
        let nodata_opt = if has_nodata != 0 { Some(nodata) } else { None };
        let bbox_opt = opt_cstr(bbox)
            .map(|s| {
                parse_bbox_strict(&s).ok_or_else(|| {
                    RasterH3Error::InvalidParameter(format!(
                        "Invalid bbox '{s}': expected four numbers min_lon,min_lat,max_lon,max_lat"
                    ))
                })
            })
            .transpose()?;
        let cols_opt = opt_cstr(columns).map(|s| {
            s.split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
        });
        let limit_opt = if limit >= 0 {
            Some(limit as usize)
        } else {
            None
        };

        Ok(PyScanConfig {
            path: path_str,
            resolutions: res_vec,
            categorical: categorical != 0,
            format: fmt,
            crs: crs_opt,
            nodata: nodata_opt,
            bbox: bbox_opt,
            columns: cols_opt,
            batch_size: 2048,
            limit: limit_opt,
        })
    }
}

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
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_err` is null or a writable `char **` (as `set_c_err`
    // requires), and error strings are freed by the caller with `raster_h3_py_free_string`.
    // The returned box is freed by `raster_h3_py_free_arrow_stream`.
    unsafe {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let cfg = build_py_scan_config(
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
                let (schema, iter) = PyRasterScanner::new(cfg).batch_iter()?;
                Ok(PyBatchReader { schema, iter })
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
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires; `out_err` is null or a writable `char **` (as `set_c_err`
    // requires), and error strings are freed by the caller with `raster_h3_py_free_string`.
    // The returned JSON string is freed with `raster_h3_py_free_string`.
    unsafe {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let cfg = build_py_scan_config(
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
            PyRasterScanner::new(cfg).to_columnar_json()
        }));

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

/// Free a C string returned by `raster_h3_py_*`.
///
/// # Safety
/// `ptr` must be null or a pointer returned by `CString::into_raw` from this crate.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_free_string(ptr: *mut c_char) {
    // SAFETY: every string this crate returns comes from `CString::into_raw`; non-null
    // `ptr` is one of them, freed once (caller contract).
    unsafe {
        if !ptr.is_null() {
            drop(CString::from_raw(ptr));
        }
    }
}

/// C-ABI scalar helper: `h3_to_string(cell) -> *mut c_char` (NULL if invalid/0).
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_to_string(cell: u64) -> *mut c_char {
    catch_unwind_or(ptr::null_mut(), || match h3_to_string(cell) {
        Some(s) => CString::new(s).unwrap_or_default().into_raw(),
        None => ptr::null_mut(),
    })
}

/// C-ABI scalar helper: `string_to_h3(hex) -> u64`.
///
/// # Safety
/// `hex` must be a valid NUL-terminated string or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_string_to_h3(hex: *const c_char) -> u64 {
    // SAFETY: `hex` is null or NUL-terminated (caller contract), as `opt_cstr` requires.
    unsafe {
        catch_unwind_or(0, || match opt_cstr(hex) {
            Some(s) => string_to_h3(&s),
            None => 0,
        })
    }
}

/// C-ABI scalar helper: `h3_to_lat(cell) -> f64`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_to_lat(cell: u64) -> f64 {
    catch_unwind_or(f64::NAN, || h3_to_lat(cell))
}

/// C-ABI scalar helper: `h3_to_lng(cell) -> f64`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_to_lng(cell: u64) -> f64 {
    catch_unwind_or(f64::NAN, || h3_to_lng(cell))
}

/// C-ABI scalar helper: `h3_get_resolution(cell) -> i64`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_get_resolution(cell: u64) -> i64 {
    catch_unwind_or(-1, || h3_get_resolution(cell))
}

/// C-ABI scalar helper: `h3_is_valid_u64(cell) -> i32`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_is_valid_u64(cell: u64) -> c_int {
    catch_unwind_or(0, || c_int::from(h3_is_valid_u64(cell)))
}

/// C-ABI scalar helper: `h3_is_valid_str(hex) -> i32`.
///
/// # Safety
/// `hex` must be a valid NUL-terminated string or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_is_valid_str(hex: *const c_char) -> c_int {
    // SAFETY: `hex` is null or NUL-terminated (caller contract), as `opt_cstr` requires.
    unsafe {
        catch_unwind_or(0, || match opt_cstr(hex) {
            Some(s) => c_int::from(h3_is_valid_str(&s)),
            None => 0,
        })
    }
}

/// C-ABI scalar helper: `h3_cell_to_parent_u64(cell, parent_res) -> u64` (0 if invalid).
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_cell_to_parent_u64(cell: u64, parent_res: i64) -> u64 {
    catch_unwind_or(0, || h3_cell_to_parent_u64(cell, parent_res).unwrap_or(0))
}

/// C-ABI scalar helper: `h3_cell_to_parent_str(hex, parent_res) -> *mut c_char`.
///
/// # Safety
/// `hex` must be a valid NUL-terminated string or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_cell_to_parent_str(
    hex: *const c_char,
    parent_res: i64,
) -> *mut c_char {
    // SAFETY: `hex` is null or NUL-terminated (caller contract), as `opt_cstr` requires;
    // the returned string is freed with `raster_h3_py_free_string`.
    unsafe {
        catch_unwind_or(ptr::null_mut(), || {
            let Some(s) = opt_cstr(hex) else {
                return ptr::null_mut();
            };
            match h3_cell_to_parent_str(&s, parent_res) {
                Some(out) => CString::new(out).unwrap_or_default().into_raw(),
                None => ptr::null_mut(),
            }
        })
    }
}

/// C-ABI helper: write WKB polygon for `cell` into `out_buf` (capacity of
/// `raster_h3_core::encoding::WKB_BUF_LEN` bytes always suffices).
/// Returns number of bytes written, or `0` if `cell` is invalid or `buf_cap` is too small.
///
/// # Safety
/// `out_buf` must point to at least `buf_cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_to_wkb(
    cell: u64,
    out_buf: *mut u8,
    buf_cap: usize,
) -> usize {
    // SAFETY: non-null `out_buf` has `buf_cap` writable bytes (caller contract) and
    // `wkb.len() <= buf_cap` was checked, so the copy stays in bounds; `wkb` is a separate
    // Rust allocation, so the regions cannot overlap.
    unsafe {
        if out_buf.is_null() {
            return 0;
        }
        let Some(wkb) = catch_unwind_or(None, || h3_to_wkb_u64(cell)) else {
            return 0;
        };
        if wkb.len() > buf_cap {
            return 0;
        }
        ptr::copy_nonoverlapping(wkb.as_ptr(), out_buf, wkb.len());
        wkb.len()
    }
}

/// C-ABI scalar helper: `h3_polygon_to_cells(wkb, res, containment)`.
///
/// On success (`0`) writes a heap array of `*out_len` cells to `*out_cells`; release it with
/// [`raster_h3_py_free_cells`]. A NULL `containment` means `contains_centroid`. Other return
/// codes leave the outputs untouched: `1` invalid geometry or resolution (or an internal
/// panic), `2` unknown containment mode, `3` more cells than `H3_POLYGON_TO_CELLS_MAX_CELLS`,
/// `-1` NULL pointers.
///
/// # Safety
/// `wkb` must point to `wkb_len` readable bytes, `containment` must be NULL or a valid
/// NUL-terminated string, and `out_cells` / `out_len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_polygon_to_cells(
    wkb: *const u8,
    wkb_len: usize,
    res: i64,
    containment: *const c_char,
    out_cells: *mut *mut u64,
    out_len: *mut usize,
) -> c_int {
    // SAFETY: `containment` is null or NUL-terminated, `wkb` addresses `wkb_len` readable
    // bytes that stay unchanged during the call, and the out-pointers are non-null
    // (checked) and writable (caller contract). The boxed slice handed out is freed by
    // `raster_h3_py_free_cells` with the same length.
    unsafe {
        if wkb.is_null() || out_cells.is_null() || out_len.is_null() {
            return -1;
        }
        let mode = match opt_cstr(containment) {
            None => ContainmentMode::ContainsCentroid,
            Some(name) => match parse_containment_mode(&name) {
                Some(mode) => mode,
                None => return 2,
            },
        };
        let bytes = std::slice::from_raw_parts(wkb, wkb_len);
        let Some(result) =
            catch_unwind_or(None, || Some(h3_polygon_to_cells_wkb(bytes, res, mode)))
        else {
            return 1;
        };
        match result {
            Ok(cells) => {
                let cells = cells.into_boxed_slice();
                *out_len = cells.len();
                *out_cells = Box::into_raw(cells) as *mut u64;
                0
            }
            Err(PolygonToCellsError::TooManyCells { .. }) => 3,
            Err(_) => 1,
        }
    }
}

/// Free a cell array returned by [`raster_h3_py_h3_polygon_to_cells`].
///
/// # Safety
/// `cells` / `len` must come from one successful `raster_h3_py_h3_polygon_to_cells` call
/// and not have been freed already.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_free_cells(cells: *mut u64, len: usize) {
    // SAFETY: non-null `cells`/`len` came from one `Box<[u64]>` leaked by
    // `raster_h3_py_h3_polygon_to_cells` (caller contract), so rebuilding the fat pointer
    // with the same length reproduces that box, which is freed once.
    unsafe {
        if !cells.is_null() {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(cells, len)));
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::NamedTempFile;
    use tiff::encoder::colortype::Gray32Float;
    use tiff::encoder::TiffEncoder;

    fn write_test_tif(width: u32, height: u32, val: f32) -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let file = File::create(tmp.path()).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut img = encoder.new_image::<Gray32Float>(width, height).unwrap();
        img.encoder()
            .write_tag(
                tiff::tags::Tag::Unknown(33550),
                &[0.001f64, 0.001f64, 0.0f64][..],
            )
            .unwrap();
        img.encoder()
            .write_tag(
                tiff::tags::Tag::Unknown(33922),
                &[0.0f64, 0.0f64, 0.0f64, -122.45f64, 37.80f64, 0.0f64][..],
            )
            .unwrap();
        let data = vec![val; (width * height) as usize];
        img.write_data(&data).unwrap();
        tmp
    }

    #[test]
    fn test_py_scan_config_rejects_malformed_bbox() {
        let path_c = CString::new("/tmp/does_not_matter.tif").unwrap();
        let bad_bbox = CString::new("1,garbage,2,3,4").unwrap();
        let good_bbox = CString::new("-122.5,37.5,-122.0,38.0").unwrap();
        // SAFETY: `path_c` and the bbox strings are live CStrings (or null) for every call.
        unsafe {
            let build = |bbox: *const c_char| {
                build_py_scan_config(
                    path_c.as_ptr(),
                    ptr::null(),
                    0,
                    ptr::null(),
                    ptr::null(),
                    0,
                    0.0,
                    bbox,
                    ptr::null(),
                    -1,
                )
            };
            assert!(build(bad_bbox.as_ptr()).is_err());
            assert_eq!(
                build(good_bbox.as_ptr()).unwrap().bbox,
                Some([-122.5, 37.5, -122.0, 38.0])
            );
            assert_eq!(build(ptr::null()).unwrap().bbox, None);
        }
    }

    #[test]
    fn test_py_scanner_arrow_c_stream_and_c_abi() {
        let tif = write_test_tif(16, 16, 25.5);
        let path_c = CString::new(tif.path().to_string_lossy().as_bytes()).unwrap();
        let res_c = CString::new("8").unwrap();
        let crs_c = CString::new("EPSG:4326").unwrap();
        let cols_c = CString::new("h3_index,h3_hex,mean").unwrap();

        // SAFETY: string arguments are live `CString`s owned by this frame (or null), and
        // the returned stream is a fresh box that is drained and then freed exactly once.
        unsafe {
            let mut err: *mut c_char = ptr::null_mut();
            let stream_ptr = raster_h3_py_create_arrow_stream(
                path_c.as_ptr(),
                res_c.as_ptr(),
                0,
                ptr::null(),
                crs_c.as_ptr(),
                0,
                0.0,
                ptr::null(),
                cols_c.as_ptr(),
                5,
                &mut err,
            );
            assert!(err.is_null());
            assert!(!stream_ptr.is_null());

            let (schema, batches) = (*stream_ptr).collect_batches().unwrap();
            assert_eq!(schema.fields.len(), 3);
            let rows: usize = batches.iter().map(|b| b.num_rows).sum();
            assert!(rows > 0 && rows <= 5);
            raster_h3_py_free_arrow_stream(stream_ptr);

            let json_ptr = raster_h3_py_scan_json(
                path_c.as_ptr(),
                res_c.as_ptr(),
                0,
                ptr::null(),
                crs_c.as_ptr(),
                0,
                0.0,
                ptr::null(),
                cols_c.as_ptr(),
                3,
                &mut err,
            );
            assert!(!json_ptr.is_null());
            let json_s = CStr::from_ptr(json_ptr).to_string_lossy().into_owned();
            assert!(json_s.contains("\"h3_index\""));
            assert!(json_s.contains("25.5"));
            raster_h3_py_free_string(json_ptr);
        }
    }

    #[test]
    fn test_py_h3_to_wkb_fits_odd_res_pentagon() {
        // Res-1 pentagon: 10 boundary vertices -> 13 + 16 * 11 = 189 bytes of WKB.
        let cell = 0x0810_83ff_ffff_ffffu64;
        let n = h3o::CellIndex::try_from(cell).unwrap().boundary().len();
        assert!(n > 6, "expected >6 boundary vertices, got {n}");
        let mut buf = [0u8; raster_h3_core::encoding::WKB_BUF_LEN];
        // SAFETY: `buf` provides `buf.len()` writable bytes.
        let written = unsafe { raster_h3_py_h3_to_wkb(cell, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(written, 13 + 16 * (n + 1));
        assert!(written > 128);
        let mut small = [0u8; 128];
        assert_eq!(
            // SAFETY: `small` provides `small.len()` writable bytes.
            unsafe { raster_h3_py_h3_to_wkb(cell, small.as_mut_ptr(), small.len()) },
            0
        );
    }
}
