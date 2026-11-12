//! Mock DuckDB C-FFI callback tests for extension initialization,
//! scalar functions, continuous & categorical table functions (Wide and Long),
//! projection pushdown, and direct Parquet / PMTiles export table functions.
//! These do not execute SQL; real-engine coverage lives in the root `tests/it/duckdb_sql_smoke.rs`.

#![cfg(unix)]
#![allow(clippy::missing_safety_doc)]

mod exports;
mod mock_api;
mod registration;
mod scalars;
mod strict_transforms;
mod table_scans;

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::ptr;

use raster_h3_duckdb::ffi::duckdb_c::*;

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
    aggregate_functions: Vec<RegisteredAggregateFn>,
}

struct RegisteredAggregateFn {
    name: String,
    params: Vec<DuckDBType>,
    ret: DuckDBType,
    has_functions: bool,
    has_destructor: bool,
}

#[derive(Default)]
struct MockAggregateFunction {
    name: String,
    params: Vec<DuckDBType>,
    ret: Option<DuckDBType>,
    has_functions: bool,
    has_destructor: bool,
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
                // SAFETY: `bind_data` was installed by the extension together with
                // `bind_destroy`, its matching destructor, and is nulled right after, so it
                // is freed exactly once.
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
                // SAFETY: `init_data` was installed by the extension together with
                // `init_destroy`, its matching destructor, and is nulled right after, so it
                // is freed exactly once.
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
        // SAFETY: `data` holds `capacity * 2` 8-aligned `u64`s, i.e. 16 bytes per row, and
        // `row < capacity`, so slot `row` of the 16-byte, 8-aligned `duckdb_string_t` is in
        // bounds; `&mut self` makes the borrow exclusive.
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
