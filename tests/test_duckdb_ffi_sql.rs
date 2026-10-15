//! Mock DuckDB C-FFI callback tests for extension initialization,
//! scalar functions, continuous & categorical table functions (Wide and Long),
//! projection pushdown, and direct Parquet / PMTiles export table functions.
//! These do not execute SQL; real-engine coverage lives in test_duckdb_sql_smoke.rs.

#![cfg(unix)]
#![allow(clippy::missing_safety_doc)]

mod helpers;

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

use helpers::TestGeoTiffBuilder;
use raster_h3::ffi::duckdb_c::*;
use raster_h3::functions::categorical_table_function::{
    raster_h3_categorical_bind, raster_h3_categorical_init, raster_h3_categorical_init_local,
    raster_h3_categorical_scan,
};
use raster_h3::functions::parquet_table_function::{parquet_bind, parquet_init, parquet_scan};
use raster_h3::functions::pmtiles_table_function::{
    parquet_pmtiles_bind, parquet_pmtiles_init, parquet_pmtiles_scan, pmtiles_bind, pmtiles_init,
    pmtiles_scan,
};
use raster_h3::functions::scalar::*;
use raster_h3::functions::table_function::{
    raster_h3_bind, raster_h3_init, raster_h3_init_local, raster_h3_scan,
};

// =============================================================================
// In-Process DuckDB C-API Shim for Full FFI Callback Execution
// =============================================================================

#[derive(Clone, Debug)]
struct MockLogicalType {
    ty: DuckDBType,
    alias: Option<String>,
}

#[derive(Clone, Debug)]
enum MockVal {
    Str(String),
    Int(i64),
    UInt(u64),
    Double(f64),
    Bool(bool),
}

struct RegisteredTableFn {
    name: String,
    positional_params: Vec<DuckDBType>,
    named_params: HashMap<String, DuckDBType>,
    projection_pushdown: bool,
}

struct RegisteredScalarFn {
    name: String,
    params: Vec<DuckDBType>,
    ret: DuckDBType,
}

#[derive(Default)]
struct MockConnection {
    table_functions: Vec<RegisteredTableFn>,
    scalar_functions: Vec<RegisteredScalarFn>,
}

struct MockTableFunction {
    name: String,
    positional_params: Vec<DuckDBType>,
    named_params: HashMap<String, DuckDBType>,
    projection_pushdown: bool,
}

struct MockScalarFunction {
    name: String,
    params: Vec<DuckDBType>,
    ret: DuckDBType,
}

struct MockBindContext {
    positional: Vec<MockVal>,
    named: HashMap<String, MockVal>,
    result_columns: Vec<(String, DuckDBType, Option<String>)>,
    bind_data: *mut c_void,
    bind_destroy: Option<duckdb_delete_callback_t>,
    cardinality: (idx_t, bool),
    error: Option<String>,
}

impl MockBindContext {
    fn new(positional: Vec<MockVal>, named: HashMap<String, MockVal>) -> Self {
        Self {
            positional,
            named,
            result_columns: Vec::new(),
            bind_data: ptr::null_mut(),
            bind_destroy: None,
            cardinality: (0, false),
            error: None,
        }
    }
}

impl Drop for MockBindContext {
    fn drop(&mut self) {
        if !self.bind_data.is_null() {
            if let Some(dtor) = self.bind_destroy {
                unsafe { dtor(self.bind_data) };
            }
            self.bind_data = ptr::null_mut();
        }
    }
}

struct MockInitContext {
    bind_data: *mut c_void,
    init_data: *mut c_void,
    init_destroy: Option<duckdb_delete_callback_t>,
    projected_columns: Vec<usize>,
    error: Option<String>,
}

impl MockInitContext {
    fn new(bind_data: *mut c_void, projected_columns: Vec<usize>) -> Self {
        Self {
            bind_data,
            init_data: ptr::null_mut(),
            init_destroy: None,
            projected_columns,
            error: None,
        }
    }
}

impl Drop for MockInitContext {
    fn drop(&mut self) {
        if !self.init_data.is_null() {
            if let Some(dtor) = self.init_destroy {
                unsafe { dtor(self.init_data) };
            }
            self.init_data = ptr::null_mut();
        }
    }
}

struct MockFuncContext {
    bind_data: *mut c_void,
    init_data: *mut c_void,
    local_init_data: *mut c_void,
    error: Option<String>,
}

struct MockVector {
    data: Vec<u64>, // 8-byte aligned raw storage (2048 rows * 16 bytes max per element)
    strings: Vec<Vec<u8>>,
    validity: Vec<u64>,
    has_validity: bool,
}

impl MockVector {
    fn new(capacity: usize) -> Self {
        Self {
            data: vec![0u64; capacity * 2],
            strings: vec![Vec::new(); capacity],
            validity: vec![u64::MAX; capacity.div_ceil(64)],
            has_validity: false,
        }
    }

    fn set_str_input(&mut self, row: usize, s: &str) {
        let bytes = s.as_bytes();
        self.strings[row] = bytes.to_vec();
        let slot = unsafe { &mut *(self.data.as_mut_ptr() as *mut duckdb_string_t).add(row) };
        if bytes.len() <= 12 {
            let mut inlined = [0u8; 12];
            inlined[..bytes.len()].copy_from_slice(bytes);
            slot.inlined = DuckDbStringInlined {
                length: bytes.len() as u32,
                inlined,
            };
        } else {
            let mut prefix = [0u8; 4];
            prefix.copy_from_slice(&bytes[..4]);
            slot.pointer = DuckDbStringPointer {
                length: bytes.len() as u32,
                prefix,
                ptr: self.strings[row].as_ptr() as *const c_char,
            };
        }
    }

    fn is_valid(&self, row: usize) -> bool {
        if !self.has_validity {
            return true;
        }
        let entry = row / 64;
        let bit = row % 64;
        (self.validity[entry] & (1u64 << bit)) != 0
    }
}

struct MockDataChunk {
    vectors: Vec<MockVector>,
    size: usize,
}

impl MockDataChunk {
    fn new(num_cols: usize, capacity: usize) -> Self {
        Self {
            vectors: (0..num_cols).map(|_| MockVector::new(capacity)).collect(),
            size: 0,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_connect(
    database: duckdb_database,
    out_connection: *mut duckdb_connection,
) -> DuckDBState {
    if database.is_null() || out_connection.is_null() {
        return DuckDBState::Error;
    }
    let con = Box::new(MockConnection::default());
    *out_connection = Box::into_raw(con) as duckdb_connection;
    DuckDBState::Success
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_disconnect(connection: *mut duckdb_connection) {
    if !connection.is_null() && !(*connection).is_null() {
        drop(Box::from_raw(*connection as *mut MockConnection));
        *connection = ptr::null_mut();
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
pub unsafe extern "C" fn duckdb_destroy_logical_type(type_: *mut duckdb_logical_type) {
    if !type_.is_null() && !(*type_).is_null() {
        drop(Box::from_raw(*type_ as *mut MockLogicalType));
        *type_ = ptr::null_mut();
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_logical_type_set_alias(
    type_: duckdb_logical_type,
    alias: *const c_char,
) {
    if !type_.is_null() && !alias.is_null() {
        let lt = &mut *(type_ as *mut MockLogicalType);
        lt.alias = Some(CStr::from_ptr(alias).to_string_lossy().into_owned());
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_logical_type_get_alias(type_: duckdb_logical_type) -> *mut c_char {
    if type_.is_null() {
        return ptr::null_mut();
    }
    let lt = &*(type_ as *const MockLogicalType);
    match &lt.alias {
        Some(a) => CString::new(a.as_str()).unwrap().into_raw(),
        None => ptr::null_mut(),
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
    if !table_function.is_null() && !(*table_function).is_null() {
        drop(Box::from_raw(*table_function as *mut MockTableFunction));
        *table_function = ptr::null_mut();
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_set_name(
    table_function: duckdb_table_function,
    name: *const c_char,
) {
    let tf = &mut *(table_function as *mut MockTableFunction);
    tf.name = CStr::from_ptr(name).to_string_lossy().into_owned();
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_add_parameter(
    table_function: duckdb_table_function,
    type_: duckdb_logical_type,
) {
    let tf = &mut *(table_function as *mut MockTableFunction);
    let lt = &*(type_ as *const MockLogicalType);
    tf.positional_params.push(lt.ty);
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_table_function_add_named_parameter(
    table_function: duckdb_table_function,
    name: *const c_char,
    type_: duckdb_logical_type,
) {
    let tf = &mut *(table_function as *mut MockTableFunction);
    let lt = &*(type_ as *const MockLogicalType);
    let param_name = CStr::from_ptr(name).to_string_lossy().into_owned();
    tf.named_params.insert(param_name, lt.ty);
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
    let tf = &mut *(table_function as *mut MockTableFunction);
    tf.projection_pushdown = pushdown;
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_register_table_function(
    con: duckdb_connection,
    table_function: duckdb_table_function,
) -> DuckDBState {
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

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_get_parameter_count(info: duckdb_bind_info) -> idx_t {
    let b = &*(info as *const MockBindContext);
    b.positional.len() as idx_t
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_get_parameter(
    info: duckdb_bind_info,
    index: idx_t,
) -> duckdb_value {
    let b = &*(info as *const MockBindContext);
    match b.positional.get(index as usize) {
        Some(v) => Box::into_raw(Box::new(v.clone())) as duckdb_value,
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_get_named_parameter(
    info: duckdb_bind_info,
    name: *const c_char,
) -> duckdb_value {
    let b = &*(info as *const MockBindContext);
    let key = CStr::from_ptr(name).to_string_lossy();
    match b.named.get(key.as_ref()) {
        Some(v) => Box::into_raw(Box::new(v.clone())) as duckdb_value,
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_add_result_column(
    info: duckdb_bind_info,
    name: *const c_char,
    type_: duckdb_logical_type,
) {
    let b = &mut *(info as *mut MockBindContext);
    let col_name = CStr::from_ptr(name).to_string_lossy().into_owned();
    let lt = &*(type_ as *const MockLogicalType);
    b.result_columns.push((col_name, lt.ty, lt.alias.clone()));
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_set_bind_data(
    info: duckdb_bind_info,
    extra_data: *mut c_void,
    destroy: Option<duckdb_delete_callback_t>,
) {
    let b = &mut *(info as *mut MockBindContext);
    b.bind_data = extra_data;
    b.bind_destroy = destroy;
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_set_cardinality(
    info: duckdb_bind_info,
    cardinality: idx_t,
    is_exact: bool,
) {
    let b = &mut *(info as *mut MockBindContext);
    b.cardinality = (cardinality, is_exact);
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_bind_set_error(info: duckdb_bind_info, error: *const c_char) {
    let b = &mut *(info as *mut MockBindContext);
    b.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_get_bind_data(info: duckdb_init_info) -> *mut c_void {
    let i = &*(info as *const MockInitContext);
    i.bind_data
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_set_init_data(
    info: duckdb_init_info,
    extra_data: *mut c_void,
    destroy: Option<duckdb_delete_callback_t>,
) {
    let i = &mut *(info as *mut MockInitContext);
    i.init_data = extra_data;
    i.init_destroy = destroy;
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_get_column_count(info: duckdb_init_info) -> idx_t {
    let i = &*(info as *const MockInitContext);
    i.projected_columns.len() as idx_t
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_get_column_index(
    info: duckdb_init_info,
    column_index: idx_t,
) -> idx_t {
    let i = &*(info as *const MockInitContext);
    i.projected_columns[column_index as usize] as idx_t
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_init_set_error(info: duckdb_init_info, error: *const c_char) {
    let i = &mut *(info as *mut MockInitContext);
    i.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_get_bind_data(info: duckdb_function_info) -> *mut c_void {
    let f = &*(info as *const MockFuncContext);
    f.bind_data
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_get_init_data(info: duckdb_function_info) -> *mut c_void {
    let f = &*(info as *const MockFuncContext);
    f.init_data
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_get_local_init_data(
    info: duckdb_function_info,
) -> *mut c_void {
    let f = &*(info as *const MockFuncContext);
    f.local_init_data
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_function_set_error(
    info: duckdb_function_info,
    error: *const c_char,
) {
    let f = &mut *(info as *mut MockFuncContext);
    f.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_set_error(
    info: duckdb_function_info,
    error: *const c_char,
) {
    if !info.is_null() {
        let f = &mut *(info as *mut MockFuncContext);
        f.error = Some(CStr::from_ptr(error).to_string_lossy().into_owned());
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_get_column_count(chunk: duckdb_data_chunk) -> idx_t {
    let c = &*(chunk as *const MockDataChunk);
    c.vectors.len() as idx_t
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_get_vector(
    chunk: duckdb_data_chunk,
    col_idx: idx_t,
) -> duckdb_vector {
    let c = &mut *(chunk as *mut MockDataChunk);
    c.vectors
        .get_mut(col_idx as usize)
        .map(|v| v as *mut MockVector as duckdb_vector)
        .unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_get_size(chunk: duckdb_data_chunk) -> idx_t {
    let c = &*(chunk as *const MockDataChunk);
    c.size as idx_t
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_data_chunk_set_size(chunk: duckdb_data_chunk, size: idx_t) {
    if !chunk.is_null() {
        let c = &mut *(chunk as *mut MockDataChunk);
        c.size = size as usize;
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_get_data(vector: duckdb_vector) -> *mut c_void {
    if vector.is_null() {
        return ptr::null_mut();
    }
    let v = &mut *(vector as *mut MockVector);
    v.data.as_mut_ptr() as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_assign_string_element(
    vector: duckdb_vector,
    index: idx_t,
    str_ptr: *const c_char,
) {
    let bytes = CStr::from_ptr(str_ptr).to_bytes();
    duckdb_vector_assign_string_element_len(vector, index, str_ptr, bytes.len() as idx_t);
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_assign_string_element_len(
    vector: duckdb_vector,
    index: idx_t,
    str_ptr: *const c_char,
    str_len: idx_t,
) {
    let v = &mut *(vector as *mut MockVector);
    let bytes = std::slice::from_raw_parts(str_ptr as *const u8, str_len as usize);
    v.strings[index as usize] = bytes.to_vec();
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_get_validity(vector: duckdb_vector) -> *mut u64 {
    let v = &mut *(vector as *mut MockVector);
    if v.has_validity {
        v.validity.as_mut_ptr()
    } else {
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_ensure_validity_writable(vector: duckdb_vector) {
    let v = &mut *(vector as *mut MockVector);
    v.has_validity = true;
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_row_is_valid(validity: *mut u64, row: idx_t) -> bool {
    if validity.is_null() {
        return true;
    }
    let entry = (row / 64) as usize;
    let bit = row % 64;
    (*validity.add(entry) & (1u64 << bit)) != 0
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_set_row_valid(validity: *mut u64, row: idx_t) {
    if !validity.is_null() {
        let entry = (row / 64) as usize;
        let bit = row % 64;
        *validity.add(entry) |= 1u64 << bit;
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_set_row_invalid(validity: *mut u64, row: idx_t) {
    if !validity.is_null() {
        let entry = (row / 64) as usize;
        let bit = row % 64;
        *validity.add(entry) &= !(1u64 << bit);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_validity_set_row_validity(
    validity: *mut u64,
    row: idx_t,
    valid: bool,
) {
    if valid {
        duckdb_validity_set_row_valid(validity, row);
    } else {
        duckdb_validity_set_row_invalid(validity, row);
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_vector_size() -> idx_t {
    2048
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_varchar(val: duckdb_value) -> *mut c_char {
    let v = &*(val as *const MockVal);
    match v {
        MockVal::Str(s) => CString::new(s.as_str()).unwrap().into_raw(),
        _ => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_int64(val: duckdb_value) -> i64 {
    match &*(val as *const MockVal) {
        MockVal::Int(i) => *i,
        MockVal::UInt(u) => *u as i64,
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_uint64(val: duckdb_value) -> u64 {
    match &*(val as *const MockVal) {
        MockVal::UInt(u) => *u,
        MockVal::Int(i) => *i as u64,
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_double(val: duckdb_value) -> f64 {
    match &*(val as *const MockVal) {
        MockVal::Double(d) => *d,
        MockVal::Int(i) => *i as f64,
        _ => 0.0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_get_bool(val: duckdb_value) -> bool {
    match &*(val as *const MockVal) {
        MockVal::Bool(b) => *b,
        _ => false,
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_free(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(CString::from_raw(ptr as *mut c_char));
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_destroy_value(val: *mut duckdb_value) {
    if !val.is_null() && !(*val).is_null() {
        drop(Box::from_raw(*val as *mut MockVal));
        *val = ptr::null_mut();
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
    if !scalar_function.is_null() && !(*scalar_function).is_null() {
        drop(Box::from_raw(*scalar_function as *mut MockScalarFunction));
        *scalar_function = ptr::null_mut();
    }
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_set_name(
    scalar_function: duckdb_scalar_function,
    name: *const c_char,
) {
    let sf = &mut *(scalar_function as *mut MockScalarFunction);
    sf.name = CStr::from_ptr(name).to_string_lossy().into_owned();
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_add_parameter(
    scalar_function: duckdb_scalar_function,
    type_: duckdb_logical_type,
) {
    let sf = &mut *(scalar_function as *mut MockScalarFunction);
    let lt = &*(type_ as *const MockLogicalType);
    sf.params.push(lt.ty);
}

#[no_mangle]
pub unsafe extern "C" fn duckdb_scalar_function_set_return_type(
    scalar_function: duckdb_scalar_function,
    type_: duckdb_logical_type,
) {
    let sf = &mut *(scalar_function as *mut MockScalarFunction);
    let lt = &*(type_ as *const MockLogicalType);
    sf.ret = lt.ty;
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
    let c = &mut *(con as *mut MockConnection);
    let sf = &*(scalar_function as *const MockScalarFunction);
    c.scalar_functions.push(RegisteredScalarFn {
        name: sf.name.clone(),
        params: sf.params.clone(),
        ret: sf.ret,
    });
    DuckDBState::Success
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

// =============================================================================
// 1. Extension Initialization & Function Registration Tests
// =============================================================================

/// Points the extension's DuckDB C API table at the mocks above.
fn install_mock_api() {
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
        duckdb_create_logical_type,
        duckdb_create_scalar_function,
        duckdb_create_table_function,
        duckdb_data_chunk_get_column_count,
        duckdb_data_chunk_get_size,
        duckdb_data_chunk_get_vector,
        duckdb_data_chunk_set_size,
        duckdb_destroy_data_chunk,
        duckdb_destroy_logical_type,
        duckdb_destroy_result,
        duckdb_destroy_scalar_function,
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
    unsafe { install_api_table(table as *const c_void) };
}

#[test]
fn test_extension_init_and_registration_completeness() {
    install_mock_api();
    let ver = unsafe { CStr::from_ptr(raster_h3::raster_h3_version()).to_string_lossy() };
    assert_eq!(ver, "v0.0.1");

    let mut dummy_db = 1usize;
    let db_ptr = &mut dummy_db as *mut usize as duckdb_database;
    assert!(unsafe { raster_h3::raster_h3_init(db_ptr) });

    // Also test C API entrypoint
    unsafe extern "C" fn get_db(info: duckdb_extension_info) -> *mut duckdb_database {
        info as *mut duckdb_database
    }
    let mut db_handle = db_ptr;
    let access = duckdb_extension_access {
        set_error: None,
        get_database: Some(get_db),
        get_api: None,
    };
    assert!(unsafe {
        raster_h3::raster_h3_init_c_api(
            &mut db_handle as *mut duckdb_database as duckdb_extension_info,
            &access,
        )
    });

    // Inspect registered functions on a connection
    let mut con = MockConnection::default();
    let con_ptr = &mut con as *mut MockConnection as duckdb_connection;
    unsafe {
        raster_h3::functions::register_table_function(con_ptr).unwrap();
        raster_h3::functions::register_categorical_table_function(con_ptr).unwrap();
        raster_h3::functions::register_scalar_functions(con_ptr).unwrap();
        raster_h3::functions::register_pmtiles_table_function(con_ptr).unwrap();
        raster_h3::functions::register_parquet_table_function(con_ptr).unwrap();
    }

    assert_eq!(con.scalar_functions.len(), 15);
    for sf in &con.scalar_functions {
        assert!(!sf.name.is_empty());
        assert!(!sf.params.is_empty());
        assert_ne!(sf.ret, DuckDBType::Invalid);
    }
    let tf_names: Vec<&str> = con
        .table_functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    for expected in [
        "h3_raster_continuous_aggregate",
        "h3_raster_continuous",
        "raster_h3",
        "h3_raster_categorical_aggregate",
        "h3_raster_categorical",
        "h3_raster_to_pmtiles",
        "h3_parquet_to_pmtiles",
        "h3_raster_to_parquet",
    ] {
        let tf = con
            .table_functions
            .iter()
            .find(|f| f.name == expected)
            .unwrap_or_else(|| panic!("Missing table function {expected}"));
        assert!(!tf.positional_params.is_empty());
        if expected.contains("aggregate") || expected == "raster_h3" {
            assert!(tf.projection_pushdown);
        }
        assert!(tf_names.contains(&expected));
    }

    // Verify h3_raster_to_parquet and h3_raster_to_pmtiles register source_crs/crs and overlap_rule
    for export_fn in ["h3_raster_to_parquet", "h3_raster_to_pmtiles"] {
        let tf = con
            .table_functions
            .iter()
            .find(|f| f.name == export_fn)
            .unwrap();
        for param in [
            "source_crs",
            "crs",
            "overlap_rule",
            "workers",
            "decode_workers",
            "fetch_workers",
            "decode_budget_mb",
        ] {
            assert!(
                tf.named_params.contains_key(param),
                "{export_fn} missing registered named parameter '{param}'"
            );
        }
    }
}

// =============================================================================
// 2. Scalar Function Execution & NULL Propagation Tests
// =============================================================================

#[test]
fn test_all_scalar_functions_and_null_propagation() {
    install_mock_api();
    let valid_cell: u64 = 0x8828308281fffff; // Res 8 SF
    let invalid_cell: u64 = 0;

    // 1. h3_to_string & string_to_h3 with NULL row at index 1
    let mut chunk_u64 = MockDataChunk::new(1, 4);
    chunk_u64.size = 3;
    unsafe {
        let data = chunk_u64.vectors[0].data.as_mut_ptr();
        *data.add(0) = valid_cell;
        *data.add(1) = valid_cell;
        *data.add(2) = invalid_cell;
        chunk_u64.vectors[0].has_validity = true;
        duckdb_validity_set_row_invalid(chunk_u64.vectors[0].validity.as_mut_ptr(), 1);
    }

    let mut out_str = MockVector::new(4);
    unsafe {
        scalar_h3_to_string(
            ptr::null_mut(),
            &mut chunk_u64 as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_str as *mut MockVector as duckdb_vector,
        );
    }
    assert!(out_str.is_valid(0));
    assert!(
        !out_str.is_valid(1),
        "NULL input must propagate to NULL output"
    );
    assert_eq!(
        std::str::from_utf8(&out_str.strings[0]).unwrap(),
        "8828308281fffff"
    );

    // 2. h3_to_lat, h3_to_lng, h3_get_resolution, h3_is_valid(u64), h3_to_wkb(u64), h3_to_geometry(u64)
    let mut out_lat = MockVector::new(4);
    let mut out_lng = MockVector::new(4);
    let mut out_res = MockVector::new(4);
    let mut out_valid = MockVector::new(4);
    let mut out_wkb = MockVector::new(4);
    let mut out_geom = MockVector::new(4);
    unsafe {
        let in_ptr = &mut chunk_u64 as *mut MockDataChunk as duckdb_data_chunk;
        scalar_h3_to_lat(
            ptr::null_mut(),
            in_ptr,
            &mut out_lat as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_lng(
            ptr::null_mut(),
            in_ptr,
            &mut out_lng as *mut MockVector as duckdb_vector,
        );
        scalar_h3_get_resolution(
            ptr::null_mut(),
            in_ptr,
            &mut out_res as *mut MockVector as duckdb_vector,
        );
        scalar_h3_is_valid_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_valid as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_wkb_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_wkb as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_geometry_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_geom as *mut MockVector as duckdb_vector,
        );
    }
    unsafe {
        let lats = out_lat.data.as_ptr() as *const f64;
        let lngs = out_lng.data.as_ptr() as *const f64;
        let res = out_res.data.as_ptr() as *const i64;
        let valid = out_valid.data.as_ptr() as *const bool;
        assert!((*lats.add(0) - 37.77).abs() < 0.1);
        assert!((*lats.add(2)).is_nan());
        assert!((*lngs.add(0) - (-122.42)).abs() < 0.1);
        assert!((*lngs.add(2)).is_nan());
        assert_eq!(*res.add(0), 8);
        assert_eq!(*res.add(2), -1);
        assert!(*valid.add(0));
        assert!(!*valid.add(2));
        assert_eq!(out_wkb.strings[0].len(), 125);
        assert!(
            !out_wkb.is_valid(2),
            "Invalid H3 cell must produce NULL WKB"
        );
        assert_eq!(out_geom.strings[0], out_wkb.strings[0]);
    }

    // 3. String scalar functions: string_to_h3, h3_is_valid(VARCHAR), h3_to_wkb(VARCHAR), h3_to_geometry(VARCHAR)
    let mut chunk_str = MockDataChunk::new(1, 4);
    chunk_str.size = 2;
    chunk_str.vectors[0].set_str_input(0, "8828308281fffff");
    chunk_str.vectors[0].set_str_input(1, "invalid_hex");

    let mut out_u64 = MockVector::new(4);
    let mut out_valid_s = MockVector::new(4);
    let mut out_wkb_s = MockVector::new(4);
    let mut out_geom_s = MockVector::new(4);
    unsafe {
        let in_ptr = &mut chunk_str as *mut MockDataChunk as duckdb_data_chunk;
        scalar_string_to_h3(
            ptr::null_mut(),
            in_ptr,
            &mut out_u64 as *mut MockVector as duckdb_vector,
        );
        scalar_h3_is_valid_str(
            ptr::null_mut(),
            in_ptr,
            &mut out_valid_s as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_wkb_str(
            ptr::null_mut(),
            in_ptr,
            &mut out_wkb_s as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_geometry_str(
            ptr::null_mut(),
            in_ptr,
            &mut out_geom_s as *mut MockVector as duckdb_vector,
        );
        let parsed = out_u64.data.as_ptr();
        let valid_s = out_valid_s.data.as_ptr() as *const bool;
        assert_eq!(*parsed.add(0), valid_cell);
        assert_eq!(*parsed.add(1), 0);
        assert!(*valid_s.add(0));
        assert!(!*valid_s.add(1));
        assert_eq!(out_wkb_s.strings[0].len(), 125);
        assert!(!out_wkb_s.is_valid(1));
        assert_eq!(out_geom_s.strings[0].len(), 125);
    }

    // 4. Binary scalar functions: h3_cell_to_parent(u64, i64) and h3_cell_to_parent(VARCHAR, i64)
    let mut chunk_bin_u64 = MockDataChunk::new(2, 4);
    chunk_bin_u64.size = 2;
    unsafe {
        *chunk_bin_u64.vectors[0].data.as_mut_ptr().add(0) = valid_cell;
        *chunk_bin_u64.vectors[0].data.as_mut_ptr().add(1) = valid_cell;
        *(chunk_bin_u64.vectors[1].data.as_mut_ptr() as *mut i64).add(0) = 7;
        *(chunk_bin_u64.vectors[1].data.as_mut_ptr() as *mut i64).add(1) = 9; // invalid coarser > 8
    }
    let mut out_parent_u64 = MockVector::new(4);
    unsafe {
        scalar_h3_cell_to_parent_u64(
            ptr::null_mut(),
            &mut chunk_bin_u64 as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_parent_u64 as *mut MockVector as duckdb_vector,
        );
        let p = out_parent_u64.data.as_ptr();
        let expected_parent: u64 = h3o::CellIndex::try_from(valid_cell)
            .unwrap()
            .parent(h3o::Resolution::Seven)
            .unwrap()
            .into();
        assert_eq!(*p.add(0), expected_parent);
        assert_eq!(*p.add(1), 0);
    }

    let mut chunk_bin_str = MockDataChunk::new(2, 4);
    chunk_bin_str.size = 2;
    chunk_bin_str.vectors[0].set_str_input(0, "8828308281fffff");
    chunk_bin_str.vectors[0].set_str_input(1, "8828308281fffff");
    unsafe {
        *(chunk_bin_str.vectors[1].data.as_mut_ptr() as *mut i64).add(0) = 7;
        *(chunk_bin_str.vectors[1].data.as_mut_ptr() as *mut i64).add(1) = 9;
    }
    let mut out_parent_str = MockVector::new(4);
    unsafe {
        scalar_h3_cell_to_parent_str(
            ptr::null_mut(),
            &mut chunk_bin_str as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_parent_str as *mut MockVector as duckdb_vector,
        );
        assert!(out_parent_str.is_valid(0));
        assert!(!out_parent_str.is_valid(1));
        assert!(std::str::from_utf8(&out_parent_str.strings[0])
            .unwrap()
            .starts_with("8728308"));
    }
}

// =============================================================================
// 3. Continuous & Categorical (Wide + Long) Table Function Execution via FFI
// =============================================================================

#[test]
fn test_continuous_table_function_full_scan_and_projection_pushdown() {
    install_mock_api();
    let (_tmp, path) = TestGeoTiffBuilder::new(32, 32)
        .origin(-122.45, 37.80)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| (c + r + 10) as f32);

    let mut named = HashMap::new();
    named.insert("resolution".to_string(), MockVal::Int(8));
    named.insert("geom".to_string(), MockVal::Bool(true));
    named.insert("quantiles".to_string(), MockVal::Str("p50,iqr".to_string()));
    named.insert("min_count".to_string(), MockVal::Double(0.5));
    named.insert("workers".to_string(), MockVal::UInt(2));

    let mut bind_ctx = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        named,
    );
    unsafe {
        raster_h3_bind(&mut bind_ctx as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(
        bind_ctx.error.is_none(),
        "Bind failed: {:?}",
        bind_ctx.error
    );
    // 10 base columns + 1 geom + 2 quantiles = 13 columns
    assert_eq!(bind_ctx.result_columns.len(), 13);
    assert_eq!(bind_ctx.result_columns[10].0, "geom");
    assert_eq!(bind_ctx.result_columns[11].0, "p50");
    assert_eq!(bind_ctx.result_columns[12].0, "iqr");

    let all_cols: Vec<usize> = (0..13).collect();
    let mut init_ctx = MockInitContext::new(bind_ctx.bind_data, all_cols);
    let mut local_ctx = MockInitContext::new(bind_ctx.bind_data, Vec::new());
    unsafe {
        raster_h3_init(&mut init_ctx as *mut MockInitContext as duckdb_init_info);
        raster_h3_init_local(&mut local_ctx as *mut MockInitContext as duckdb_init_info);
    }
    assert!(init_ctx.error.is_none());

    let mut func_ctx = MockFuncContext {
        bind_data: bind_ctx.bind_data,
        init_data: init_ctx.init_data,
        local_init_data: local_ctx.init_data,
        error: None,
    };

    let mut total_rows = 0usize;
    let mut total_pixel_count = 0.0f64;
    loop {
        let mut chunk = MockDataChunk::new(13, 2048);
        unsafe {
            raster_h3_scan(
                &mut func_ctx as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        assert!(func_ctx.error.is_none());
        if chunk.size == 0 {
            break;
        }
        total_rows += chunk.size;
        unsafe {
            let counts = chunk.vectors[4].data.as_ptr() as *const f64;
            let p50s = chunk.vectors[11].data.as_ptr() as *const f64;
            let iqrs = chunk.vectors[12].data.as_ptr() as *const f64;
            for i in 0..chunk.size {
                total_pixel_count += *counts.add(i);
                assert!((*p50s.add(i)).is_finite());
                assert!((*iqrs.add(i)) >= 0.0);
                assert_eq!(chunk.vectors[9].strings[i].len(), 125);
                assert_eq!(chunk.vectors[10].strings[i].len(), 125);
            }
        }
    }
    assert!(total_rows > 0);
    assert!((total_pixel_count - 1024.0).abs() < 1e-6);

    // Zero-column projection pushdown (SELECT count(*))
    let mut init_zero = MockInitContext::new(bind_ctx.bind_data, Vec::new());
    unsafe {
        raster_h3_init(&mut init_zero as *mut MockInitContext as duckdb_init_info);
    }
    let mut func_zero = MockFuncContext {
        bind_data: bind_ctx.bind_data,
        init_data: init_zero.init_data,
        local_init_data: local_ctx.init_data,
        error: None,
    };
    let mut zero_col_rows = 0usize;
    loop {
        let mut chunk = MockDataChunk::new(0, 2048);
        unsafe {
            raster_h3_scan(
                &mut func_zero as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        if chunk.size == 0 {
            break;
        }
        zero_col_rows += chunk.size;
    }
    assert_eq!(zero_col_rows, total_rows);
}

#[test]
fn test_categorical_table_function_wide_and_long_formats() {
    install_mock_api();
    let (_tmp, path) = TestGeoTiffBuilder::new(32, 32)
        .origin(-122.45, 37.80)
        .pixel_size(0.001)
        .create_tempfile::<u8, _>(|c, r| ((c + r) % 3 + 1) as u8);

    // 0. Invalid remap syntax sets bind error cleanly
    let mut bad_remap = HashMap::new();
    bad_remap.insert(
        "remap".to_string(),
        MockVal::Str("invalid_without_colon".to_string()),
    );
    let mut bind_bad = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        bad_remap,
    );
    unsafe {
        raster_h3_categorical_bind(&mut bind_bad as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(bind_bad.error.is_some());

    // 1. Wide Format with remap
    let mut named_wide = HashMap::new();
    named_wide.insert("resolution".to_string(), MockVal::Int(8));
    named_wide.insert("format".to_string(), MockVal::Str("wide".to_string()));
    named_wide.insert(
        "remap".to_string(),
        MockVal::Str("1: 10, 2: 20, 3: 30".to_string()),
    );
    named_wide.insert("geom".to_string(), MockVal::Bool(true));

    let mut bind_wide = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        named_wide,
    );
    unsafe {
        raster_h3_categorical_bind(&mut bind_wide as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(bind_wide.error.is_none());
    assert_eq!(bind_wide.result_columns.len(), 14);

    let wide_cols: Vec<usize> = (0..14).collect();
    let mut init_wide = MockInitContext::new(bind_wide.bind_data, wide_cols);
    let mut local_wide = MockInitContext::new(bind_wide.bind_data, Vec::new());
    unsafe {
        raster_h3_categorical_init(&mut init_wide as *mut MockInitContext as duckdb_init_info);
        raster_h3_categorical_init_local(
            &mut local_wide as *mut MockInitContext as duckdb_init_info,
        );
    }
    let mut func_wide = MockFuncContext {
        bind_data: bind_wide.bind_data,
        init_data: init_wide.init_data,
        local_init_data: local_wide.init_data,
        error: None,
    };

    let mut wide_cells = 0usize;
    let mut wide_total_pixels = 0.0f64;
    loop {
        let mut chunk = MockDataChunk::new(14, 2048);
        unsafe {
            raster_h3_categorical_scan(
                &mut func_wide as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        if chunk.size == 0 {
            break;
        }
        wide_cells += chunk.size;
        unsafe {
            let maj_classes = chunk.vectors[2].data.as_ptr() as *const i64;
            let totals = chunk.vectors[6].data.as_ptr() as *const f64;
            for i in 0..chunk.size {
                let mc = *maj_classes.add(i);
                assert!(
                    mc == 10 || mc == 20 || mc == 30,
                    "Remapped majority class must be in {{10, 20, 30}}, got {mc}"
                );
                wide_total_pixels += *totals.add(i);
                let hist = std::str::from_utf8(&chunk.vectors[7].strings[i]).unwrap();
                assert!(hist.starts_with('{') && hist.ends_with('}'));
            }
        }
    }
    assert!(wide_cells > 0);
    assert!((wide_total_pixels - 1024.0).abs() < 1e-6);

    // 2. Long Format
    let mut named_long = HashMap::new();
    named_long.insert("resolution".to_string(), MockVal::Int(8));
    named_long.insert("format".to_string(), MockVal::Str("long".to_string()));
    named_long.insert(
        "remap".to_string(),
        MockVal::Str("1: 10, 2: 20, 3: 30".to_string()),
    );
    named_long.insert("geom".to_string(), MockVal::Bool(true));

    let mut bind_long = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        named_long,
    );
    unsafe {
        raster_h3_categorical_bind(&mut bind_long as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(bind_long.error.is_none());
    assert_eq!(bind_long.result_columns.len(), 13);

    let long_cols: Vec<usize> = (0..13).collect();
    let mut init_long = MockInitContext::new(bind_long.bind_data, long_cols);
    let mut local_long = MockInitContext::new(bind_long.bind_data, Vec::new());
    unsafe {
        raster_h3_categorical_init(&mut init_long as *mut MockInitContext as duckdb_init_info);
        raster_h3_categorical_init_local(
            &mut local_long as *mut MockInitContext as duckdb_init_info,
        );
    }
    let mut func_long = MockFuncContext {
        bind_data: bind_long.bind_data,
        init_data: init_long.init_data,
        local_init_data: local_long.init_data,
        error: None,
    };

    let mut long_rows = 0usize;
    let mut long_total_pixels = 0.0f64;
    loop {
        let mut chunk = MockDataChunk::new(13, 2048);
        unsafe {
            raster_h3_categorical_scan(
                &mut func_long as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        if chunk.size == 0 {
            break;
        }
        long_rows += chunk.size;
        unsafe {
            let cats = chunk.vectors[2].data.as_ptr() as *const i64;
            let counts = chunk.vectors[3].data.as_ptr() as *const f64;
            let fracs = chunk.vectors[4].data.as_ptr() as *const f64;
            for i in 0..chunk.size {
                let c = *cats.add(i);
                assert!(c == 10 || c == 20 || c == 30);
                assert!(*fracs.add(i) > 0.0 && *fracs.add(i) <= 1.0);
                long_total_pixels += *counts.add(i);
                assert_eq!(chunk.vectors[11].strings[i].len(), 125);
                assert_eq!(chunk.vectors[12].strings[i].len(), 125);
            }
        }
    }
    assert!(
        long_rows > wide_cells,
        "Long format must expand multi-class cells into multiple rows"
    );
    assert!((long_total_pixels - 1024.0).abs() < 1e-6);
}

// =============================================================================
// 4. Direct Export Table Functions (Parquet & PMTiles) with CRS Override
// =============================================================================

#[test]
fn test_export_table_functions_with_crs_override() {
    install_mock_api();
    // Create an unreferenced GeoTIFF (no GeoKeys) so `crs := 'EPSG:4326'` is required
    let (_tmp, tif_path) = TestGeoTiffBuilder::new(24, 24)
        .origin(-122.45, 37.80)
        .pixel_size(0.001)
        .with_geokeys(false)
        .create_constant_tempfile(42.0f32);

    let out_dir = tempfile::tempdir().unwrap();
    let parquet_path = out_dir.path().join("exported.parquet");
    let pmtiles_path = out_dir.path().join("exported.pmtiles");
    let transcode_pmtiles_path = out_dir.path().join("transcoded.pmtiles");

    // 1. h3_raster_to_parquet with crs := 'EPSG:4326' and geoparquet := true
    let mut pq_named = HashMap::new();
    pq_named.insert("resolution".to_string(), MockVal::Int(8));
    pq_named.insert("crs".to_string(), MockVal::Str("EPSG:4326".to_string()));
    pq_named.insert("geoparquet".to_string(), MockVal::Bool(true));

    let mut pq_bind = MockBindContext::new(
        vec![
            MockVal::Str(tif_path.to_string_lossy().into_owned()),
            MockVal::Str(parquet_path.to_string_lossy().into_owned()),
        ],
        pq_named,
    );
    unsafe {
        parquet_bind(&mut pq_bind as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(pq_bind.error.is_none());

    let mut pq_init = MockInitContext::new(pq_bind.bind_data, (0..6).collect());
    unsafe {
        parquet_init(&mut pq_init as *mut MockInitContext as duckdb_init_info);
    }
    let mut pq_func = MockFuncContext {
        bind_data: pq_bind.bind_data,
        init_data: pq_init.init_data,
        local_init_data: ptr::null_mut(),
        error: None,
    };
    let mut pq_chunk = MockDataChunk::new(6, 2048);
    unsafe {
        parquet_scan(
            &mut pq_func as *mut MockFuncContext as duckdb_function_info,
            &mut pq_chunk as *mut MockDataChunk as duckdb_data_chunk,
        );
    }
    assert_eq!(pq_chunk.size, 1);
    let pq_status = std::str::from_utf8(&pq_chunk.vectors[5].strings[0]).unwrap();
    assert_eq!(pq_status, "SUCCESS", "Parquet export failed: {pq_status}");
    let pq_hex_count = unsafe { *(pq_chunk.vectors[0].data.as_ptr() as *const i64) };
    assert!(pq_hex_count > 0);
    assert!(parquet_path.exists());

    // 2. h3_raster_to_pmtiles with source_crs := 'EPSG:4326'
    let mut pm_named = HashMap::new();
    pm_named.insert("resolution".to_string(), MockVal::Int(8));
    pm_named.insert(
        "source_crs".to_string(),
        MockVal::Str("EPSG:4326".to_string()),
    );
    pm_named.insert(
        "properties".to_string(),
        MockVal::Str("mean,count".to_string()),
    );

    let mut pm_bind = MockBindContext::new(
        vec![
            MockVal::Str(tif_path.to_string_lossy().into_owned()),
            MockVal::Str(pmtiles_path.to_string_lossy().into_owned()),
        ],
        pm_named,
    );
    unsafe {
        pmtiles_bind(&mut pm_bind as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(pm_bind.error.is_none());

    let mut pm_init = MockInitContext::new(pm_bind.bind_data, (0..7).collect());
    unsafe {
        pmtiles_init(&mut pm_init as *mut MockInitContext as duckdb_init_info);
    }
    let mut pm_func = MockFuncContext {
        bind_data: pm_bind.bind_data,
        init_data: pm_init.init_data,
        local_init_data: ptr::null_mut(),
        error: None,
    };
    let mut pm_chunk = MockDataChunk::new(7, 2048);
    unsafe {
        pmtiles_scan(
            &mut pm_func as *mut MockFuncContext as duckdb_function_info,
            &mut pm_chunk as *mut MockDataChunk as duckdb_data_chunk,
        );
    }
    assert_eq!(pm_chunk.size, 1);
    let pm_status = std::str::from_utf8(&pm_chunk.vectors[6].strings[0]).unwrap();
    assert_eq!(pm_status, "SUCCESS", "PMTiles export failed: {pm_status}");
    assert!(pmtiles_path.exists());

    // 3. h3_parquet_to_pmtiles on the generated Parquet file
    let mut pp_bind = MockBindContext::new(
        vec![
            MockVal::Str(parquet_path.to_string_lossy().into_owned()),
            MockVal::Str(transcode_pmtiles_path.to_string_lossy().into_owned()),
        ],
        HashMap::new(),
    );
    unsafe {
        parquet_pmtiles_bind(&mut pp_bind as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(pp_bind.error.is_none());

    let mut pp_init = MockInitContext::new(pp_bind.bind_data, (0..7).collect());
    unsafe {
        parquet_pmtiles_init(&mut pp_init as *mut MockInitContext as duckdb_init_info);
    }
    let mut pp_func = MockFuncContext {
        bind_data: pp_bind.bind_data,
        init_data: pp_init.init_data,
        local_init_data: ptr::null_mut(),
        error: None,
    };
    let mut pp_chunk = MockDataChunk::new(7, 2048);
    unsafe {
        parquet_pmtiles_scan(
            &mut pp_func as *mut MockFuncContext as duckdb_function_info,
            &mut pp_chunk as *mut MockDataChunk as duckdb_data_chunk,
        );
    }
    assert_eq!(pp_chunk.size, 1);
    let pp_status = std::str::from_utf8(&pp_chunk.vectors[6].strings[0]).unwrap();
    assert_eq!(pp_status, "SUCCESS", "Parquet->PMTiles failed: {pp_status}");
    let transcoded_count = unsafe { *(pp_chunk.vectors[0].data.as_ptr() as *const i64) };
    assert_eq!(transcoded_count, pq_hex_count);
    assert!(transcode_pmtiles_path.exists());
}
