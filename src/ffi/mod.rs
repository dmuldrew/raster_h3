pub mod duckdb_c;
pub mod spatial_detect;

pub use duckdb_c::*;
pub use spatial_detect::*;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// Helper to convert a Rust string slice to a CString raw pointer
pub fn to_c_string(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// Helper to convert a DuckDB allocated C string to a Rust String and free the C string
pub unsafe fn from_duckdb_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    duckdb_free(ptr as *mut std::ffi::c_void);
    Some(s)
}

/// Extracts a human-readable panic message from a panic payload while ensuring
/// payload disposal does not escape with a secondary panic across FFI boundaries.
pub fn safe_panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    let msg_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic payload".to_string()
        }
    }));

    // Dispose of the payload within a catch boundary; if payload's Drop panics, forget secondary payload
    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
    {
        std::mem::forget(secondary);
    }

    msg_res.unwrap_or_else(|secondary| {
        std::mem::forget(secondary);
        "Unknown panic (payload downcast failed)".to_string()
    })
}

/// Extracts a human-readable panic message from a panic payload.
pub fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    safe_panic_payload_to_string(payload)
}

/// Write error message to stderr without aborting on BrokenPipe or other stderr write errors.
pub fn safe_eprintln(prefix: &str, msg: &str) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{prefix}: {msg}");
    }));
}

/// Generic FFI panic guard executing `body` safely and invoking `set_error` on panic.
pub unsafe fn ffi_guard<T, F, E>(context: T, set_error: E, body: F)
where
    F: FnOnce(),
    E: FnOnce(T, *const c_char),
{
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        let msg = safe_panic_payload_to_string(payload);
        let c_msg_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            to_c_string(&format!("Panic in FFI callback: {msg}"))
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match c_msg_res {
            Ok(c_msg) => set_error(context, c_msg.as_ptr()),
            Err(secondary) => {
                std::mem::forget(secondary);
                set_error(context, c"Panic in FFI callback".as_ptr());
            }
        }))
        .map_err(|secondary| {
            std::mem::forget(secondary);
        });
    }
}

/// Guard for DuckDB table function bind callbacks (`duckdb_bind_info`).
pub unsafe fn ffi_bind_guard<F: FnOnce()>(info: duckdb_bind_info, body: F) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let msg = safe_panic_payload_to_string(payload);
            let c_msg = to_c_string(&format!("Panic in bind callback: {msg}"));
            duckdb_bind_set_error(info, c_msg.as_ptr());
        }))
        .map_err(|secondary| {
            std::mem::forget(secondary);
            duckdb_bind_set_error(info, c"Panic in bind callback".as_ptr());
        });
    }
}

/// Guard for DuckDB table function init callbacks (`duckdb_init_info`).
pub unsafe fn ffi_init_guard<F: FnOnce()>(info: duckdb_init_info, body: F) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let msg = safe_panic_payload_to_string(payload);
            let c_msg = to_c_string(&format!("Panic in init callback: {msg}"));
            duckdb_init_set_error(info, c_msg.as_ptr());
        }))
        .map_err(|secondary| {
            std::mem::forget(secondary);
            duckdb_init_set_error(info, c"Panic in init callback".as_ptr());
        });
    }
}

/// Guard for DuckDB table function scan callbacks (`duckdb_function_info`, `duckdb_data_chunk`).
pub unsafe fn ffi_scan_guard<F: FnOnce()>(
    info: duckdb_function_info,
    output: duckdb_data_chunk,
    body: F,
) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let msg = safe_panic_payload_to_string(payload);
            let c_msg = to_c_string(&format!("Panic in scan callback: {msg}"));
            duckdb_function_set_error(info, c_msg.as_ptr());
            duckdb_data_chunk_set_size(output, 0);
        }))
        .map_err(|secondary| {
            std::mem::forget(secondary);
            duckdb_function_set_error(info, c"Panic in scan callback".as_ptr());
            duckdb_data_chunk_set_size(output, 0);
        });
    }
}

/// Guard for DuckDB scalar function callbacks (`duckdb_function_info`).
pub unsafe fn ffi_scalar_guard<F: FnOnce()>(info: duckdb_function_info, body: F) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let msg = safe_panic_payload_to_string(payload);
            let c_msg = to_c_string(&format!("Panic in scalar callback: {msg}"));
            duckdb_scalar_function_set_error(info, c_msg.as_ptr());
        }))
        .map_err(|secondary| {
            std::mem::forget(secondary);
            duckdb_scalar_function_set_error(info, c"Panic in scalar callback".as_ptr());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_panic_payload_to_string() {
        let err_str = std::panic::catch_unwind(|| {
            panic!("test panic string slice");
        })
        .unwrap_err();
        assert_eq!(panic_payload_to_string(err_str), "test panic string slice");

        let err_string = std::panic::catch_unwind(|| {
            panic!("{}", format!("test formatted panic {}", 42));
        })
        .unwrap_err();
        assert_eq!(
            panic_payload_to_string(err_string),
            "test formatted panic 42"
        );
    }

    #[test]
    fn test_ffi_guard_catches_panics() {
        let mut captured_error = String::new();
        unsafe {
            ffi_guard(
                &mut captured_error,
                |ctx, err_ptr| {
                    if !err_ptr.is_null() {
                        *ctx = CStr::from_ptr(err_ptr).to_string_lossy().into_owned();
                    }
                },
                || {
                    panic!("simulated panic");
                },
            );
        }
        assert!(captured_error.contains("Panic in FFI callback: simulated panic"));
    }

    struct DoublePanicPayload;
    impl Drop for DoublePanicPayload {
        fn drop(&mut self) {
            panic!("secondary panic in payload drop");
        }
    }

    #[test]
    fn test_safe_panic_payload_to_string_secondary_panic_containment() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(DoublePanicPayload);
        let msg = safe_panic_payload_to_string(payload);
        assert_eq!(msg, "Unknown panic payload");
    }
}
