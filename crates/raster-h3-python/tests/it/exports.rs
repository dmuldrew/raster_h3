//! `raster_h3_py_export_parquet` / `raster_h3_py_export_pmtiles`: error paths return `-1`
//! with `out_err` set and leave no output; successful exports equal the core exporters.

use std::ffi::{c_char, CString};
use std::path::Path;
use std::ptr;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::pmtiles::H3PmtilesTiler;
use raster_h3_python::{raster_h3_py_export_parquet, raster_h3_py_export_pmtiles};

use crate::c_abi::{classes_tif, core_rows, gradient_tif, take_err, Scan};

type Export = fn(Option<&str>, Option<&str>, Option<&str>, bool) -> Result<i64, String>;

fn c(s: Option<&str>) -> Option<CString> {
    s.map(|s| CString::new(s).unwrap())
}

fn p(s: &Option<CString>) -> *const c_char {
    s.as_ref().map_or(ptr::null(), |c| c.as_ptr())
}

fn parquet(
    input: Option<&str>,
    output: Option<&str>,
    res: Option<&str>,
    categorical: bool,
) -> Result<i64, String> {
    let (i, o, r) = (c(input), c(output), c(res));
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let n = raster_h3_py_export_parquet(
            p(&i),
            p(&o),
            p(&r),
            i32::from(categorical),
            ptr::null(),
            1,
            &mut err,
        );
        let msg = take_err(err);
        if n < 0 {
            assert_eq!(n, -1);
            Err(msg.expect("-1 must set out_err"))
        } else {
            assert!(msg.is_none());
            Ok(n)
        }
    }
}

fn pmtiles(
    input: Option<&str>,
    output: Option<&str>,
    res: Option<&str>,
    categorical: bool,
) -> Result<i64, String> {
    let (i, o, r) = (c(input), c(output), c(res));
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let n = raster_h3_py_export_pmtiles(
            p(&i),
            p(&o),
            p(&r),
            i32::from(categorical),
            ptr::null(),
            &mut err,
        );
        let msg = take_err(err);
        if n < 0 {
            assert_eq!(n, -1);
            Err(msg.expect("-1 must set out_err"))
        } else {
            assert!(msg.is_none());
            Ok(n)
        }
    }
}

fn s(p: &Path) -> Option<&str> {
    p.to_str()
}

#[test]
fn export_error_paths_return_minus_one_with_a_message() {
    let (_tmp, tif) = gradient_tif();
    let dir = tempfile::tempdir().unwrap();
    let not_tiff = dir.path().join("not.tif");
    std::fs::write(&not_tiff, b"not a tiff").unwrap();
    let missing = dir.path().join("missing.tif");
    let corrupt = crate::helpers::corrupt_strip_geotiff();
    let pq = dir.path().join("out.parquet");
    let pm = dir.path().join("out.pmtiles");
    // Missing parent directories are created, but not below a regular file.
    let unwritable = not_tiff.join("out");

    for (export, out) in [(parquet as Export, &pq), (pmtiles as Export, &pm)] {
        let e = export(None, s(out), None, false).unwrap_err();
        assert!(e.contains("Missing input_path"), "{e}");
        let e = export(s(&tif), None, None, false).unwrap_err();
        assert!(e.contains("Missing output_path"), "{e}");
        // Empty strings count as missing.
        let e = export(Some(""), s(out), None, false).unwrap_err();
        assert!(e.contains("Missing input_path"), "{e}");

        for categorical in [false, true] {
            assert!(!export(s(&missing), s(out), None, categorical)
                .unwrap_err()
                .is_empty());
            assert!(!export(s(&not_tiff), s(out), None, categorical)
                .unwrap_err()
                .is_empty());
            assert!(!export(s(corrupt.path()), s(out), None, categorical)
                .unwrap_err()
                .is_empty());
            assert!(!export(s(&tif), s(&unwritable), None, categorical)
                .unwrap_err()
                .is_empty());
            assert!(!out.exists(), "a failed export must not leave {out:?}");
        }
    }

    // NULL out_err is tolerated.
    let n = unsafe {
        raster_h3_py_export_parquet(
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
            ptr::null(),
            0,
            ptr::null_mut(),
        )
    };
    assert_eq!(n, -1);
    let n = unsafe {
        raster_h3_py_export_pmtiles(
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
            ptr::null(),
            ptr::null_mut(),
        )
    };
    assert_eq!(n, -1);
}

#[test]
fn successful_exports_report_core_row_counts() {
    let (_tmp, tif) = gradient_tif();
    let (_ctmp, classes) = classes_tif();
    let dir = tempfile::tempdir().unwrap();

    // Parquet: the returned count is the number of core rows; an unparsable resolution
    // list falls back to resolution 8.
    for (path, categorical) in [(&tif, false), (&classes, true)] {
        for (res, cfg_res) in [(Some("7,8"), "7,8"), (Some("x"), "8"), (None, "8")] {
            let out = dir.path().join(format!("{categorical}-{cfg_res}.parquet"));
            let n = parquet(s(path), s(&out), res, categorical).unwrap();
            let scan = Scan {
                resolutions: Some(cfg_res.into()),
                categorical,
                ..Scan::of(path)
            };
            assert_eq!(n as usize, core_rows(&scan.core_batches(), &[]).len());
            assert!(out.exists());
        }
    }

    // PMTiles: byte-identical to the core tiler with the same configuration.
    for (path, categorical) in [(&tif, false), (&classes, true)] {
        let out = dir.path().join(format!("{categorical}.pmtiles"));
        let reference = dir.path().join(format!("{categorical}-core.pmtiles"));
        let n = pmtiles(s(path), s(&out), Some("7,8"), categorical).unwrap();
        let cfg = MultiResolutionConfig::new(vec![7, 8]);
        let core_n = if categorical {
            H3PmtilesTiler::process_categorical_source_to_pmtiles(path, &reference, cfg)
        } else {
            H3PmtilesTiler::process_raster_source_to_pmtiles(path, &reference, cfg)
        }
        .unwrap();
        assert_eq!(n as usize, core_n);
        assert_eq!(
            std::fs::read(&out).unwrap(),
            std::fs::read(&reference).unwrap()
        );
    }
}
