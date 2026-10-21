//! Loadable-module glue for a real PostgreSQL server (`pg-extension` feature).
//!
//! Provides the `PG_MODULE_MAGIC` block, the `text`/`bytea` scalar functions and the
//! value-per-call set-returning functions declared in `sql/raster_h3--0.4.0.sql`.
//! These call into the server (`palloc`, `ereport`, `funcapi`), so they are only
//! compiled when building the extension library itself.
//!
//! PostgreSQL reports errors with `longjmp`. Every entry point therefore finishes all
//! Rust work (dropping owned values) before calling [`raise_error`], and copies the
//! message into `palloc`'d memory first.

use std::ffi::{c_char, c_int, c_short, c_void};
use std::mem::size_of;
use std::ptr;

use raster_h3_core::error::safe_panic_payload_to_string;
use raster_h3_core::scalar_ops::{
    h3_is_valid_str, h3_to_string, h3_to_wkb_u64, parse_bbox_str, try_string_to_h3,
};

use crate::{
    pg_func_call_info, Datum, NullableDatum, Oid, PgCategoricalRow, PgCategoricalSrf,
    PgContinuousRow, PgContinuousSrf, Pg_finfo_record, PG_FINFO_V1,
};

#[cfg(target_endian = "big")]
compile_error!("raster-h3-pg varlena handling assumes a little-endian target");
#[cfg(not(target_pointer_width = "64"))]
compile_error!("raster-h3-pg assumes 64-bit Datums (FLOAT8PASSBYVAL)");

const fn parse_u32(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v = 0u32;
    while i < b.len() {
        v = v * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    v
}

const PG_MAJOR: c_int = parse_u32(env!("RASTER_H3_PG_MAJOR")) as c_int;
const ERROR_LEVEL: c_int = parse_u32(env!("RASTER_H3_PG_ERROR_LEVEL")) as c_int;

// =============================================================================
// PG_MODULE_MAGIC
// =============================================================================

const FUNC_MAX_ARGS: c_int = 100;
const INDEX_MAX_KEYS: c_int = 32;
const NAMEDATALEN: c_int = 64;

#[cfg(any(pg_magic = "v15", pg_magic = "v18"))]
const fn abi_extra() -> [u8; 32] {
    let mut out = [0u8; 32];
    let s = b"PostgreSQL";
    let mut i = 0;
    while i < s.len() {
        out[i] = s[i];
        i += 1;
    }
    out
}

#[cfg(pg_magic = "v13")]
#[repr(C)]
pub struct Pg_magic_struct {
    len: c_int,
    version: c_int,
    funcmaxargs: c_int,
    indexmaxkeys: c_int,
    namedatalen: c_int,
    float8byval: c_int,
}

#[cfg(pg_magic = "v13")]
static PG_MAGIC: Pg_magic_struct = Pg_magic_struct {
    len: size_of::<Pg_magic_struct>() as c_int,
    version: PG_MAJOR * 100,
    funcmaxargs: FUNC_MAX_ARGS,
    indexmaxkeys: INDEX_MAX_KEYS,
    namedatalen: NAMEDATALEN,
    float8byval: 1,
};

#[cfg(pg_magic = "v15")]
#[repr(C)]
pub struct Pg_magic_struct {
    len: c_int,
    version: c_int,
    funcmaxargs: c_int,
    indexmaxkeys: c_int,
    namedatalen: c_int,
    float8byval: c_int,
    abi_extra: [u8; 32],
}

#[cfg(pg_magic = "v15")]
static PG_MAGIC: Pg_magic_struct = Pg_magic_struct {
    len: size_of::<Pg_magic_struct>() as c_int,
    version: PG_MAJOR * 100,
    funcmaxargs: FUNC_MAX_ARGS,
    indexmaxkeys: INDEX_MAX_KEYS,
    namedatalen: NAMEDATALEN,
    float8byval: 1,
    abi_extra: abi_extra(),
};

#[cfg(pg_magic = "v18")]
#[repr(C)]
struct Pg_abi_values {
    version: c_int,
    funcmaxargs: c_int,
    indexmaxkeys: c_int,
    namedatalen: c_int,
    float8byval: c_int,
    abi_extra: [u8; 32],
}

#[cfg(pg_magic = "v18")]
#[repr(C)]
pub struct Pg_magic_struct {
    len: c_int,
    abi_fields: Pg_abi_values,
    name: *const c_char,
    version: *const c_char,
}

// SAFETY: the pointers reference immutable 'static C string literals.
#[cfg(pg_magic = "v18")]
unsafe impl Sync for Pg_magic_struct {}

#[cfg(pg_magic = "v18")]
static PG_MAGIC: Pg_magic_struct = Pg_magic_struct {
    len: size_of::<Pg_magic_struct>() as c_int,
    abi_fields: Pg_abi_values {
        version: PG_MAJOR * 100,
        funcmaxargs: FUNC_MAX_ARGS,
        indexmaxkeys: INDEX_MAX_KEYS,
        namedatalen: NAMEDATALEN,
        float8byval: 1,
        abi_extra: abi_extra(),
    },
    name: c"raster_h3".as_ptr(),
    version: c"0.4.0".as_ptr(),
};

/// `PG_MODULE_MAGIC`: lets the server verify the module was built for its ABI.
#[no_mangle]
pub extern "C" fn Pg_magic_func() -> *const Pg_magic_struct {
    &PG_MAGIC
}

// =============================================================================
// Server ABI (fmgr / funcapi / memory contexts / elog)
// =============================================================================

#[repr(C)]
struct FmgrInfo {
    fn_addr: *mut c_void,
    fn_oid: Oid,
    fn_nargs: c_short,
    fn_strict: bool,
    fn_retset: bool,
    fn_stats: u8,
    fn_extra: *mut c_void,
    fn_mcxt: *mut c_void,
    fn_expr: *mut c_void,
}

#[repr(C)]
struct ReturnSetInfo {
    node_tag: c_int,
    econtext: *mut c_void,
    expected_desc: *mut c_void,
    allowed_modes: c_int,
    return_mode: c_int,
    is_done: c_int,
    set_result: *mut c_void,
    set_desc: *mut c_void,
}

#[repr(C)]
struct FuncCallContext {
    call_cntr: u64,
    max_calls: u64,
    user_fctx: *mut c_void,
    attinmeta: *mut c_void,
    multi_call_memory_ctx: *mut c_void,
    tuple_desc: *mut c_void,
}

#[repr(C)]
struct HeapTupleData {
    t_len: u32,
    t_self: [u16; 3],
    t_table_oid: Oid,
    t_data: *mut c_void,
}

#[repr(C)]
struct MemoryContextCallback {
    func: Option<unsafe extern "C" fn(arg: *mut c_void)>,
    arg: *mut c_void,
    next: *mut MemoryContextCallback,
}

const EXPR_MULTIPLE_RESULT: c_int = 1;
const EXPR_END_RESULT: c_int = 2;
const TYPEFUNC_COMPOSITE: c_int = 1;
const TYPEFUNC_COMPOSITE_DOMAIN: c_int = 2;

extern "C" {
    static mut CurrentMemoryContext: *mut c_void;

    fn palloc(size: usize) -> *mut c_void;
    fn pg_detoast_datum_packed(datum: *mut c_void) -> *mut c_void;

    fn errstart(elevel: c_int, domain: *const c_char) -> bool;
    fn errmsg(fmt: *const c_char, ...) -> c_int;
    fn errfinish(filename: *const c_char, lineno: c_int, funcname: *const c_char);

    fn init_MultiFuncCall(fcinfo: pg_func_call_info) -> *mut FuncCallContext;
    fn per_MultiFuncCall(fcinfo: pg_func_call_info) -> *mut FuncCallContext;
    fn end_MultiFuncCall(fcinfo: pg_func_call_info, funcctx: *mut FuncCallContext);
    fn get_call_result_type(
        fcinfo: pg_func_call_info,
        result_type_id: *mut Oid,
        result_tuple_desc: *mut *mut c_void,
    ) -> c_int;
    fn TupleDescGetAttInMetadata(tupdesc: *mut c_void) -> *mut c_void;
    fn BuildTupleFromCStrings(
        attinmeta: *mut c_void,
        values: *mut *mut c_char,
    ) -> *mut HeapTupleData;
    fn HeapTupleHeaderGetDatum(tuple: *mut c_void) -> Datum;
    fn MemoryContextRegisterResetCallback(context: *mut c_void, cb: *mut MemoryContextCallback);
}

/// Copy `s` into a NUL-terminated `palloc`'d string (interior NULs are dropped).
unsafe fn pstrdup(s: &str) -> *mut c_char {
    let bytes = s.as_bytes();
    let out = palloc(bytes.len() + 1) as *mut u8;
    let mut n = 0;
    for &b in bytes.iter().filter(|&&b| b != 0) {
        *out.add(n) = b;
        n += 1;
    }
    *out.add(n) = 0;
    out as *mut c_char
}

/// `ereport(ERROR, errmsg("%s", msg))`. Never returns: the server longjmps out.
///
/// # Safety
/// No live values with destructors may exist in calling Rust frames.
unsafe fn raise_error(msg: *const c_char) -> ! {
    if errstart(ERROR_LEVEL, ptr::null()) {
        errmsg(c"%s".as_ptr(), msg);
        errfinish(c"raster_h3_pg".as_ptr(), line!() as c_int, ptr::null());
    }
    // errstart never declines ERROR and errfinish never returns from it.
    std::process::abort()
}

/// Run `f`, turning errors and panics into a `palloc`'d message for [`raise_error`].
fn guarded<T>(
    f: impl FnOnce() -> std::result::Result<T, String>,
) -> std::result::Result<T, *mut c_char> {
    let msg = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(v)) => return Ok(v),
        Ok(Err(m)) => m,
        Err(payload) => format!(
            "raster_h3 panicked: {}",
            safe_panic_payload_to_string(payload)
        ),
    };
    Err(unsafe { pstrdup(&msg) })
}

#[inline]
unsafe fn arg(fcinfo: pg_func_call_info, idx: usize) -> Option<Datum> {
    if idx >= (*fcinfo).nargs.max(0) as usize {
        return None;
    }
    // Index through a raw pointer: the server allocates only `nargs` slots.
    let slot = ptr::addr_of!((*fcinfo).args)
        .cast::<NullableDatum>()
        .add(idx)
        .read();
    if slot.isnull {
        None
    } else {
        Some(slot.value)
    }
}

#[inline]
unsafe fn return_null(fcinfo: pg_func_call_info) -> Datum {
    ptr::addr_of_mut!((*fcinfo).isnull).write(true);
    0
}

/// Borrow the payload of a `text`/`bytea` argument (detoasting it if needed).
unsafe fn arg_str<'a>(fcinfo: pg_func_call_info, idx: usize) -> Option<&'a [u8]> {
    let datum = arg(fcinfo, idx)?;
    let p = pg_detoast_datum_packed(datum as *mut c_void) as *const u8;
    let first = *p;
    if first & 0x01 == 0x01 {
        // 1-byte header: total length (including the header byte) in the upper 7 bits.
        let total = (first >> 1) as usize;
        Some(std::slice::from_raw_parts(
            p.add(1),
            total.saturating_sub(1),
        ))
    } else {
        let header = (p as *const u32).read_unaligned();
        let total = ((header >> 2) & 0x3FFF_FFFF) as usize;
        Some(std::slice::from_raw_parts(
            p.add(4),
            total.saturating_sub(4),
        ))
    }
}

/// Build a `palloc`'d varlena (`text`/`bytea`) Datum from `bytes`.
unsafe fn varlena_datum(bytes: &[u8]) -> Datum {
    let total = bytes.len() + 4;
    let p = palloc(total) as *mut u8;
    (p as *mut u32).write_unaligned((total as u32) << 2);
    ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(4), bytes.len());
    p as Datum
}

// =============================================================================
// text / bytea scalar functions
// =============================================================================

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_to_string() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// `h3_to_string(int8) -> text`
///
/// # Safety
/// Called by the PostgreSQL function manager with a valid `fcinfo`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_to_string(fcinfo: pg_func_call_info) -> Datum {
    let Some(cell) = arg(fcinfo, 0) else {
        return return_null(fcinfo);
    };
    let mut buf = [0u8; 16];
    match h3_to_string(cell as u64) {
        Some(s) => {
            let n = s.len().min(16);
            buf[..n].copy_from_slice(&s.as_bytes()[..n]);
            drop(s);
            varlena_datum(&buf[..n])
        }
        None => return_null(fcinfo),
    }
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_string_to_h3() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// `string_to_h3(text) -> int8` (NULL when the text is not hexadecimal).
///
/// # Safety
/// Called by the PostgreSQL function manager with a valid `fcinfo`.
#[no_mangle]
pub unsafe extern "C" fn pg_string_to_h3(fcinfo: pg_func_call_info) -> Datum {
    let parsed = arg_str(fcinfo, 0)
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(try_string_to_h3);
    match parsed {
        Some(v) => v as i64 as Datum,
        None => return_null(fcinfo),
    }
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_is_valid_text() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// `h3_is_valid(text) -> bool`
///
/// # Safety
/// Called by the PostgreSQL function manager with a valid `fcinfo`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_is_valid_text(fcinfo: pg_func_call_info) -> Datum {
    match arg_str(fcinfo, 0) {
        Some(b) => usize::from(std::str::from_utf8(b).map(h3_is_valid_str).unwrap_or(false)),
        None => return_null(fcinfo),
    }
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_to_wkb() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// `h3_to_wkb(int8) -> bytea`
///
/// # Safety
/// Called by the PostgreSQL function manager with a valid `fcinfo`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_to_wkb(fcinfo: pg_func_call_info) -> Datum {
    let Some(cell) = arg(fcinfo, 0) else {
        return return_null(fcinfo);
    };
    match h3_to_wkb_u64(cell as u64) {
        Some(wkb) => {
            let d = varlena_datum(&wkb);
            drop(wkb);
            d
        }
        None => return_null(fcinfo),
    }
}

// =============================================================================
// Value-per-call set-returning functions
// =============================================================================

/// One output row as text values (`None` = SQL NULL) for `BuildTupleFromCStrings`.
type RowText = Vec<Option<String>>;

/// Arguments shared by both aggregate SRFs, borrowed from the call's Datums.
struct SrfArgs<'a> {
    raster_path: Option<&'a [u8]>,
    h3_res: Option<i32>,
    source_crs: Option<&'a [u8]>,
    nodata: Option<f64>,
    bbox: Option<&'a [u8]>,
}

impl SrfArgs<'_> {
    unsafe fn read(fcinfo: pg_func_call_info) -> Self {
        Self {
            raster_path: arg_str(fcinfo, 0),
            h3_res: arg(fcinfo, 1).map(|d| d as i32),
            source_crs: arg_str(fcinfo, 2),
            nodata: arg(fcinfo, 3).map(|d| f64::from_bits(d as u64)),
            bbox: arg_str(fcinfo, 4),
        }
    }

    #[allow(clippy::type_complexity)]
    fn parse(
        &self,
    ) -> std::result::Result<
        (
            String,
            Vec<u8>,
            Option<String>,
            Option<f64>,
            Option<[f64; 4]>,
        ),
        String,
    > {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let path = self
            .raster_path
            .map(text)
            .filter(|s| !s.is_empty())
            .ok_or("raster_path must not be NULL or empty")?;
        let res = match self.h3_res {
            None => 8,
            Some(r) if (0..=15).contains(&r) => r as u8,
            Some(r) => return Err(format!("h3_res must be between 0 and 15, got {r}")),
        };
        let crs = self.source_crs.map(text).filter(|s| !s.is_empty());
        let bbox = match self.bbox.map(text).filter(|s| !s.is_empty()) {
            None => None,
            Some(s) => Some(parse_bbox_str(&s).ok_or_else(|| {
                format!("bbox must be 'min_lon,min_lat,max_lon,max_lat', got '{s}'")
            })?),
        };
        Ok((path, vec![res], crs, self.nodata, bbox))
    }
}

fn float_text(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        v.to_string()
    }
}

fn bytea_text(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn continuous_values(r: PgContinuousRow) -> RowText {
    vec![
        Some(r.h3_index.to_string()),
        Some(r.h3_hex),
        Some(float_text(r.mean)),
        r.stddev.map(float_text),
        Some(float_text(r.count)),
        Some(float_text(r.min)),
        Some(float_text(r.max)),
        Some(float_text(r.sum)),
        Some(r.resolution.to_string()),
        Some(bytea_text(&r.wkb)),
    ]
}

fn categorical_values(r: PgCategoricalRow) -> RowText {
    vec![
        Some(r.h3_index.to_string()),
        Some(r.h3_hex),
        Some(r.majority_class.to_string()),
        Some(float_text(r.majority_fraction)),
        Some(float_text(r.majority_count)),
        Some(r.unique_classes.to_string()),
        Some(float_text(r.total_count)),
        Some(r.histogram),
        Some(r.resolution.to_string()),
        Some(float_text(r.shannon_entropy)),
        Some(bytea_text(&r.wkb)),
    ]
}

/// Drops the Rust SRF state when its multi-call memory context goes away, which covers
/// normal completion, early termination (e.g. `LIMIT`) and error aborts.
unsafe extern "C" fn drop_srf_state<S>(arg: *mut c_void) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(Box::from_raw(arg as *mut S))
    }));
}

/// Copy row values into a `palloc`'d `char *values[]` for `BuildTupleFromCStrings`.
unsafe fn palloc_values(values: &[Option<String>]) -> *mut *mut c_char {
    let arr = palloc(values.len() * size_of::<*mut c_char>()) as *mut *mut c_char;
    for (i, v) in values.iter().enumerate() {
        *arr.add(i) = match v {
            Some(s) => pstrdup(s),
            None => ptr::null_mut(),
        };
    }
    arr
}

unsafe fn set_is_done(fcinfo: pg_func_call_info, is_done: c_int) {
    let rsi = (*fcinfo).resultinfo as *mut ReturnSetInfo;
    if !rsi.is_null() {
        (*rsi).is_done = is_done;
    }
}

/// Generic value-per-call SRF driver (`SRF_FIRSTCALL_INIT` / `SRF_PERCALL_SETUP` /
/// `SRF_RETURN_NEXT` / `SRF_RETURN_DONE`).
unsafe fn run_srf<S: 'static>(
    fcinfo: pg_func_call_info,
    open: fn(&SrfArgs) -> std::result::Result<S, String>,
    next: fn(&mut S) -> std::result::Result<Option<RowText>, String>,
) -> Datum {
    let flinfo = (*fcinfo).flinfo as *mut FmgrInfo;
    if (*flinfo).fn_extra.is_null() {
        let funcctx = init_MultiFuncCall(fcinfo);
        let ctx = (*funcctx).multi_call_memory_ctx;
        let old_ctx = CurrentMemoryContext;
        CurrentMemoryContext = ctx;

        let mut tupdesc: *mut c_void = ptr::null_mut();
        let class = get_call_result_type(fcinfo, ptr::null_mut(), &mut tupdesc);
        if class != TYPEFUNC_COMPOSITE && class != TYPEFUNC_COMPOSITE_DOMAIN {
            raise_error(c"raster_h3 set-returning function must return a composite type".as_ptr());
        }
        (*funcctx).attinmeta = TupleDescGetAttInMetadata(tupdesc);

        let args = SrfArgs::read(fcinfo);
        let opened = guarded(|| open(&args).map(|s| Box::into_raw(Box::new(s))));
        let state = match opened {
            Ok(state) => state,
            Err(msg) => raise_error(msg),
        };
        (*funcctx).user_fctx = state as *mut c_void;

        let cb = palloc(size_of::<MemoryContextCallback>()) as *mut MemoryContextCallback;
        cb.write(MemoryContextCallback {
            func: Some(drop_srf_state::<S>),
            arg: state as *mut c_void,
            next: ptr::null_mut(),
        });
        MemoryContextRegisterResetCallback(ctx, cb);

        CurrentMemoryContext = old_ctx;
    }

    let funcctx = per_MultiFuncCall(fcinfo);
    let state = (*funcctx).user_fctx as *mut S;
    let step = guarded(|| {
        next(&mut *state).map(|row| row.map(|values| unsafe { palloc_values(&values) }))
    });
    match step {
        Ok(Some(values)) => {
            let tuple = BuildTupleFromCStrings((*funcctx).attinmeta, values);
            (*funcctx).call_cntr += 1;
            set_is_done(fcinfo, EXPR_MULTIPLE_RESULT);
            HeapTupleHeaderGetDatum((*tuple).t_data)
        }
        Ok(None) => {
            // Deleting the multi-call context runs `drop_srf_state`.
            end_MultiFuncCall(fcinfo, funcctx);
            set_is_done(fcinfo, EXPR_END_RESULT);
            return_null(fcinfo)
        }
        Err(msg) => raise_error(msg),
    }
}

fn open_continuous(args: &SrfArgs) -> std::result::Result<PgContinuousSrf, String> {
    let (path, res, crs, nodata, bbox) = args.parse()?;
    PgContinuousSrf::open(&path, res, crs, nodata, bbox).map_err(|e| e.to_string())
}

fn next_continuous(srf: &mut PgContinuousSrf) -> std::result::Result<Option<RowText>, String> {
    srf.next_row()
        .map(|row| row.map(continuous_values))
        .map_err(|e| e.to_string())
}

fn open_categorical(args: &SrfArgs) -> std::result::Result<PgCategoricalSrf, String> {
    let (path, res, crs, nodata, bbox) = args.parse()?;
    PgCategoricalSrf::open(&path, res, crs, nodata, bbox).map_err(|e| e.to_string())
}

fn next_categorical(srf: &mut PgCategoricalSrf) -> std::result::Result<Option<RowText>, String> {
    srf.next_row()
        .map(|row| row.map(categorical_values))
        .map_err(|e| e.to_string())
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_raster_continuous_aggregate() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// `h3_raster_continuous_aggregate(text, int4, text, float8, text) -> SETOF record`
///
/// # Safety
/// Called by the PostgreSQL function manager with a valid `fcinfo`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_raster_continuous_aggregate(fcinfo: pg_func_call_info) -> Datum {
    run_srf(fcinfo, open_continuous, next_continuous)
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_raster_categorical_aggregate() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// `h3_raster_categorical_aggregate(text, int4, text, float8, text) -> SETOF record`
///
/// # Safety
/// Called by the PostgreSQL function manager with a valid `fcinfo`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_raster_categorical_aggregate(fcinfo: pg_func_call_info) -> Datum {
    run_srf(fcinfo, open_categorical, next_categorical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_encodings() {
        assert_eq!(float_text(f64::NAN), "NaN");
        assert_eq!(float_text(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(float_text(1.5), "1.5");
        assert_eq!(bytea_text(&[0x01, 0xab]), "\\x01ab");
    }

    #[test]
    fn test_srf_args_validation() {
        let base = SrfArgs {
            raster_path: Some(b"a.tif"),
            h3_res: None,
            source_crs: None,
            nodata: None,
            bbox: None,
        };
        let (path, res, ..) = base.parse().unwrap();
        assert_eq!((path.as_str(), res), ("a.tif", vec![8]));

        assert!(SrfArgs {
            h3_res: Some(16),
            ..base_clone(&base)
        }
        .parse()
        .is_err());
        assert!(SrfArgs {
            bbox: Some(b"1,2,3"),
            ..base_clone(&base)
        }
        .parse()
        .is_err());
        assert!(SrfArgs {
            raster_path: None,
            ..base_clone(&base)
        }
        .parse()
        .is_err());
    }

    fn base_clone<'a>(a: &SrfArgs<'a>) -> SrfArgs<'a> {
        SrfArgs {
            raster_path: a.raster_path,
            h3_res: a.h3_res,
            source_crs: a.source_crs,
            nodata: a.nodata,
            bbox: a.bbox,
        }
    }

    #[test]
    fn test_magic_block_size() {
        #[cfg(pg_magic = "v13")]
        assert_eq!(size_of::<Pg_magic_struct>(), 24);
        #[cfg(pg_magic = "v15")]
        assert_eq!(size_of::<Pg_magic_struct>(), 56);
        #[cfg(pg_magic = "v18")]
        assert_eq!(size_of::<Pg_magic_struct>(), 72);
    }
}
