//! Rust and C-ABI SRF paths compared with core batch readers: the categorical SRF with
//! NoData/bbox/CRS options, and the `pg_srf_continuous_*` lifecycle including errors.

use std::collections::BTreeMap;
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::{
    CategoricalBatchReader, CategoricalFormat, ColumnArray, ContinuousBatchReader, RecordBatch,
};
use raster_h3_pg::{
    pg_srf_continuous_close, pg_srf_continuous_next, pg_srf_continuous_open, PgCContinuousRow,
    PgCategoricalRow, PgCategoricalSrf,
};
use tempfile::NamedTempFile;

use crate::helpers::TestGeoTiffBuilder;

/// 48x48 u8 classes 1..=5 in 4-pixel blocks, 0.001° pixels.
pub fn classes_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(48, 48)
        .pixel_size(0.001)
        .create_tempfile(|c, r| ((c / 4 + r / 4) % 5 + 1) as u8)
}

/// 32x32 f32 gradient with values 1..=10 at 0.001° pixels.
pub fn gradient_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(32, 32)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| ((c + r) % 10 + 1) as f32)
}

/// Floats keep 12 significant digits: shard merge order may differ between runs.
pub fn f(v: f64) -> String {
    format!("{v:.11e}")
}

fn batches_of(
    reader: impl Iterator<Item = raster_h3_core::error::Result<RecordBatch>>,
) -> Vec<RecordBatch> {
    reader.map(|b| b.unwrap()).collect()
}

fn col<'a>(b: &'a RecordBatch, name: &str) -> &'a ColumnArray {
    b.column_by_name(name).unwrap()
}

fn f64s<'a>(b: &'a RecordBatch, name: &str) -> &'a [f64] {
    match col(b, name) {
        ColumnArray::Float64 { values, .. } => values,
        other => panic!("{name}: {:?}", other.data_type()),
    }
}

fn i64s<'a>(b: &'a RecordBatch, name: &str) -> &'a [i64] {
    match col(b, name) {
        ColumnArray::Int64 { values, .. } => values,
        other => panic!("{name}: {:?}", other.data_type()),
    }
}

fn cells(b: &RecordBatch) -> &[u64] {
    match col(b, "h3_index") {
        ColumnArray::UInt64 { values, .. } => values,
        other => panic!("h3_index: {:?}", other.data_type()),
    }
}

/// A categorical row in text form, keyed by `h3_index`, as PostgreSQL would print it.
pub type CatRow = Vec<String>;

pub fn pg_categorical(r: &PgCategoricalRow) -> (i64, CatRow) {
    (
        r.h3_index,
        vec![
            r.h3_hex.clone(),
            r.majority_class.to_string(),
            f(r.majority_fraction),
            f(r.majority_count),
            r.unique_classes.to_string(),
            f(r.total_count),
            r.histogram.clone(),
            r.resolution.to_string(),
            f(r.shannon_entropy),
            format!("{:?}", r.wkb),
        ],
    )
}

pub fn core_categorical(path: &Path, cfg: &MultiResolutionConfig) -> BTreeMap<i64, CatRow> {
    let batches = batches_of(
        CategoricalBatchReader::open(
            &[path.to_path_buf()],
            cfg,
            CategoricalFormat::Wide,
            None,
            2048,
            None,
        )
        .unwrap(),
    );
    let mut out = BTreeMap::new();
    for b in &batches {
        let (ColumnArray::Utf8 { values: hex, .. }, ColumnArray::Utf8 { values: hist, .. }) =
            (col(b, "h3_hex"), col(b, "histogram"))
        else {
            panic!("h3_hex/histogram must be Utf8");
        };
        let (ColumnArray::UInt8 { values: res, .. }, ColumnArray::Binary { values: wkb, .. }) =
            (col(b, "resolution"), col(b, "wkb"))
        else {
            panic!("resolution/wkb types");
        };
        for i in 0..b.num_rows {
            out.insert(
                cells(b)[i] as i64,
                vec![
                    hex[i].clone(),
                    i64s(b, "majority_class")[i].to_string(),
                    f(f64s(b, "majority_fraction")[i]),
                    f(f64s(b, "majority_count")[i]),
                    i64s(b, "unique_classes")[i].to_string(),
                    f(f64s(b, "total_count")[i]),
                    hist[i].clone(),
                    res[i].to_string(),
                    f(f64s(b, "shannon_entropy")[i]),
                    format!("{:?}", wkb[i]),
                ],
            );
        }
    }
    out
}

/// `h3_index -> [mean, stddev, count, min, max, sum, resolution]` from the core reader.
fn core_continuous(path: &Path, cfg: &MultiResolutionConfig) -> BTreeMap<i64, Vec<String>> {
    let batches = batches_of(
        ContinuousBatchReader::open(&[path.to_path_buf()], cfg, None, 2048, None).unwrap(),
    );
    let mut out = BTreeMap::new();
    for b in &batches {
        let ColumnArray::Float64 {
            values: sd,
            validity: sd_valid,
        } = col(b, "stddev")
        else {
            panic!("stddev must be Float64");
        };
        let ColumnArray::UInt8 { values: res, .. } = col(b, "resolution") else {
            panic!("resolution must be UInt8");
        };
        for i in 0..b.num_rows {
            let stddev = if sd_valid.as_ref().is_some_and(|v| !v[i]) || !sd[i].is_finite() {
                "NULL".to_string()
            } else {
                f(sd[i])
            };
            out.insert(
                cells(b)[i] as i64,
                vec![
                    f(f64s(b, "mean")[i]),
                    stddev,
                    f(f64s(b, "count")[i]),
                    f(f64s(b, "min")[i]),
                    f(f64s(b, "max")[i]),
                    f(f64s(b, "sum")[i]),
                    res[i].to_string(),
                ],
            );
        }
    }
    out
}

#[test]
fn categorical_srf_matches_core_with_options() {
    let (_tmp, path) = classes_tif();
    let source = path.to_str().unwrap();
    let bbox = [-122.45, 37.775, -122.425, 37.795];

    type Case<'a> = (Vec<u8>, Option<&'a str>, Option<f64>, Option<[f64; 4]>);
    let cases: [Case; 4] = [
        (vec![8], None, None, None),
        (vec![7, 8], Some("EPSG:4326"), None, None),
        (vec![8], None, Some(3.0), None),
        (vec![9], None, Some(1.0), Some(bbox)),
    ];
    let mut sizes = Vec::new();
    for (res, crs, nodata, bbox) in cases {
        let srf = PgCategoricalSrf::open(source, res.clone(), crs.map(String::from), nodata, bbox)
            .unwrap();
        let got: BTreeMap<i64, CatRow> = srf.map(|r| pg_categorical(&r.unwrap())).collect();

        let mut cfg = MultiResolutionConfig::new(res.clone());
        cfg.custom_crs = crs.map(String::from);
        cfg.custom_nodata = nodata;
        cfg.bbox = bbox;
        let want = core_categorical(&path, &cfg);
        assert!(!want.is_empty());
        assert_eq!(
            got, want,
            "res {res:?} crs {crs:?} nodata {nodata:?} bbox {bbox:?}"
        );
        if let Some(nd) = nodata {
            let key = format!("\"{nd}\"");
            assert!(
                got.values().all(|r| !r[6].contains(&key)),
                "class {nd} kept"
            );
        }
        sizes.push(got.len());
    }
    // A bbox at a finer resolution still covers fewer cells than the whole raster would.
    let whole_res9 = PgCategoricalSrf::open(source, vec![9], None, Some(1.0), None)
        .unwrap()
        .count();
    assert!(sizes[3] < whole_res9);

    // Empty resolutions default to 8.
    let default_res = PgCategoricalSrf::open(source, vec![], None, None, None)
        .unwrap()
        .map(|r| pg_categorical(&r.unwrap()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        default_res,
        core_categorical(&path, &MultiResolutionConfig::new(vec![8]))
    );

    assert!(PgCategoricalSrf::open("/no/such/raster.tif", vec![8], None, None, None).is_err());
    let corrupt = crate::helpers::corrupt_strip_geotiff();
    let failed: Result<Vec<_>, _> =
        PgCategoricalSrf::open(corrupt.path().to_str().unwrap(), vec![8], None, None, None)
            .unwrap()
            .collect();
    assert!(failed.is_err());
}

struct CSrf {
    path: CString,
    res: Option<CString>,
    crs: Option<CString>,
    bbox: Option<CString>,
    nodata: Option<f64>,
}

fn opt(s: &Option<CString>) -> *const c_char {
    s.as_ref().map_or(ptr::null(), |c| c.as_ptr())
}

impl CSrf {
    fn new(path: &Path) -> Self {
        Self {
            path: CString::new(path.to_str().unwrap()).unwrap(),
            res: None,
            crs: None,
            bbox: None,
            nodata: None,
        }
    }

    /// Drains the C-ABI SRF into the same shape as [`core_continuous`].
    fn rows(&self) -> Result<BTreeMap<i64, Vec<String>>, String> {
        unsafe {
            let mut err: *mut c_char = ptr::null_mut();
            let srf = pg_srf_continuous_open(
                self.path.as_ptr(),
                opt(&self.res),
                opt(&self.crs),
                i32::from(self.nodata.is_some()),
                self.nodata.unwrap_or(0.0),
                opt(&self.bbox),
                &mut err,
            );
            if srf.is_null() {
                assert!(!err.is_null(), "NULL handle must set out_err");
                return Err(CString::from_raw(err).into_string().unwrap());
            }
            assert!(err.is_null());
            let mut out = BTreeMap::new();
            let mut row: PgCContinuousRow = std::mem::zeroed();
            let status = loop {
                match pg_srf_continuous_next(srf, &mut row) {
                    1 => {}
                    rc => break rc,
                }
                let hex = CStr::from_bytes_until_nul(&row.h3_hex)
                    .unwrap()
                    .to_str()
                    .unwrap();
                assert_eq!(u64::from_str_radix(hex, 16).unwrap(), row.h3_index as u64);
                let wkb = raster_h3_core::scalar_ops::h3_to_wkb_u64(row.h3_index as u64).unwrap();
                assert_eq!(&row.wkb[..row.wkb_len], &wkb[..], "{hex}");
                out.insert(
                    row.h3_index,
                    vec![
                        f(row.mean),
                        if row.has_stddev == 1 {
                            f(row.stddev)
                        } else {
                            "NULL".into()
                        },
                        f(row.count),
                        f(row.min),
                        f(row.max),
                        f(row.sum),
                        row.resolution.to_string(),
                    ],
                );
            };
            // EOF stays EOF.
            if status == 0 {
                assert_eq!(pg_srf_continuous_next(srf, &mut row), 0);
            }
            pg_srf_continuous_close(srf);
            match status {
                0 => Ok(out),
                -1 => Err("next returned -1".into()),
                rc => panic!("unexpected pg_srf_continuous_next status {rc}"),
            }
        }
    }
}

#[test]
fn continuous_c_abi_srf_matches_core_with_options() {
    let (_tmp, path) = gradient_tif();

    let plain = CSrf::new(&path).rows().unwrap();
    assert_eq!(
        plain,
        core_continuous(&path, &MultiResolutionConfig::new(vec![8]))
    );

    let opts = CSrf {
        res: Some(CString::new("8,9").unwrap()),
        crs: Some(CString::new("EPSG:4326").unwrap()),
        bbox: Some(CString::new("-122.45,37.78,-122.435,37.795").unwrap()),
        nodata: Some(5.0),
        ..CSrf::new(&path)
    };
    let mut cfg = MultiResolutionConfig::new(vec![8, 9]);
    cfg.custom_crs = Some("EPSG:4326".into());
    cfg.bbox = Some([-122.45, 37.78, -122.435, 37.795]);
    cfg.custom_nodata = Some(5.0);
    let got = opts.rows().unwrap();
    assert_eq!(got, core_continuous(&path, &cfg));
    assert!(got.values().any(|r| r[6] == "9"));

    let bad_bbox = CSrf {
        bbox: Some(CString::new("1,2,3").unwrap()),
        ..CSrf::new(&path)
    };
    let err = bad_bbox.rows().unwrap_err();
    assert!(err.contains("bbox"), "{err}");
}

#[test]
fn continuous_c_abi_srf_errors() {
    let dir = tempfile::tempdir().unwrap();
    let missing = CSrf::new(&dir.path().join("missing.tif"))
        .rows()
        .unwrap_err();
    assert!(!missing.is_empty());

    let not_tiff = dir.path().join("x.tif");
    std::fs::write(&not_tiff, b"not a tiff").unwrap();
    assert!(!CSrf::new(&not_tiff).rows().unwrap_err().is_empty());

    // A corrupt strip fails part-way: next() returns -1 rather than a short EOF.
    let corrupt = crate::helpers::corrupt_strip_geotiff();
    assert_eq!(
        CSrf::new(corrupt.path()).rows().unwrap_err(),
        "next returned -1"
    );

    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let srf = pg_srf_continuous_open(
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
            0.0,
            ptr::null(),
            &mut err,
        );
        assert!(srf.is_null());
        let msg = CString::from_raw(err).into_string().unwrap();
        assert!(msg.contains("Missing raster_path"), "{msg}");
        // NULL out_err is tolerated.
        assert!(pg_srf_continuous_open(
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
            0.0,
            ptr::null(),
            ptr::null_mut()
        )
        .is_null());

        let mut row: PgCContinuousRow = std::mem::zeroed();
        assert_eq!(pg_srf_continuous_next(ptr::null_mut(), &mut row), -1);
        let (_tmp, path) = gradient_tif();
        let c = CString::new(path.to_str().unwrap()).unwrap();
        let srf = pg_srf_continuous_open(
            c.as_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            0.0,
            ptr::null(),
            ptr::null_mut(),
        );
        assert!(!srf.is_null());
        assert_eq!(pg_srf_continuous_next(srf, ptr::null_mut()), -1);
        // Closing before EOF and closing NULL are both fine.
        assert_eq!(pg_srf_continuous_next(srf, &mut row), 1);
        pg_srf_continuous_close(srf);
        pg_srf_continuous_close(ptr::null_mut());
    }
}
