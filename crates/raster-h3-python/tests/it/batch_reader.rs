//! `raster_h3_py_open_batch_reader` / `_batch_reader_next` / `_free_batch_reader`:
//! incremental single-batch Arrow streams compared with core readers.

use std::ffi::c_char;
use std::ptr;

use raster_h3_core::columnar::RecordBatch;
use raster_h3_python::{
    raster_h3_py_batch_reader_next, raster_h3_py_free_arrow_stream, raster_h3_py_free_batch_reader,
    raster_h3_py_open_batch_reader, PyBatchReader,
};

use crate::c_abi::{classes_tif, core_rows, gradient_tif, take_err, Scan};

/// Opens a reader for `scan`, returning it or the `out_err` message.
fn open(scan: &Scan, batch_size: i64) -> Result<*mut PyBatchReader, String> {
    let a = scan.c_args();
    // SAFETY: string arguments are live `CString`s owned by this frame (or null), and `err`
    // is a writable out-pointer whose string `take_err` frees.
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let reader = raster_h3_py_open_batch_reader(
            a.ptr(0),
            a.ptr(1),
            i32::from(scan.categorical),
            a.ptr(2),
            a.ptr(3),
            i32::from(scan.nodata.is_some()),
            scan.nodata.unwrap_or(0.0),
            a.ptr(4),
            a.ptr(5),
            scan.limit.unwrap_or(-1),
            batch_size,
            &mut err,
        );
        if reader.is_null() {
            Err(take_err(err).expect("NULL reader must set out_err"))
        } else {
            assert!(err.is_null());
            Ok(reader)
        }
    }
}

/// Drains `reader`: every `next` call yields a stream holding exactly one batch.
fn drain(reader: *mut PyBatchReader) -> Result<Vec<RecordBatch>, String> {
    let mut out = Vec::new();
    // SAFETY: `reader` is a live reader from `open` (not freed until the caller is done),
    // each returned stream is a fresh, valid box drained once and freed once, and
    // `take_err` frees any error string.
    unsafe {
        loop {
            let mut err: *mut c_char = ptr::null_mut();
            let stream = raster_h3_py_batch_reader_next(reader, &mut err);
            if stream.is_null() {
                return match take_err(err) {
                    Some(msg) => Err(msg),
                    None => Ok(out),
                };
            }
            let (_schema, batches) = (*stream).collect_batches().unwrap();
            raster_h3_py_free_arrow_stream(stream);
            assert_eq!(batches.len(), 1, "each next() stream carries one batch");
            out.extend(batches);
        }
    }
}

#[test]
fn batches_are_bounded_and_match_core() {
    let (_tmp, path) = gradient_tif();
    let scan = Scan {
        resolutions: Some("9".into()),
        crs: Some("EPSG:4326".into()),
        nodata: Some(5.0),
        ..Scan::of(&path)
    };
    let cols = ["mean", "count", "min", "max", "sum", "resolution"];
    let expected = core_rows(&scan.core_batches(), &cols);
    assert!(expected.len() > 7);

    let reader = open(&scan, 7).unwrap();
    let batches = drain(reader).unwrap();
    // Draining past the end keeps returning NULL without an error.
    assert!(drain(reader).unwrap().is_empty());
    // SAFETY: `reader` came from `open` and is freed exactly once, after its last use.
    unsafe { raster_h3_py_free_batch_reader(reader) };

    assert!(batches.len() > 1);
    assert!(batches.iter().all(|b| b.num_rows > 0 && b.num_rows <= 7));
    assert_eq!(core_rows(&batches, &cols), expected);

    // batch_size <= 0 falls back to the default (one batch for this small raster).
    let reader = open(&scan, 0).unwrap();
    let batches = drain(reader).unwrap();
    // SAFETY: `reader` came from `open` and is freed exactly once, after its last use.
    unsafe { raster_h3_py_free_batch_reader(reader) };
    assert_eq!(batches.len(), 1);
    assert_eq!(core_rows(&batches, &cols), expected);
}

#[test]
fn projection_limit_and_categorical_long() {
    let (_tmp, path) = classes_tif();
    let scan = Scan {
        categorical: true,
        format: Some("long".into()),
        columns: Some("h3_index,category,count".into()),
        limit: Some(5),
        ..Scan::of(&path)
    };
    let reader = open(&scan, 2).unwrap();
    let batches = drain(reader).unwrap();
    // SAFETY: `reader` came from `open` and is freed exactly once, after its last use.
    unsafe { raster_h3_py_free_batch_reader(reader) };

    let names: Vec<&str> = batches[0]
        .schema
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(names, ["h3_index", "category", "count"]);
    assert_eq!(batches.iter().map(|b| b.num_rows).sum::<usize>(), 5);
    let all = core_rows(&scan.core_batches(), &["count"]);
    for (k, v) in core_rows(&batches, &["count"]) {
        assert_eq!(all.get(&k), Some(&v), "row {k} not in core output");
    }
}

#[test]
fn open_and_next_errors() {
    let (_tmp, path) = gradient_tif();

    let missing = open(&Scan::default(), 0).unwrap_err();
    assert!(missing.contains("Missing input raster path"), "{missing}");
    let bad_col = open(
        &Scan {
            columns: Some("wkb,bogus".into()),
            ..Scan::of(&path)
        },
        0,
    )
    .unwrap_err();
    assert!(bad_col.contains("bogus"), "{bad_col}");
    let dir = tempfile::tempdir().unwrap();
    assert!(!open(&Scan::of(&dir.path().join("none.tif")), 0)
        .unwrap_err()
        .is_empty());

    // A NULL reader is reported, not dereferenced; freeing NULL is a no-op.
    // SAFETY: a null reader is rejected before any dereference, `err` is writable, and
    // freeing null is a no-op.
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        assert!(raster_h3_py_batch_reader_next(ptr::null_mut(), &mut err).is_null());
        assert_eq!(take_err(err).as_deref(), Some("batch reader is NULL"));
        raster_h3_py_free_batch_reader(ptr::null_mut());
    }

    // A decode failure part-way through surfaces from next() as an error.
    let corrupt = crate::helpers::corrupt_strip_geotiff();
    let reader = open(&Scan::of(corrupt.path()), 1).unwrap();
    let failed = drain(reader);
    // SAFETY: `reader` came from `open` and is freed exactly once, after its last use.
    unsafe { raster_h3_py_free_batch_reader(reader) };
    assert!(failed.is_err(), "corrupt strip must not end as a clean EOF");

    // Freeing a reader before draining it releases it without hanging.
    let reader = open(&Scan::of(&path), 1).unwrap();
    // SAFETY: `reader` is a live reader from `open` (not freed until the caller is done),
    // each returned stream is a fresh, valid box drained once and freed once, and
    // `take_err` frees any error string.
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let first = raster_h3_py_batch_reader_next(reader, &mut err);
        assert!(!first.is_null());
        raster_h3_py_free_arrow_stream(first);
        raster_h3_py_free_batch_reader(reader);
    }
}
