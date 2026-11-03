//! DuckDB session configuration options and the DuckDB file-system source
//! factory.
//!
//! The v1.5.6 API build registers `raster_h3_*` extension options on the
//! loading connection so sessions can `SET` them (environment variables remain
//! as fallbacks). On the default v1.2.0 build every entry point is a no-op.

use raster_h3_core::raster::http_range::RemoteSourceFactory;

#[cfg(feature = "duckdb-api-v1_5_6")]
use crate::duckdb_connection;
use crate::ffi::duckdb_bind_info;

/// `SET raster_h3_use_duckdb_fs = true`: route raster byte-range reads through
/// DuckDB's file system (httpfs, S3/Azure/GCS, `CREATE SECRET` credentials).
#[cfg(feature = "duckdb-api-v1_5_6")]
pub(crate) const USE_DUCKDB_FS_OPTION: &str = "raster_h3_use_duckdb_fs";
/// `SET raster_h3_fetch_workers = n`: remote prefetch worker count.
#[cfg(feature = "duckdb-api-v1_5_6")]
pub(crate) const FETCH_WORKERS_OPTION: &str = "raster_h3_fetch_workers";
/// `SET raster_h3_decode_budget_mb = n`: decoded sample budget in MiB.
#[cfg(feature = "duckdb-api-v1_5_6")]
pub(crate) const DECODE_BUDGET_MB_OPTION: &str = "raster_h3_decode_budget_mb";
/// `SET raster_h3_workers = n`: dedicated aggregation pool size.
#[cfg(feature = "duckdb-api-v1_5_6")]
pub(crate) const WORKERS_OPTION: &str = "raster_h3_workers";

#[cfg(feature = "duckdb-api-v1_5_6")]
const USE_DUCKDB_FS_ENV: &str = "RASTER_H3_USE_DUCKDB_FS";
#[cfg(feature = "duckdb-api-v1_5_6")]
const FETCH_WORKERS_ENV: &str = "RASTER_H3_FETCH_WORKERS";
#[cfg(feature = "duckdb-api-v1_5_6")]
const DECODE_BUDGET_MB_ENV: &str = "RASTER_H3_DECODE_BUDGET_MB";
#[cfg(feature = "duckdb-api-v1_5_6")]
const WORKERS_ENV: &str = "RASTER_H3_WORKERS";

#[cfg(feature = "duckdb-api-v1_5_6")]
fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.as_str(),
                "1" | "true" | "TRUE" | "on" | "ON" | "yes" | "YES"
            )
        })
        .unwrap_or(false)
}

/// Register the `raster_h3_*` extension options on the loading connection.
/// No-op on the v1.2.0 API build.
///
/// # Safety
/// `con` must be the live connection passed to the extension entry point.
#[cfg(feature = "duckdb-api-v1_5_6")]
pub(crate) unsafe fn register_duckdb_config_options(con: duckdb_connection) {
    #[cfg(feature = "duckdb-api-v1_5_6")]
    // SAFETY: `con` is the live extension-init connection (caller contract).
    unsafe {
        register_options_impl(con)
    };
    #[cfg(not(feature = "duckdb-api-v1_5_6"))]
    let _ = con;
}

/// Build a raster source factory that opens every source through the DuckDB
/// file system when `raster_h3_use_duckdb_fs` (or `RASTER_H3_USE_DUCKDB_FS`)
/// is enabled. Always `None` on the v1.2.0 API build.
///
/// # Safety
/// `info` must be the live bind handle of a table-function bind callback.
pub(crate) unsafe fn raster_source_factory_for_bind(
    info: duckdb_bind_info,
) -> Option<RemoteSourceFactory> {
    #[cfg(feature = "duckdb-api-v1_5_6")]
    {
        // SAFETY: `info` is the live bind handle (caller contract).
        unsafe { factory_for_bind_impl(info) }
    }
    #[cfg(not(feature = "duckdb-api-v1_5_6"))]
    {
        let _ = info;
        None
    }
}

/// Fill unset execution parameters from `SET` options (falling back to the
/// `RASTER_H3_*` environment variables). No-op on the v1.2.0 API build.
///
/// # Safety
/// `info` must be the live bind handle of a table-function bind callback.
pub(crate) unsafe fn apply_duckdb_config_options(
    info: duckdb_bind_info,
    execution: &mut super::bind_utils::ExecutionParams,
) {
    #[cfg(feature = "duckdb-api-v1_5_6")]
    // SAFETY: `info` is the live bind handle (caller contract).
    unsafe {
        apply_options_impl(info, execution)
    };
    #[cfg(not(feature = "duckdb-api-v1_5_6"))]
    {
        let _ = info;
        let _ = execution;
    }
}

#[cfg(feature = "duckdb-api-v1_5_6")]
fn option_cstring(name: &str) -> std::ffi::CString {
    std::ffi::CString::new(name).expect("option names contain no NUL bytes")
}

#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe fn register_options_impl(con: duckdb_connection) {
    use crate::ffi::*;

    fn register_one(
        con: duckdb_connection,
        name: &std::ffi::CStr,
        ty: DuckDBType,
        default: duckdb_value,
        description: &std::ffi::CStr,
    ) {
        // SAFETY: all temporaries created here are owned by us and released
        // as documented; `con` is the live init connection (caller contract).
        unsafe {
            let option = duckdb_create_config_option();
            if option.is_null() {
                return;
            }
            duckdb_config_option_set_name(option, name.as_ptr());
            let logical = duckdb_create_logical_type(ty);
            // `set_type` copies the LogicalType, so the caller-owned one is
            // destroyed right after.
            duckdb_config_option_set_type(option, logical);
            duckdb_destroy_logical_type(&mut { logical });
            // `set_default_value` likewise copies the value.
            duckdb_config_option_set_default_value(option, default);
            duckdb_destroy_value(&mut { default });
            duckdb_config_option_set_default_scope(option, DuckDBConfigOptionScope::Session);
            duckdb_config_option_set_description(option, description.as_ptr());
            // `register` copies; failure means the option already exists.
            let _ = duckdb_register_config_option(con, option);
            let mut option = option;
            duckdb_destroy_config_option(&mut option);
        }
    }

    // SAFETY: wrapper call into the initialized DuckDB C API;
    // pointer args are non-null and borrowed per call contract.
    unsafe {
        register_one(
            con,
            c"raster_h3_use_duckdb_fs",
            DuckDBType::Boolean,
            duckdb_create_bool(false),
            c"Route raster reads through the DuckDB file system (httpfs, S3/Azure/GCS, secrets)",
        );
        register_one(
            con,
            c"raster_h3_fetch_workers",
            DuckDBType::BigInt,
            duckdb_create_int64(0),
            c"Remote prefetch worker count (0 = default)",
        );
        register_one(
            con,
            c"raster_h3_decode_budget_mb",
            DuckDBType::BigInt,
            duckdb_create_int64(0),
            c"Decoded raster sample budget in MiB (0 = default)",
        );
        register_one(
            con,
            c"raster_h3_workers",
            DuckDBType::BigInt,
            duckdb_create_int64(0),
            c"Dedicated aggregation worker pool size (0 = default)",
        );
    }
}

#[cfg(feature = "duckdb-api-v1_5_6")]
fn bind_client_context(info: duckdb_bind_info) -> crate::ffi::duckdb_client_context {
    let mut ctx = std::ptr::null_mut();
    // SAFETY: `info` is the live bind handle; `ctx` is written on success.
    unsafe { crate::ffi::duckdb_table_function_get_client_context(info, &mut ctx) };
    ctx
}

#[cfg(feature = "duckdb-api-v1_5_6")]
fn read_option_bool(ctx: crate::ffi::duckdb_client_context, name: &str) -> Option<bool> {
    use crate::ffi::*;
    let c_name = option_cstring(name);
    let mut scope = DuckDBConfigOptionScope::Invalid;
    // SAFETY: `ctx`/`c_name` valid; returned value is owned by us.
    let value =
        unsafe { duckdb_client_context_get_config_option(ctx, c_name.as_ptr(), &mut scope) };
    if value.is_null() {
        return None;
    }
    // SAFETY: `value` is an owned duckdb_value we destroy once.
    // SAFETY: `context` is a valid client context; `value` is the owned
    // option value returned by DuckDB.
    let result = unsafe {
        if duckdb_is_null_value(value) {
            None
        } else {
            Some(duckdb_get_bool(value))
        }
    };
    // SAFETY: `value` is an owned value returned by DuckDB.
    unsafe { duckdb_destroy_value(&mut { value }) };
    result
}

#[cfg(feature = "duckdb-api-v1_5_6")]
fn read_option_i64(ctx: crate::ffi::duckdb_client_context, name: &str) -> Option<i64> {
    use crate::ffi::*;
    let c_name = option_cstring(name);
    let mut scope = DuckDBConfigOptionScope::Invalid;
    // SAFETY: `ctx`/`c_name` valid; returned value is owned by us.
    let value =
        unsafe { duckdb_client_context_get_config_option(ctx, c_name.as_ptr(), &mut scope) };
    if value.is_null() {
        return None;
    }
    // SAFETY: `context` is a valid client context; `value` is the owned
    // option value returned by DuckDB.
    let result = unsafe {
        if duckdb_is_null_value(value) {
            None
        } else {
            Some(duckdb_get_int64(value))
        }
    };
    // SAFETY: `value` is an owned value returned by DuckDB.
    unsafe { duckdb_destroy_value(&mut { value }) };
    result
}

#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe fn factory_for_bind_impl(info: duckdb_bind_info) -> Option<RemoteSourceFactory> {
    use crate::ffi::*;
    use std::sync::Arc;

    let mut ctx = bind_client_context(info);
    let enabled = if ctx.is_null() {
        env_truthy(USE_DUCKDB_FS_ENV)
    } else {
        // SAFETY: ctx is a live client context handle.
        read_option_bool(ctx, USE_DUCKDB_FS_OPTION).unwrap_or_else(|| env_truthy(USE_DUCKDB_FS_ENV))
    };
    if !enabled {
        if !ctx.is_null() {
            // SAFETY: ctx is owned by us.
            unsafe { duckdb_destroy_client_context(&mut ctx) };
        }
        return None;
    }
    if ctx.is_null() {
        return None;
    }
    // SAFETY: ctx is a live client context; returned file-system handle is
    // owned by us until destroyed in `DuckDbFileSystem::drop`.
    let fs_handle = unsafe { duckdb_client_context_get_file_system(ctx) };
    // SAFETY: ctx is owned by us.
    unsafe { duckdb_destroy_client_context(&mut ctx) };
    if fs_handle.is_null() {
        return None;
    }
    // SAFETY: fs_handle is a live, un-owned handle; `from_raw` takes ownership.
    let fs = Arc::new(unsafe { DuckDbFileSystem::from_raw(fs_handle) });
    Some(RemoteSourceFactory::new(move |path| {
        let transport = DuckDbFileTransport::open(Arc::clone(&fs), path)?;
        // SAFETY of correctness: DuckDbFileTransport is a RemoteTransport that
        // issues seek+read range requests through DuckDB's file layer.
        raster_h3_core::raster::http_range::RemoteHttpSource::from_transport(
            path.to_string(),
            Box::new(transport),
        )
        .map(Arc::new)
    }))
}

#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe fn apply_options_impl(
    info: duckdb_bind_info,
    execution: &mut super::bind_utils::ExecutionParams,
) {
    let mut ctx = bind_client_context(info);
    if ctx.is_null() {
        return;
    }
    // SAFETY: ctx is a live client context handle owned by us.
    {
        if execution.fetch_workers.is_none() {
            execution.fetch_workers = read_option_i64(ctx, FETCH_WORKERS_OPTION)
                .filter(|&v| v > 0)
                .map(|v| v as usize)
                .or_else(|| env_i64(FETCH_WORKERS_ENV).map(|v| v as usize));
        }
        if execution.decode_budget_mb.is_none() {
            execution.decode_budget_mb = read_option_i64(ctx, DECODE_BUDGET_MB_OPTION)
                .filter(|&v| v > 0)
                .map(|v| v as usize)
                .or_else(|| env_i64(DECODE_BUDGET_MB_ENV).map(|v| v as usize));
        }
        if execution.aggregation_workers.is_none() {
            execution.aggregation_workers = read_option_i64(ctx, WORKERS_OPTION)
                .filter(|&v| v > 0)
                .map(|v| v as usize)
                .or_else(|| env_i64(WORKERS_ENV).map(|v| v as usize));
        }
    }
    // SAFETY: ctx is owned by us.
    unsafe { crate::ffi::duckdb_destroy_client_context(&mut ctx) };
}

#[cfg(feature = "duckdb-api-v1_5_6")]
fn env_i64(name: &str) -> Option<i64> {
    std::env::var(name).ok()?.parse().ok().filter(|&v| v > 0)
}
