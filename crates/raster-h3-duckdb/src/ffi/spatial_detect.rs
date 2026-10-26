use crate::ffi::duckdb_c::*;
use crate::ffi::to_c_string;
use std::sync::atomic::{AtomicBool, Ordering};

static SPATIAL_LOADED: AtomicBool = AtomicBool::new(false);

/// Sets the global flag indicating whether spatial extension or GEOMETRY type is active
pub fn set_spatial_loaded(loaded: bool) {
    SPATIAL_LOADED.store(loaded, Ordering::Release);
}

/// Parses a DuckDB version string (e.g. "v1.2.0", "v1.5.0") and checks if it is at least (req_major, req_minor)
pub fn parse_version_string(ver_str: &str, req_major: u32, req_minor: u32) -> bool {
    let s = ver_str.strip_prefix('v').unwrap_or(ver_str);
    let mut parts = s.split('.');
    if let (Some(maj_s), Some(min_s)) = (parts.next(), parts.next()) {
        if let (Ok(maj), Ok(min)) = (maj_s.parse::<u32>(), min_s.parse::<u32>()) {
            if maj > req_major || (maj == req_major && min >= req_minor) {
                return true;
            }
        }
    }
    false
}

#[cfg(all(unix, target_os = "macos"))]
const RTLD_DEFAULT: *mut std::ffi::c_void = -2isize as *mut std::ffi::c_void;
#[cfg(all(unix, not(target_os = "macos")))]
const RTLD_DEFAULT: *mut std::ffi::c_void = std::ptr::null_mut();

#[cfg(unix)]
extern "C" {
    fn dlsym(
        handle: *mut std::ffi::c_void,
        symbol: *const std::os::raw::c_char,
    ) -> *mut std::ffi::c_void;
}

#[cfg(windows)]
extern "system" {
    fn GetModuleHandleA(lpModuleName: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn GetProcAddress(
        hModule: *mut std::ffi::c_void,
        lpProcName: *const std::os::raw::c_char,
    ) -> *mut std::ffi::c_void;
}

/// Dynamically looks up a symbol from DuckDB's runtime environment
///
/// # Safety
/// `symbol` must be NUL-terminated. Transmuting the returned pointer to a
/// function type is only sound if it matches the symbol's C signature.
pub unsafe fn dlsym_duckdb_symbol(symbol: &[u8]) -> *mut std::ffi::c_void {
    debug_assert_eq!(symbol.last(), Some(&0), "symbol must be NUL-terminated");
    #[cfg(unix)]
    // SAFETY: `symbol` is NUL-terminated (caller contract) and `RTLD_DEFAULT` is
    // the platform's pseudo-handle for the global symbol scope; `dlsym` only
    // reads the name and returns null when the symbol is absent.
    unsafe {
        dlsym(RTLD_DEFAULT, symbol.as_ptr() as *const _)
    }
    #[cfg(windows)]
    // SAFETY: `symbol` is NUL-terminated (caller contract), as is the
    // "duckdb.dll" literal; `GetModuleHandleA(NULL)` returns the host executable
    // and neither call takes ownership of anything; both return null on failure.
    unsafe {
        let mut handle = GetModuleHandleA(std::ptr::null());
        let mut sym = if !handle.is_null() {
            GetProcAddress(handle, symbol.as_ptr() as *const _)
        } else {
            std::ptr::null_mut()
        };
        if sym.is_null() {
            handle = GetModuleHandleA(b"duckdb.dll\0".as_ptr() as *const _);
            if !handle.is_null() {
                sym = GetProcAddress(handle, symbol.as_ptr() as *const _);
            }
        }
        sym
    }
}

/// Checks if the runtime DuckDB library version is at least (req_major, req_minor)
pub fn is_duckdb_version_at_least(req_major: u32, req_minor: u32) -> bool {
    // SAFETY: the name is NUL-terminated and matches its table index, and
    // `duckdb_library_version` has signature `const char *(void)`, returning a
    // static NUL-terminated string (or null, which is checked).
    unsafe {
        let sym = lookup_api_fn(
            api_index::duckdb_library_version,
            "duckdb_library_version\0",
        );
        if !sym.is_null() {
            let func: unsafe extern "C" fn() -> *const std::os::raw::c_char =
                std::mem::transmute(sym);
            let ptr = func();
            if !ptr.is_null() {
                let ver_str = std::ffi::CStr::from_ptr(ptr).to_string_lossy();
                return parse_version_string(&ver_str, req_major, req_minor);
            }
        }
    }
    false
}

/// Checks if DuckDB has spatial capabilities or native GEOMETRY type available
pub fn is_geometry_available() -> bool {
    // 1. In DuckDB >= 1.5, GEOMETRY is a built-in native type in core DuckDB
    if is_duckdb_version_at_least(1, 5) {
        return true;
    }
    // 2. Otherwise check if spatial was detected at init or runtime
    SPATIAL_LOADED.load(Ordering::Acquire)
}

/// Probes the DuckDB database to check if spatial extension is loaded or GEOMETRY type is registered
///
/// # Safety
/// `con` must be a valid, open DuckDB connection.
pub unsafe fn is_spatial_loaded_query(con: duckdb_connection) -> bool {
    let mut result = duckdb_result::default();
    let query = to_c_string(
        "SELECT 1 FROM duckdb_extensions() WHERE extension_name = 'spatial' AND loaded = true \
         UNION SELECT 1 FROM duckdb_types() WHERE type_name = 'GEOMETRY' LIMIT 1;",
    );
    // SAFETY: `con` is a valid open connection (caller contract), `query` is a
    // NUL-terminated string outliving the call, and `result` is a local the
    // query fills in. A non-null fetched chunk is owned by us and destroyed
    // once; `result` is destroyed once on every path, which DuckDB requires
    // even when the query fails.
    unsafe {
        let state = duckdb_query(con, query.as_ptr(), &mut result);
        let is_loaded = if state == DuckDBState::Success {
            let mut chunk = duckdb_fetch_chunk(result);
            if chunk.is_null() {
                false
            } else {
                let has_rows = duckdb_data_chunk_get_size(chunk) > 0;
                duckdb_destroy_data_chunk(&mut chunk);
                has_rows
            }
        } else {
            false
        };
        duckdb_destroy_result(&mut result);
        is_loaded
    }
}

/// Creates a DuckDB GEOMETRY logical type using the appropriate strategy:
/// 1. If DuckDB >= 1.5, attempts to create native DuckDBType::Geometry (40).
/// 2. Fallback: Creates DuckDBType::Blob and aliases it as "GEOMETRY",
///    which is the universal DuckDB spatial convention.
///
/// # Safety
/// The DuckDB C API must be initialized. The caller owns the returned
/// type and must release it with `duckdb_destroy_logical_type`.
pub unsafe fn create_geometry_logical_type() -> duckdb_logical_type {
    // SAFETY: the C API is initialized (caller contract). `Geometry` (40) is only
    // requested from DuckDB >= 1.5, which defines it; ownership of the returned
    // type passes to the caller. `set_alias` copies the NUL-terminated alias.
    unsafe {
        if is_duckdb_version_at_least(1, 5) {
            let geom_type = duckdb_create_logical_type(DuckDBType::Geometry);
            if !geom_type.is_null() {
                return geom_type;
            }
        }
        let blob_type = duckdb_create_logical_type(DuckDBType::Blob);
        let alias = to_c_string("GEOMETRY");
        duckdb_logical_type_set_alias(blob_type, alias.as_ptr());
        blob_type
    }
}
