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
    Enum = 20,
    List = 21,
    Struct = 22,
    Map = 23,
    Array = 24,
    Uuid = 25,
    Union = 26,
    Bit = 27,
    TimeTz = 28,
    TimestampTz = 29,
    UHugeInt = 32,
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

pub type duckdb_table_function_bind_t = unsafe extern "C" fn(info: duckdb_bind_info);
pub type duckdb_table_function_init_t = unsafe extern "C" fn(info: duckdb_init_info);
pub type duckdb_table_function_t = unsafe extern "C" fn(info: duckdb_function_info, output: duckdb_data_chunk);
pub type duckdb_scalar_function_t =
    unsafe extern "C" fn(info: duckdb_function_info, input: duckdb_data_chunk, output: duckdb_vector);
pub type duckdb_delete_callback_t = unsafe extern "C" fn(data: *mut c_void);

extern "C" {
    pub fn duckdb_connect(database: duckdb_database, out_connection: *mut duckdb_connection) -> DuckDBState;
    pub fn duckdb_disconnect(connection: *mut duckdb_connection);

    // Logical types
    pub fn duckdb_create_logical_type(type_: DuckDBType) -> duckdb_logical_type;
    pub fn duckdb_destroy_logical_type(type_: *mut duckdb_logical_type);

    // Table functions
    pub fn duckdb_create_table_function() -> duckdb_table_function;
    pub fn duckdb_destroy_table_function(table_function: *mut duckdb_table_function);
    pub fn duckdb_table_function_set_name(table_function: duckdb_table_function, name: *const c_char);
    pub fn duckdb_table_function_add_parameter(table_function: duckdb_table_function, type_: duckdb_logical_type);
    pub fn duckdb_table_function_add_named_parameter(
        table_function: duckdb_table_function,
        name: *const c_char,
        type_: duckdb_logical_type,
    );
    pub fn duckdb_table_function_set_bind(table_function: duckdb_table_function, bind: duckdb_table_function_bind_t);
    pub fn duckdb_table_function_set_init(table_function: duckdb_table_function, init: duckdb_table_function_init_t);
    pub fn duckdb_table_function_set_function(
        table_function: duckdb_table_function,
        function: duckdb_table_function_t,
    );
    pub fn duckdb_register_table_function(
        con: duckdb_connection,
        table_function: duckdb_table_function,
    ) -> DuckDBState;

    // Bind info
    pub fn duckdb_bind_get_parameter_count(info: duckdb_bind_info) -> idx_t;
    pub fn duckdb_bind_get_parameter(info: duckdb_bind_info, index: idx_t) -> duckdb_value;
    pub fn duckdb_bind_get_named_parameter(info: duckdb_bind_info, name: *const c_char) -> duckdb_value;
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
    pub fn duckdb_bind_set_error(info: duckdb_bind_info, error: *const c_char);

    // Init info
    pub fn duckdb_init_get_bind_data(info: duckdb_init_info) -> *mut c_void;
    pub fn duckdb_init_set_init_data(
        info: duckdb_init_info,
        extra_data: *mut c_void,
        destroy: Option<duckdb_delete_callback_t>,
    );
    pub fn duckdb_init_get_init_data(info: duckdb_init_info) -> *mut c_void;
    pub fn duckdb_init_set_error(info: duckdb_init_info, error: *const c_char);

    // Function info
    pub fn duckdb_function_get_bind_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_get_init_data(info: duckdb_function_info) -> *mut c_void;
    pub fn duckdb_function_set_error(info: duckdb_function_info, error: *const c_char);

    // Data chunks & Vectors
    pub fn duckdb_data_chunk_get_column_count(chunk: duckdb_data_chunk) -> idx_t;
    pub fn duckdb_data_chunk_get_vector(chunk: duckdb_data_chunk, col_idx: idx_t) -> duckdb_vector;
    pub fn duckdb_data_chunk_get_size(chunk: duckdb_data_chunk) -> idx_t;
    pub fn duckdb_data_chunk_set_size(chunk: duckdb_data_chunk, size: idx_t);
    pub fn duckdb_vector_get_data(vector: duckdb_vector) -> *mut c_void;
    pub fn duckdb_vector_assign_string_element(vector: duckdb_vector, index: idx_t, str: *const c_char);
    pub fn duckdb_vector_assign_string_element_len(
        vector: duckdb_vector,
        index: idx_t,
        str: *const c_char,
        str_len: idx_t,
    );

    // Values
    pub fn duckdb_get_varchar(val: duckdb_value) -> *mut c_char;
    pub fn duckdb_get_int64(val: duckdb_value) -> i64;
    pub fn duckdb_get_uint64(val: duckdb_value) -> u64;
    pub fn duckdb_get_double(val: duckdb_value) -> f64;
    pub fn duckdb_free(ptr: *mut c_void);
    pub fn duckdb_destroy_value(val: *mut duckdb_value);

    // Scalar functions
    pub fn duckdb_create_scalar_function() -> duckdb_scalar_function;
    pub fn duckdb_destroy_scalar_function(scalar_function: *mut duckdb_scalar_function);
    pub fn duckdb_scalar_function_set_name(scalar_function: duckdb_scalar_function, name: *const c_char);
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
}
