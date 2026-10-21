#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// Minimum C extension API version requested from DuckDB's `get_api`.
pub const DUCKDB_C_API_VERSION: &std::ffi::CStr = c"v1.2.0";

/// Process-wide copy of the `duckdb_ext_api_v1` function table handed over
/// by DuckDB at load time. Reinstalling overwrites it in place, so repeated
/// extension loads never allocate.
static API_TABLE: [AtomicPtr<c_void>; API_TABLE_LEN] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; API_TABLE_LEN];
static API_TABLE_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Installs a copy of the `duckdb_ext_api_v1` table returned by
/// `access->get_api`, which DuckDB only keeps alive during extension init.
/// A null `table` uninstalls it.
///
/// # Safety
/// `table` must be null or point to a `duckdb_ext_api_v1` struct of at
/// least `DUCKDB_C_API_VERSION` (`API_TABLE_LEN` function pointers).
pub unsafe fn install_api_table(table: *const c_void) {
    if table.is_null() {
        API_TABLE_INSTALLED.store(false, Ordering::Release);
        return;
    }
    // SAFETY: per the caller contract `table` points to at least `API_TABLE_LEN`
    // pointer-sized, pointer-aligned function slots (a `duckdb_ext_api_v1`). We
    // copy them out immediately because DuckDB only keeps the table alive
    // during extension init.
    let entries = unsafe { std::slice::from_raw_parts(table as *const *mut c_void, API_TABLE_LEN) };
    for (slot, &func) in API_TABLE.iter().zip(entries) {
        slot.store(func, Ordering::Relaxed);
    }
    API_TABLE_INSTALLED.store(true, Ordering::Release);
}

/// Resolves the DuckDB C API function at `index` of `duckdb_ext_api_v1`.
/// Without an installed table (unit tests, hosts that export the C API)
/// it falls back to looking `name` up in the running process.
///
/// # Safety
/// `name` must be NUL-terminated and `index` must be its position in
/// `duckdb_ext_api_v1`.
#[inline(always)]
unsafe fn resolve_api_fn(
    index: usize,
    name: &'static str,
    fallback: &AtomicPtr<c_void>,
) -> *mut c_void {
    let func = if !API_TABLE_INSTALLED.load(Ordering::Acquire) {
        let mut func = fallback.load(Ordering::Relaxed);
        if func.is_null() {
            // SAFETY: `name` is NUL-terminated (caller contract); the caller is
            // responsible for transmuting the result to the matching signature.
            func = unsafe { crate::ffi::spatial_detect::dlsym_duckdb_symbol(name.as_bytes()) };
            fallback.store(func, Ordering::Relaxed);
        }
        func
    } else {
        API_TABLE[index].load(Ordering::Relaxed)
    };
    if func.is_null() {
        missing_api_fn(name);
    }
    func
}

/// Looks up the DuckDB C API function at `index` (table) or `name`
/// (process symbols), returning null when it is unavailable.
///
/// # Safety
/// `name` must be NUL-terminated and `index` must be its position in
/// `duckdb_ext_api_v1`.
pub unsafe fn lookup_api_fn(index: usize, name: &str) -> *mut c_void {
    if !API_TABLE_INSTALLED.load(Ordering::Acquire) {
        // SAFETY: `name` is NUL-terminated (caller contract).
        unsafe { crate::ffi::spatial_detect::dlsym_duckdb_symbol(name.as_bytes()) }
    } else {
        API_TABLE[index].load(Ordering::Relaxed)
    }
}

#[cold]
#[inline(never)]
fn missing_api_fn(name: &str) -> ! {
    panic!(
        "DuckDB C API function {} is unavailable",
        name.trim_end_matches('\0')
    );
}

/// Declares DuckDB C API functions as wrappers that call through the
/// `duckdb_ext_api_v1` table. Linux DuckDB binaries do not export the C API
/// symbols, so the extension must not link against them directly. Each
/// `[index]` is the field position in DuckDB v1.2.0's `duckdb_ext_api_v1`.
macro_rules! duckdb_api {
    ($(
        [$index:literal] pub fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;
    )*) => {
        $(
            /// # Safety
            /// Same contract as the DuckDB C API function of this name.
            #[inline(always)]
            pub unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                static FALLBACK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
                // SAFETY: `resolve_api_fn` never returns null (it panics
                // instead), and slot `$index` of `duckdb_ext_api_v1` v1.2.0 (or
                // the exported symbol of the same name) has exactly this C
                // signature, as transcribed from `duckdb_extension.h`. The name
                // literal is NUL-terminated by `concat!`. Calling it is then
                // sound under this wrapper's own contract, which is DuckDB's.
                unsafe {
                    let func: unsafe extern "C" fn($($ty),*) $(-> $ret)? = std::mem::transmute(
                        resolve_api_fn($index, concat!(stringify!($name), "\0"), &FALLBACK),
                    );
                    func($($arg),*)
                }
            }
        )*

        /// Field positions of the wrapped functions in `duckdb_ext_api_v1`,
        /// for building a substitute table (e.g. test mocks).
        #[allow(non_upper_case_globals)]
        pub mod api_index {
            $(pub const $name: usize = $index;)*
        }
    };
}

/// Number of function pointers in DuckDB v1.2.0's stable `duckdb_ext_api_v1`.
pub const API_TABLE_LEN: usize = 357;

pub type idx_t = u64;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DuckDBType {
    Invalid = 0,
    Boolean = 1,
    TinyInt = 2,
    SmallInt = 3,
    Integer = 4,
    BigInt = 5,
    UTinyInt = 6,
    USmallInt = 7,
    UInteger = 8,
    UBigInt = 9,
    Float = 10,
    Double = 11,
    Timestamp = 12,
    Date = 13,
    Time = 14,
    Interval = 15,
    HugeInt = 16,
    Varchar = 17,
    Blob = 18,
    Decimal = 19,
    TimestampS = 20,
    TimestampMs = 21,
    TimestampNs = 22,
    Enum = 23,
    List = 24,
    Struct = 25,
    Map = 26,
    Uuid = 27,
    Union = 28,
    Bit = 29,
    TimeTz = 30,
    TimestampTz = 31,
    UHugeInt = 32,
    Array = 33,
    Geometry = 40,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DuckDBState {
    Success = 0,
    Error = 1,
}

pub type duckdb_database = *mut c_void;
pub type duckdb_connection = *mut c_void;
pub type duckdb_table_function = *mut c_void;
pub type duckdb_bind_info = *mut c_void;
pub type duckdb_init_info = *mut c_void;
pub type duckdb_function_info = *mut c_void;
pub type duckdb_data_chunk = *mut c_void;
pub type duckdb_vector = *mut c_void;
pub type duckdb_logical_type = *mut c_void;
pub type duckdb_value = *mut c_void;
pub type duckdb_scalar_function = *mut c_void;
pub type duckdb_scalar_function_set = *mut c_void;
pub type duckdb_extension_info = *mut c_void;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct duckdb_result {
    pub deprecated_column_count: idx_t,
    pub deprecated_row_count: idx_t,
    pub deprecated_rows_changed: idx_t,
    pub deprecated_columns: *mut c_void,
    pub deprecated_error_message: *mut c_char,
    pub internal_data: *mut c_void,
}

impl Default for duckdb_result {
    fn default() -> Self {
        Self {
            deprecated_column_count: 0,
            deprecated_row_count: 0,
            deprecated_rows_changed: 0,
            deprecated_columns: std::ptr::null_mut(),
            deprecated_error_message: std::ptr::null_mut(),
            internal_data: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
pub struct duckdb_extension_access {
    pub set_error: Option<unsafe extern "C" fn(info: duckdb_extension_info, error: *const c_char)>,
    pub get_database:
        Option<unsafe extern "C" fn(info: duckdb_extension_info) -> *mut duckdb_database>,
    pub get_api: Option<
        unsafe extern "C" fn(info: duckdb_extension_info, version: *const c_char) -> *const c_void,
    >,
}

const _: () = {
    assert!(size_of::<duckdb_extension_access>() == 3 * size_of::<usize>());
    assert!(core::mem::offset_of!(duckdb_extension_access, set_error) == 0);
    assert!(core::mem::offset_of!(duckdb_extension_access, get_database) == size_of::<usize>());
    assert!(core::mem::offset_of!(duckdb_extension_access, get_api) == 2 * size_of::<usize>());
};

#[repr(C)]
#[derive(Copy, Clone)]
pub struct DuckDbStringPointer {
    pub length: u32,
    pub prefix: [u8; 4],
    pub ptr: *const c_char,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct DuckDbStringInlined {
    pub length: u32,
    pub inlined: [u8; 12],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union duckdb_string_t {
    pub pointer: DuckDbStringPointer,
    pub inlined: DuckDbStringInlined,
}

impl duckdb_string_t {
    /// # Safety
    /// `self` must be a `duckdb_string_t` initialized by DuckDB; both union
    /// variants begin with the length, so either may be active.
    #[inline(always)]
    pub unsafe fn length(&self) -> u32 {
        // SAFETY: both variants are `repr(C)` and start with the same `u32`
        // length at offset 0, so reading it through either is valid once DuckDB
        // has initialized the value (caller contract).
        unsafe { self.inlined.length }
    }

    /// Borrows the raw bytes (VARCHAR or BLOB payload).
    ///
    /// # Safety
    /// The caller must ensure that the underlying DuckDB vector is live and
    /// valid for the duration of the returned reference. For external strings
    /// (length > 12), `pointer.ptr` must point to at least `length` initialized
    /// bytes.
    #[inline(always)]
    pub unsafe fn as_bytes(&self) -> &[u8] {
        // SAFETY: the value was initialized by DuckDB (caller contract).
        let len = unsafe { self.length() } as usize;
        if len <= 12 {
            // SAFETY: DuckDB stores strings of at most 12 bytes inline, so the
            // `inlined` variant is the active one.
            unsafe { &self.inlined.inlined[..len] }
        } else {
            // SAFETY: longer strings use the `pointer` variant.
            let ptr = unsafe { self.pointer.ptr } as *const u8;
            if ptr.is_null() {
                &[]
            } else {
                // SAFETY: the caller guarantees `ptr` addresses `len` initialized
                // bytes owned by a vector that outlives the returned borrow.
                unsafe { std::slice::from_raw_parts(ptr, len) }
            }
        }
    }

    /// Borrows the string data as a Rust string slice (`""` if not UTF-8).
    ///
    /// # Safety
    /// Same as [`Self::as_bytes`].
    #[inline(always)]
    pub unsafe fn as_str(&self) -> &str {
        // SAFETY: same contract as `as_bytes`, forwarded from our caller.
        std::str::from_utf8(unsafe { self.as_bytes() }).unwrap_or("")
    }
}

/// Element of a LIST vector's data buffer: a slice of its child vector.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct duckdb_list_entry {
    pub offset: u64,
    pub length: u64,
}

pub type duckdb_table_function_bind_t = unsafe extern "C" fn(info: duckdb_bind_info);
pub type duckdb_table_function_init_t = unsafe extern "C" fn(info: duckdb_init_info);
pub type duckdb_table_function_t =
    unsafe extern "C" fn(info: duckdb_function_info, output: duckdb_data_chunk);
pub type duckdb_scalar_function_t = unsafe extern "C" fn(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
);
pub type duckdb_delete_callback_t = unsafe extern "C" fn(data: *mut c_void);

duckdb_api! {
    [3] pub fn duckdb_connect(
        database: duckdb_database,
        out_connection: *mut duckdb_connection,
    ) -> DuckDBState;
    [6] pub fn duckdb_disconnect(connection: *mut duckdb_connection);

    // Logical types
    [156] pub fn duckdb_create_logical_type(type_: DuckDBType) -> duckdb_logical_type;
    [184] pub fn duckdb_destroy_logical_type(type_: *mut duckdb_logical_type);
    [159] pub fn duckdb_create_list_type(type_: duckdb_logical_type) -> duckdb_logical_type;
    [158] pub fn duckdb_logical_type_set_alias(type_: duckdb_logical_type, alias: *const c_char);
    [157] pub fn duckdb_logical_type_get_alias(type_: duckdb_logical_type) -> *mut c_char;

    // Table functions
    [242] pub fn duckdb_create_table_function() -> duckdb_table_function;
    [243] pub fn duckdb_destroy_table_function(table_function: *mut duckdb_table_function);
    [244] pub fn duckdb_table_function_set_name(
        table_function: duckdb_table_function,
        name: *const c_char,
    );
    [245] pub fn duckdb_table_function_add_parameter(
        table_function: duckdb_table_function,
        type_: duckdb_logical_type,
    );
    [246] pub fn duckdb_table_function_add_named_parameter(
        table_function: duckdb_table_function,
        name: *const c_char,
        type_: duckdb_logical_type,
    );
    [248] pub fn duckdb_table_function_set_bind(
        table_function: duckdb_table_function,
        bind: duckdb_table_function_bind_t,
    );
    [249] pub fn duckdb_table_function_set_init(
        table_function: duckdb_table_function,
        init: duckdb_table_function_init_t,
    );
    [250] pub fn duckdb_table_function_set_local_init(
        table_function: duckdb_table_function,
        init_local: duckdb_table_function_init_t,
    );
    [251] pub fn duckdb_table_function_set_function(
        table_function: duckdb_table_function,
        function: duckdb_table_function_t,
    );
    [252] pub fn duckdb_table_function_supports_projection_pushdown(
        table_function: duckdb_table_function,
        pushdown: bool,
    );
    [253] pub fn duckdb_register_table_function(
        con: duckdb_connection,
        table_function: duckdb_table_function,
    ) -> DuckDBState;

    // Bind info
    [256] pub fn duckdb_bind_get_parameter_count(info: duckdb_bind_info) -> idx_t;
    [257] pub fn duckdb_bind_get_parameter(info: duckdb_bind_info, index: idx_t) -> duckdb_value;
    [258] pub fn duckdb_bind_get_named_parameter(
        info: duckdb_bind_info,
        name: *const c_char,
    ) -> duckdb_value;
    [255] pub fn duckdb_bind_add_result_column(
        info: duckdb_bind_info,
        name: *const c_char,
        type_: duckdb_logical_type,
    );
    [259] pub fn duckdb_bind_set_bind_data(
        info: duckdb_bind_info,
        extra_data: *mut c_void,
        destroy: Option<duckdb_delete_callback_t>,
    );
    [260] pub fn duckdb_bind_set_cardinality(info: duckdb_bind_info, cardinality: idx_t, is_exact: bool);
    [261] pub fn duckdb_bind_set_error(info: duckdb_bind_info, error: *const c_char);

    // Init info
    [263] pub fn duckdb_init_get_bind_data(info: duckdb_init_info) -> *mut c_void;
    [264] pub fn duckdb_init_set_init_data(
        info: duckdb_init_info,
        extra_data: *mut c_void,
        destroy: Option<duckdb_delete_callback_t>,
    );
    [265] pub fn duckdb_init_get_column_count(info: duckdb_init_info) -> idx_t;
    [266] pub fn duckdb_init_get_column_index(info: duckdb_init_info, column_index: idx_t) -> idx_t;
    [268] pub fn duckdb_init_set_error(info: duckdb_init_info, error: *const c_char);

    // Function info
    [270] pub fn duckdb_function_get_bind_data(info: duckdb_function_info) -> *mut c_void;
    [271] pub fn duckdb_function_get_init_data(info: duckdb_function_info) -> *mut c_void;
    [272] pub fn duckdb_function_get_local_init_data(info: duckdb_function_info) -> *mut c_void;
    [273] pub fn duckdb_function_set_error(info: duckdb_function_info, error: *const c_char);
    [221] pub fn duckdb_scalar_function_set_error(info: duckdb_function_info, error: *const c_char);

    // Data chunks & Vectors
    [187] pub fn duckdb_destroy_data_chunk(chunk: *mut duckdb_data_chunk);
    [189] pub fn duckdb_data_chunk_get_column_count(chunk: duckdb_data_chunk) -> idx_t;
    [190] pub fn duckdb_data_chunk_get_vector(chunk: duckdb_data_chunk, col_idx: idx_t) -> duckdb_vector;
    [191] pub fn duckdb_data_chunk_get_size(chunk: duckdb_data_chunk) -> idx_t;
    [192] pub fn duckdb_data_chunk_set_size(chunk: duckdb_data_chunk, size: idx_t);
    [194] pub fn duckdb_vector_get_data(vector: duckdb_vector) -> *mut c_void;
    [197] pub fn duckdb_vector_assign_string_element(
        vector: duckdb_vector,
        index: idx_t,
        str: *const c_char,
    );
    [198] pub fn duckdb_vector_assign_string_element_len(
        vector: duckdb_vector,
        index: idx_t,
        str: *const c_char,
        str_len: idx_t,
    );

    // List vectors
    [199] pub fn duckdb_list_vector_get_child(vector: duckdb_vector) -> duckdb_vector;
    [201] pub fn duckdb_list_vector_set_size(vector: duckdb_vector, size: idx_t) -> DuckDBState;
    [202] pub fn duckdb_list_vector_reserve(
        vector: duckdb_vector,
        required_capacity: idx_t,
    ) -> DuckDBState;

    // Validity mask
    [195] pub fn duckdb_vector_get_validity(vector: duckdb_vector) -> *mut u64;
    [196] pub fn duckdb_vector_ensure_validity_writable(vector: duckdb_vector);
    [205] pub fn duckdb_validity_row_is_valid(validity: *mut u64, row: idx_t) -> bool;
    [208] pub fn duckdb_validity_set_row_valid(validity: *mut u64, row: idx_t);
    [207] pub fn duckdb_validity_set_row_invalid(validity: *mut u64, row: idx_t);
    [206] pub fn duckdb_validity_set_row_validity(validity: *mut u64, row: idx_t, valid: bool);

    // Vector size
    [26] pub fn duckdb_vector_size() -> idx_t;

    // Values
    [142] pub fn duckdb_get_varchar(val: duckdb_value) -> *mut c_char;
    [125] pub fn duckdb_get_int64(val: duckdb_value) -> i64;
    [126] pub fn duckdb_get_uint64(val: duckdb_value) -> u64;
    [130] pub fn duckdb_get_double(val: duckdb_value) -> f64;
    [118] pub fn duckdb_get_bool(val: duckdb_value) -> bool;
    [25] pub fn duckdb_free(ptr: *mut c_void);
    [92] pub fn duckdb_destroy_value(val: *mut duckdb_value);

    // Scalar functions
    [209] pub fn duckdb_create_scalar_function() -> duckdb_scalar_function;
    [210] pub fn duckdb_destroy_scalar_function(scalar_function: *mut duckdb_scalar_function);
    [211] pub fn duckdb_scalar_function_set_name(
        scalar_function: duckdb_scalar_function,
        name: *const c_char,
    );
    [215] pub fn duckdb_scalar_function_add_parameter(
        scalar_function: duckdb_scalar_function,
        type_: duckdb_logical_type,
    );
    [216] pub fn duckdb_scalar_function_set_return_type(
        scalar_function: duckdb_scalar_function,
        type_: duckdb_logical_type,
    );
    [218] pub fn duckdb_scalar_function_set_function(
        scalar_function: duckdb_scalar_function,
        function: duckdb_scalar_function_t,
    );
    [219] pub fn duckdb_register_scalar_function(
        con: duckdb_connection,
        scalar_function: duckdb_scalar_function,
    ) -> DuckDBState;
    [222] pub fn duckdb_create_scalar_function_set(name: *const c_char) -> duckdb_scalar_function_set;
    [223] pub fn duckdb_destroy_scalar_function_set(scalar_function_set: *mut duckdb_scalar_function_set);
    [224] pub fn duckdb_add_scalar_function_to_set(
        set: duckdb_scalar_function_set,
        function: duckdb_scalar_function,
    ) -> DuckDBState;
    [225] pub fn duckdb_register_scalar_function_set(
        con: duckdb_connection,
        set: duckdb_scalar_function_set,
    ) -> DuckDBState;

    [7] pub fn duckdb_library_version() -> *const c_char;

    // Queries
    [13] pub fn duckdb_query(
        connection: duckdb_connection,
        query: *const c_char,
        out_result: *mut duckdb_result,
    ) -> DuckDBState;
    [14] pub fn duckdb_destroy_result(result: *mut duckdb_result);
    [306] pub fn duckdb_fetch_chunk(result: duckdb_result) -> duckdb_data_chunk;
    [19] pub fn duckdb_column_count(result: *mut duckdb_result) -> idx_t;
    [21] pub fn duckdb_result_error(result: *mut duckdb_result) -> *const c_char;
}

/// Check if a specific row in a validity mask is valid.
/// If `validity` is NULL, all rows in DuckDB are considered valid.
///
/// # Safety
/// `validity` must be null or the validity mask of a live vector, and
/// `row` must be below that vector's capacity.
#[inline(always)]
pub unsafe fn duckdb_validity_is_valid(validity: *mut u64, row: idx_t) -> bool {
    if validity.is_null() {
        true
    } else {
        // SAFETY: non-null mask of a live vector with `row` below its capacity
        // (caller contract), so the bit DuckDB reads is in bounds.
        unsafe { duckdb_validity_row_is_valid(validity, row) }
    }
}

/// Mark a specific row in a vector as invalid (NULL).
///
/// # Safety
/// `vector` must be null or a live, writable vector, and `row` must be
/// below its capacity.
#[inline(always)]
pub unsafe fn duckdb_vector_set_row_invalid(vector: duckdb_vector, row: idx_t) {
    if vector.is_null() {
        return;
    }
    // SAFETY: `vector` is a live, writable vector and `row` is below its
    // capacity (caller contract). `ensure_validity_writable` allocates the mask
    // if needed, and the mask returned afterwards covers the full capacity.
    unsafe {
        duckdb_vector_ensure_validity_writable(vector);
        let validity = duckdb_vector_get_validity(vector);
        if !validity.is_null() {
            duckdb_validity_set_row_invalid(validity, row);
        }
    }
}

/// Optional lookup of duckdb_vector_size with a fallback, allowing safe execution
/// inside DuckDB processes and safe testing outside of DuckDB.
pub fn get_vector_size() -> usize {
    // SAFETY: the name is NUL-terminated and slot 26 of `duckdb_ext_api_v1` is
    // `idx_t duckdb_vector_size(void)`, so a non-null result can be transmuted
    // to that signature and called with no preconditions.
    unsafe {
        // 26: duckdb_vector_size in duckdb_ext_api_v1 (not wrapped; optional here).
        let sym = lookup_api_fn(26, "duckdb_vector_size\0");
        if !sym.is_null() {
            let func: unsafe extern "C" fn() -> idx_t = std::mem::transmute(sym);
            let sz = func() as usize;
            if sz > 0 {
                return sz;
            }
        }
    }
    2048
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_duckdb_extension_access_layout_and_offsets() {
        assert_eq!(size_of::<duckdb_extension_access>(), 3 * size_of::<usize>());
        assert_eq!(core::mem::offset_of!(duckdb_extension_access, set_error), 0);
        assert_eq!(
            core::mem::offset_of!(duckdb_extension_access, get_database),
            size_of::<usize>()
        );
        assert_eq!(
            core::mem::offset_of!(duckdb_extension_access, get_api),
            2 * size_of::<usize>()
        );
    }

    #[test]
    fn test_duckdb_validity_null_pointer_is_valid() {
        // If validity is NULL, all rows are treated as valid
        // SAFETY: a null mask is never dereferenced.
        assert!(unsafe { duckdb_validity_is_valid(std::ptr::null_mut(), 0) });
        // SAFETY: as above.
        assert!(unsafe { duckdb_validity_is_valid(std::ptr::null_mut(), 100) });
    }

    #[test]
    fn test_get_vector_size_fallback() {
        let sz = get_vector_size();
        assert!(sz > 0);
    }

    #[test]
    fn test_duckdb_string_t_memory_layout_and_alignment() {
        assert_eq!(size_of::<duckdb_string_t>(), 16);
        assert_eq!(size_of::<DuckDbStringInlined>(), 16);
        assert_eq!(size_of::<DuckDbStringPointer>(), 16);
        assert_eq!(align_of::<duckdb_string_t>(), 8);
    }

    #[test]
    fn test_duckdb_string_t_exhaustive_lengths() {
        // 1. Length 0 (empty string)
        let s0 = duckdb_string_t {
            inlined: DuckDbStringInlined {
                length: 0,
                inlined: [0u8; 12],
            },
        };
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s0.as_str() }, "");
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s0.length() }, 0);

        // 2. Lengths 1 to 12 (inlined strings)
        for len in 1..=12 {
            let mut inlined = [0u8; 12];
            let raw = b"abcdefghijkl";
            inlined[..len].copy_from_slice(&raw[..len]);
            let s = duckdb_string_t {
                inlined: DuckDbStringInlined {
                    length: len as u32,
                    inlined,
                },
            };
            // SAFETY: the test built this value with the variant matching its length; any
            // out-of-line pointer refers to a live local buffer of at least `length` bytes.
            assert_eq!(unsafe { s.length() }, len as u32);
            assert_eq!(
                // SAFETY: inline variant with `length <= 12`.
                unsafe { s.as_str() },
                std::str::from_utf8(&raw[..len]).unwrap()
            );
        }

        // 3. Length 13 (first pointer length, just above 12-byte inlined boundary)
        let raw13 = b"1234567890123\0";
        let s13 = duckdb_string_t {
            pointer: DuckDbStringPointer {
                length: 13,
                prefix: [raw13[0], raw13[1], raw13[2], raw13[3]],
                ptr: raw13.as_ptr() as *const c_char,
            },
        };
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s13.length() }, 13);
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s13.as_str() }, "1234567890123");

        // 4. Length 15 (standard H3 hex string, e.g. "8828308281fffff")
        let h3_hex = b"8828308281fffff\0";
        let s15 = duckdb_string_t {
            pointer: DuckDbStringPointer {
                length: 15,
                prefix: [h3_hex[0], h3_hex[1], h3_hex[2], h3_hex[3]],
                ptr: h3_hex.as_ptr() as *const c_char,
            },
        };
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s15.length() }, 15);
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s15.as_str() }, "8828308281fffff");

        // 5. Length 32 (longer pointer string)
        let long_str = b"this_is_a_longer_32_byte_string!\0";
        let s32 = duckdb_string_t {
            pointer: DuckDbStringPointer {
                length: 32,
                prefix: [long_str[0], long_str[1], long_str[2], long_str[3]],
                ptr: long_str.as_ptr() as *const c_char,
            },
        };
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s32.length() }, 32);
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s32.as_str() }, "this_is_a_longer_32_byte_string!");
    }

    #[test]
    fn test_duckdb_string_t_null_and_invalid_utf8_safety() {
        // 1. Pointer string with null pointer
        let s_null = duckdb_string_t {
            pointer: DuckDbStringPointer {
                length: 20,
                prefix: [0; 4],
                ptr: std::ptr::null(),
            },
        };
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s_null.as_str() }, "");

        // 2. Inlined string with invalid UTF-8 bytes (should gracefully return "" without panicking)
        let s_invalid_inline = duckdb_string_t {
            inlined: DuckDbStringInlined {
                length: 3,
                inlined: [0xFF, 0xFE, 0xFD, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            },
        };
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s_invalid_inline.as_str() }, "");

        // 3. Pointer string with invalid UTF-8 bytes
        let invalid_bytes = [
            0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8, 0xF7, 0xF6, 0xF5, 0xF4, 0xF3, 0,
        ];
        let s_invalid_ptr = duckdb_string_t {
            pointer: DuckDbStringPointer {
                length: 13,
                prefix: [0xFF, 0xFE, 0xFD, 0xFC],
                ptr: invalid_bytes.as_ptr() as *const c_char,
            },
        };
        // SAFETY: the test built this value with the variant matching its length; any
        // out-of-line pointer refers to a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s_invalid_ptr.as_str() }, "");
    }

    #[test]
    fn test_delete_boxed_lifecycle_and_null_safety() {
        struct DropTracker {
            dropped: Arc<AtomicBool>,
        }

        impl Drop for DropTracker {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        // 1. Verify delete_boxed deallocates and invokes Drop
        let flag = Arc::new(AtomicBool::new(false));
        let tracker = Box::new(DropTracker {
            dropped: Arc::clone(&flag),
        });
        let raw_ptr = Box::into_raw(tracker) as *mut c_void;
        assert!(!flag.load(Ordering::SeqCst));

        // SAFETY: `raw_ptr` came from `Box::into_raw(Box<DropTracker>)` and is
        // freed exactly once here.
        unsafe {
            crate::functions::bind_utils::delete_boxed::<DropTracker>(raw_ptr);
        }
        assert!(
            flag.load(Ordering::SeqCst),
            "delete_boxed must invoke Drop and reclaim memory"
        );

        // 2. Verify delete_boxed handles null pointer safely without panic
        // SAFETY: null is explicitly allowed and ignored.
        unsafe {
            crate::functions::bind_utils::delete_boxed::<DropTracker>(std::ptr::null_mut());
        }
    }

    #[test]
    fn test_duckdb_result_default_safety() {
        let res = duckdb_result::default();
        assert_eq!(res.deprecated_column_count, 0);
        assert_eq!(res.deprecated_row_count, 0);
        assert!(res.deprecated_columns.is_null());
        assert!(res.deprecated_error_message.is_null());
        assert!(res.internal_data.is_null());
    }

    #[test]
    fn test_duckdb_type_discriminants_official_abi() {
        assert_eq!(DuckDBType::Invalid as u32, 0);
        assert_eq!(DuckDBType::Boolean as u32, 1);
        assert_eq!(DuckDBType::TinyInt as u32, 2);
        assert_eq!(DuckDBType::SmallInt as u32, 3);
        assert_eq!(DuckDBType::Integer as u32, 4);
        assert_eq!(DuckDBType::BigInt as u32, 5);
        assert_eq!(DuckDBType::UTinyInt as u32, 6);
        assert_eq!(DuckDBType::USmallInt as u32, 7);
        assert_eq!(DuckDBType::UInteger as u32, 8);
        assert_eq!(DuckDBType::UBigInt as u32, 9);
        assert_eq!(DuckDBType::Float as u32, 10);
        assert_eq!(DuckDBType::Double as u32, 11);
        assert_eq!(DuckDBType::Timestamp as u32, 12);
        assert_eq!(DuckDBType::Date as u32, 13);
        assert_eq!(DuckDBType::Time as u32, 14);
        assert_eq!(DuckDBType::Interval as u32, 15);
        assert_eq!(DuckDBType::HugeInt as u32, 16);
        assert_eq!(DuckDBType::Varchar as u32, 17);
        assert_eq!(DuckDBType::Blob as u32, 18);
        assert_eq!(DuckDBType::Decimal as u32, 19);
        assert_eq!(DuckDBType::TimestampS as u32, 20);
        assert_eq!(DuckDBType::TimestampMs as u32, 21);
        assert_eq!(DuckDBType::TimestampNs as u32, 22);
        assert_eq!(DuckDBType::Enum as u32, 23);
        assert_eq!(DuckDBType::List as u32, 24);
        assert_eq!(DuckDBType::Struct as u32, 25);
        assert_eq!(DuckDBType::Map as u32, 26);
        assert_eq!(DuckDBType::Uuid as u32, 27);
        assert_eq!(DuckDBType::Union as u32, 28);
        assert_eq!(DuckDBType::Bit as u32, 29);
        assert_eq!(DuckDBType::TimeTz as u32, 30);
        assert_eq!(DuckDBType::TimestampTz as u32, 31);
        assert_eq!(DuckDBType::UHugeInt as u32, 32);
        assert_eq!(DuckDBType::Array as u32, 33);
        assert_eq!(DuckDBType::Geometry as u32, 40);
    }
}
