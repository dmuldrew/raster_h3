#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::c_void;
use std::os::raw::c_char;

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
    assert!(std::mem::size_of::<duckdb_extension_access>() == 3 * std::mem::size_of::<usize>());
    assert!(core::mem::offset_of!(duckdb_extension_access, set_error) == 0);
    assert!(
        core::mem::offset_of!(duckdb_extension_access, get_database)
            == std::mem::size_of::<usize>()
    );
    assert!(
        core::mem::offset_of!(duckdb_extension_access, get_api) == 2 * std::mem::size_of::<usize>()
    );
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
    #[inline(always)]
    pub unsafe fn length(&self) -> u32 {
        self.inlined.length
    }

    /// Borrows the string data as a Rust string slice.
    ///
    /// # Safety
    /// The caller must ensure that the underlying DuckDB vector is live and
    /// valid for the duration of the returned reference. For external strings
    /// (length > 12), `pointer.ptr` must point to at least `length` initialized
    /// UTF-8 bytes.
    #[inline(always)]
    pub unsafe fn as_str(&self) -> &str {
        let len = self.length() as usize;
        if len <= 12 {
            std::str::from_utf8(&self.inlined.inlined[..len]).unwrap_or("")
        } else {
            let ptr = self.pointer.ptr as *const u8;
            if ptr.is_null() {
                ""
            } else {
                let slice = std::slice::from_raw_parts(ptr, len);
                std::str::from_utf8(slice).unwrap_or("")
            }
        }
    }
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

#[cfg_attr(windows, link(name = "duckdb"))]
extern "C" {
    pub fn duckdb_connect(
        database: duckdb_database,
        out_connection: *mut duckdb_connection,
    ) -> DuckDBState;
    pub fn duckdb_disconnect(connection: *mut duckdb_connection);

    // Logical types
    pub fn duckdb_create_logical_type(type_: DuckDBType) -> duckdb_logical_type;
    pub fn duckdb_destroy_logical_type(type_: *mut duckdb_logical_type);
    pub fn duckdb_logical_type_set_alias(type_: duckdb_logical_type, alias: *const c_char);
    pub fn duckdb_logical_type_get_alias(type_: duckdb_logical_type) -> *mut c_char;

    // Table functions
    pub fn duckdb_create_table_function() -> duckdb_table_function;
    pub fn duckdb_destroy_table_function(table_function: *mut duckdb_table_function);
    pub fn duckdb_table_function_set_name(
        table_function: duckdb_table_function,
        name: *const c_char,
    );
    pub fn duckdb_table_function_add_parameter(
        table_function: duckdb_table_function,
        type_: duckdb_logical_type,
    );
    pub fn duckdb_table_function_add_named_parameter(
        table_function: duckdb_table_function,
        name: *const c_char,
        type_: duckdb_logical_type,
    );
    pub fn duckdb_table_function_set_bind(
        table_function: duckdb_table_function,
        bind: duckdb_table_function_bind_t,
    );
    pub fn duckdb_table_function_set_init(
        table_function: duckdb_table_function,
        init: duckdb_table_function_init_t,
    );
    pub fn duckdb_table_function_set_local_init(
        table_function: duckdb_table_function,
        init_local: duckdb_table_function_init_t,
    );
    pub fn duckdb_table_function_set_function(
        table_function: duckdb_table_function,
        function: duckdb_table_function_t,
    );
    pub fn duckdb_table_function_supports_projection_pushdown(
        table_function: duckdb_table_function,
        pushdown: bool,
    );
    pub fn duckdb_register_table_function(
        con: duckdb_connection,
        table_function: duckdb_table_function,
    ) -> DuckDBState;

    // Bind info
    pub fn duckdb_bind_get_parameter_count(info: duckdb_bind_info) -> idx_t;
    pub fn duckdb_bind_get_parameter(info: duckdb_bind_info, index: idx_t) -> duckdb_value;
    pub fn duckdb_bind_get_named_parameter(
        info: duckdb_bind_info,
        name: *const c_char,
    ) -> duckdb_value;
    pub fn duckdb_bind_add_result_column(
        info: duckdb_bind_info,
        name: *const c_char,
        type_: duckdb_logical_type,
    );
    pub fn duckdb_bind_set_bind_data(
        info: duckdb_bind_info,
        extra_data: *mut c_void,
        destroy: Option<duckdb_delete_callback_t>,
    );
    pub fn duckdb_bind_get_bind_data(info: duckdb_bind_info) -> *mut c_void;
    pub fn duckdb_bind_set_cardinality(info: duckdb_bind_info, cardinality: idx_t, is_exact: bool);
    pub fn duckdb_bind_set_error(info: duckdb_bind_info, error: *const c_char);

    // Init info
    pub fn duckdb_init_get_bind_data(info: duckdb_init_info) -> *mut c_void;
    pub fn duckdb_init_set_init_data(
        info: duckdb_init_info,
        extra_data: *mut c_void,
        destroy: Option<duckdb_delete_callback_t>,
    );
    pub fn duckdb_init_get_init_data(info: duckdb_init_info) -> *mut c_void;
    pub fn duckdb_init_get_column_count(info: duckdb_init_info) -> idx_t;
    pub fn duckdb_init_get_column_index(info: duckdb_init_info, column_index: idx_t) -> idx_t;
    pub fn duckdb_init_set_error(info: duckdb_init_info, error: *const c_char);

    // Function info
    pub fn duckdb_function_get_bind_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_get_init_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_get_local_init_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_set_error(info: duckdb_function_info, error: *const c_char);
    pub fn duckdb_scalar_function_set_error(info: duckdb_function_info, error: *const c_char);

    // Data chunks & Vectors
    pub fn duckdb_data_chunk_get_column_count(chunk: duckdb_data_chunk) -> idx_t;
    pub fn duckdb_data_chunk_get_vector(chunk: duckdb_data_chunk, col_idx: idx_t) -> duckdb_vector;
    pub fn duckdb_data_chunk_get_size(chunk: duckdb_data_chunk) -> idx_t;
    pub fn duckdb_data_chunk_set_size(chunk: duckdb_data_chunk, size: idx_t);
    pub fn duckdb_vector_get_data(vector: duckdb_vector) -> *mut c_void;
    pub fn duckdb_vector_assign_string_element(
        vector: duckdb_vector,
        index: idx_t,
        str: *const c_char,
    );
    pub fn duckdb_vector_assign_string_element_len(
        vector: duckdb_vector,
        index: idx_t,
        str: *const c_char,
        str_len: idx_t,
    );

    // Validity mask
    pub fn duckdb_vector_get_validity(vector: duckdb_vector) -> *mut u64;
    pub fn duckdb_vector_ensure_validity_writable(vector: duckdb_vector);
    pub fn duckdb_validity_row_is_valid(validity: *mut u64, row: idx_t) -> bool;
    pub fn duckdb_validity_set_row_valid(validity: *mut u64, row: idx_t);
    pub fn duckdb_validity_set_row_invalid(validity: *mut u64, row: idx_t);
    pub fn duckdb_validity_set_row_validity(validity: *mut u64, row: idx_t, valid: bool);

    // Vector size
    pub fn duckdb_vector_size() -> idx_t;

    // Values
    pub fn duckdb_get_varchar(val: duckdb_value) -> *mut c_char;
    pub fn duckdb_get_int64(val: duckdb_value) -> i64;
    pub fn duckdb_get_uint64(val: duckdb_value) -> u64;
    pub fn duckdb_get_double(val: duckdb_value) -> f64;
    pub fn duckdb_get_bool(val: duckdb_value) -> bool;
    pub fn duckdb_free(ptr: *mut c_void);
    pub fn duckdb_destroy_value(val: *mut duckdb_value);

    // Scalar functions
    pub fn duckdb_create_scalar_function() -> duckdb_scalar_function;
    pub fn duckdb_destroy_scalar_function(scalar_function: *mut duckdb_scalar_function);
    pub fn duckdb_scalar_function_set_name(
        scalar_function: duckdb_scalar_function,
        name: *const c_char,
    );
    pub fn duckdb_scalar_function_add_parameter(
        scalar_function: duckdb_scalar_function,
        type_: duckdb_logical_type,
    );
    pub fn duckdb_scalar_function_set_return_type(
        scalar_function: duckdb_scalar_function,
        type_: duckdb_logical_type,
    );
    pub fn duckdb_scalar_function_set_function(
        scalar_function: duckdb_scalar_function,
        function: duckdb_scalar_function_t,
    );
    pub fn duckdb_register_scalar_function(
        con: duckdb_connection,
        scalar_function: duckdb_scalar_function,
    ) -> DuckDBState;

    pub fn duckdb_library_version() -> *const c_char;

    // Queries
    pub fn duckdb_query(
        connection: duckdb_connection,
        query: *const c_char,
        out_result: *mut duckdb_result,
    ) -> DuckDBState;
    pub fn duckdb_destroy_result(result: *mut duckdb_result);
    pub fn duckdb_row_count(result: *mut duckdb_result) -> idx_t;
    pub fn duckdb_column_count(result: *mut duckdb_result) -> idx_t;
    pub fn duckdb_result_error(result: *mut duckdb_result) -> *const c_char;
}

/// Check if a specific row in a validity mask is valid.
/// If `validity` is NULL, all rows in DuckDB are considered valid.
#[inline(always)]
pub unsafe fn duckdb_validity_is_valid(validity: *mut u64, row: idx_t) -> bool {
    if validity.is_null() {
        true
    } else {
        duckdb_validity_row_is_valid(validity, row)
    }
}

/// Mark a specific row in a vector as invalid (NULL).
#[inline(always)]
pub unsafe fn duckdb_vector_set_row_invalid(vector: duckdb_vector, row: idx_t) {
    if vector.is_null() {
        return;
    }
    duckdb_vector_ensure_validity_writable(vector);
    let validity = duckdb_vector_get_validity(vector);
    if !validity.is_null() {
        duckdb_validity_set_row_invalid(validity, row);
    }
}

/// Dynamic lookup or fallback for duckdb_vector_size, allowing safe execution
/// inside DuckDB processes and safe testing outside of DuckDB.
pub fn get_vector_size() -> usize {
    unsafe {
        let sym = crate::ffi::spatial_detect::dlsym_duckdb_symbol(b"duckdb_vector_size\0");
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
        assert_eq!(
            std::mem::size_of::<duckdb_extension_access>(),
            3 * std::mem::size_of::<usize>()
        );
        assert_eq!(core::mem::offset_of!(duckdb_extension_access, set_error), 0);
        assert_eq!(
            core::mem::offset_of!(duckdb_extension_access, get_database),
            std::mem::size_of::<usize>()
        );
        assert_eq!(
            core::mem::offset_of!(duckdb_extension_access, get_api),
            2 * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn test_duckdb_validity_null_pointer_is_valid() {
        // If validity is NULL, all rows are treated as valid
        assert!(unsafe { duckdb_validity_is_valid(std::ptr::null_mut(), 0) });
        assert!(unsafe { duckdb_validity_is_valid(std::ptr::null_mut(), 100) });
    }

    #[test]
    fn test_get_vector_size_fallback() {
        let sz = get_vector_size();
        assert!(sz > 0);
    }

    #[test]
    fn test_duckdb_string_t_memory_layout_and_alignment() {
        assert_eq!(std::mem::size_of::<duckdb_string_t>(), 16);
        assert_eq!(std::mem::size_of::<DuckDbStringInlined>(), 16);
        assert_eq!(std::mem::size_of::<DuckDbStringPointer>(), 16);
        assert_eq!(std::mem::align_of::<duckdb_string_t>(), 8);
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
        assert_eq!(unsafe { s0.as_str() }, "");
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
            assert_eq!(unsafe { s.length() }, len as u32);
            assert_eq!(
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
        assert_eq!(unsafe { s13.length() }, 13);
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
        assert_eq!(unsafe { s15.length() }, 15);
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
        assert_eq!(unsafe { s32.length() }, 32);
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
        assert_eq!(unsafe { s_null.as_str() }, "");

        // 2. Inlined string with invalid UTF-8 bytes (should gracefully return "" without panicking)
        let s_invalid_inline = duckdb_string_t {
            inlined: DuckDbStringInlined {
                length: 3,
                inlined: [0xFF, 0xFE, 0xFD, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            },
        };
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

        unsafe {
            crate::functions::bind_utils::delete_boxed::<DropTracker>(raw_ptr);
        }
        assert!(
            flag.load(Ordering::SeqCst),
            "delete_boxed must invoke Drop and reclaim memory"
        );

        // 2. Verify delete_boxed handles null pointer safely without panic
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
