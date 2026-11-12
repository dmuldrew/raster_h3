//! `PyCapsule` destructors and the CPython capsule API hooks they call.

use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use raster_h3_core::columnar::{ArrowArrayStream, ArrowSchema};
use raster_h3_core::error::catch_unwind_or;

/// `const char *PyCapsule_GetName(PyObject *)`. Stored as a pointer, not an integer, so it
/// keeps its provenance and can be called again.
static CAPSULE_GET_NAME: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
/// `void *PyCapsule_GetPointer(PyObject *, const char *)`.
static CAPSULE_GET_POINTER: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Register the interpreter's `PyCapsule_GetName` and `PyCapsule_GetPointer`, which
/// [`raster_h3_py_stream_capsule_destructor`] and
/// [`raster_h3_py_schema_capsule_destructor`] need to reach a capsule's payload.
///
/// This library never links libpython, so the Python package passes the two addresses
/// once after loading it. A null address leaves the destructors inert.
///
/// # Safety
/// Each non-null argument must be the address of the named CPython function of the
/// interpreter that will destroy the capsules.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_set_capsule_api(
    get_name: *const c_void,
    get_pointer: *const c_void,
) {
    CAPSULE_GET_NAME.store(get_name.cast_mut(), Ordering::Release);
    CAPSULE_GET_POINTER.store(get_pointer.cast_mut(), Ordering::Release);
}

/// Payload pointer of a live capsule, or null when the capsule API is not registered.
///
/// # Safety
/// `capsule` must be a live `PyCapsule` and the caller must hold the GIL.
unsafe fn capsule_payload(capsule: *mut c_void) -> *mut c_void {
    let get_name = CAPSULE_GET_NAME.load(Ordering::Acquire);
    let get_pointer = CAPSULE_GET_POINTER.load(Ordering::Acquire);
    if capsule.is_null() || get_name.is_null() || get_pointer.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: both addresses were registered through `raster_h3_py_set_capsule_api`,
    // whose contract makes them the CPython functions of these signatures; `capsule`
    // is live and the GIL is held (caller contract).
    unsafe {
        let get_name: unsafe extern "C" fn(*mut c_void) -> *const c_char =
            std::mem::transmute::<*mut c_void, _>(get_name);
        let get_pointer: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void =
            std::mem::transmute::<*mut c_void, _>(get_pointer);
        // A consumer may rename a capsule it took ownership of (e.g. to
        // `used_arrow_array_stream`); looking the payload up by the current name
        // cannot fail, so it leaves the interpreter's error state alone.
        get_pointer(capsule, get_name(capsule))
    }
}

/// `PyCapsule` destructor for an `ArrowArrayStream` capsule: frees the boxed stream,
/// releasing it unless a consumer already moved it out.
///
/// A capsule can be destroyed while an exception is propagating, for example when a
/// consumer raises the stream's own error and then drops the capsule. A destructor
/// written in Python cannot run in that state without clearing the pending exception,
/// which turns the stream's error into a `SystemError`. This one calls nothing that
/// reads or writes the error state.
///
/// # Safety
/// Must only be installed as the destructor of a capsule whose payload came from one
/// of the `raster_h3_py_create_arrow_stream*` functions or
/// `raster_h3_py_batch_reader_next`. CPython calls it with the GIL held.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_stream_capsule_destructor(capsule: *mut c_void) {
    // SAFETY: the capsule is live and the GIL is held (caller contract); its payload
    // is an unfreed boxed stream, freed here exactly once.
    unsafe {
        let payload = capsule_payload(capsule) as *mut ArrowArrayStream;
        if !payload.is_null() {
            let stream = Box::from_raw(payload);
            catch_unwind_or((), || drop(stream));
        }
    }
}

/// `PyCapsule` destructor for an `ArrowSchema` capsule. See
/// [`raster_h3_py_stream_capsule_destructor`].
///
/// # Safety
/// Must only be installed as the destructor of a capsule whose payload came from one
/// of the `raster_h3_py_create_arrow_schema*` functions. CPython calls it with the GIL
/// held.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_schema_capsule_destructor(capsule: *mut c_void) {
    // SAFETY: as in `raster_h3_py_stream_capsule_destructor`, for a boxed schema.
    unsafe {
        let payload = capsule_payload(capsule) as *mut ArrowSchema;
        if !payload.is_null() {
            let schema = Box::from_raw(payload);
            catch_unwind_or((), || drop(schema));
        }
    }
}
