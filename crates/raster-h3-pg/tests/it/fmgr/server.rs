//! In-process stand-ins for the PostgreSQL server symbols `raster_h3_pg::fmgr` imports
//! (`palloc`, detoasting, `funcapi` multi-call helpers, memory-context callbacks, `elog`).
//!
//! They follow the server's contracts closely enough to drive the value-per-call SRF
//! protocol: `init_MultiFuncCall` stores the context in `flinfo->fn_extra`,
//! `end_MultiFuncCall` resets the multi-call context (running its callbacks), and
//! `BuildTupleFromCStrings` records each row's text values for the test to read back.
//! Errors cannot be raised in process (the server longjmps), so `errfinish` aborts.

#![allow(non_snake_case, non_upper_case_globals)]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_short, c_void, CStr};
use std::ptr;

use raster_h3_pg::{Datum, Oid};

/// Mirror of the server's `FmgrInfo`.
#[repr(C)]
pub struct FmgrInfo {
    pub fn_addr: *mut c_void,
    pub fn_oid: Oid,
    pub fn_nargs: c_short,
    pub fn_strict: bool,
    pub fn_retset: bool,
    pub fn_stats: u8,
    pub fn_extra: *mut c_void,
    pub fn_mcxt: *mut c_void,
    pub fn_expr: *mut c_void,
}

impl Default for FmgrInfo {
    fn default() -> Self {
        Self {
            fn_addr: ptr::null_mut(),
            fn_oid: 0,
            fn_nargs: 5,
            fn_strict: false,
            fn_retset: true,
            fn_stats: 0,
            fn_extra: ptr::null_mut(),
            fn_mcxt: ptr::null_mut(),
            fn_expr: ptr::null_mut(),
        }
    }
}

/// Mirror of the server's `ReturnSetInfo`.
#[repr(C)]
#[derive(Default)]
pub struct ReturnSetInfo {
    pub node_tag: c_int,
    pub econtext: usize,
    pub expected_desc: usize,
    pub allowed_modes: c_int,
    pub return_mode: c_int,
    pub is_done: c_int,
    pub set_result: usize,
    pub set_desc: usize,
}

pub const EXPR_MULTIPLE_RESULT: c_int = 1;
pub const EXPR_END_RESULT: c_int = 2;

#[repr(C)]
#[allow(dead_code)]
struct FuncCallContext {
    call_cntr: u64,
    max_calls: u64,
    user_fctx: *mut c_void,
    attinmeta: *mut c_void,
    multi_call_memory_ctx: *mut c_void,
    tuple_desc: *mut c_void,
}

#[repr(C)]
#[allow(dead_code)]
struct HeapTupleData {
    t_len: u32,
    t_self: [u16; 3],
    t_table_oid: Oid,
    t_data: *mut c_void,
}

#[repr(C)]
#[allow(dead_code)]
struct MemoryContextCallback {
    func: Option<unsafe extern "C" fn(arg: *mut c_void)>,
    arg: *mut c_void,
    next: *mut MemoryContextCallback,
}

/// Per-thread server state; each test drives its SRF on its own thread.
#[derive(Default)]
pub struct Server {
    /// Columns of the declared result type (`BuildTupleFromCStrings` reads this many).
    pub natts: usize,
    /// Rows built so far, as the text values the extension passed.
    pub rows: Vec<Vec<Option<String>>>,
    /// Reset callbacks registered on live multi-call contexts.
    callbacks: Vec<(*mut c_void, *mut MemoryContextCallback)>,
    /// How many reset callbacks have run.
    pub callbacks_run: usize,
    pub contexts_live: usize,
}

thread_local! {
    pub static SERVER: RefCell<Server> = RefCell::new(Server::default());
}

/// Starts a fresh call sequence whose result type has `natts` columns.
pub fn reset(natts: usize) {
    SERVER.with_borrow_mut(|s| {
        *s = Server {
            natts,
            ..Default::default()
        }
    });
}

/// Runs the reset callbacks of `ctx`, as deleting a memory context would.
pub fn reset_context(ctx: *mut c_void) {
    let cbs: Vec<_> = SERVER.with_borrow_mut(|s| {
        let (run, keep) = s.callbacks.drain(..).partition(|(c, _)| *c == ctx);
        s.callbacks = keep;
        run
    });
    for (_, cb) in cbs {
        unsafe {
            if let Some(func) = (*cb).func {
                func((*cb).arg);
            }
        }
        SERVER.with_borrow_mut(|s| s.callbacks_run += 1);
    }
}

#[no_mangle]
pub static mut CurrentMemoryContext: *mut c_void = ptr::null_mut();

/// Test allocations are never freed: each test allocates a few kilobytes.
#[no_mangle]
pub unsafe extern "C" fn palloc(size: usize) -> *mut c_void {
    Box::into_raw(vec![0u8; size.max(1)].into_boxed_slice()) as *mut c_void
}

/// Test datums are never toasted.
#[no_mangle]
pub unsafe extern "C" fn pg_detoast_datum_packed(datum: *mut c_void) -> *mut c_void {
    datum
}

#[no_mangle]
pub unsafe extern "C" fn errstart(_elevel: c_int, _domain: *const c_char) -> bool {
    true
}

/// Declared variadic by the extension; only the format string is read here.
#[no_mangle]
pub unsafe extern "C" fn errmsg(fmt: *const c_char) -> c_int {
    eprintln!("ereport: {}", CStr::from_ptr(fmt).to_string_lossy());
    0
}

#[no_mangle]
pub unsafe extern "C" fn errfinish(
    _filename: *const c_char,
    _lineno: c_int,
    _funcname: *const c_char,
) {
    eprintln!("ereport(ERROR) cannot longjmp in process; aborting");
    std::process::abort();
}

#[no_mangle]
pub unsafe extern "C" fn init_MultiFuncCall(
    fcinfo: raster_h3_pg::pg_func_call_info,
) -> *mut c_void {
    let flinfo = (*fcinfo).flinfo as *mut FmgrInfo;
    assert!(
        (*flinfo).fn_extra.is_null(),
        "init_MultiFuncCall called twice"
    );
    let ctx = Box::into_raw(Box::new(0u64)) as *mut c_void;
    let funcctx = Box::into_raw(Box::new(FuncCallContext {
        call_cntr: 0,
        max_calls: 0,
        user_fctx: ptr::null_mut(),
        attinmeta: ptr::null_mut(),
        multi_call_memory_ctx: ctx,
        tuple_desc: ptr::null_mut(),
    }));
    (*flinfo).fn_extra = funcctx as *mut c_void;
    SERVER.with_borrow_mut(|s| s.contexts_live += 1);
    funcctx as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn per_MultiFuncCall(fcinfo: raster_h3_pg::pg_func_call_info) -> *mut c_void {
    let flinfo = (*fcinfo).flinfo as *mut FmgrInfo;
    assert!(
        !(*flinfo).fn_extra.is_null(),
        "per_MultiFuncCall before init"
    );
    (*flinfo).fn_extra
}

#[no_mangle]
pub unsafe extern "C" fn end_MultiFuncCall(
    fcinfo: raster_h3_pg::pg_func_call_info,
    funcctx: *mut c_void,
) {
    let flinfo = (*fcinfo).flinfo as *mut FmgrInfo;
    (*flinfo).fn_extra = ptr::null_mut();
    let funcctx = Box::from_raw(funcctx as *mut FuncCallContext);
    reset_context(funcctx.multi_call_memory_ctx);
    drop(Box::from_raw(funcctx.multi_call_memory_ctx as *mut u64));
    SERVER.with_borrow_mut(|s| s.contexts_live -= 1);
}

/// Every caller in these tests declares a composite result type.
#[no_mangle]
pub unsafe extern "C" fn get_call_result_type(
    _fcinfo: raster_h3_pg::pg_func_call_info,
    _result_type_id: *mut Oid,
    result_tuple_desc: *mut *mut c_void,
) -> c_int {
    *result_tuple_desc = 0x7475_706c as *mut c_void;
    1 // TYPEFUNC_COMPOSITE
}

#[no_mangle]
pub unsafe extern "C" fn TupleDescGetAttInMetadata(tupdesc: *mut c_void) -> *mut c_void {
    tupdesc
}

#[no_mangle]
pub unsafe extern "C" fn BuildTupleFromCStrings(
    _attinmeta: *mut c_void,
    values: *mut *mut c_char,
) -> *mut c_void {
    let row = SERVER.with_borrow_mut(|s| {
        let row: Vec<Option<String>> = (0..s.natts)
            .map(|i| {
                let p = *values.add(i);
                (!p.is_null()).then(|| CStr::from_ptr(p).to_str().unwrap().to_string())
            })
            .collect();
        s.rows.push(row);
        s.rows.len()
    });
    Box::into_raw(Box::new(HeapTupleData {
        t_len: 0,
        t_self: [0; 3],
        t_table_oid: 0,
        t_data: row as *mut c_void,
    })) as *mut c_void
}

/// Returns the row number recorded by `BuildTupleFromCStrings` as the tuple Datum.
#[no_mangle]
pub unsafe extern "C" fn HeapTupleHeaderGetDatum(tuple: *mut c_void) -> Datum {
    tuple as Datum
}

#[no_mangle]
pub unsafe extern "C" fn MemoryContextRegisterResetCallback(context: *mut c_void, cb: *mut c_void) {
    SERVER.with_borrow_mut(|s| {
        s.callbacks
            .push((context, cb as *mut MemoryContextCallback))
    });
}
