//! Runs the built `raster-h3-cli` binary (`CARGO_BIN_EXE_raster-h3-cli`) and compares its
//! output with core batch readers and exporters run on the same synthetic raster.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::ipc::read_ipc_stream;
use raster_h3_core::columnar::{
    CategoricalBatchReader, CategoricalFormat, ColumnArray, ContinuousBatchReader, RecordBatch,
};
use raster_h3_core::pmtiles::H3PmtilesTiler;
use tempfile::NamedTempFile;

use crate::helpers::{corrupt_strip_geotiff, TestGeoTiffBuilder};

const BIN: &str = env!("CARGO_BIN_EXE_raster-h3-cli");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run raster-h3-cli")
}

fn run_ok(args: &[&str]) -> Output {
    let out = run(args);
    assert!(
        out.status.success(),
        "raster-h3-cli {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Asserts a non-zero exit whose stderr mentions `needle`.
fn run_err(args: &[&str], needle: &str) -> Output {
    let out = run(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "raster-h3-cli {args:?} should fail; stdout {} bytes",
        out.stdout.len()
    );
    assert!(
        stderr.contains(needle),
        "stderr of {args:?} should mention '{needle}':\n{stderr}"
    );
    out
}

/// 32x32 f32 gradient with values 1..=10 at 0.001° pixels.
fn gradient_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(32, 32)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| ((c + r) % 10 + 1) as f32)
}

/// 48x48 u8 classes 1..=5 in 4-pixel blocks.
fn classes_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(48, 48)
        .pixel_size(0.001)
        .create_tempfile(|c, r| ((c / 4 + r / 4) % 5 + 1) as u8)
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// `h3_index -> (mean, count, sum)` for continuous batches.
fn continuous_map(batches: &[RecordBatch]) -> BTreeMap<u64, (f64, f64, f64)> {
    let mut out = BTreeMap::new();
    for b in batches {
        let col = |name: &str| match b.column_by_name(name).unwrap() {
            ColumnArray::Float64 { values, .. } => values.clone(),
            other => panic!("{name}: {:?}", other.data_type()),
        };
        let ColumnArray::UInt64 { values: cells, .. } = b.column_by_name("h3_index").unwrap()
        else {
            panic!("h3_index must be UInt64");
        };
        let (mean, count, sum) = (col("mean"), col("count"), col("sum"));
        for i in 0..b.num_rows {
            out.insert(cells[i], (mean[i], count[i], sum[i]));
        }
    }
    out
}

fn core_continuous(path: &Path, cfg: &MultiResolutionConfig) -> BTreeMap<u64, (f64, f64, f64)> {
    let reader = ContinuousBatchReader::open(&[path.to_path_buf()], cfg, None, 2048, None).unwrap();
    continuous_map(&reader.map(|b| b.unwrap()).collect::<Vec<_>>())
}

/// `h3_index -> (majority_class, total_count)` from the core categorical reader.
fn core_categorical(path: &Path, cfg: &MultiResolutionConfig) -> BTreeMap<u64, (i64, f64)> {
    let reader = CategoricalBatchReader::open(
        &[path.to_path_buf()],
        cfg,
        CategoricalFormat::Wide,
        None,
        2048,
        None,
    )
    .unwrap();
    let mut out = BTreeMap::new();
    for b in reader.map(|b| b.unwrap()) {
        let (
            ColumnArray::UInt64 { values: cells, .. },
            ColumnArray::Int64 { values: class, .. },
            ColumnArray::Float64 { values: total, .. },
        ) = (
            b.column_by_name("h3_index").unwrap(),
            b.column_by_name("majority_class").unwrap(),
            b.column_by_name("total_count").unwrap(),
        )
        else {
            panic!("unexpected categorical column types");
        };
        for i in 0..b.num_rows {
            out.insert(cells[i], (class[i], total[i]));
        }
    }
    out
}

fn number(field: &Field) -> Option<f64> {
    Some(match *field {
        Field::Int(v) => f64::from(v),
        Field::Long(v) => v as f64,
        Field::UInt(v) => f64::from(v),
        Field::ULong(v) => v as f64,
        Field::Float(v) => f64::from(v),
        Field::Double(v) => v,
        _ => return None,
    })
}

fn ipc_map(stdout: &[u8]) -> BTreeMap<u64, (f64, f64, f64)> {
    let (_schema, batches) = read_ipc_stream(&mut Cursor::new(stdout)).unwrap();
    continuous_map(&batches)
}

fn jsonl_rows(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8(stdout.to_vec())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn bbox_narrows_output_to_the_core_bbox_result() {
    let (_tmp, path) = gradient_tif();
    let full = ipc_map(&run_ok(&["ipc", s(&path)]).stdout);
    assert_eq!(
        full,
        core_continuous(&path, &MultiResolutionConfig::new(vec![8]))
    );

    let bbox = [-122.45, 37.78, -122.435, 37.795];
    let out = run_ok(&["ipc", s(&path), "--bbox", "-122.45,37.78,-122.435,37.795"]);
    let narrowed = ipc_map(&out.stdout);
    let mut cfg = MultiResolutionConfig::new(vec![8]);
    cfg.bbox = Some(bbox);
    assert_eq!(narrowed, core_continuous(&path, &cfg));
    assert!(!narrowed.is_empty() && narrowed.len() < full.len());
    let total = |m: &BTreeMap<u64, (f64, f64, f64)>| m.values().map(|v| v.1).sum::<f64>();
    assert!(total(&narrowed) < total(&full));

    run_err(&["ipc", s(&path), "--bbox", "1,2,3"], "--bbox");
}

#[test]
fn nodata_override_matches_core_and_drops_pixels() {
    let (_tmp, path) = gradient_tif();
    let full = ipc_map(&run_ok(&["ipc", s(&path), "--crs", "EPSG:4326"]).stdout);
    let masked = ipc_map(&run_ok(&["ipc", s(&path), "--crs", "EPSG:4326", "--nodata", "5"]).stdout);

    let mut cfg = MultiResolutionConfig::new(vec![8]);
    cfg.custom_crs = Some("EPSG:4326".into());
    cfg.custom_nodata = Some(5.0);
    assert_eq!(masked, core_continuous(&path, &cfg));

    let count = |m: &BTreeMap<u64, (f64, f64, f64)>| m.values().map(|v| v.1).sum::<f64>();
    let fives = (0..32u32)
        .flat_map(|r| (0..32u32).map(move |c| (c + r) % 10 + 1))
        .filter(|&v| v == 5)
        .count() as f64;
    assert_eq!(count(&full) - count(&masked), fives);

    run_err(&["ipc", s(&path), "--nodata", "five"], "--nodata");
}

#[test]
fn limit_caps_rows_on_ipc_and_jsonl() {
    let (_tmp, path) = gradient_tif();
    let core = core_continuous(&path, &MultiResolutionConfig::new(vec![9]));
    assert!(core.len() > 5, "fixture needs more than 5 cells");

    let limited = ipc_map(&run_ok(&["ipc", s(&path), "-r", "9", "--limit", "5"]).stdout);
    assert_eq!(limited.len(), 5);
    for (cell, v) in &limited {
        assert_eq!(core.get(cell), Some(v), "{cell:#x} is not a core row");
    }

    let rows = jsonl_rows(&run_ok(&["jsonl", s(&path), "-r", "9", "--limit", "3"]).stdout);
    assert_eq!(rows.len(), 3);
    for row in &rows {
        let cell = row["h3_index"].as_u64().unwrap();
        assert_eq!(row["mean"].as_f64(), core.get(&cell).map(|v| v.0));
    }

    assert!(run_ok(&["jsonl", s(&path), "--limit", "0"])
        .stdout
        .is_empty());
    run_err(&["ipc", s(&path), "--limit", "-1"], "--limit");
}

#[test]
fn stdout_output_equals_file_output_and_core() {
    let (_tmp, path) = gradient_tif();
    let dir = tempfile::tempdir().unwrap();
    let core = core_continuous(&path, &MultiResolutionConfig::new(vec![8]));

    // No -o and `-o -` both write to stdout; `-o <file>` writes the same bytes to the file.
    let implicit = run_ok(&["ipc", s(&path)]).stdout;
    let dash = run_ok(&["ipc", s(&path), "-o", "-"]).stdout;
    let file = dir.path().join("out.ipc");
    let to_file = run_ok(&["ipc", s(&path), "-o", s(&file)]);
    assert!(to_file.stdout.is_empty());
    assert_eq!(ipc_map(&implicit), core);
    assert_eq!(ipc_map(&dash), core);
    assert_eq!(ipc_map(&std::fs::read(&file).unwrap()), core);

    let rows = jsonl_rows(&run_ok(&["jsonl", s(&path), "--columns", "h3_index,mean,count"]).stdout);
    assert_eq!(rows.len(), core.len());
    for row in &rows {
        let obj = row.as_object().unwrap();
        assert_eq!(obj.len(), 3, "projection keeps only 3 keys: {obj:?}");
        let (mean, count, _) = core[&row["h3_index"].as_u64().unwrap()];
        assert_eq!(row["mean"].as_f64(), Some(mean));
        assert_eq!(row["count"].as_f64(), Some(count));
    }
    let jsonl_file = dir.path().join("out.jsonl");
    run_ok(&[
        "jsonl",
        s(&path),
        "--columns",
        "h3_index,mean,count",
        "-o",
        s(&jsonl_file),
    ]);
    assert_eq!(
        jsonl_rows(&std::fs::read(&jsonl_file).unwrap()).len(),
        core.len()
    );

    // Categorical long format over stdout.
    let (_ctmp, classes) = classes_tif();
    let long = jsonl_rows(&run_ok(&["jsonl", s(&classes), "-c", "--format", "long"]).stdout);
    let cat_core = core_categorical(&classes, &MultiResolutionConfig::new(vec![8]));
    let mut per_cell: BTreeMap<u64, f64> = BTreeMap::new();
    for row in &long {
        *per_cell
            .entry(row["h3_index"].as_u64().unwrap())
            .or_default() += row["count"].as_f64().unwrap();
    }
    assert_eq!(per_cell.len(), cat_core.len());
    for (cell, total) in per_cell {
        assert!((total - cat_core[&cell].1).abs() < 1e-9);
    }
}

#[test]
fn categorical_parquet_matches_core() {
    let (_tmp, path) = classes_tif();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cat.parquet");
    run_ok(&["parquet", s(&path), "-c", "-r", "8", "-o", s(&out)]);

    let reader = SerializedFileReader::new(File::open(&out).unwrap()).unwrap();
    // Parquet names the core `majority_class` / `total_count` columns `majority` / `pixel_count`.
    let mut got = BTreeMap::new();
    for row in reader.get_row_iter(None).unwrap() {
        let row = row.unwrap();
        let cols: BTreeMap<&str, f64> = row
            .get_column_iter()
            .filter_map(|(name, field)| number(field).map(|v| (name.as_str(), v)))
            .collect();
        let h3 = row
            .get_column_iter()
            .find_map(|(name, field)| match (name.as_str(), field) {
                ("h3_index", Field::ULong(v)) => Some(*v),
                ("h3_index", Field::Long(v)) => Some(*v as u64),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no integer h3_index in {cols:?}"));
        got.insert(h3, (cols["majority"] as i64, cols["pixel_count"]));
    }
    assert_eq!(
        got,
        core_categorical(&path, &MultiResolutionConfig::new(vec![8]))
    );

    // GeoParquet metadata is written by default.
    let kv = reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .cloned()
        .unwrap_or_default();
    assert!(kv.iter().any(|k| k.key == "geo"));
}

#[test]
fn categorical_pmtiles_equals_core_export() {
    let (_tmp, path) = classes_tif();
    let dir = tempfile::tempdir().unwrap();
    let cli_out = dir.path().join("cli.pmtiles");
    let core_out = dir.path().join("core.pmtiles");
    run_ok(&["pmtiles", s(&path), "-c", "-r", "7,8", "-o", s(&cli_out)]);

    H3PmtilesTiler::process_categorical_source_to_pmtiles(
        &path,
        &core_out,
        MultiResolutionConfig::new(vec![7, 8]),
    )
    .unwrap();
    let cli_bytes = std::fs::read(&cli_out).unwrap();
    assert_eq!(&cli_bytes[..7], b"PMTiles");
    assert_eq!(cli_bytes[7], 3, "PMTiles spec version");
    assert_eq!(cli_bytes, std::fs::read(&core_out).unwrap());

    // The continuous archive of the same raster differs (different feature properties).
    let cont_out = dir.path().join("cont.pmtiles");
    run_ok(&["pmtiles", s(&path), "-r", "7,8", "-o", s(&cont_out)]);
    assert_ne!(std::fs::read(&cont_out).unwrap(), cli_bytes);
}

#[test]
fn bad_invocations_exit_non_zero_with_a_message() {
    let (_tmp, path) = gradient_tif();
    let dir = tempfile::tempdir().unwrap();

    run_err(
        &["frobnicate", s(&path)],
        "Unknown subcommand: 'frobnicate'",
    );
    let usage = run(&[]);
    assert!(!usage.status.success());
    assert!(String::from_utf8_lossy(&usage.stderr).contains("Usage:"));
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stderr).contains("Usage:"));

    run_err(&["ipc"], "Missing required input raster path");
    run_err(&["ipc", s(&path), "--bogus"], "Unknown option '--bogus'");
    run_err(&["ipc", s(&path), "extra.tif"], "Unexpected argument");
    run_err(&["ipc", s(&path), "-r", "16"], "-r");
    run_err(&["ipc", s(&path), "--format", "tall"], "--format");
    run_err(&["ipc", s(&path), "--columns", "h3_index,nope"], "nope");
    run_err(&["ipc", s(&path), "--crs"], "Missing value for --crs");
    run_err(
        &["parquet", s(&path)],
        "Missing -o / --output path for parquet",
    );
    run_err(
        &["pmtiles", s(&path)],
        "Missing -o / --output path for pmtiles",
    );

    // Files that do not exist, are not TIFFs, or fail to decode part-way through.
    let missing = dir.path().join("missing.tif");
    let not_tiff = dir.path().join("not.tif");
    std::fs::write(&not_tiff, b"definitely not a tiff").unwrap();
    let corrupt = corrupt_strip_geotiff();
    for sub in ["ipc", "jsonl"] {
        run_err(&[sub, s(&missing)], "Error:");
        run_err(&[sub, s(&not_tiff)], "Error:");
        run_err(&[sub, s(corrupt.path()), "--crs", "EPSG:4326"], "Error:");
    }
    let pq = dir.path().join("bad.parquet");
    let pm = dir.path().join("bad.pmtiles");
    run_err(&["parquet", s(&not_tiff), "-o", s(&pq)], "Error:");
    run_err(&["pmtiles", s(&not_tiff), "-o", s(&pm)], "Error:");
    run_err(&["parquet", s(corrupt.path()), "-o", s(&pq)], "Error:");
    assert!(
        !pq.exists(),
        "a failed export must not leave a parquet file"
    );
}
