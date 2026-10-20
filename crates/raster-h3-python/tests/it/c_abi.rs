//! Shared C-ABI call helpers, plus `raster_h3_py_scan_json` compared with core readers.

use std::collections::BTreeMap;
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::{
    CategoricalBatchReader, CategoricalFormat, ColumnArray, ContinuousBatchReader, RecordBatch,
};
use raster_h3_python::{raster_h3_py_free_string, raster_h3_py_scan_json};
use tempfile::NamedTempFile;

use crate::helpers::TestGeoTiffBuilder;

/// 32x32 f32 gradient with values 1..=10 at 0.001° pixels.
pub fn gradient_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(32, 32)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| ((c + r) % 10 + 1) as f32)
}

/// 48x48 u8 classes 1..=5 in 4-pixel blocks.
pub fn classes_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(48, 48)
        .pixel_size(0.001)
        .create_tempfile(|c, r| ((c / 4 + r / 4) % 5 + 1) as u8)
}

/// Takes ownership of an `out_err` string set by the library.
///
/// # Safety
/// `err` must be null or a string allocated by `raster_h3_py_*`.
pub unsafe fn take_err(err: *mut c_char) -> Option<String> {
    if err.is_null() {
        return None;
    }
    let msg = CStr::from_ptr(err).to_string_lossy().into_owned();
    raster_h3_py_free_string(err);
    Some(msg)
}

/// Scan arguments in Rust form; `None` passes NULL.
#[derive(Default, Clone)]
pub struct Scan {
    pub path: Option<String>,
    pub resolutions: Option<String>,
    pub categorical: bool,
    pub format: Option<String>,
    pub crs: Option<String>,
    pub nodata: Option<f64>,
    pub bbox: Option<String>,
    pub columns: Option<String>,
    pub limit: Option<i64>,
}

/// The C strings behind a [`Scan`], kept alive for the duration of a call.
pub struct CArgs {
    strings: Vec<Option<CString>>,
}

impl CArgs {
    pub fn ptr(&self, i: usize) -> *const c_char {
        self.strings[i].as_ref().map_or(ptr::null(), |c| c.as_ptr())
    }
}

impl Scan {
    pub fn of(path: &Path) -> Self {
        Self {
            path: Some(path.to_str().unwrap().to_string()),
            ..Default::default()
        }
    }

    /// `[path, resolutions, format, crs, bbox, columns]` as C strings.
    pub fn c_args(&self) -> CArgs {
        let c = |s: &Option<String>| s.as_ref().map(|s| CString::new(s.as_str()).unwrap());
        CArgs {
            strings: vec![
                c(&self.path),
                c(&self.resolutions),
                c(&self.format),
                c(&self.crs),
                c(&self.bbox),
                c(&self.columns),
            ],
        }
    }

    /// Equivalent core configuration (the Python scanner always uses `Cutline`).
    pub fn config(&self) -> MultiResolutionConfig {
        let res = self
            .resolutions
            .as_deref()
            .map(raster_h3_core::scalar_ops::parse_resolutions_str)
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| vec![8]);
        let mut cfg = MultiResolutionConfig::new(res);
        cfg.custom_crs = self.crs.clone();
        cfg.custom_nodata = self.nodata;
        cfg.bbox = self
            .bbox
            .as_deref()
            .and_then(raster_h3_core::scalar_ops::parse_bbox_str);
        cfg
    }

    pub fn format(&self) -> CategoricalFormat {
        match self.format.as_deref() {
            Some(f) if f.eq_ignore_ascii_case("long") => CategoricalFormat::Long,
            _ => CategoricalFormat::Wide,
        }
    }

    /// All rows a core reader produces for this scan (no projection, no limit).
    pub fn core_batches(&self) -> Vec<RecordBatch> {
        let paths = vec![PathBuf::from(self.path.as_ref().unwrap())];
        let cfg = self.config();
        if self.categorical {
            CategoricalBatchReader::open(&paths, &cfg, self.format(), None, 2048, None)
                .unwrap()
                .map(|b| b.unwrap())
                .collect()
        } else {
            ContinuousBatchReader::open(&paths, &cfg, None, 2048, None)
                .unwrap()
                .map(|b| b.unwrap())
                .collect()
        }
    }

    /// Calls `raster_h3_py_scan_json`, returning the parsed object or the error message.
    pub fn json(&self) -> Result<serde_json::Map<String, serde_json::Value>, String> {
        let a = self.c_args();
        unsafe {
            let mut err: *mut c_char = ptr::null_mut();
            let out = raster_h3_py_scan_json(
                a.ptr(0),
                a.ptr(1),
                i32::from(self.categorical),
                a.ptr(2),
                a.ptr(3),
                i32::from(self.nodata.is_some()),
                self.nodata.unwrap_or(0.0),
                a.ptr(4),
                a.ptr(5),
                self.limit.unwrap_or(-1),
                &mut err,
            );
            if out.is_null() {
                return Err(take_err(err).expect("NULL result must set out_err"));
            }
            assert!(err.is_null());
            let text = CStr::from_ptr(out).to_str().unwrap().to_string();
            raster_h3_py_free_string(out);
            match serde_json::from_str(&text).unwrap() {
                serde_json::Value::Object(m) => Ok(m),
                other => panic!("scan_json must return an object, got {other}"),
            }
        }
    }
}

/// Floats keep 12 significant digits: shard merge order may differ between runs.
fn norm(v: serde_json::Value) -> serde_json::Value {
    match v.as_number() {
        Some(n) if n.is_f64() => format!("{:.11e}", n.as_f64().unwrap()).into(),
        _ => v,
    }
}

/// Row key -> JSON values of the remaining columns, from core batches. The key is
/// `h3_index` (plus `category` for long-format rows).
pub fn core_rows(
    batches: &[RecordBatch],
    columns: &[&str],
) -> BTreeMap<String, Vec<serde_json::Value>> {
    let value = |col: &ColumnArray, i: usize| -> serde_json::Value {
        if col.validity().is_some_and(|v| !v[i]) {
            return serde_json::Value::Null;
        }
        match col {
            ColumnArray::UInt64 { values, .. } => values[i].into(),
            ColumnArray::Int64 { values, .. } => values[i].into(),
            ColumnArray::UInt8 { values, .. } => values[i].into(),
            ColumnArray::Float64 { values, .. } if values[i].is_finite() => values[i].into(),
            ColumnArray::Float64 { .. } => serde_json::Value::Null,
            ColumnArray::Boolean { values, .. } => values[i].into(),
            ColumnArray::Utf8 { values, .. } => values[i].clone().into(),
            ColumnArray::Binary { values, .. } => values[i]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
                .into(),
        }
    };
    let mut out = BTreeMap::new();
    for b in batches {
        let key_cols: Vec<&ColumnArray> = ["h3_index", "category"]
            .iter()
            .filter_map(|n| b.column_by_name(n))
            .collect();
        for i in 0..b.num_rows {
            let key = key_cols
                .iter()
                .map(|c| value(c, i).to_string())
                .collect::<Vec<_>>()
                .join("/");
            let vals = columns
                .iter()
                .map(|n| norm(value(b.column_by_name(n).unwrap(), i)))
                .collect();
            out.insert(key, vals);
        }
    }
    out
}

/// Same shape as [`core_rows`], from a columnar JSON object.
pub fn json_rows(
    obj: &serde_json::Map<String, serde_json::Value>,
    columns: &[&str],
) -> BTreeMap<String, Vec<serde_json::Value>> {
    let col = |n: &str| obj[n].as_array().unwrap();
    let n = col("h3_index").len();
    let mut out = BTreeMap::new();
    for i in 0..n {
        let key = ["h3_index", "category"]
            .iter()
            .filter(|k| obj.contains_key(**k))
            .map(|k| col(k)[i].to_string())
            .collect::<Vec<_>>()
            .join("/");
        out.insert(
            key,
            columns.iter().map(|c| norm(col(c)[i].clone())).collect(),
        );
    }
    out
}

#[test]
fn scan_json_matches_core_with_options() {
    let (_tmp, path) = gradient_tif();
    let cols = [
        "h3_hex",
        "mean",
        "stddev",
        "count",
        "min",
        "max",
        "sum",
        "resolution",
        "wkb",
    ];

    let full = Scan {
        resolutions: Some("8,9".into()),
        crs: Some("EPSG:4326".into()),
        ..Scan::of(&path)
    };
    let obj = full.json().unwrap();
    let names: Vec<&String> = obj.keys().collect();
    for c in cols {
        assert!(obj.contains_key(c), "missing {c} in {names:?}");
    }
    assert_eq!(
        json_rows(&obj, &cols),
        core_rows(&full.core_batches(), &cols)
    );

    // NoData and bbox reach the core config.
    let filtered = Scan {
        nodata: Some(5.0),
        bbox: Some("-122.45,37.78,-122.435,37.795".into()),
        ..full.clone()
    };
    let fobj = filtered.json().unwrap();
    let frows = json_rows(&fobj, &cols);
    assert_eq!(frows, core_rows(&filtered.core_batches(), &cols));
    assert!(!frows.is_empty() && frows.len() < json_rows(&obj, &cols).len());

    // Projection keeps only the named columns, in order; limit caps the rows.
    let projected = Scan {
        columns: Some("h3_index, mean".into()),
        limit: Some(4),
        ..full.clone()
    }
    .json()
    .unwrap();
    assert_eq!(
        projected.keys().collect::<Vec<_>>(),
        vec!["h3_index", "mean"]
    );
    let prows = json_rows(&projected, &["mean"]);
    assert_eq!(prows.len(), 4);
    let all = core_rows(&full.core_batches(), &["mean"]);
    for (k, v) in &prows {
        assert_eq!(all.get(k), Some(v));
    }
    let empty = Scan {
        limit: Some(0),
        ..full.clone()
    }
    .json()
    .unwrap();
    assert!(empty["h3_index"].as_array().unwrap().is_empty());
}

#[test]
fn scan_json_categorical_wide_and_long_match_core() {
    let (_tmp, path) = classes_tif();
    let wide = Scan {
        categorical: true,
        ..Scan::of(&path)
    };
    let wcols = [
        "majority_class",
        "majority_fraction",
        "total_count",
        "histogram",
        "resolution",
    ];
    assert_eq!(
        json_rows(&wide.json().unwrap(), &wcols),
        core_rows(&wide.core_batches(), &wcols)
    );

    let long = Scan {
        format: Some("LONG".into()),
        ..wide.clone()
    };
    let lcols = ["count", "fraction", "total_count"];
    let lobj = long.json().unwrap();
    assert!(lobj.contains_key("category"));
    assert_eq!(
        json_rows(&lobj, &lcols),
        core_rows(&long.core_batches(), &lcols)
    );
}

#[test]
fn scan_json_errors_set_out_err() {
    let (_tmp, path) = gradient_tif();
    let missing = Scan::default().json().unwrap_err();
    assert!(missing.contains("Missing input raster path"), "{missing}");

    let bad_col = Scan {
        columns: Some("h3_index,nope".into()),
        ..Scan::of(&path)
    }
    .json()
    .unwrap_err();
    assert!(bad_col.contains("nope"), "{bad_col}");

    let dir = tempfile::tempdir().unwrap();
    let not_tiff = dir.path().join("x.tif");
    std::fs::write(&not_tiff, b"nope").unwrap();
    assert!(!Scan::of(&not_tiff).json().unwrap_err().is_empty());

    let corrupt = crate::helpers::corrupt_strip_geotiff();
    assert!(!Scan::of(corrupt.path()).json().unwrap_err().is_empty());

    // A NULL out_err pointer is tolerated on failure.
    let result = unsafe {
        raster_h3_py_scan_json(
            ptr::null(),
            ptr::null(),
            0,
            ptr::null(),
            ptr::null(),
            0,
            0.0,
            ptr::null(),
            ptr::null(),
            -1,
            ptr::null_mut(),
        )
    };
    assert!(result.is_null());
}
