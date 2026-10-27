//! The `_ex` scan entry points: lenient scans report dropped samples, strict
//! scans fail with a `strict_transforms` error.

use std::ffi::c_char;
use std::ptr;

use raster_h3_python::{
    raster_h3_py_batch_reader_next, raster_h3_py_batch_reader_transform_failures,
    raster_h3_py_create_arrow_stream_ex, raster_h3_py_free_arrow_stream,
    raster_h3_py_free_batch_reader, raster_h3_py_free_string, raster_h3_py_open_batch_reader_ex,
    raster_h3_py_scan_json_ex,
};

use crate::c_abi::{take_err, Scan};

/// 10 x 20 one-degree pixels from 95N: the top 5 rows (50 pixels) lie beyond the pole.
fn past_pole_scan() -> (tempfile::NamedTempFile, Scan) {
    let (tmp, path) = crate::helpers::TestGeoTiffBuilder::new(10, 20)
        .origin(0.0, 95.0)
        .pixel_size(1.0)
        .create_constant_tempfile(3.0f32);
    let scan = Scan {
        resolutions: Some("3".into()),
        ..Scan::of(&path)
    };
    (tmp, scan)
}

/// Drains a batch reader; returns (transform failures, first error).
fn batch_reader(scan: &Scan, strict: bool) -> (u64, Option<String>) {
    let a = scan.c_args();
    // SAFETY: string arguments are live `CString`s owned by this frame (or null); `err`
    // is a writable out-pointer whose string `take_err` frees; the reader and each
    // stream it returns are freed exactly once.
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let reader = raster_h3_py_open_batch_reader_ex(
            a.ptr(0),
            a.ptr(1),
            0,
            a.ptr(2),
            a.ptr(3),
            0,
            0.0,
            a.ptr(4),
            a.ptr(5),
            -1,
            64,
            i32::from(strict),
            &mut err,
        );
        assert!(!reader.is_null(), "{:?}", take_err(err));
        let error = loop {
            let mut err: *mut c_char = ptr::null_mut();
            let stream = raster_h3_py_batch_reader_next(reader, &mut err);
            if stream.is_null() {
                break take_err(err);
            }
            raster_h3_py_free_arrow_stream(stream);
        };
        let failures = raster_h3_py_batch_reader_transform_failures(reader);
        raster_h3_py_free_batch_reader(reader);
        (failures, error)
    }
}

#[test]
fn batch_reader_counts_or_rejects_transform_failures() {
    let (_tmp, scan) = past_pole_scan();
    assert_eq!(batch_reader(&scan, false), (50, None));
    let (failures, err) = batch_reader(&scan, true);
    let err = err.expect("strict reader must fail");
    assert!(err.contains("strict_transforms"), "{err}");
    assert!(failures > 0);
    // SAFETY: a null reader is accepted and reports no failures.
    let null_failures = unsafe { raster_h3_py_batch_reader_transform_failures(ptr::null()) };
    assert_eq!(null_failures, 0);
}

#[test]
fn scan_json_counts_or_rejects_transform_failures() {
    let (_tmp, scan) = past_pole_scan();
    let a = scan.c_args();
    for strict in [false, true] {
        let mut failures = u64::MAX;
        let mut err: *mut c_char = ptr::null_mut();
        // SAFETY: string arguments are live `CString`s owned by this frame (or null);
        // `failures` and `err` are writable out-pointers; the JSON string and any error
        // string are freed exactly once.
        unsafe {
            let json = raster_h3_py_scan_json_ex(
                a.ptr(0),
                a.ptr(1),
                0,
                a.ptr(2),
                a.ptr(3),
                0,
                0.0,
                a.ptr(4),
                a.ptr(5),
                -1,
                i32::from(strict),
                &mut failures,
                &mut err,
            );
            let err = take_err(err);
            if strict {
                assert!(json.is_null());
                let err = err.expect("strict scan must fail");
                assert!(err.contains("strict_transforms"), "{err}");
                assert!(failures > 0);
            } else {
                assert!(!json.is_null(), "{err:?}");
                raster_h3_py_free_string(json);
                assert_eq!(failures, 50);
            }
        }
    }
}

#[test]
fn strict_arrow_stream_fails_while_reading() {
    let (_tmp, scan) = past_pole_scan();
    let a = scan.c_args();
    // SAFETY: string arguments are live `CString`s owned by this frame (or null); `err` is
    // a writable out-pointer; the stream is drained once and freed once.
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let stream = raster_h3_py_create_arrow_stream_ex(
            a.ptr(0),
            a.ptr(1),
            0,
            a.ptr(2),
            a.ptr(3),
            0,
            0.0,
            a.ptr(4),
            a.ptr(5),
            -1,
            1,
            &mut err,
        );
        assert!(!stream.is_null(), "{:?}", take_err(err));
        let result = (*stream).collect_batches();
        raster_h3_py_free_arrow_stream(stream);
        let err = result.expect_err("strict stream must fail").to_string();
        assert!(err.contains("strict_transforms"), "{err}");
    }
}
