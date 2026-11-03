//! `COPY ... TO ... (FORMAT pmtiles)` support via the v1.5.6 stable C API
//! `duckdb_copy_function` family.
//!
//! Contract for the source query:
//! - column 1 must be the H3 cell index as `UBIGINT`/`BIGINT` or a
//!   hex `VARCHAR`;
//! - every remaining column becomes an MVT property named `column_<i>`
//!   (the stable C API does not expose input column names);
//! - supported options are `MIN_ZOOM`/`MAX_ZOOM`, which filter cells by the
//!   PMTiles tile zoom derived from their H3 resolution.
//!
//! The archive is assembled by `H3PmtilesTiler::export_h3_features` into a
//! temporary file, then streamed to the destination through DuckDB's file
//! system (so `s3://`/`file://` and `CREATE SECRET` credentials apply), falling
//! back to a local copy when the file layer is unavailable.

use std::ffi::c_void;
use std::sync::Mutex;

use raster_h3_core::pmtiles::features::H3Feature;
use raster_h3_core::pmtiles::mvt::MvtValue;
use raster_h3_core::pmtiles::tiler::H3PmtilesTiler;

use crate::ffi::*;
use crate::functions::bind_utils::delete_boxed;
use crate::functions::fast_hex::parse_hex_u64;

/// One sink column's logical type, resolved at bind time.
type CopyColumns = Vec<DuckDBType>;

struct PmtilesCopyBindData {
    column_types: CopyColumns,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
}

struct PmtilesCopyGlobal {
    file_path: String,
    writer: Option<Mutex<DuckDbFileWriter>>,
    features: Mutex<Vec<H3Feature>>,
    // Kept alive for the writer's lifetime: the file handle is only valid
    // while its file system is.
    _file_system: Option<DuckDbFileSystem>,
}

fn copy_bind_error(info: duckdb_copy_function_bind_info, msg: String) {
    let c_msg = to_c_string(&msg);
    // SAFETY: `info` is the live bind handle; DuckDB copies the message.
    unsafe { duckdb_copy_function_bind_set_error(info, c_msg.as_ptr()) };
}

fn copy_init_error(info: duckdb_copy_function_global_init_info, msg: String) {
    let c_msg = to_c_string(&msg);
    // SAFETY: `info` is the live global-init handle.
    unsafe { duckdb_copy_function_global_init_set_error(info, c_msg.as_ptr()) };
}

fn copy_sink_error(info: duckdb_copy_function_sink_info, msg: String) {
    let c_msg = to_c_string(&msg);
    // SAFETY: `info` is the live sink handle.
    unsafe { duckdb_copy_function_sink_set_error(info, c_msg.as_ptr()) };
}

fn copy_finalize_error(info: duckdb_copy_function_finalize_info, msg: String) {
    let c_msg = to_c_string(&msg);
    // SAFETY: `info` is the live finalize handle.
    unsafe { duckdb_copy_function_finalize_set_error(info, c_msg.as_ptr()) };
}

/// Parse the `(MIN_ZOOM 4, MAX_ZOOM 10)`-style option struct DuckDB hands the
/// bind callback.
///
/// # Safety
/// `info` must be the live copy-bind handle.
unsafe fn parse_copy_options(
    info: duckdb_copy_function_bind_info,
) -> Result<(Option<u8>, Option<u8>), String> {
    let mut min_zoom = None;
    let mut max_zoom = None;
    // SAFETY: `info` is the live bind handle; the returned options value is
    // owned by us and destroyed below.
    let mut options = unsafe { duckdb_copy_function_bind_get_options(info) };
    // SAFETY: `options` is either null or a valid DuckDB value.
    let options_is_null = unsafe { duckdb_is_null_value(options) };
    if options.is_null() || options_is_null {
        if !options.is_null() {
            // SAFETY: `options` is our owned value.
            unsafe { duckdb_destroy_value(&mut options) };
        }
        return Ok((min_zoom, max_zoom));
    }
    // The options value is a STRUCT {option_name: value}.
    let result = (|| {
        // SAFETY: `options` is a live owned value; the returned logical type
        // is borrowed from it (see note at the end of this function).
        let ty = unsafe { duckdb_get_value_type(options) };
        // SAFETY: `ty` is either null or a valid logical type.
        let ty_id = unsafe { duckdb_get_type_id(ty) };
        if ty.is_null() || ty_id != DuckDBType::Struct {
            return Err("expected STRUCT copy options".to_string());
        }
        // SAFETY: `ty` is a valid STRUCT logical type.
        let count = unsafe { duckdb_struct_type_child_count(ty) };
        for i in 0..count {
            // SAFETY: `ty` is a valid STRUCT logical type; the returned C
            // string is allocated by DuckDB and freed by `from_duckdb_string`.
            let name_ptr = unsafe { duckdb_struct_type_child_name(ty, i) };
            // SAFETY: `name_ptr` came from `duckdb_struct_type_child_name`.
            let name = unsafe { from_duckdb_string(name_ptr) };
            let name = match name {
                Some(n) => n.to_ascii_uppercase(),
                None => continue,
            };
            // SAFETY: `options` is a live STRUCT value; the returned child
            // value is owned by us and destroyed below.
            let mut child = unsafe { duckdb_get_struct_child(options, i) };
            let mut int_val = None;
            // SAFETY: `child` is either null or a valid DuckDB value.
            let child_is_null = unsafe { duckdb_is_null_value(child) };
            let child_was_null = child.is_null() || child_is_null;
            if !child_was_null {
                // SAFETY: `child` is a live owned value; the returned logical
                // type is borrowed from it.
                let c_ty = unsafe { duckdb_get_value_type(child) };
                // SAFETY: `c_ty` is either null or a valid logical type.
                let id = unsafe { duckdb_get_type_id(c_ty) };
                // SAFETY: `child` is a live DuckDB value; integer getters
                // coerce when the underlying type is integer-compatible.
                int_val = unsafe {
                    match id {
                        DuckDBType::BigInt
                        | DuckDBType::Integer
                        | DuckDBType::SmallInt
                        | DuckDBType::TinyInt
                        | DuckDBType::UTinyInt
                        | DuckDBType::USmallInt
                        | DuckDBType::UInteger
                        | DuckDBType::UBigInt => Some(duckdb_get_int64(child)),
                        _ => None,
                    }
                };
            }
            if !child.is_null() {
                // SAFETY: `child` is our owned value.
                unsafe { duckdb_destroy_value(&mut child) };
            }
            // A NULL option value means "unset"; any other value that is
            // not an integer in 0..=255 is a user error, not a silent default.
            match name.as_str() {
                "MIN_ZOOM" | "MAX_ZOOM" => {
                    if !child_was_null {
                        let zoom = int_val
                            .ok_or_else(|| {
                                format!("pmtiles copy option {name} expects an integer 0-255")
                            })
                            .and_then(|v| {
                                u8::try_from(v).map_err(|_| {
                                    format!(
                                        "pmtiles copy option {name} expects an integer 0-255, got {v}"
                                    )
                                })
                            })?;
                        if name == "MIN_ZOOM" {
                            min_zoom = Some(zoom);
                        } else {
                            max_zoom = Some(zoom);
                        }
                    }
                }
                _ => {
                    return Err(format!(
                        "unsupported pmtiles copy option {name}; supported: MIN_ZOOM, MAX_ZOOM"
                    ));
                }
            }
        }
        // NB: the logical types returned by `duckdb_get_value_type` and
        // `duckdb_copy_function_bind_get_column_type` are borrowed in this
        // API revision; destroying them corrupts DuckDB internals.
        Ok((min_zoom, max_zoom))
    })();
    // SAFETY: `options` is our owned value.
    unsafe { duckdb_destroy_value(&mut options) };
    if let Ok((Some(min), Some(max))) = result {
        if min > max {
            return Err("MIN_ZOOM must be <= MAX_ZOOM".to_string());
        }
    }
    result
}

unsafe extern "C" fn pmtiles_copy_bind(info: duckdb_copy_function_bind_info) {
    let run = || {
        // SAFETY: `info` is the live copy-bind handle (callback contract).
        unsafe {
            let column_count = duckdb_copy_function_bind_get_column_count(info) as usize;
            if column_count < 1 {
                copy_bind_error(
                    info,
                    "COPY (FORMAT pmtiles) needs at least one column: the H3 cell index"
                        .to_string(),
                );
                return;
            }
            let mut column_types = Vec::with_capacity(column_count);
            for i in 0..column_count as u64 {
                let lt = duckdb_copy_function_bind_get_column_type(info, i);
                if lt.is_null() {
                    column_types.push(DuckDBType::Invalid);
                    continue;
                }
                column_types.push(duckdb_get_type_id(lt));
                // `lt` is borrowed from the bind input; do not destroy it.
            }
            let (min_zoom, max_zoom) = match parse_copy_options(info) {
                Ok(v) => v,
                Err(e) => {
                    copy_bind_error(info, e);
                    return;
                }
            };
            let bind_data = Box::new(PmtilesCopyBindData {
                column_types,
                min_zoom,
                max_zoom,
            });
            duckdb_copy_function_bind_set_bind_data(
                info,
                Box::into_raw(bind_data) as *mut c_void,
                Some(delete_boxed::<PmtilesCopyBindData>),
            );
        }
    };
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        let msg = safe_panic_payload_to_string(payload);
        copy_bind_error(info, format!("Panic in pmtiles copy bind: {msg}"));
    }
}

unsafe extern "C" fn pmtiles_copy_global_init(info: duckdb_copy_function_global_init_info) {
    let run = || {
        // SAFETY: `info` is the live global-init handle.
        unsafe {
            let path_ptr = duckdb_copy_function_global_init_get_file_path(info);
            if path_ptr.is_null() {
                copy_init_error(
                    info,
                    "COPY (FORMAT pmtiles) requires a file path".to_string(),
                );
                return;
            }
            // SAFETY: NUL-terminated path owned by DuckDB for the call.
            let file_path = std::ffi::CStr::from_ptr(path_ptr)
                .to_string_lossy()
                .into_owned();

            // Open the destination through DuckDB's file layer so httpfs/S3
            // destinations and secrets apply. If DuckDB rejects the path we
            // surface that error instead of bypassing its file policy with a
            // plain local write; the std::fs fallback is only for the
            // (unexpected) case of no client context at all.
            let ctx = duckdb_copy_function_global_init_get_client_context(info);
            let mut fs_opt = None;
            if !ctx.is_null() {
                let mut ctx = ctx;
                let fs_handle = duckdb_client_context_get_file_system(ctx);
                duckdb_destroy_client_context(&mut ctx);
                if !fs_handle.is_null() {
                    fs_opt = Some(DuckDbFileSystem::from_raw(fs_handle));
                }
            }
            let mut writer = None;
            if let Some(fs) = fs_opt.as_ref() {
                match fs.open_write(&file_path) {
                    Ok(h) => writer = Some(Mutex::new(DuckDbFileWriter::new(h))),
                    Err(e) => {
                        copy_init_error(
                            info,
                            format!(
                                "cannot open COPY destination {file_path} via DuckDB file system: {e}"
                            ),
                        );
                        return;
                    }
                }
            }

            let global = Box::new(PmtilesCopyGlobal {
                file_path,
                writer,
                features: Mutex::new(Vec::new()),
                _file_system: fs_opt,
            });
            duckdb_copy_function_global_init_set_global_state(
                info,
                Box::into_raw(global) as *mut c_void,
                Some(delete_boxed::<PmtilesCopyGlobal>),
            );
        }
    };
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        let msg = safe_panic_payload_to_string(payload);
        copy_init_error(info, format!("Panic in pmtiles copy global init: {msg}"));
    }
}

/// Convert one sink vector cell to an `MvtValue`. Returns `None` for NULL or
/// unsupported types.
///
/// # Safety
/// `vector`/`validity` must be live vector internals for `row`.
unsafe fn cell_mvt_value(
    vector: duckdb_vector,
    validity: *mut u64,
    ty: DuckDBType,
    row: u64,
) -> Option<MvtValue> {
    // SAFETY: `vector`/`validity` are live DuckDB vector internals supplied by
    // the sink callback contract.
    unsafe {
        if !duckdb_validity_is_valid(validity, row) {
            return None;
        }
        let data = duckdb_vector_get_data(vector);
        if data.is_null() {
            return None;
        }
        Some(match ty {
            // SAFETY: wrapper call into the initialized DuckDB C API;
            // pointer args are non-null and borrowed per call contract.
            DuckDBType::UBigInt => MvtValue::UInt(*(data as *const u64).add(row as usize)),
            DuckDBType::BigInt => MvtValue::Int(*(data as *const i64).add(row as usize)),
            DuckDBType::UInteger | DuckDBType::USmallInt | DuckDBType::UTinyInt => {
                MvtValue::UInt(match ty {
                    DuckDBType::UInteger => *(data as *const u32).add(row as usize) as u64,
                    DuckDBType::USmallInt => *(data as *const u16).add(row as usize) as u64,
                    _ => *(data as *const u8).add(row as usize) as u64,
                })
            }
            DuckDBType::Integer | DuckDBType::SmallInt | DuckDBType::TinyInt => {
                MvtValue::Int(match ty {
                    DuckDBType::Integer => *(data as *const i32).add(row as usize) as i64,
                    DuckDBType::SmallInt => *(data as *const i16).add(row as usize) as i64,
                    _ => *(data as *const i8).add(row as usize) as i64,
                })
            }
            DuckDBType::Double => MvtValue::Double(*(data as *const f64).add(row as usize)),
            DuckDBType::Float => MvtValue::Float(*(data as *const f32).add(row as usize)),
            DuckDBType::Boolean => MvtValue::Bool(*(data as *const bool).add(row as usize)),
            DuckDBType::Varchar => {
                let elem = (data as *const duckdb_string_t).add(row as usize);
                MvtValue::String((*elem).as_str().to_string())
            }
            _ => return None,
        })
    }
}

unsafe extern "C" fn pmtiles_copy_sink(
    info: duckdb_copy_function_sink_info,
    input: duckdb_data_chunk,
) {
    let run = || {
        // SAFETY: `info`/`input` are the live callback handles.
        unsafe {
            let bind_ptr =
                duckdb_copy_function_sink_get_bind_data(info) as *const PmtilesCopyBindData;
            let global_ptr =
                duckdb_copy_function_sink_get_global_state(info) as *const PmtilesCopyGlobal;
            if bind_ptr.is_null() || global_ptr.is_null() {
                copy_sink_error(info, "missing pmtiles copy state".to_string());
                return;
            }
            let bind_data = &*bind_ptr;
            let global = &*global_ptr;

            let row_count = duckdb_data_chunk_get_size(input);
            let column_count = duckdb_data_chunk_get_column_count(input) as usize;
            if column_count < 1 {
                copy_sink_error(info, "empty pmtiles input chunk".to_string());
                return;
            }
            let cell_vector = duckdb_data_chunk_get_vector(input, 0);
            let cell_validity = duckdb_vector_get_validity(cell_vector);
            let cell_ty = bind_data
                .column_types
                .first()
                .copied()
                .unwrap_or(DuckDBType::Invalid);

            let mut property_vectors = Vec::with_capacity(column_count - 1);
            for col in 1..column_count as u64 {
                let vector = duckdb_data_chunk_get_vector(input, col);
                let validity = duckdb_vector_get_validity(vector);
                let ty = bind_data
                    .column_types
                    .get(col as usize)
                    .copied()
                    .unwrap_or(DuckDBType::Invalid);
                property_vectors.push((vector, validity, ty));
            }

            let mut features = match global.features.lock() {
                Ok(f) => f,
                Err(e) => {
                    copy_sink_error(info, format!("pmtiles feature lock poisoned: {e}"));
                    return;
                }
            };

            for row in 0..row_count {
                if !duckdb_validity_is_valid(cell_validity, row) {
                    continue;
                }
                let cell_data = duckdb_vector_get_data(cell_vector);
                if cell_data.is_null() {
                    break;
                }
                let h3_index = match cell_ty {
                    DuckDBType::UBigInt => *(cell_data as *const u64).add(row as usize),
                    DuckDBType::BigInt => {
                        let v = *(cell_data as *const i64).add(row as usize);
                        if v < 0 {
                            continue;
                        }
                        v as u64
                    }
                    DuckDBType::Varchar => {
                        let elem = (cell_data as *const duckdb_string_t).add(row as usize);
                        match parse_hex_u64((*elem).as_str()) {
                            Some(v) => v,
                            None => continue,
                        }
                    }
                    _ => {
                        copy_sink_error(
                            info,
                            "first column of COPY (FORMAT pmtiles) must be UBIGINT, BIGINT, or hex VARCHAR".to_string(),
                        );
                        return;
                    }
                };

                let mut properties = Vec::with_capacity(property_vectors.len());
                for (i, (vector, validity, ty)) in property_vectors.iter().enumerate() {
                    if let Some(value) = cell_mvt_value(*vector, *validity, *ty, row) {
                        let key: std::borrow::Cow<'static, str> =
                            std::borrow::Cow::Owned(format!("column_{}", i + 2));
                        properties.push((key, value));
                    }
                }
                features.push(H3Feature {
                    h3_index,
                    properties,
                });
            }
        }
    };
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        let msg = safe_panic_payload_to_string(payload);
        copy_sink_error(info, format!("Panic in pmtiles copy sink: {msg}"));
    }
}

unsafe extern "C" fn pmtiles_copy_finalize(info: duckdb_copy_function_finalize_info) {
    let run = || {
        // SAFETY: `info` is the live finalize handle.
        unsafe {
            let bind_ptr =
                duckdb_copy_function_finalize_get_bind_data(info) as *const PmtilesCopyBindData;
            let global_ptr =
                duckdb_copy_function_finalize_get_global_state(info) as *const PmtilesCopyGlobal;
            if bind_ptr.is_null() || global_ptr.is_null() {
                copy_finalize_error(info, "missing pmtiles copy state".to_string());
                return;
            }
            let bind_data = &*bind_ptr;
            let global = &*global_ptr;
            let features = match global.features.lock() {
                Ok(mut f) => std::mem::take(&mut *f),
                Err(e) => {
                    copy_finalize_error(info, format!("pmtiles feature lock poisoned: {e}"));
                    return;
                }
            };
            let filtered = features.into_iter().filter(|f| {
                let res = h3o::CellIndex::try_from(f.h3_index)
                    .map(|c| u8::from(c.resolution()))
                    .unwrap_or(0);
                let zoom = raster_h3_core::pmtiles::pyramid::h3_res_to_zoom(res);
                bind_data.min_zoom.is_none_or(|z| zoom >= z)
                    && bind_data.max_zoom.is_none_or(|z| zoom <= z)
            });

            static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let tmp_path = std::env::temp_dir().join(format!(
                "raster_h3_copy_{}_{}.pmtiles",
                std::process::id(),
                TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            if let Err(e) = H3PmtilesTiler::export_h3_features(filtered, &tmp_path) {
                copy_finalize_error(info, format!("pmtiles export failed: {e}"));
                let _ = std::fs::remove_file(&tmp_path);
                return;
            }
            // Stream the finished archive in chunks instead of materializing
            // it again in memory (the feature set is already held by the
            // PMTiles pyramid builder).
            let copy_result = (|| -> Result<(), String> {
                let mut src = std::fs::File::open(&tmp_path)
                    .map_err(|e| format!("reading pmtiles temp file failed: {e}"))?;
                if let Some(writer) = global.writer.as_ref() {
                    let mut w = writer
                        .lock()
                        .map_err(|e| format!("writer lock poisoned: {e}"))?;
                    std::io::copy(&mut src, &mut *w).map_err(|e| e.to_string())?;
                    use std::io::Write;
                    w.flush().map_err(|e| e.to_string())?;
                } else {
                    let mut dst =
                        std::fs::File::create(&global.file_path).map_err(|e| e.to_string())?;
                    std::io::copy(&mut src, &mut dst).map_err(|e| e.to_string())?;
                }
                Ok(())
            })();
            let _ = std::fs::remove_file(&tmp_path);
            if let Err(e) = copy_result {
                copy_finalize_error(info, format!("writing {} failed: {e}", global.file_path));
            }
        }
    };
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        let msg = safe_panic_payload_to_string(payload);
        copy_finalize_error(info, format!("Panic in pmtiles copy finalize: {msg}"));
    }
}

/// Register the `pmtiles` COPY format function on `con`.
///
/// # Safety
/// `con` must be the live connection passed to the extension entry point.
#[allow(clippy::result_unit_err)]
pub unsafe fn register_copy_functions(con: duckdb_connection) -> Result<(), ()> {
    // SAFETY: `con` is the live extension-init connection (caller contract);
    // the copy function handle is ours and destroyed after registration, which
    // copies it into the catalog.
    unsafe {
        let mut func = duckdb_create_copy_function();
        if func.is_null() {
            return Err(());
        }
        duckdb_copy_function_set_name(func, c"pmtiles".as_ptr());
        duckdb_copy_function_set_bind(func, pmtiles_copy_bind);
        duckdb_copy_function_set_global_init(func, pmtiles_copy_global_init);
        duckdb_copy_function_set_sink(func, pmtiles_copy_sink);
        duckdb_copy_function_set_finalize(func, pmtiles_copy_finalize);
        let ok = duckdb_register_copy_function(con, func) == DuckDBState::Success;
        duckdb_destroy_copy_function(&mut func);
        if ok {
            Ok(())
        } else {
            Err(())
        }
    }
}
