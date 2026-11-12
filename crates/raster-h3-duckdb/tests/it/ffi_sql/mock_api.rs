//! Mock implementations of the DuckDB C API functions the extension calls,
//! and the API table that points the extension at them.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

use raster_h3_duckdb::ffi::duckdb_c::*;

use super::{
    MockAggregateFunction, MockBindContext, MockConnection, MockDataChunk, MockFuncContext,
    MockInitContext, MockLogicalType, MockScalarFunction, MockTableFunction, MockVal, MockVector,
    RegisteredAggregateFn, RegisteredScalarFn, RegisteredTableFn,
};

#[no_mangle]
pub unsafe extern "C" fn duckdb_connect(
    database: duckdb_database,
    out_connection: *mut duckdb_connection,
) -> DuckDBState {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `out_connection` is a writable out-parameter.
    unsafe {
        if database.is_null() || out_connection.is_null() {
            return DuckDBState::Error;
        }
        let con = Box::new(MockConnection::default());
        *out_connection = Box::into_raw(con) as duckdb_connection;
        DuckDBState::Success
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_disconnect(connection: *mut duckdb_connection) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: a handle freed here came from this mock's own `Box::into_raw` and the
    // out-pointer is nulled so it cannot be freed twice.
    unsafe {
        if !connection.is_null() && !(*connection).is_null() {
            drop(Box::from_raw(*connection as *mut MockConnection));
            *connection = ptr::null_mut();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_create_logical_type(type_: DuckDBType) -> duckdb_logical_type {
    Box::into_raw(Box::new(MockLogicalType {
        ty: type_,
        alias: None,
    })) as duckdb_logical_type
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_create_struct_type(
    _member_types: *mut duckdb_logical_type,
    _member_names: *mut *const c_char,
    _member_count: idx_t,
) -> duckdb_logical_type {
    // SAFETY: test mock of the DuckDB C API; forwards to another mock with the same contract.
    unsafe { duckdb_create_logical_type(DuckDBType::Struct) }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_create_aggregate_function() -> duckdb_aggregate_function {
    Box::into_raw(Box::<MockAggregateFunction>::default()) as duckdb_aggregate_function
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_aggregate_function(
    aggregate_function: *mut duckdb_aggregate_function,
) {
    // SAFETY: test mock of the DuckDB C API: `*aggregate_function` came from
    // `duckdb_create_aggregate_function` and is destroyed once.
    unsafe {
        if !aggregate_function.is_null() && !(*aggregate_function).is_null() {
            drop(Box::from_raw(
                *aggregate_function as *mut MockAggregateFunction,
            ));
            *aggregate_function = ptr::null_mut();
        }
    }
}

/// # Safety
/// `f` must come from `duckdb_create_aggregate_function` and not be aliased.
unsafe fn mock_agg<'a>(f: duckdb_aggregate_function) -> &'a mut MockAggregateFunction {
    // SAFETY: caller contract.
    unsafe { &mut *(f as *mut MockAggregateFunction) }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_aggregate_function_set_name(
    aggregate_function: duckdb_aggregate_function,
    name: *const c_char,
) {
    // SAFETY: test mock; `name` is NUL-terminated and the function is a live mock.
    unsafe {
        mock_agg(aggregate_function).name = CStr::from_ptr(name).to_string_lossy().into_owned()
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_aggregate_function_add_parameter(
    aggregate_function: duckdb_aggregate_function,
    type_: duckdb_logical_type,
) {
    // SAFETY: test mock; `type_` is a live `MockLogicalType`.
    unsafe {
        let ty = (*(type_ as *const MockLogicalType)).ty;
        mock_agg(aggregate_function).params.push(ty);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_aggregate_function_set_return_type(
    aggregate_function: duckdb_aggregate_function,
    type_: duckdb_logical_type,
) {
    // SAFETY: test mock; `type_` is a live `MockLogicalType`.
    unsafe { mock_agg(aggregate_function).ret = Some((*(type_ as *const MockLogicalType)).ty) }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_aggregate_function_set_functions(
    aggregate_function: duckdb_aggregate_function,
    _state_size: duckdb_aggregate_state_size,
    _state_init: duckdb_aggregate_init_t,
    _update: duckdb_aggregate_update_t,
    _combine: duckdb_aggregate_combine_t,
    _finalize: duckdb_aggregate_finalize_t,
) {
    // SAFETY: test mock; the function is a live mock.
    unsafe { mock_agg(aggregate_function).has_functions = true }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_aggregate_function_set_destructor(
    aggregate_function: duckdb_aggregate_function,
    _destroy: duckdb_aggregate_destroy_t,
) {
    // SAFETY: test mock; the function is a live mock.
    unsafe { mock_agg(aggregate_function).has_destructor = true }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_register_aggregate_function(
    con: duckdb_connection,
    aggregate_function: duckdb_aggregate_function,
) -> DuckDBState {
    // SAFETY: test mock; `con` is a live `MockConnection` and the function a live mock.
    unsafe {
        let c = &mut *(con as *mut MockConnection);
        let f = mock_agg(aggregate_function);
        c.aggregate_functions.push(RegisteredAggregateFn {
            name: f.name.clone(),
            params: f.params.clone(),
            ret: f.ret.unwrap_or(DuckDBType::Invalid),
            has_functions: f.has_functions,
            has_destructor: f.has_destructor,
        });
        DuckDBState::Success
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_create_list_type(
    _child: duckdb_logical_type,
) -> duckdb_logical_type {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: forwards its arguments unchanged to another mock with the same contract.
    unsafe { duckdb_create_logical_type(DuckDBType::List) }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_logical_type(type_: *mut duckdb_logical_type) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: a handle freed here came from this mock's own `Box::into_raw` and the
    // out-pointer is nulled so it cannot be freed twice.
    unsafe {
        if !type_.is_null() && !(*type_).is_null() {
            drop(Box::from_raw(*type_ as *mut MockLogicalType));
            *type_ = ptr::null_mut();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_logical_type_set_alias(
    type_: duckdb_logical_type,
    alias: *const c_char,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `type_` points to a live `MockLogicalType` created by this mock or by the
    // test, and no other reference to it is active during the call; C strings passed in are
    // NUL-terminated, as the C API requires.
    unsafe {
        if !type_.is_null() && !alias.is_null() {
            let lt = &mut *(type_ as *mut MockLogicalType);
            lt.alias = Some(CStr::from_ptr(alias).to_string_lossy().into_owned());
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_logical_type_get_alias(type_: duckdb_logical_type) -> *mut c_char {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `type_` points to a live `MockLogicalType` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        if type_.is_null() {
            return ptr::null_mut();
        }
        let lt = &*(type_ as *const MockLogicalType);
        match &lt.alias {
            Some(a) => CString::new(a.as_str()).unwrap().into_raw(),
            None => ptr::null_mut(),
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_create_table_function() -> duckdb_table_function {
    Box::into_raw(Box::new(MockTableFunction {
        name: String::new(),
        positional_params: Vec::new(),
        named_params: HashMap::new(),
        projection_pushdown: false,
    })) as duckdb_table_function
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_table_function(table_function: *mut duckdb_table_function) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: a handle freed here came from this mock's own `Box::into_raw` and the
    // out-pointer is nulled so it cannot be freed twice.
    unsafe {
        if !table_function.is_null() && !(*table_function).is_null() {
            drop(Box::from_raw(*table_function as *mut MockTableFunction));
            *table_function = ptr::null_mut();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_set_name(
    table_function: duckdb_table_function,
    name: *const c_char,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `table_function` points to a live `MockTableFunction` created by this
    // mock or by the test, and no other reference to it is active during the call; C
    // strings passed in are NUL-terminated, as the C API requires.
    unsafe {
        let tf = &mut *(table_function as *mut MockTableFunction);
        tf.name = CStr::from_ptr(name).to_string_lossy().into_owned();
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_add_parameter(
    table_function: duckdb_table_function,
    type_: duckdb_logical_type,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `table_function` points to a live `MockTableFunction`, `type_` points to
    // a live `MockLogicalType` created by this mock or by the test, and no other reference
    // to it is active during the call.
    unsafe {
        let tf = &mut *(table_function as *mut MockTableFunction);
        let lt = &*(type_ as *const MockLogicalType);
        tf.positional_params.push(lt.ty);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_add_named_parameter(
    table_function: duckdb_table_function,
    name: *const c_char,
    type_: duckdb_logical_type,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `table_function` points to a live `MockTableFunction`, `type_` points to
    // a live `MockLogicalType` created by this mock or by the test, and no other reference
    // to it is active during the call; C strings passed in are NUL-terminated, as the C API
    // requires.
    unsafe {
        let tf = &mut *(table_function as *mut MockTableFunction);
        let lt = &*(type_ as *const MockLogicalType);
        let param_name = CStr::from_ptr(name).to_string_lossy().into_owned();
        tf.named_params.insert(param_name, lt.ty);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_set_bind(
    _table_function: duckdb_table_function,
    _bind: duckdb_table_function_bind_t,
) {
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_set_init(
    _table_function: duckdb_table_function,
    _init: duckdb_table_function_init_t,
) {
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_set_local_init(
    _table_function: duckdb_table_function,
    _init_local: duckdb_table_function_init_t,
) {
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_set_function(
    _table_function: duckdb_table_function,
    _function: duckdb_table_function_t,
) {
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_supports_projection_pushdown(
    table_function: duckdb_table_function,
    pushdown: bool,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `table_function` points to a live `MockTableFunction` created by this
    // mock or by the test, and no other reference to it is active during the call.
    unsafe {
        let tf = &mut *(table_function as *mut MockTableFunction);
        tf.projection_pushdown = pushdown;
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_register_table_function(
    con: duckdb_connection,
    table_function: duckdb_table_function,
) -> DuckDBState {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `con` points to a live `MockConnection`, `table_function` points to a
    // live `MockTableFunction` created by this mock or by the test, and no other reference
    // to it is active during the call.
    unsafe {
        if con.is_null() || table_function.is_null() {
            return DuckDBState::Error;
        }
        let c = &mut *(con as *mut MockConnection);
        let tf = &*(table_function as *const MockTableFunction);
        c.table_functions.push(RegisteredTableFn {
            name: tf.name.clone(),
            positional_params: tf.positional_params.clone(),
            named_params: tf.named_params.clone(),
            projection_pushdown: tf.projection_pushdown,
        });
        DuckDBState::Success
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_get_parameter_count(info: duckdb_bind_info) -> idx_t {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockBindContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let b = &*(info as *const MockBindContext);
        b.positional.len() as idx_t
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_get_parameter(
    info: duckdb_bind_info,
    index: idx_t,
) -> duckdb_value {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockBindContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let b = &*(info as *const MockBindContext);
        match b.positional.get(index as usize) {
            Some(v) => Box::into_raw(Box::new(v.clone())) as duckdb_value,
            None => ptr::null_mut(),
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_get_named_parameter(
    info: duckdb_bind_info,
    name: *const c_char,
) -> duckdb_value {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockBindContext` created by this mock or by the
    // test, and no other reference to it is active during the call; C strings passed in are
    // NUL-terminated, as the C API requires.
    unsafe {
        let b = &*(info as *const MockBindContext);
        let key = CStr::from_ptr(name).to_string_lossy();
        match b.named.get(key.as_ref()) {
            Some(v) => Box::into_raw(Box::new(v.clone())) as duckdb_value,
            None => ptr::null_mut(),
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_add_result_column(
    info: duckdb_bind_info,
    name: *const c_char,
    type_: duckdb_logical_type,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockBindContext`, `type_` points to a live
    // `MockLogicalType` created by this mock or by the test, and no other reference to it
    // is active during the call; C strings passed in are NUL-terminated, as the C API
    // requires.
    unsafe {
        let b = &mut *(info as *mut MockBindContext);
        let col_name = CStr::from_ptr(name).to_string_lossy().into_owned();
        let lt = &*(type_ as *const MockLogicalType);
        b.result_columns.push((col_name, lt.ty, lt.alias.clone()));
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_set_bind_data(
    info: duckdb_bind_info,
    extra_data: *mut c_void,
    destroy: Option<duckdb_delete_callback_t>,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockBindContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let b = &mut *(info as *mut MockBindContext);
        b.bind_data = extra_data;
        b.bind_destroy = destroy;
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_set_cardinality(
    info: duckdb_bind_info,
    cardinality: idx_t,
    is_exact: bool,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockBindContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let b = &mut *(info as *mut MockBindContext);
        b.cardinality = (cardinality, is_exact);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_set_error(info: duckdb_bind_info, error: *const c_char) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockBindContext` created by this mock or by the
    // test, and no other reference to it is active during the call; C strings passed in are
    // NUL-terminated, as the C API requires.
    unsafe {
        let b = &mut *(info as *mut MockBindContext);
        b.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_get_bind_data(info: duckdb_init_info) -> *mut c_void {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockInitContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let i = &*(info as *const MockInitContext);
        i.bind_data
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_set_init_data(
    info: duckdb_init_info,
    extra_data: *mut c_void,
    destroy: Option<duckdb_delete_callback_t>,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockInitContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let i = &mut *(info as *mut MockInitContext);
        i.init_data = extra_data;
        i.init_destroy = destroy;
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_get_column_count(info: duckdb_init_info) -> idx_t {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockInitContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let i = &*(info as *const MockInitContext);
        i.projected_columns.len() as idx_t
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_get_column_index(
    info: duckdb_init_info,
    column_index: idx_t,
) -> idx_t {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockInitContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let i = &*(info as *const MockInitContext);
        i.projected_columns[column_index as usize] as idx_t
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_set_error(info: duckdb_init_info, error: *const c_char) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockInitContext` created by this mock or by the
    // test, and no other reference to it is active during the call; C strings passed in are
    // NUL-terminated, as the C API requires.
    unsafe {
        let i = &mut *(info as *mut MockInitContext);
        i.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_get_bind_data(info: duckdb_function_info) -> *mut c_void {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockFuncContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let f = &*(info as *const MockFuncContext);
        f.bind_data
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_get_init_data(info: duckdb_function_info) -> *mut c_void {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockFuncContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let f = &*(info as *const MockFuncContext);
        f.init_data
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_get_local_init_data(
    info: duckdb_function_info,
) -> *mut c_void {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockFuncContext` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let f = &*(info as *const MockFuncContext);
        f.local_init_data
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_set_error(
    info: duckdb_function_info,
    error: *const c_char,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockFuncContext` created by this mock or by the
    // test, and no other reference to it is active during the call; C strings passed in are
    // NUL-terminated, as the C API requires.
    unsafe {
        let f = &mut *(info as *mut MockFuncContext);
        f.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_set_error(
    info: duckdb_function_info,
    error: *const c_char,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `info` points to a live `MockFuncContext` created by this mock or by the
    // test, and no other reference to it is active during the call; C strings passed in are
    // NUL-terminated, as the C API requires.
    unsafe {
        if !info.is_null() {
            let f = &mut *(info as *mut MockFuncContext);
            f.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_get_column_count(chunk: duckdb_data_chunk) -> idx_t {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `chunk` points to a live `MockDataChunk` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let c = &*(chunk as *const MockDataChunk);
        c.vectors.len() as idx_t
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_get_vector(
    chunk: duckdb_data_chunk,
    col_idx: idx_t,
) -> duckdb_vector {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `chunk` points to a live `MockDataChunk` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let c = &mut *(chunk as *mut MockDataChunk);
        c.vectors
            .get_mut(col_idx as usize)
            .map(|v| v as *mut MockVector as duckdb_vector)
            .unwrap_or(ptr::null_mut())
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_get_size(chunk: duckdb_data_chunk) -> idx_t {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `chunk` points to a live `MockDataChunk` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let c = &*(chunk as *const MockDataChunk);
        c.size as idx_t
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_set_size(chunk: duckdb_data_chunk, size: idx_t) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `chunk` points to a live `MockDataChunk` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        if !chunk.is_null() {
            let c = &mut *(chunk as *mut MockDataChunk);
            c.size = size as usize;
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_get_data(vector: duckdb_vector) -> *mut c_void {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `vector` points to a live `MockVector` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        if vector.is_null() {
            return ptr::null_mut();
        }
        let v = &mut *(vector as *mut MockVector);
        v.data.as_mut_ptr() as *mut c_void
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_assign_string_element(
    vector: duckdb_vector,
    index: idx_t,
    str_ptr: *const c_char,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: C strings passed in are NUL-terminated, as the C API requires.
    unsafe {
        let bytes = CStr::from_ptr(str_ptr).to_bytes();
        duckdb_vector_assign_string_element_len(vector, index, str_ptr, bytes.len() as idx_t);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_assign_string_element_len(
    vector: duckdb_vector,
    index: idx_t,
    str_ptr: *const c_char,
    str_len: idx_t,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `vector` points to a live `MockVector` created by this mock or by the
    // test, and no other reference to it is active during the call; `str_ptr` addresses
    // `str_len` readable bytes, as the C API requires.
    unsafe {
        let v = &mut *(vector as *mut MockVector);
        let bytes = std::slice::from_raw_parts(str_ptr as *const u8, str_len as usize);
        v.strings[index as usize] = bytes.to_vec();
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_get_validity(vector: duckdb_vector) -> *mut u64 {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `vector` points to a live `MockVector` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let v = &mut *(vector as *mut MockVector);
        if v.has_validity {
            v.validity.as_mut_ptr()
        } else {
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_ensure_validity_writable(vector: duckdb_vector) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `vector` points to a live `MockVector` created by this mock or by the
    // test, and no other reference to it is active during the call.
    unsafe {
        let v = &mut *(vector as *mut MockVector);
        v.has_validity = true;
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_row_is_valid(validity: *mut u64, row: idx_t) -> bool {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `validity` is a mock mask sized for the vector capacity and `row` is
    // below that capacity.
    unsafe {
        if validity.is_null() {
            return true;
        }
        let entry = (row / 64) as usize;
        let bit = row % 64;
        (*validity.add(entry) & (1u64 << bit)) != 0
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_set_row_valid(validity: *mut u64, row: idx_t) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `validity` is a mock mask sized for the vector capacity and `row` is
    // below that capacity.
    unsafe {
        if !validity.is_null() {
            let entry = (row / 64) as usize;
            let bit = row % 64;
            *validity.add(entry) |= 1u64 << bit;
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_set_row_invalid(validity: *mut u64, row: idx_t) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `validity` is a mock mask sized for the vector capacity and `row` is
    // below that capacity.
    unsafe {
        if !validity.is_null() {
            let entry = (row / 64) as usize;
            let bit = row % 64;
            *validity.add(entry) &= !(1u64 << bit);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_set_row_validity(
    validity: *mut u64,
    row: idx_t,
    valid: bool,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: forwards its arguments unchanged to another mock with the same contract.
    unsafe {
        if valid {
            duckdb_validity_set_row_valid(validity, row);
        } else {
            duckdb_validity_set_row_invalid(validity, row);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_size() -> idx_t {
    2048
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_varchar(val: duckdb_value) -> *mut c_char {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `val` points to a live `MockVal` created by this mock or by the test, and
    // no other reference to it is active during the call.
    unsafe {
        let v = &*(val as *const MockVal);
        match v {
            MockVal::Str(s) => CString::new(s.as_str()).unwrap().into_raw(),
            _ => ptr::null_mut(),
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_int64(val: duckdb_value) -> i64 {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `val` points to a live `MockVal` created by this mock or by the test, and
    // no other reference to it is active during the call.
    unsafe {
        match &*(val as *const MockVal) {
            MockVal::Int(i) => *i,
            MockVal::UInt(u) => *u as i64,
            _ => 0,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_uint64(val: duckdb_value) -> u64 {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `val` points to a live `MockVal` created by this mock or by the test, and
    // no other reference to it is active during the call.
    unsafe {
        match &*(val as *const MockVal) {
            MockVal::UInt(u) => *u,
            MockVal::Int(i) => *i as u64,
            _ => 0,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_double(val: duckdb_value) -> f64 {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `val` points to a live `MockVal` created by this mock or by the test, and
    // no other reference to it is active during the call.
    unsafe {
        match &*(val as *const MockVal) {
            MockVal::Double(d) => *d,
            MockVal::Int(i) => *i as f64,
            _ => 0.0,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_bool(val: duckdb_value) -> bool {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `val` points to a live `MockVal` created by this mock or by the test, and
    // no other reference to it is active during the call.
    unsafe {
        match &*(val as *const MockVal) {
            MockVal::Bool(b) => *b,
            _ => false,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_free(ptr: *mut c_void) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: every string the mock hands out (`duckdb_get_varchar`,
    // `duckdb_logical_type_get_alias`) comes from `CString::into_raw`, so this is the
    // matching deallocator.
    unsafe {
        if !ptr.is_null() {
            drop(CString::from_raw(ptr as *mut c_char));
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_value(val: *mut duckdb_value) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: a handle freed here came from this mock's own `Box::into_raw` and the
    // out-pointer is nulled so it cannot be freed twice.
    unsafe {
        if !val.is_null() && !(*val).is_null() {
            drop(Box::from_raw(*val as *mut MockVal));
            *val = ptr::null_mut();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_create_scalar_function() -> duckdb_scalar_function {
    Box::into_raw(Box::new(MockScalarFunction {
        name: String::new(),
        params: Vec::new(),
        ret: DuckDBType::Invalid,
    })) as duckdb_scalar_function
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_scalar_function(
    scalar_function: *mut duckdb_scalar_function,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: a handle freed here came from this mock's own `Box::into_raw` and the
    // out-pointer is nulled so it cannot be freed twice.
    unsafe {
        if !scalar_function.is_null() && !(*scalar_function).is_null() {
            drop(Box::from_raw(*scalar_function as *mut MockScalarFunction));
            *scalar_function = ptr::null_mut();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_set_name(
    scalar_function: duckdb_scalar_function,
    name: *const c_char,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `scalar_function` points to a live `MockScalarFunction` created by this
    // mock or by the test, and no other reference to it is active during the call; C
    // strings passed in are NUL-terminated, as the C API requires.
    unsafe {
        let sf = &mut *(scalar_function as *mut MockScalarFunction);
        sf.name = CStr::from_ptr(name).to_string_lossy().into_owned();
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_add_parameter(
    scalar_function: duckdb_scalar_function,
    type_: duckdb_logical_type,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `scalar_function` points to a live `MockScalarFunction`, `type_` points
    // to a live `MockLogicalType` created by this mock or by the test, and no other
    // reference to it is active during the call.
    unsafe {
        let sf = &mut *(scalar_function as *mut MockScalarFunction);
        let lt = &*(type_ as *const MockLogicalType);
        sf.params.push(lt.ty);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_set_return_type(
    scalar_function: duckdb_scalar_function,
    type_: duckdb_logical_type,
) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `scalar_function` points to a live `MockScalarFunction`, `type_` points
    // to a live `MockLogicalType` created by this mock or by the test, and no other
    // reference to it is active during the call.
    unsafe {
        let sf = &mut *(scalar_function as *mut MockScalarFunction);
        let lt = &*(type_ as *const MockLogicalType);
        sf.ret = lt.ty;
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_set_function(
    _scalar_function: duckdb_scalar_function,
    _function: duckdb_scalar_function_t,
) {
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_register_scalar_function(
    con: duckdb_connection,
    scalar_function: duckdb_scalar_function,
) -> DuckDBState {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `con` points to a live `MockConnection`, `scalar_function` points to a
    // live `MockScalarFunction` created by this mock or by the test, and no other reference
    // to it is active during the call.
    unsafe {
        let c = &mut *(con as *mut MockConnection);
        let sf = &*(scalar_function as *const MockScalarFunction);
        c.scalar_functions.push(RegisteredScalarFn {
            name: sf.name.clone(),
            params: sf.params.clone(),
            ret: sf.ret,
        });
        DuckDBState::Success
    }
}

struct MockScalarFunctionSet {
    name: String,
    functions: Vec<RegisteredScalarFn>,
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_create_scalar_function_set(
    name: *const c_char,
) -> duckdb_scalar_function_set {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: C strings passed in are NUL-terminated, as the C API requires.
    unsafe {
        Box::into_raw(Box::new(MockScalarFunctionSet {
            name: CStr::from_ptr(name).to_string_lossy().into_owned(),
            functions: Vec::new(),
        })) as duckdb_scalar_function_set
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_scalar_function_set(set: *mut duckdb_scalar_function_set) {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: a handle freed here came from this mock's own `Box::into_raw` and the
    // out-pointer is nulled so it cannot be freed twice.
    unsafe {
        if !set.is_null() && !(*set).is_null() {
            drop(Box::from_raw(*set as *mut MockScalarFunctionSet));
            *set = ptr::null_mut();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_add_scalar_function_to_set(
    set: duckdb_scalar_function_set,
    function: duckdb_scalar_function,
) -> DuckDBState {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `function` points to a live `MockScalarFunction`, `set` points to a live
    // `MockScalarFunctionSet` created by this mock or by the test, and no other reference
    // to it is active during the call.
    unsafe {
        let set = &mut *(set as *mut MockScalarFunctionSet);
        let sf = &*(function as *const MockScalarFunction);
        set.functions.push(RegisteredScalarFn {
            name: set.name.clone(),
            params: sf.params.clone(),
            ret: sf.ret,
        });
        DuckDBState::Success
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_register_scalar_function_set(
    con: duckdb_connection,
    set: duckdb_scalar_function_set,
) -> DuckDBState {
    // SAFETY: test mock of the DuckDB C API, driven single-threaded by the extension code
    // under test: `con` points to a live `MockConnection`, `set` points to a live
    // `MockScalarFunctionSet` created by this mock or by the test, and no other reference
    // to it is active during the call.
    unsafe {
        let c = &mut *(con as *mut MockConnection);
        let set = &*(set as *const MockScalarFunctionSet);
        // Mirrors DuckDB < 1.5, which rejects a second registration under an existing name.
        if c.scalar_functions.iter().any(|f| f.name == set.name) {
            return DuckDBState::Error;
        }
        c.scalar_functions
            .extend(set.functions.iter().map(|f| RegisteredScalarFn {
                name: f.name.clone(),
                params: f.params.clone(),
                ret: f.ret,
            }));
        DuckDBState::Success
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_library_version() -> *const c_char {
    c"v1.2.0".as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_query(
    _connection: duckdb_connection,
    _query: *const c_char,
    _out_result: *mut duckdb_result,
) -> DuckDBState {
    DuckDBState::Success
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_result(_result: *mut duckdb_result) {}

#[no_mangle]
pub unsafe extern "C" fn duckdb_fetch_chunk(_result: duckdb_result) -> duckdb_data_chunk {
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_data_chunk(_chunk: *mut duckdb_data_chunk) {}

#[no_mangle]
pub unsafe extern "C" fn duckdb_column_count(_result: *mut duckdb_result) -> idx_t {
    0
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_result_error(_result: *mut duckdb_result) -> *const c_char {
    ptr::null()
}

/// Points the extension's DuckDB C API table at the mocks above.
pub(super) fn install_mock_api() {
    static TABLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let table = *TABLE.get_or_init(|| {
        let mut table = vec![ptr::null::<c_void>(); API_TABLE_LEN];
        macro_rules! mock {
            ($($name:ident,)*) => {
                $(table[api_index::$name] = $name as *const c_void;)*
            };
        }
        mock! {
        duckdb_bind_add_result_column,
        duckdb_bind_get_named_parameter,
        duckdb_bind_get_parameter,
        duckdb_bind_get_parameter_count,
        duckdb_bind_set_bind_data,
        duckdb_bind_set_cardinality,
        duckdb_bind_set_error,
        duckdb_column_count,
        duckdb_connect,
        duckdb_create_list_type,
        duckdb_create_logical_type,
        duckdb_create_struct_type,
        duckdb_create_aggregate_function,
        duckdb_destroy_aggregate_function,
        duckdb_aggregate_function_set_name,
        duckdb_aggregate_function_add_parameter,
        duckdb_aggregate_function_set_return_type,
        duckdb_aggregate_function_set_functions,
        duckdb_aggregate_function_set_destructor,
        duckdb_register_aggregate_function,
        duckdb_add_scalar_function_to_set,
        duckdb_create_scalar_function,
        duckdb_create_scalar_function_set,
        duckdb_create_table_function,
        duckdb_data_chunk_get_column_count,
        duckdb_data_chunk_get_size,
        duckdb_data_chunk_get_vector,
        duckdb_data_chunk_set_size,
        duckdb_destroy_data_chunk,
        duckdb_destroy_logical_type,
        duckdb_destroy_result,
        duckdb_destroy_scalar_function,
        duckdb_destroy_scalar_function_set,
        duckdb_destroy_table_function,
        duckdb_destroy_value,
        duckdb_disconnect,
        duckdb_fetch_chunk,
        duckdb_free,
        duckdb_function_get_bind_data,
        duckdb_function_get_init_data,
        duckdb_function_get_local_init_data,
        duckdb_function_set_error,
        duckdb_get_bool,
        duckdb_get_double,
        duckdb_get_int64,
        duckdb_get_uint64,
        duckdb_get_varchar,
        duckdb_init_get_bind_data,
        duckdb_init_get_column_count,
        duckdb_init_get_column_index,
        duckdb_init_set_error,
        duckdb_init_set_init_data,
        duckdb_library_version,
        duckdb_logical_type_get_alias,
        duckdb_logical_type_set_alias,
        duckdb_query,
        duckdb_register_scalar_function,
        duckdb_register_scalar_function_set,
        duckdb_register_table_function,
        duckdb_result_error,
        duckdb_scalar_function_add_parameter,
        duckdb_scalar_function_set_error,
        duckdb_scalar_function_set_function,
        duckdb_scalar_function_set_name,
        duckdb_scalar_function_set_return_type,
        duckdb_table_function_add_named_parameter,
        duckdb_table_function_add_parameter,
        duckdb_table_function_set_bind,
        duckdb_table_function_set_function,
        duckdb_table_function_set_init,
        duckdb_table_function_set_local_init,
        duckdb_table_function_set_name,
        duckdb_table_function_supports_projection_pushdown,
        duckdb_validity_row_is_valid,
        duckdb_validity_set_row_invalid,
        duckdb_validity_set_row_valid,
        duckdb_validity_set_row_validity,
        duckdb_vector_assign_string_element,
        duckdb_vector_assign_string_element_len,
        duckdb_vector_ensure_validity_writable,
        duckdb_vector_get_data,
        duckdb_vector_get_validity,
        duckdb_vector_size,
        }
        Box::leak(table.into_boxed_slice()).as_ptr() as usize
    });
    // SAFETY: `table` is a leaked (never freed) array of `API_TABLE_LEN` pointers whose
    // filled slots hold mocks with exactly the signatures of those slots.
    unsafe { install_api_table(table as *const c_void) };
}
