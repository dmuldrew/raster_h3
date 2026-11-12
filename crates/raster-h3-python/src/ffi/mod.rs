// =============================================================================
// C-ABI & PyCapsule Entry Points for Python (`ctypes` / `PyCapsule_New`)
// =============================================================================

mod args;
mod arrow;
mod batch_reader;
mod capsules;
mod exports;
mod json;
mod partitions;
mod scalars;
mod v2;

pub use arrow::*;
pub use batch_reader::*;
pub use capsules::*;
pub use exports::*;
pub use json::*;
pub use partitions::*;
pub use scalars::*;
pub use v2::*;

use std::ffi::{c_char, CStr, CString};
use std::ptr;

use raster_h3_core::error::{safe_panic_payload_to_string, Result};

/// # Safety
/// `ptr` must be null or a valid NUL-terminated C string.
pub(crate) unsafe fn opt_cstr(ptr: *const c_char) -> Option<String> {
    // SAFETY: `ptr` is non-null and NUL-terminated (caller contract); the bytes are copied
    // into an owned `String` before returning.
    unsafe {
        if ptr.is_null() {
            return None;
        }
        let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// # Safety
/// `out_err` must be null or a writable `char **`.
pub(crate) unsafe fn set_c_err(out_err: *mut *mut c_char, msg: &str) {
    // SAFETY: non-null `out_err` is a writable `char **` (caller contract). Ownership of
    // the `CString::into_raw` string passes to the caller, who frees it with
    // `raster_h3_py_free_string`.
    unsafe {
        if !out_err.is_null() {
            let c = CString::new(msg).unwrap_or_default();
            *out_err = c.into_raw();
        }
    }
}

/// Run `f`, converting errors and panics into `out_err` and a null result.
///
/// # Safety
/// `out_err` must be null or a writable `char **`.
pub(crate) unsafe fn ffi_call<T>(
    what: &str,
    out_err: *mut *mut c_char,
    f: impl FnOnce() -> Result<T>,
) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            // SAFETY: `out_err` meets `set_c_err`'s contract (caller contract).
            unsafe { set_c_err(out_err, &e.to_string()) };
            None
        }
        Err(payload) => {
            let msg = safe_panic_payload_to_string(payload);
            // SAFETY: as above.
            unsafe { set_c_err(out_err, &format!("Panic in {what}: {msg}")) };
            None
        }
    }
}

/// Free a C string returned by `raster_h3_py_*`.
///
/// # Safety
/// `ptr` must be null or a pointer returned by `CString::into_raw` from this crate.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_free_string(ptr: *mut c_char) {
    // SAFETY: every string this crate returns comes from `CString::into_raw`; non-null
    // `ptr` is one of them, freed once (caller contract).
    unsafe {
        if !ptr.is_null() {
            drop(CString::from_raw(ptr));
        }
    }
}
