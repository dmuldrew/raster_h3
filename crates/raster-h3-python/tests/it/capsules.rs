//! Native `PyCapsule` destructors, driven through stand-ins for the two CPython
//! functions the Python package registers.

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use raster_h3_core::columnar::{
    ArrowArrayStream, ArrowDataType, ArrowSchema, ColumnField, ColumnSchema, RecordBatch,
};
use raster_h3_core::error::Result;
use raster_h3_python::{
    raster_h3_py_schema_capsule_destructor, raster_h3_py_set_capsule_api,
    raster_h3_py_stream_capsule_destructor,
};

/// The registered capsule API is process-wide; tests that change it take this lock.
static API: Mutex<()> = Mutex::new(());

/// What a `PyCapsule` holds, as far as the destructors can see.
struct FakeCapsule {
    name: CString,
    payload: *mut c_void,
}

unsafe extern "C" fn get_name(capsule: *mut c_void) -> *const c_char {
    // SAFETY: the tests only pass pointers to live `FakeCapsule`s.
    unsafe { (*(capsule as *const FakeCapsule)).name.as_ptr() }
}

/// Like `PyCapsule_GetPointer`, hands out the payload only for the capsule's own name.
unsafe extern "C" fn get_pointer(capsule: *mut c_void, name: *const c_char) -> *mut c_void {
    // SAFETY: as in `get_name`.
    let capsule = unsafe { &*(capsule as *const FakeCapsule) };
    if name == capsule.name.as_ptr() {
        capsule.payload
    } else {
        ptr::null_mut()
    }
}

fn register_fakes() {
    // SAFETY: the stand-ins have the registered functions' signatures.
    unsafe {
        raster_h3_py_set_capsule_api(get_name as *const c_void, get_pointer as *const c_void);
    }
}

fn schema() -> Arc<ColumnSchema> {
    Arc::new(ColumnSchema::new(vec![ColumnField::new(
        "h3_index",
        ArrowDataType::UInt64,
        false,
    )]))
}

/// Sets its flag when the stream that owns it is released.
struct Released(Arc<AtomicBool>);

impl Drop for Released {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Iterator for Released {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

/// A boxed stream as the `create_arrow_stream*` functions hand it out, and its flag.
fn boxed_stream() -> (*mut c_void, Arc<AtomicBool>) {
    let released = Arc::new(AtomicBool::new(false));
    let stream = ArrowArrayStream::from_iterator(schema(), Released(Arc::clone(&released)));
    (Box::into_raw(Box::new(stream)) as *mut c_void, released)
}

#[test]
fn stream_destructor_releases_the_payload_under_its_current_name() {
    let _api = API.lock().unwrap();
    register_fakes();
    // A consumer that took ownership may have renamed the capsule.
    for name in ["arrow_array_stream", "used_arrow_array_stream"] {
        let (payload, released) = boxed_stream();
        let mut capsule = FakeCapsule {
            name: CString::new(name).unwrap(),
            payload,
        };
        // SAFETY: `capsule` is live and its payload is an unfreed boxed stream.
        unsafe {
            raster_h3_py_stream_capsule_destructor(&mut capsule as *mut FakeCapsule as *mut c_void);
        }
        assert!(released.load(Ordering::SeqCst), "{name}");
    }
}

#[test]
fn schema_destructor_frees_the_payload() {
    let _api = API.lock().unwrap();
    register_fakes();
    let payload = Box::into_raw(Box::new(ArrowSchema::from_column_schema(&schema())));
    let mut capsule = FakeCapsule {
        name: CString::new("arrow_schema").unwrap(),
        payload: payload as *mut c_void,
    };
    // SAFETY: `capsule` is live and its payload is an unfreed boxed schema. Miri and
    // the leak checker see the box freed exactly once.
    unsafe {
        raster_h3_py_schema_capsule_destructor(&mut capsule as *mut FakeCapsule as *mut c_void);
    }
}

#[test]
fn destructors_are_inert_without_a_capsule_or_a_registered_api() {
    let _api = API.lock().unwrap();
    // SAFETY: null addresses are allowed and leave the destructors inert.
    unsafe { raster_h3_py_set_capsule_api(ptr::null(), ptr::null()) };
    let (payload, released) = boxed_stream();
    let mut capsule = FakeCapsule {
        name: CString::new("arrow_array_stream").unwrap(),
        payload,
    };
    // SAFETY: without a registered API the capsule is never dereferenced.
    unsafe {
        raster_h3_py_stream_capsule_destructor(&mut capsule as *mut FakeCapsule as *mut c_void);
        raster_h3_py_schema_capsule_destructor(&mut capsule as *mut FakeCapsule as *mut c_void);
    }
    assert!(!released.load(Ordering::SeqCst));

    register_fakes();
    // SAFETY: a null capsule is never dereferenced.
    unsafe {
        raster_h3_py_stream_capsule_destructor(ptr::null_mut());
        raster_h3_py_schema_capsule_destructor(ptr::null_mut());
    }
    assert!(!released.load(Ordering::SeqCst));

    // The stream is still ours to free.
    // SAFETY: `payload` came from `Box::into_raw` above and was not freed.
    drop(unsafe { Box::from_raw(payload as *mut ArrowArrayStream) });
    assert!(released.load(Ordering::SeqCst));
}
