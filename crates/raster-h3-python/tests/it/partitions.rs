//! Partitioned scans through the C ABI: `raster_h3_py_plan_partitions`, the
//! `_partitioned` stream, and `raster_h3_py_merge_partials`, compared with an unpartitioned
//! core scan.

use std::collections::BTreeMap;
use std::ffi::{c_char, CString};
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use raster_h3_core::columnar::{
    ArrowArrayStream, ArrowDataType, ColumnArray, ColumnField, ColumnSchema, RecordBatch,
    PARTIAL_STATE_COLUMN,
};
use raster_h3_python::{
    raster_h3_py_create_arrow_schema_partitioned, raster_h3_py_create_arrow_stream_partitioned,
    raster_h3_py_free_arrow_schema, raster_h3_py_free_string, raster_h3_py_merge_partials,
    raster_h3_py_plan_partitions,
};

use crate::c_abi::{take_err, Scan};
use crate::helpers::{write_geotiff, Crs, Layout};

const SF: Crs = Crs::Geographic {
    lon: -122.45,
    lat: 37.80,
    pixel_deg: 0.0005,
};

fn gradient(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("gradient.tif");
    write_geotiff(
        &path,
        (160, 120),
        Layout::Tiles(32, 32),
        SF,
        "-9999",
        |c, r| ((c * 7 + r * 3) % 50) as f32 + 1.0,
    );
    path
}

fn classes(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("classes.tif");
    write_geotiff(&path, (160, 120), Layout::Strips(8), SF, "0", |c, r| {
        ((c / 5 + r / 7) % 4 + 1) as u8
    });
    path
}

fn plan(path: &Path, target: i64) -> Result<Vec<String>, String> {
    let path = CString::new(path.to_str().unwrap()).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    // SAFETY: valid C strings or NULL, and a writable error pointer.
    let json = unsafe {
        raster_h3_py_plan_partitions(path.as_ptr(), ptr::null(), ptr::null(), target, &mut err)
    };
    // SAFETY: `err` was set by the library or left NULL.
    if let Some(msg) = unsafe { take_err(err) } {
        return Err(msg);
    }
    // SAFETY: a non-null result is a NUL-terminated string from this library, freed once.
    let text = unsafe { std::ffi::CStr::from_ptr(json) }
        .to_str()
        .unwrap()
        .to_owned();
    // SAFETY: `json` came from this library and is freed exactly once.
    unsafe { raster_h3_py_free_string(json) };
    let plan: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    Ok(plan.iter().map(|p| p.to_string()).collect())
}

/// Scan one partition through the C ABI.
fn scan_partition(scan: &Scan, partition: &str) -> Result<Vec<RecordBatch>, String> {
    let args = scan.c_args();
    let partition = CString::new(partition).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    // SAFETY: valid C strings or NULL, and a writable error pointer.
    let stream = unsafe {
        raster_h3_py_create_arrow_stream_partitioned(
            args.ptr(0),
            args.ptr(1),
            i32::from(scan.categorical),
            args.ptr(2),
            args.ptr(3),
            0,
            0.0,
            args.ptr(4),
            args.ptr(5),
            scan.limit.unwrap_or(-1),
            0,
            partition.as_ptr(),
            &mut err,
        )
    };
    // SAFETY: `err` was set by the library or left NULL.
    if let Some(msg) = unsafe { take_err(err) } {
        return Err(msg);
    }
    // SAFETY: a non-null result is a boxed, unreleased stream owned by us.
    let mut stream = unsafe { Box::from_raw(stream) };
    // SAFETY: `stream` is the valid, unreleased stream returned above.
    unsafe { stream.collect_batches() }
        .map(|(_, b)| b)
        .map_err(|e| e.to_string())
}

/// Merge partial rows through the C ABI. Also checks that the input stream was moved.
fn merge(
    partials: Vec<RecordBatch>,
    schema: Arc<ColumnSchema>,
    categorical: bool,
) -> Result<Vec<RecordBatch>, String> {
    let mut input = ArrowArrayStream::from_iterator(schema, partials.into_iter().map(Ok));
    let mut err: *mut c_char = ptr::null_mut();
    // SAFETY: `input` is a valid stream; the call moves it out.
    let out = unsafe {
        raster_h3_py_merge_partials(&mut input, i32::from(categorical), ptr::null(), &mut err)
    };
    assert!(
        input.release.is_none(),
        "merge must take ownership of its input stream"
    );
    // SAFETY: `err` was set by the library or left NULL.
    if let Some(msg) = unsafe { take_err(err) } {
        return Err(msg);
    }
    // SAFETY: a non-null result is a boxed, unreleased stream owned by us.
    let mut out = unsafe { Box::from_raw(out) };
    // SAFETY: `out` is the valid, unreleased stream returned above.
    unsafe { out.collect_batches() }
        .map(|(_, b)| b)
        .map_err(|e| e.to_string())
}

fn key_schema() -> Arc<ColumnSchema> {
    Arc::new(ColumnSchema::new(vec![
        ColumnField::new("resolution", ArrowDataType::UInt8, false),
        ColumnField::new("h3_index", ArrowDataType::UInt64, false),
        ColumnField::new(PARTIAL_STATE_COLUMN, ArrowDataType::Binary, true),
    ]))
}

/// Values of every column (as JSON) by `(resolution, h3_index)`, excluding `partial_state`.
type Rows = BTreeMap<(u8, u64), Vec<(String, serde_json::Value)>>;

fn value(col: &ColumnArray, i: usize) -> serde_json::Value {
    match col {
        ColumnArray::UInt64 { values, .. } => values[i].into(),
        ColumnArray::Int64 { values, .. } => values[i].into(),
        ColumnArray::UInt8 { values, .. } => values[i].into(),
        ColumnArray::Float64 { values, .. } => serde_json::json!(values[i]),
        ColumnArray::Boolean { values, .. } => values[i].into(),
        ColumnArray::Utf8 { values, .. } => values[i].clone().into(),
        ColumnArray::Binary { values, .. } => serde_json::json!(values[i]),
    }
}

fn add_rows(rows: &mut Rows, batch: &RecordBatch, keep: impl Fn(usize) -> bool) {
    let (
        Some(ColumnArray::UInt8 { values: res, .. }),
        Some(ColumnArray::UInt64 { values: h3, .. }),
    ) = (
        batch.column_by_name("resolution"),
        batch.column_by_name("h3_index"),
    )
    else {
        panic!("missing key columns");
    };
    for i in (0..batch.num_rows).filter(|&i| keep(i)) {
        let row = batch
            .schema
            .fields
            .iter()
            .zip(&batch.columns)
            .filter(|(f, _)| f.name != PARTIAL_STATE_COLUMN)
            .map(|(f, c)| (f.name.clone(), value(c, i)))
            .collect();
        assert!(
            rows.insert((res[i], h3[i]), row).is_none(),
            "cell emitted twice"
        );
    }
}

/// Plan, scan every partition, merge the partials, and return all final rows.
fn partitioned_rows(scan: &Scan, target: i64) -> (Rows, usize, usize) {
    let path = Path::new(scan.path.as_ref().unwrap());
    let plan = plan(path, target).unwrap();
    let mut rows = Rows::new();
    let mut partials = Vec::new();
    for spec in &plan {
        for batch in scan_partition(scan, spec).unwrap() {
            let Some(ColumnArray::Binary { values, validity }) =
                batch.column_by_name(PARTIAL_STATE_COLUMN)
            else {
                panic!("partitioned scans have a partial_state column");
            };
            let is_partial = |i: usize| validity.as_ref().is_none_or(|v| v[i]);
            add_rows(&mut rows, &batch, |i| !is_partial(i));
            let idx: Vec<usize> = (0..batch.num_rows).filter(|&i| is_partial(i)).collect();
            if idx.is_empty() {
                continue;
            }
            let pick = |name: &str| batch.column_by_name(name).unwrap();
            let (ColumnArray::UInt8 { values: r, .. }, ColumnArray::UInt64 { values: h, .. }) =
                (pick("resolution"), pick("h3_index"))
            else {
                panic!("key types");
            };
            partials.push(
                RecordBatch::try_new(
                    key_schema(),
                    vec![
                        ColumnArray::UInt8 {
                            values: idx.iter().map(|&i| r[i]).collect(),
                            validity: None,
                        },
                        ColumnArray::UInt64 {
                            values: idx.iter().map(|&i| h[i]).collect(),
                            validity: None,
                        },
                        ColumnArray::Binary {
                            values: idx.iter().map(|&i| values[i].clone()).collect(),
                            validity: None,
                        },
                    ],
                )
                .unwrap(),
            );
        }
    }
    let n_partials: usize = partials.iter().map(|b| b.num_rows).sum();
    for batch in merge(partials, key_schema(), scan.categorical).unwrap() {
        add_rows(&mut rows, &batch, |_| true);
    }
    (rows, plan.len(), n_partials)
}

fn assert_rows_match(single: &Rows, partitioned: &Rows) {
    assert_eq!(
        single.keys().collect::<Vec<_>>(),
        partitioned.keys().collect::<Vec<_>>()
    );
    for (key, want) in single {
        let got = &partitioned[key];
        for ((name, a), (_, b)) in want.iter().zip(got) {
            match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) if a.is_f64() => {
                    assert!(
                        (x - y).abs() <= 1e-9 * x.abs().max(1.0),
                        "{name} of {key:?}: {x} vs {y}"
                    )
                }
                _ if name == "histogram" => {
                    // JSON object order is not significant.
                    let parse = |v: &serde_json::Value| -> serde_json::Value {
                        serde_json::from_str(v.as_str().unwrap()).unwrap()
                    };
                    assert_eq!(parse(a), parse(b), "{name} of {key:?}");
                }
                _ => assert_eq!(a, b, "{name} of {key:?}"),
            }
        }
    }
}

fn single_rows(scan: &Scan) -> Rows {
    let mut rows = Rows::new();
    for batch in scan.core_batches() {
        add_rows(&mut rows, &batch, |_| true);
    }
    rows
}

#[test]
fn continuous_partitions_match_a_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut scan = Scan::of(&gradient(dir.path()));
    scan.resolutions = Some("8,9".into());
    let single = single_rows(&scan);
    for target in [1, 4, 100] {
        let (rows, partitions, partials) = partitioned_rows(&scan, target);
        assert_rows_match(&single, &rows);
        if partitions > 1 {
            assert!(partials > 0, "{partitions} partitions without partial rows");
        } else {
            assert_eq!(partials, 0);
        }
    }
}

#[test]
fn categorical_partitions_match_a_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut scan = Scan::of(&classes(dir.path()));
    scan.categorical = true;
    scan.resolutions = Some("9".into());
    let single = single_rows(&scan);
    let (rows, partitions, _) = partitioned_rows(&scan, 3);
    assert!(partitions > 1);
    assert_rows_match(&single, &rows);
}

#[test]
fn partitioned_scans_reject_unsupported_options() {
    let dir = tempfile::tempdir().unwrap();
    let path = gradient(dir.path());
    let spec = plan(&path, 4).unwrap().remove(0);

    let mut limited = Scan::of(&path);
    limited.limit = Some(10);
    assert!(scan_partition(&limited, &spec)
        .unwrap_err()
        .contains("limit"));

    let mut no_key = Scan::of(&path);
    no_key.columns = Some("mean,count".into());
    assert!(scan_partition(&no_key, &spec)
        .unwrap_err()
        .contains("h3_index"));

    // `partial_state` may be listed among the requested columns.
    let mut listed = Scan::of(&path);
    listed.columns = Some("h3_index,resolution,mean,partial_state".into());
    let batches = scan_partition(&listed, &spec).unwrap();
    let names: Vec<_> = batches[0]
        .schema
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["h3_index", "resolution", "mean", PARTIAL_STATE_COLUMN]
    );

    let classes = classes(dir.path());
    let mut long = Scan::of(&classes);
    long.categorical = true;
    long.format = Some("long".into());
    let class_spec = plan(&classes, 4).unwrap().remove(0);
    assert!(scan_partition(&long, &class_spec)
        .unwrap_err()
        .contains("wide"));

    // A partition of one raster is refused by another.
    assert!(scan_partition(&Scan::of(&path), &class_spec)
        .unwrap_err()
        .contains("different mosaic"));
    assert!(scan_partition(&Scan::of(&path), "{}").is_err());
    assert!(plan(&path, 0).is_err());

    // Schema of a partitioned scan.
    let mut err: *mut c_char = ptr::null_mut();
    // SAFETY: NULL strings and a writable error pointer.
    let schema = unsafe {
        raster_h3_py_create_arrow_schema_partitioned(0, ptr::null(), ptr::null(), &mut err)
    };
    // SAFETY: `err` was set by the library or left NULL.
    assert!(unsafe { take_err(err) }.is_none());
    // SAFETY: a non-null result is a boxed schema owned by us.
    let names: Vec<String> = unsafe { (*schema).to_column_schema() }
        .unwrap()
        .fields
        .into_iter()
        .map(|f| f.name)
        .collect();
    // SAFETY: `schema` came from this library and is freed exactly once.
    unsafe { raster_h3_py_free_arrow_schema(schema) };
    assert_eq!(names.last().map(String::as_str), Some(PARTIAL_STATE_COLUMN));
}

#[test]
fn merge_rejects_rows_without_a_state() {
    let batch = RecordBatch::try_new(
        key_schema(),
        vec![
            ColumnArray::UInt8 {
                values: vec![9],
                validity: None,
            },
            ColumnArray::UInt64 {
                values: vec![0x8928308280fffff],
                validity: None,
            },
            ColumnArray::Binary {
                values: vec![Vec::new()],
                validity: Some(vec![false]),
            },
        ],
    )
    .unwrap();
    assert!(merge(vec![batch], key_schema(), false)
        .unwrap_err()
        .contains("partial rows"));
    // An empty input merges to no rows.
    assert!(merge(Vec::new(), key_schema(), false)
        .unwrap()
        .iter()
        .all(|b| b.num_rows == 0));
}
