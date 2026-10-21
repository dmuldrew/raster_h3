//! End-to-End Multi-Engine Integration & Parity Test Suite
//!
//! Exercises all 6 workspace crates (`raster-h3-core`, `raster-h3-cli`, `raster-h3-duckdb`,
//! `raster-h3-datafusion`, `raster-h3-python`, and `raster-h3-pg`) on identical synthetic
//! continuous and categorical GeoTIFFs and verifies bit-for-bit / numerical parity across:
//! 1. `raster-h3-core`: `ContinuousBatchReader`, `CategoricalBatchReader`, `ArrowArrayStream` C Data Interface.
//! 2. `raster-h3-cli`: Real subprocess execution of `raster-h3` (`ipc`, `jsonl`, `parquet`, `pmtiles`).
//! 3. `raster-h3-datafusion`: `RasterH3SessionCatalog`, `TableProvider` scan + pushdown, `ExecutionPlan`
//!    Arrow C Stream, and vectorized `ScalarUDFImpl` execution.
//! 4. `raster-h3-python`: Real Python subprocess loading `libraster_h3_python`, testing
//!    `RasterH3ArrowStream` (`to_dict()`, `__arrow_c_schema__()`, `__arrow_c_stream__()` C-ABI consumption),
//!    `raster_to_parquet()`, `raster_to_pmtiles()`, and scalar H3 ops.
//! 5. `raster-h3-pg`: `PgContinuousSrf` and `PgCategoricalSrf` Set-Returning Functions (both Rust & C-ABI) +
//!    Postgres V1 `fmgr` scalar C-ABI functions (`pg_h3_to_lat`, `pg_h3_to_lng`, `pg_h3_get_resolution`,
//!    `pg_h3_is_valid`, `pg_h3_cell_to_parent`, `pg_scalar_*`).
//! 6. `raster-h3-duckdb`: Real DuckDB CLI execution (when `duckdb` binary is present on PATH) +
//!    `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate` SQL parity.

use crate::helpers;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use h3o::{CellIndex, LatLng};
use helpers::TestGeoTiffBuilder;
use raster_h3::core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3::core::columnar::ipc::read_ipc_stream;
use raster_h3::core::columnar::{
    ArrowArrayStream, CategoricalBatchReader, CategoricalFormat, ColumnArray, ContinuousBatchReader,
};
use raster_h3::datafusion::{FilterPredicate, RasterH3SessionCatalog, TableProvider};
use raster_h3::pg::{
    datum_to_float8, datum_to_int64, int64_to_datum, pg_h3_cell_to_parent, pg_h3_get_resolution,
    pg_h3_is_valid, pg_h3_to_lat, pg_h3_to_lng, pg_scalar_h3_to_string, pg_scalar_h3_to_wkb_u64,
    pg_scalar_string_to_h3, pg_srf_continuous_close, pg_srf_continuous_next,
    pg_srf_continuous_open, FunctionCallInfoBaseData, NullableDatum, PgCContinuousRow,
    PgCategoricalSrf,
};

#[derive(Debug, Clone)]
struct ContinuousCellSummary {
    mean: f64,
    count: f64,
    min: f64,
    max: f64,
    sum: f64,
    resolution: u8,
}

#[derive(Debug, Clone)]
struct CategoricalCellSummary {
    majority_class: i64,
    majority_fraction: f64,
    total_count: f64,
    unique_classes: i64,
    shannon_entropy: f64,
    resolution: u8,
}

fn assert_continuous_maps_equal(
    engine_name: &str,
    expected: &BTreeMap<u64, ContinuousCellSummary>,
    actual: &BTreeMap<u64, ContinuousCellSummary>,
) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "{engine_name}: cell count mismatch (expected {}, got {})",
        expected.len(),
        actual.len()
    );
    for (cell, exp) in expected {
        let act = actual
            .get(cell)
            .unwrap_or_else(|| panic!("{engine_name}: missing H3 cell {cell:#x}"));
        assert_eq!(
            exp.resolution, act.resolution,
            "{engine_name}: resolution mismatch for {cell:#x}"
        );
        assert!(
            (exp.mean - act.mean).abs() < 1e-6,
            "{engine_name}: mean mismatch for {cell:#x}: {} vs {}",
            exp.mean,
            act.mean
        );
        assert!(
            (exp.count - act.count).abs() < 1e-6,
            "{engine_name}: count mismatch for {cell:#x}: {} vs {}",
            exp.count,
            act.count
        );
        assert!(
            (exp.min - act.min).abs() < 1e-6,
            "{engine_name}: min mismatch for {cell:#x}: {} vs {}",
            exp.min,
            act.min
        );
        assert!(
            (exp.max - act.max).abs() < 1e-6,
            "{engine_name}: max mismatch for {cell:#x}: {} vs {}",
            exp.max,
            act.max
        );
        assert!(
            (exp.sum - act.sum).abs() < 1e-5,
            "{engine_name}: sum mismatch for {cell:#x}: {} vs {}",
            exp.sum,
            act.sum
        );
    }
}

fn assert_categorical_maps_equal(
    engine_name: &str,
    expected: &BTreeMap<u64, CategoricalCellSummary>,
    actual: &BTreeMap<u64, CategoricalCellSummary>,
) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "{engine_name}: categorical cell count mismatch (expected {}, got {})",
        expected.len(),
        actual.len()
    );
    for (cell, exp) in expected {
        let act = actual
            .get(cell)
            .unwrap_or_else(|| panic!("{engine_name}: missing categorical H3 cell {cell:#x}"));
        assert_eq!(
            exp.resolution, act.resolution,
            "{engine_name}: res mismatch"
        );
        assert_eq!(
            exp.majority_class, act.majority_class,
            "{engine_name}: majority_class mismatch for {cell:#x}"
        );
        assert!(
            (exp.majority_fraction - act.majority_fraction).abs() < 1e-6,
            "{engine_name}: majority_fraction mismatch for {cell:#x}"
        );
        assert!(
            (exp.total_count - act.total_count).abs() < 1e-6,
            "{engine_name}: total_count mismatch for {cell:#x}"
        );
        assert_eq!(
            exp.unique_classes, act.unique_classes,
            "{engine_name}: unique_classes mismatch for {cell:#x}"
        );
        assert!(
            (exp.shannon_entropy - act.shannon_entropy).abs() < 1e-6,
            "{engine_name}: shannon_entropy mismatch for {cell:#x}"
        );
    }
}

fn collect_continuous_from_batches(
    batches: &[raster_h3::core::columnar::RecordBatch],
) -> BTreeMap<u64, ContinuousCellSummary> {
    let mut map = BTreeMap::new();
    for batch in batches {
        let h3_idx = match batch.column_by_name("h3_index").unwrap() {
            ColumnArray::UInt64 { values, .. } => values,
            other => panic!("unexpected h3_index type: {:?}", other.data_type()),
        };
        let mean = match batch.column_by_name("mean").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 mean"),
        };
        let count = match batch.column_by_name("count").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 count"),
        };
        let min = match batch.column_by_name("min").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 min"),
        };
        let max = match batch.column_by_name("max").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 max"),
        };
        let sum = match batch.column_by_name("sum").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 sum"),
        };
        let res = match batch.column_by_name("resolution").unwrap() {
            ColumnArray::UInt8 { values, .. } => values,
            _ => panic!("expected UInt8 resolution"),
        };
        for i in 0..batch.num_rows {
            map.insert(
                h3_idx[i],
                ContinuousCellSummary {
                    mean: mean[i],
                    count: count[i],
                    min: min[i],
                    max: max[i],
                    sum: sum[i],
                    resolution: res[i],
                },
            );
        }
    }
    map
}

fn collect_categorical_from_batches(
    batches: &[raster_h3::core::columnar::RecordBatch],
) -> BTreeMap<u64, CategoricalCellSummary> {
    let mut map = BTreeMap::new();
    for batch in batches {
        let h3_idx = match batch.column_by_name("h3_index").unwrap() {
            ColumnArray::UInt64 { values, .. } => values,
            _ => panic!("expected UInt64 h3_index"),
        };
        let maj_class = match batch.column_by_name("majority_class").unwrap() {
            ColumnArray::Int64 { values, .. } => values,
            _ => panic!("expected Int64 majority_class"),
        };
        let maj_frac = match batch.column_by_name("majority_fraction").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 majority_fraction"),
        };
        let total_count = match batch.column_by_name("total_count").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 total_count"),
        };
        let unique_classes = match batch.column_by_name("unique_classes").unwrap() {
            ColumnArray::Int64 { values, .. } => values,
            _ => panic!("expected Int64 unique_classes"),
        };
        let entropy = match batch.column_by_name("shannon_entropy").unwrap() {
            ColumnArray::Float64 { values, .. } => values,
            _ => panic!("expected Float64 shannon_entropy"),
        };
        let res = match batch.column_by_name("resolution").unwrap() {
            ColumnArray::UInt8 { values, .. } => values,
            _ => panic!("expected UInt8 resolution"),
        };
        for i in 0..batch.num_rows {
            map.insert(
                h3_idx[i],
                CategoricalCellSummary {
                    majority_class: maj_class[i],
                    majority_fraction: maj_frac[i],
                    total_count: total_count[i],
                    unique_classes: unique_classes[i],
                    shannon_entropy: entropy[i],
                    resolution: res[i],
                },
            );
        }
    }
    map
}

/// `target/<profile>`: the test binary lives in `target/<profile>/deps`.
fn target_profile_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf()
}

/// Python interpreter for subprocess tests: `$PYTHON` if set, else `python` on Windows
/// (no `python3` there) and `python3` elsewhere.
fn python_exe() -> String {
    std::env::var("PYTHON")
        .unwrap_or_else(|_| if cfg!(windows) { "python" } else { "python3" }.to_string())
}

fn ensure_cdylib_built(pkg: &str, lib_stem: &str) -> PathBuf {
    let dir = target_profile_dir();
    let lib_path = dir.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        lib_stem,
        std::env::consts::DLL_SUFFIX
    ));
    if !lib_path.exists() {
        // Build into the same profile directory the test executable runs from.
        let mut cmd = Command::new("cargo");
        cmd.args(["build", "-p", pkg, "--lib"]);
        if dir.file_name().is_some_and(|n| n == "release") {
            cmd.arg("--release");
        }
        // With `--target <triple>`, artifacts land in `target/<triple>/<profile>`.
        if let Some(triple) = dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .filter(|n| n.matches('-').count() >= 2)
        {
            cmd.args(["--target", triple]);
        }
        let status = cmd.status().expect("invoke cargo build");
        assert!(status.success(), "cargo build -p {pkg} failed");
    }
    assert!(
        lib_path.exists(),
        "Expected shared library at {}",
        lib_path.display()
    );
    lib_path
}

#[test]
fn test_cross_engine_continuous_e2e_parity() {
    // 1. Create a synthetic 24x24 continuous GeoTIFF with a spatial gradient
    let (_tmp_file, tif_path) = TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(true)
        .create_tempfile(|c, r| (r * 10 + c + 1) as f32);

    run_continuous_parity(&tif_path, "EPSG:4326", &[7, 8], (-122.5, 37.75));
}

fn run_continuous_parity(
    tif_path: &Path,
    crs: &str,
    resolutions: &[u8],
    expected_lon_lat: (f64, f64),
) -> BTreeMap<u64, ContinuousCellSummary> {
    let resolution_arg = resolutions
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");

    // =========================================================================
    // Engine 1: raster-h3-core (ContinuousBatchReader + ArrowArrayStream C-ABI)
    // =========================================================================
    let mut cfg = MultiResolutionConfig::new(resolutions.to_vec());
    cfg.custom_crs = Some(crs.to_string());
    let tif_paths = [tif_path.to_path_buf()];
    let reader = ContinuousBatchReader::open(&tif_paths, &cfg, None, 64, None).unwrap();
    let schema = reader.schema();
    let mut stream = ArrowArrayStream::from_iterator(schema, reader);
    // SAFETY: `stream` was just built by `from_iterator`, so it is valid and unreleased.
    let (_imported_schema, core_batches) = unsafe { stream.collect_batches().unwrap() };
    let core_map = collect_continuous_from_batches(&core_batches);
    assert!(
        !core_map.is_empty(),
        "core continuous aggregation must produce cells"
    );

    // =========================================================================
    // Engine 2: raster-h3-cli (Real subprocess for `ipc`, `jsonl`, `parquet`, `pmtiles`)
    // =========================================================================
    let cli_bin = env!("CARGO_BIN_EXE_raster-h3");

    // 2a. Subprocess `ipc` streaming over stdout
    let ipc_out = Command::new(cli_bin)
        .args([
            "ipc",
            tif_path.to_str().unwrap(),
            "-r",
            &resolution_arg,
            "--crs",
            crs,
        ])
        .output()
        .expect("run raster-h3 ipc");
    assert!(
        ipc_out.status.success(),
        "raster-h3 ipc failed: {}",
        String::from_utf8_lossy(&ipc_out.stderr)
    );
    let mut cursor = Cursor::new(&ipc_out.stdout);
    let (_cli_schema, cli_ipc_batches) = read_ipc_stream(&mut cursor).unwrap();
    let cli_ipc_map = collect_continuous_from_batches(&cli_ipc_batches);
    assert_continuous_maps_equal("raster-h3-cli (ipc)", &core_map, &cli_ipc_map);

    // 2b. Subprocess `jsonl` streaming over stdout
    let jsonl_out = Command::new(cli_bin)
        .args([
            "jsonl",
            tif_path.to_str().unwrap(),
            "-r",
            &resolution_arg,
            "--crs",
            crs,
            "--columns",
            "h3_index,mean,count,min,max,sum,resolution",
        ])
        .output()
        .expect("run raster-h3 jsonl");
    assert!(
        jsonl_out.status.success(),
        "raster-h3 jsonl failed: {}",
        String::from_utf8_lossy(&jsonl_out.stderr)
    );
    let mut cli_jsonl_map = BTreeMap::new();
    for line in String::from_utf8(jsonl_out.stdout).unwrap().lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        cli_jsonl_map.insert(
            v["h3_index"].as_u64().unwrap(),
            ContinuousCellSummary {
                mean: v["mean"].as_f64().unwrap(),
                count: v["count"].as_f64().unwrap(),
                min: v["min"].as_f64().unwrap(),
                max: v["max"].as_f64().unwrap(),
                sum: v["sum"].as_f64().unwrap(),
                resolution: v["resolution"].as_u64().unwrap() as u8,
            },
        );
    }
    assert_continuous_maps_equal("raster-h3-cli (jsonl)", &core_map, &cli_jsonl_map);

    // 2c. Subprocess `parquet` & `pmtiles` exports
    let tmp_dir = tempfile::tempdir().unwrap();
    let cli_pq = tmp_dir.path().join("cli_out.parquet");
    let cli_pm = tmp_dir.path().join("cli_out.pmtiles");
    let pq_status = Command::new(cli_bin)
        .args([
            "parquet",
            tif_path.to_str().unwrap(),
            "-r",
            &resolution_arg,
            "-o",
            cli_pq.to_str().unwrap(),
            "--crs",
            crs,
        ])
        .status()
        .unwrap();
    assert!(pq_status.success() && cli_pq.exists());
    let pm_status = Command::new(cli_bin)
        .args([
            "pmtiles",
            tif_path.to_str().unwrap(),
            "-r",
            &resolution_arg,
            "-o",
            cli_pm.to_str().unwrap(),
            "--crs",
            crs,
        ])
        .status()
        .unwrap();
    assert!(pm_status.success() && &std::fs::read(&cli_pm).unwrap()[..7] == b"PMTiles");

    // =========================================================================
    // Engine 3: raster-h3-datafusion (Catalog + TableProvider + ScalarUDFImpl)
    // =========================================================================
    let catalog = RasterH3SessionCatalog::new();
    let provider = catalog
        .h3_raster_continuous_aggregate(tif_path.to_str().unwrap(), resolutions.to_vec())
        .unwrap()
        .with_crs(crs);

    // Full scan via Arrow C Stream
    let plan = provider.scan(None, &[], None).unwrap();
    let mut df_stream = plan.execute_arrow_c_stream().unwrap();
    // SAFETY: `execute_arrow_c_stream` returns a freshly built, unreleased stream owned here.
    let (_df_schema, df_batches) = unsafe { df_stream.collect_batches().unwrap() };
    let df_map = collect_continuous_from_batches(&df_batches);
    assert_continuous_maps_equal("raster-h3-datafusion", &core_map, &df_map);

    // Verify vectorized ScalarUDFImpls on DataFusion output batch
    let first_batch = &df_batches[0];
    let h3_col = first_batch.column_by_name("h3_index").unwrap().clone();
    let hex_arr = catalog
        .invoke_scalar("h3_to_string", std::slice::from_ref(&h3_col))
        .unwrap();
    let roundtrip_arr = catalog.invoke_scalar("string_to_h3", &[hex_arr]).unwrap();
    assert_eq!(roundtrip_arr.len(), first_batch.num_rows);
    let res_arr = catalog
        .invoke_scalar("h3_get_resolution", std::slice::from_ref(&h3_col))
        .unwrap();
    assert_eq!(res_arr.len(), first_batch.num_rows);
    let wkb_arr = catalog.invoke_scalar("h3_to_wkb", &[h3_col]).unwrap();
    assert_eq!(wkb_arr.len(), first_batch.num_rows);

    // Verify projection + limit pushdown on DataFusion TableProvider
    let pushed_plan = provider
        .scan(Some(&[0, 2, 4]), &[FilterPredicate::Resolution(8)], Some(2))
        .unwrap();
    let pushed_batches: Vec<_> = pushed_plan.execute().unwrap().collect();
    let pushed_rows: usize = pushed_batches
        .iter()
        .map(|b| b.as_ref().unwrap().num_rows)
        .sum();
    assert!(pushed_rows <= 2 && pushed_rows > 0);

    // =========================================================================
    // Engine 4: raster-h3-python (Real Python subprocess + PyCapsule C Stream)
    // =========================================================================
    let py_lib = ensure_cdylib_built("raster-h3-python", "raster_h3_python");
    let py_pkg_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/raster-h3-python/python");
    let py_pq = tmp_dir.path().join("py_out.parquet");
    let py_pm = tmp_dir.path().join("py_out.pmtiles");

    let py_script = r#"
import ctypes, json, os, sys
import raster_h3

tif_path = sys.argv[1]
pq_path = sys.argv[2]
pm_path = sys.argv[3]
crs = sys.argv[4]
resolutions = [int(r) for r in sys.argv[5].split(",")]
resolution_arg = ",".join(str(r) for r in resolutions)

# 1. Stream via RasterH3ArrowStream.to_dict()
stream = raster_h3.RasterH3ArrowStream(tif_path, resolutions=resolutions, crs=crs)
d = stream.to_dict()

# 2. Verify __arrow_c_schema__() and __arrow_c_stream__() PyCapsules and consume C stream
schema_cap = stream.__arrow_c_schema__()
stream_cap = stream.__arrow_c_stream__()

class ArrowSchema(ctypes.Structure):
    pass
ArrowSchema._fields_ = [
    ("format", ctypes.c_char_p),
    ("name", ctypes.c_char_p),
    ("metadata", ctypes.c_char_p),
    ("flags", ctypes.c_int64),
    ("n_children", ctypes.c_int64),
    ("children", ctypes.POINTER(ctypes.POINTER(ArrowSchema))),
    ("dictionary", ctypes.POINTER(ArrowSchema)),
    ("release", ctypes.c_void_p),
    ("private_data", ctypes.c_void_p),
]

class ArrowArray(ctypes.Structure):
    pass
ArrowArray._fields_ = [
    ("length", ctypes.c_int64),
    ("null_count", ctypes.c_int64),
    ("offset", ctypes.c_int64),
    ("n_buffers", ctypes.c_int64),
    ("n_children", ctypes.c_int64),
    ("buffers", ctypes.POINTER(ctypes.c_void_p)),
    ("children", ctypes.POINTER(ctypes.POINTER(ArrowArray))),
    ("dictionary", ctypes.POINTER(ArrowArray)),
    ("release", ctypes.CFUNCTYPE(None, ctypes.POINTER(ArrowArray))),
    ("private_data", ctypes.c_void_p),
]

GET_NEXT_FN = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.POINTER(ArrowArray))

class ArrowArrayStream(ctypes.Structure):
    _fields_ = [
        ("get_schema", ctypes.c_void_p),
        ("get_next", GET_NEXT_FN),
        ("get_last_error", ctypes.c_void_p),
        ("release", ctypes.c_void_p),
        ("private_data", ctypes.c_void_p),
    ]

c_stream_ptr = ctypes.pythonapi.PyCapsule_GetPointer(id(stream_cap), b"arrow_array_stream")
c_stream = ArrowArrayStream.from_address(c_stream_ptr)
total_c_rows = 0
while True:
    arr = ArrowArray()
    rc = c_stream.get_next(c_stream_ptr, ctypes.byref(arr))
    assert rc == 0, f"get_next failed with {rc}"
    if not bool(arr.release):
        break
    total_c_rows += arr.length
    arr.release(ctypes.byref(arr))

assert total_c_rows == len(d["h3_index"]), f"C stream rows {total_c_rows} != dict rows {len(d['h3_index'])}"

# 3. Verify scalar functions on every emitted cell
for idx, hex_s, res in zip(d["h3_index"], d["h3_hex"], d["resolution"]):
    assert raster_h3.h3_is_valid(idx)
    assert raster_h3.h3_to_string(idx) == hex_s
    assert raster_h3.string_to_h3(hex_s) == idx
    assert raster_h3.h3_get_resolution(idx) == res
    wkb = raster_h3.h3_to_wkb(idx)
    assert wkb is not None and len(wkb) == 125

# 4. Verify direct Parquet and PMTiles exporters from Python
pq_count = raster_h3.raster_to_parquet(tif_path, pq_path, resolutions=resolution_arg, crs=crs)
pm_count = raster_h3.raster_to_pmtiles(tif_path, pm_path, resolutions=resolution_arg, crs=crs)
assert pq_count == len(d["h3_index"])
assert pm_count == len(d["h3_index"])

print(json.dumps(d))
"#;

    let py_out = Command::new(python_exe())
        .env("PYTHONPATH", &py_pkg_dir)
        .env("RASTER_H3_PYTHON_LIB", &py_lib)
        .args([
            "-c",
            py_script,
            tif_path.to_str().unwrap(),
            py_pq.to_str().unwrap(),
            py_pm.to_str().unwrap(),
            crs,
            &resolution_arg,
        ])
        .output()
        .expect("run python3 E2E script");
    assert!(
        py_out.status.success(),
        "Python E2E failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&py_out.stdout),
        String::from_utf8_lossy(&py_out.stderr)
    );
    let py_json: serde_json::Value = serde_json::from_slice(&py_out.stdout).unwrap();
    let mut py_map = BTreeMap::new();
    let n_py = py_json["h3_index"].as_array().unwrap().len();
    for i in 0..n_py {
        py_map.insert(
            py_json["h3_index"][i].as_u64().unwrap(),
            ContinuousCellSummary {
                mean: py_json["mean"][i].as_f64().unwrap(),
                count: py_json["count"][i].as_f64().unwrap(),
                min: py_json["min"][i].as_f64().unwrap(),
                max: py_json["max"][i].as_f64().unwrap(),
                sum: py_json["sum"][i].as_f64().unwrap(),
                resolution: py_json["resolution"][i].as_u64().unwrap() as u8,
            },
        );
    }
    assert_continuous_maps_equal("raster-h3-python", &core_map, &py_map);

    // =========================================================================
    // Engine 5: raster-h3-pg (C-ABI SRF + Postgres V1 fmgr C-ABI scalars)
    // =========================================================================
    let mut pg_map = BTreeMap::new();
    // SAFETY: string arguments are live CStrings (or null), the SRF handle is used only between
    // its open and single close, `c_row` is a fully initialized row, and each `fc*` is a local
    // call info with all 8 argument slots allocated.
    unsafe {
        let c_path = CString::new(tif_path.to_str().unwrap()).unwrap();
        let c_res = CString::new(resolution_arg.as_str()).unwrap();
        let c_crs = CString::new(crs).unwrap();
        let mut err_ptr: *mut std::ffi::c_char = std::ptr::null_mut();
        let srf_ptr = pg_srf_continuous_open(
            c_path.as_ptr(),
            c_res.as_ptr(),
            c_crs.as_ptr(),
            0,
            0.0,
            std::ptr::null(),
            &mut err_ptr,
        );
        assert!(!srf_ptr.is_null() && err_ptr.is_null());

        let mut c_row = PgCContinuousRow {
            h3_index: 0,
            h3_hex: [0u8; 16],
            mean: 0.0,
            stddev: 0.0,
            has_stddev: 0,
            count: 0.0,
            min: 0.0,
            max: 0.0,
            sum: 0.0,
            resolution: 0,
            wkb_len: 0,
            wkb: [0u8; raster_h3_core::encoding::WKB_BUF_LEN],
        };
        while pg_srf_continuous_next(srf_ptr, &mut c_row) == 1 {
            let cell_u64 = c_row.h3_index as u64;

            // Exercise Postgres V1 fmgr scalar functions on every row
            let mut fc1 = FunctionCallInfoBaseData::with_args(&[NullableDatum {
                value: int64_to_datum(c_row.h3_index),
                isnull: false,
            }]);
            let valid_d = pg_h3_is_valid(&mut fc1);
            assert!(!fc1.isnull && valid_d == 1);

            let res_d = pg_h3_get_resolution(&mut fc1);
            assert!(!fc1.isnull && datum_to_int64(res_d) as i16 == c_row.resolution);

            let lat_d = pg_h3_to_lat(&mut fc1);
            let lng_d = pg_h3_to_lng(&mut fc1);
            let lat = datum_to_float8(lat_d);
            let lng = datum_to_float8(lng_d);
            assert!(
                !fc1.isnull
                    && (lat - expected_lon_lat.1).abs() <= 0.5
                    && (lng - expected_lon_lat.0).abs() <= 0.5
            );

            let mut fc_parent = FunctionCallInfoBaseData::with_args(&[
                NullableDatum {
                    value: int64_to_datum(c_row.h3_index),
                    isnull: false,
                },
                NullableDatum {
                    value: int64_to_datum(5),
                    isnull: false,
                },
            ]);
            let parent_d = pg_h3_cell_to_parent(&mut fc_parent);
            assert!(!fc_parent.isnull && parent_d != 0);

            // Also verify string and WKB helpers exported by raster_h3_pg
            let hex_s = pg_scalar_h3_to_string(cell_u64).unwrap();
            assert_eq!(pg_scalar_string_to_h3(&hex_s), cell_u64);
            let wkb = pg_scalar_h3_to_wkb_u64(cell_u64).unwrap();
            assert_eq!(&c_row.wkb[..c_row.wkb_len], wkb.as_slice());

            pg_map.insert(
                cell_u64,
                ContinuousCellSummary {
                    mean: c_row.mean,
                    count: c_row.count,
                    min: c_row.min,
                    max: c_row.max,
                    sum: c_row.sum,
                    resolution: c_row.resolution as u8,
                },
            );
        }
        pg_srf_continuous_close(srf_ptr);
    }
    assert_continuous_maps_equal("raster-h3-pg", &core_map, &pg_map);

    // =========================================================================
    // Engine 6: raster-h3-duckdb (Real DuckDB CLI when available on PATH)
    // =========================================================================
    if let Ok(ver_out) = Command::new("duckdb").arg("--version").output() {
        if ver_out.status.success() {
            let dylib = ensure_cdylib_built("raster_h3", "raster_h3");
            let platform_out = Command::new("duckdb")
                .args(["-batch", "-csv", "-noheader", "-c", "PRAGMA platform"])
                .output()
                .unwrap();
            let platform = String::from_utf8(platform_out.stdout).unwrap();
            let ext_path = tmp_dir.path().join("raster_h3.duckdb_extension");
            let pkg_status = Command::new(env!("CARGO_BIN_EXE_package_extension"))
                .arg("-i")
                .arg(&dylib)
                .arg("-o")
                .arg(&ext_path)
                .args(["-p", platform.trim(), "-d", "v1.2.0"])
                .status()
                .unwrap();
            assert!(pkg_status.success());

            let sql = format!(
                "LOAD '{}'; \
                 SELECT h3_index, mean, count, min, max, sum, resolution \
                 FROM h3_raster_continuous_aggregate('{}', resolutions := '{}', crs := '{}');",
                ext_path.display(),
                tif_path.display(),
                resolution_arg,
                crs
            );
            let duck_out = Command::new("duckdb")
                .args(["-unsigned", "-batch", "-csv", "-noheader", "-c", &sql])
                .output()
                .unwrap();
            assert!(
                duck_out.status.success(),
                "DuckDB CLI query failed: {}",
                String::from_utf8_lossy(&duck_out.stderr)
            );
            let mut duck_map = BTreeMap::new();
            for line in String::from_utf8(duck_out.stdout).unwrap().lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.split(',').collect();
                duck_map.insert(
                    parts[0].parse::<u64>().unwrap(),
                    ContinuousCellSummary {
                        mean: parts[1].parse::<f64>().unwrap(),
                        count: parts[2].parse::<f64>().unwrap(),
                        min: parts[3].parse::<f64>().unwrap(),
                        max: parts[4].parse::<f64>().unwrap(),
                        sum: parts[5].parse::<f64>().unwrap(),
                        resolution: parts[6].parse::<u8>().unwrap(),
                    },
                );
            }
            assert_continuous_maps_equal("raster-h3-duckdb (DuckDB CLI)", &core_map, &duck_map);
        }
    }
    core_map
}

fn assert_core_cells_near(
    cells: &BTreeMap<u64, ContinuousCellSummary>,
    expected_lon_lat: (f64, f64),
    max_distance_m: f64,
) {
    let (expected_lon, expected_lat) = expected_lon_lat;
    assert!(
        !cells.is_empty(),
        "core projected aggregation must be non-empty"
    );
    for &cell in cells.keys() {
        let center = LatLng::from(CellIndex::try_from(cell).unwrap());
        let d_lat = (center.lat() - expected_lat).to_radians() * 6_378_137.0;
        let d_lon = (center.lng() - expected_lon).to_radians()
            * 6_378_137.0
            * expected_lat.to_radians().cos();
        let distance = d_lat.hypot(d_lon);
        assert!(
            distance <= max_distance_m,
            "core cell {cell:#x} at ({}, {}) is {distance:.0} m from expected location ({expected_lon}, {expected_lat})",
            center.lng(),
            center.lat()
        );
    }
}

#[test]
fn test_cross_engine_projected_continuous_parity() {
    let (_dc_file, dc_tif) = TestGeoTiffBuilder::new(40, 40)
        .epsg(5070)
        .origin(1_614_000.0, 1_930_000.0)
        .pixel_size_xy(300.0, 300.0)
        .with_geokeys(true)
        .create_tempfile(|c, r| (r * 10 + c + 1) as f32);
    let dc_lon_lat = (-77.0369, 38.9072);
    let dc_cells = run_continuous_parity(&dc_tif, "EPSG:5070", &[7, 8], dc_lon_lat);
    assert_core_cells_near(&dc_cells, dc_lon_lat, 20_000.0);

    let (_london_file, london_tif) = TestGeoTiffBuilder::new(40, 40)
        .epsg(27700)
        .origin(526_000.0, 184_000.0)
        .pixel_size_xy(300.0, 300.0)
        .with_geokeys(true)
        .create_tempfile(|c, r| (r * 10 + c + 1) as f32);
    let london_lon_lat = (-0.1276, 51.5072);
    let london_cells = run_continuous_parity(&london_tif, "EPSG:27700", &[7, 8], london_lon_lat);
    assert_core_cells_near(&london_cells, london_lon_lat, 20_000.0);
}

#[test]
fn test_cross_engine_categorical_e2e_parity() {
    // Create a synthetic 24x24 categorical GeoTIFF with 4 classes (10, 20, 30, 40)
    let (_tmp_file, tif_path) = TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(true)
        .create_tempfile(|c, r| match (r / 6 + c / 6) % 4 {
            0 => 10.0f32,
            1 => 20.0f32,
            2 => 30.0f32,
            _ => 40.0f32,
        });

    // 1. raster-h3-core CategoricalBatchReader
    let cfg = MultiResolutionConfig::new(vec![8]);
    let reader = CategoricalBatchReader::open(
        std::slice::from_ref(&tif_path),
        &cfg,
        CategoricalFormat::Wide,
        None,
        64,
        None,
    )
    .unwrap();
    let core_batches: Vec<_> = reader.map(|b| b.unwrap()).collect();
    let core_map = collect_categorical_from_batches(&core_batches);
    assert!(!core_map.is_empty());

    // 2. raster-h3-cli subprocess (`ipc` categorical wide)
    let cli_bin = env!("CARGO_BIN_EXE_raster-h3");
    let ipc_out = Command::new(cli_bin)
        .args([
            "ipc",
            tif_path.to_str().unwrap(),
            "-r",
            "8",
            "-c",
            "--format",
            "wide",
            "--crs",
            "EPSG:4326",
        ])
        .output()
        .unwrap();
    assert!(ipc_out.status.success());
    let mut cursor = Cursor::new(&ipc_out.stdout);
    let (_s, cli_batches) = read_ipc_stream(&mut cursor).unwrap();
    let cli_map = collect_categorical_from_batches(&cli_batches);
    assert_categorical_maps_equal("raster-h3-cli (categorical ipc)", &core_map, &cli_map);

    // 3. raster-h3-datafusion (`h3_raster_categorical_aggregate`)
    let catalog = RasterH3SessionCatalog::new();
    let provider = catalog
        .h3_raster_categorical_aggregate(tif_path.to_str().unwrap(), vec![8])
        .unwrap()
        .with_crs("EPSG:4326")
        .with_format(CategoricalFormat::Wide);
    let plan = provider.scan(None, &[], None).unwrap();
    let df_batches: Vec<_> = plan.execute().unwrap().map(|b| b.unwrap()).collect();
    let df_map = collect_categorical_from_batches(&df_batches);
    assert_categorical_maps_equal("raster-h3-datafusion (categorical)", &core_map, &df_map);

    // 4. raster-h3-python subprocess (`RasterH3ArrowStream(..., categorical=True)`)
    let py_lib = ensure_cdylib_built("raster-h3-python", "raster_h3_python");
    let py_pkg_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/raster-h3-python/python");
    let py_out = Command::new(python_exe())
        .env("PYTHONPATH", &py_pkg_dir)
        .env("RASTER_H3_PYTHON_LIB", &py_lib)
        .args([
            "-c",
            "import json, sys, raster_h3; s = raster_h3.RasterH3ArrowStream(sys.argv[1], resolutions=8, categorical=True, format='wide', crs='EPSG:4326'); print(json.dumps(s.to_dict()))",
            tif_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(py_out.status.success());
    let py_json: serde_json::Value = serde_json::from_slice(&py_out.stdout).unwrap();
    let mut py_map = BTreeMap::new();
    let n_py = py_json["h3_index"].as_array().unwrap().len();
    for i in 0..n_py {
        py_map.insert(
            py_json["h3_index"][i].as_u64().unwrap(),
            CategoricalCellSummary {
                majority_class: py_json["majority_class"][i].as_i64().unwrap(),
                majority_fraction: py_json["majority_fraction"][i].as_f64().unwrap(),
                total_count: py_json["total_count"][i].as_f64().unwrap(),
                unique_classes: py_json["unique_classes"][i].as_i64().unwrap(),
                shannon_entropy: py_json["shannon_entropy"][i].as_f64().unwrap(),
                resolution: py_json["resolution"][i].as_u64().unwrap() as u8,
            },
        );
    }
    assert_categorical_maps_equal("raster-h3-python (categorical)", &core_map, &py_map);

    // 5. raster-h3-pg (`PgCategoricalSrf`)
    let pg_srf = PgCategoricalSrf::open(
        tif_path.to_str().unwrap(),
        vec![8],
        Some("EPSG:4326".to_string()),
        None,
        None,
    )
    .unwrap();
    let mut pg_map = BTreeMap::new();
    for row_res in pg_srf {
        let row = row_res.unwrap();
        pg_map.insert(
            row.h3_index as u64,
            CategoricalCellSummary {
                majority_class: row.majority_class,
                majority_fraction: row.majority_fraction,
                total_count: row.total_count,
                unique_classes: row.unique_classes,
                shannon_entropy: row.shannon_entropy,
                resolution: row.resolution as u8,
            },
        );
    }
    assert_categorical_maps_equal("raster-h3-pg (categorical)", &core_map, &pg_map);

    // 6. raster-h3-duckdb (`h3_raster_categorical_aggregate` in real DuckDB CLI)
    if let Ok(ver_out) = Command::new("duckdb").arg("--version").output() {
        if ver_out.status.success() {
            let tmp_dir = tempfile::tempdir().unwrap();
            let dylib = ensure_cdylib_built("raster_h3", "raster_h3");
            let platform_out = Command::new("duckdb")
                .args(["-batch", "-csv", "-noheader", "-c", "PRAGMA platform"])
                .output()
                .unwrap();
            let platform = String::from_utf8(platform_out.stdout).unwrap();
            let ext_path = tmp_dir.path().join("raster_h3.duckdb_extension");
            let pkg_status = Command::new(env!("CARGO_BIN_EXE_package_extension"))
                .arg("-i")
                .arg(&dylib)
                .arg("-o")
                .arg(&ext_path)
                .args(["-p", platform.trim(), "-d", "v1.2.0"])
                .status()
                .unwrap();
            assert!(pkg_status.success());

            let sql = format!(
                "LOAD '{}'; \
                 SELECT h3_index, majority_class, majority_fraction, total_count, unique_classes, shannon_entropy, resolution \
                 FROM h3_raster_categorical_aggregate('{}', resolution := 8, format := 'wide', crs := 'EPSG:4326');",
                ext_path.display(),
                tif_path.display()
            );
            let duck_out = Command::new("duckdb")
                .args(["-unsigned", "-batch", "-csv", "-noheader", "-c", &sql])
                .output()
                .unwrap();
            assert!(
                duck_out.status.success(),
                "DuckDB CLI categorical query failed: {}",
                String::from_utf8_lossy(&duck_out.stderr)
            );
            let mut duck_map = BTreeMap::new();
            for line in String::from_utf8(duck_out.stdout).unwrap().lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.split(',').collect();
                duck_map.insert(
                    parts[0].parse::<u64>().unwrap(),
                    CategoricalCellSummary {
                        majority_class: parts[1].parse::<i64>().unwrap(),
                        majority_fraction: parts[2].parse::<f64>().unwrap(),
                        total_count: parts[3].parse::<f64>().unwrap(),
                        unique_classes: parts[4].parse::<i64>().unwrap(),
                        shannon_entropy: parts[5].parse::<f64>().unwrap(),
                        resolution: parts[6].parse::<u8>().unwrap(),
                    },
                );
            }
            assert_categorical_maps_equal(
                "raster-h3-duckdb (categorical DuckDB CLI)",
                &core_map,
                &duck_map,
            );
        }
    }
}

#[test]
fn test_h3_polygon_to_cells_cross_engine_parity() {
    use h3o::geom::ContainmentMode;
    use raster_h3::core::scalar_ops::h3_polygon_to_cells_wkb;

    let outer: [(f64, f64); 5] = [
        (-122.49, 37.71),
        (-122.35, 37.71),
        (-122.35, 37.83),
        (-122.49, 37.83),
        (-122.49, 37.71),
    ];
    let hole: [(f64, f64); 5] = [
        (-122.45, 37.75),
        (-122.40, 37.75),
        (-122.40, 37.79),
        (-122.45, 37.79),
        (-122.45, 37.75),
    ];
    let mut wkb = vec![1u8];
    wkb.extend(3u32.to_le_bytes());
    wkb.extend(2u32.to_le_bytes());
    for ring in [&outer[..], &hole[..]] {
        wkb.extend((ring.len() as u32).to_le_bytes());
        for &(x, y) in ring {
            wkb.extend(x.to_le_bytes());
            wkb.extend(y.to_le_bytes());
        }
    }
    let wkb_hex: String = wkb.iter().map(|byte| format!("{byte:02x}")).collect();
    let modes = [
        ("contains_centroid", ContainmentMode::ContainsCentroid),
        ("contains_boundary", ContainmentMode::ContainsBoundary),
        ("intersects_boundary", ContainmentMode::IntersectsBoundary),
        ("covers", ContainmentMode::Covers),
    ];
    let mut core_results = BTreeMap::new();
    for (name, mode) in modes {
        let cells = h3_polygon_to_cells_wkb(&wkb, 8, mode).unwrap();
        assert!(!cells.is_empty(), "{name} produced no cells");
        core_results.insert(name, cells);
    }

    let py_lib = ensure_cdylib_built("raster-h3-python", "raster_h3_python");
    let py_pkg_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/raster-h3-python/python");
    let py_script = r#"
import json, sys
import raster_h3

wkb = bytes.fromhex(sys.argv[1])
modes = ["contains_centroid", "contains_boundary", "intersects_boundary", "covers"]
cells = {mode: raster_h3.h3_polygon_to_cells(wkb, 8, mode) for mode in modes}
aliases = {
    "center": "contains_centroid",
    "full": "contains_boundary",
    "overlapping": "covers",
}
for alias, canonical in aliases.items():
    assert raster_h3.h3_polygon_to_cells(wkb, 8, alias) == cells[canonical]
print(json.dumps(cells, sort_keys=True))
"#;
    let py_out = Command::new(python_exe())
        .env("PYTHONPATH", &py_pkg_dir)
        .env("RASTER_H3_PYTHON_LIB", &py_lib)
        .args(["-c", py_script, &wkb_hex])
        .output()
        .expect("run Python polygon parity test");
    assert!(
        py_out.status.success(),
        "Python polygon parity failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&py_out.stdout),
        String::from_utf8_lossy(&py_out.stderr)
    );
    let py_json: serde_json::Value = serde_json::from_slice(&py_out.stdout).unwrap();
    for (name, expected) in &core_results {
        let actual: Vec<u64> = py_json[*name]
            .as_array()
            .unwrap()
            .iter()
            .map(|cell| cell.as_u64().unwrap())
            .collect();
        assert_eq!(&actual, expected, "Python mismatch for {name}");
    }

    let duckdb = std::env::var("DUCKDB_BIN").unwrap_or_else(|_| "duckdb".into());
    if let Ok(version_output) = Command::new(&duckdb).arg("--version").output() {
        if version_output.status.success() {
            let temp = tempfile::tempdir().unwrap();
            let library = ensure_cdylib_built("raster_h3", "raster_h3");
            let platform_output = Command::new(&duckdb)
                .args(["-batch", "-csv", "-noheader", "-c", "PRAGMA platform"])
                .output()
                .expect("read DuckDB platform");
            assert!(
                platform_output.status.success(),
                "DuckDB PRAGMA platform failed: {}",
                String::from_utf8_lossy(&platform_output.stderr)
            );
            let platform = String::from_utf8(platform_output.stdout).unwrap();
            let extension = temp.path().join("raster_h3.duckdb_extension");
            let duckdb_version =
                std::env::var("DUCKDB_VERSION").unwrap_or_else(|_| "v1.2.0".into());
            let package_output = Command::new(env!("CARGO_BIN_EXE_package_extension"))
                .arg("-i")
                .arg(&library)
                .arg("-o")
                .arg(&extension)
                .args(["-p", platform.trim(), "-d", &duckdb_version])
                .output()
                .unwrap();
            assert!(
                package_output.status.success(),
                "extension packaging failed: {}",
                String::from_utf8_lossy(&package_output.stderr)
            );

            for (name, expected) in &core_results {
                let sql = format!(
                    "LOAD '{}'; SELECT cell FROM unnest(h3_polygon_to_cells(\
                     from_hex('{wkb_hex}'), 8, '{name}')) AS cells(cell) ORDER BY cell;",
                    extension.display()
                );
                let output = Command::new(&duckdb)
                    .args(["-unsigned", "-batch", "-csv", "-noheader", "-c", &sql])
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "DuckDB polygon query failed for {name}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let actual: Vec<u64> = String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| line.parse().unwrap())
                    .collect();
                assert_eq!(&actual, expected, "DuckDB mismatch for {name}");
            }
        }
    }
}
