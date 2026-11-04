//! `plan` → `ipc --partition` per partition → `merge` produces exactly the rows of one
//! unpartitioned `ipc` scan.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use raster_h3_core::columnar::ipc::read_ipc_stream;
use raster_h3_core::columnar::{ColumnArray, ColumnSchema, RecordBatch};
use tempfile::TempDir;

use crate::helpers::{write_geotiff, write_tiled, Crs, Layout};

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

fn run_err(args: &[&str], needle: &str) {
    let out = run(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "raster-h3-cli {args:?} should fail");
    assert!(
        stderr.contains(needle),
        "stderr of {args:?} should mention '{needle}':\n{stderr}"
    );
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

const GEO: Crs = Crs::Geographic {
    lon: -100.0,
    lat: 40.0,
    pixel_deg: 0.0005,
};

/// 256x192 f32 gradient in 32 px tiles (48 tiles), with a nodata corner.
fn tiled_gradient(dir: &Path) -> PathBuf {
    let path = dir.join("gradient.tif");
    write_tiled(&path, (256, 192), (32, 32), GEO, "-9999", |c, r| {
        if c < 20 && r < 20 {
            -9999.0f32
        } else {
            (c * 7 + r * 3) as f32 % 113.0 + (c as f32 / 17.0).sin()
        }
    });
    path
}

/// 200x160 u8 classes in 8-row strips (20 strips).
fn striped_classes(dir: &Path) -> PathBuf {
    let path = dir.join("classes.tif");
    write_geotiff(&path, (200, 160), Layout::Strips(8), GEO, "0", |c, r| {
        ((c / 9 + r / 7) % 6 + 1) as u8
    });
    path
}

/// Skip the `RH3PART1` header of a partition file's bytes, leaving its IPC stream.
fn partition_stream(bytes: &[u8]) -> &[u8] {
    assert_eq!(&bytes[..8], b"RH3PART1");
    let len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    &bytes[12 + len..]
}

fn read_part(path: &Path) -> (ColumnSchema, Vec<RecordBatch>) {
    let bytes = std::fs::read(path).unwrap();
    let (schema, batches) = read_ipc_stream(&mut partition_stream(&bytes)).unwrap();
    ((*schema).clone(), batches)
}

fn read_ipc(path: &Path) -> (ColumnSchema, Vec<RecordBatch>) {
    let (schema, batches) =
        read_ipc_stream(&mut BufReader::new(File::open(path).unwrap())).unwrap();
    ((*schema).clone(), batches)
}

#[derive(Debug, PartialEq)]
enum Cell {
    Float(f64),
    Other(String),
}

/// `(resolution, h3_index) -> row` (every column, in schema order).
fn rows(batches: &[RecordBatch]) -> BTreeMap<(u8, u64), Vec<Cell>> {
    let mut out = BTreeMap::new();
    for b in batches {
        let (
            Some(ColumnArray::UInt8 { values: res, .. }),
            Some(ColumnArray::UInt64 { values: h3, .. }),
        ) = (b.column_by_name("resolution"), b.column_by_name("h3_index"))
        else {
            panic!("missing key columns");
        };
        for i in 0..b.num_rows {
            let row = b
                .columns
                .iter()
                .map(|c| {
                    if c.validity().is_some_and(|v| !v[i]) {
                        return Cell::Other("NULL".into());
                    }
                    match c {
                        ColumnArray::Float64 { values, .. } => Cell::Float(values[i]),
                        ColumnArray::UInt64 { values, .. } => Cell::Other(values[i].to_string()),
                        ColumnArray::Int64 { values, .. } => Cell::Other(values[i].to_string()),
                        ColumnArray::UInt8 { values, .. } => Cell::Other(values[i].to_string()),
                        ColumnArray::Boolean { values, .. } => Cell::Other(values[i].to_string()),
                        ColumnArray::Utf8 { values, .. } => Cell::Other(values[i].clone()),
                        ColumnArray::Binary { values, .. } => {
                            Cell::Other(format!("{:?}", values[i]))
                        }
                    }
                })
                .collect();
            assert!(
                out.insert((res[i], h3[i]), row).is_none(),
                "duplicate cell {}",
                h3[i]
            );
        }
    }
    out
}

fn assert_same_rows(single: &[RecordBatch], merged: &[RecordBatch]) {
    let (a, b) = (rows(single), rows(merged));
    assert_eq!(a.len(), b.len(), "cell counts differ");
    for ((key, ra), (kb, rb)) in a.iter().zip(&b) {
        assert_eq!(key, kb, "cell sets differ");
        for (x, y) in ra.iter().zip(rb) {
            match (x, y) {
                (Cell::Float(x), Cell::Float(y)) => assert!(
                    (x - y).abs() <= 1e-9 * x.abs().max(1.0) || (x.is_nan() && y.is_nan()),
                    "{key:?}: {x} != {y}"
                ),
                _ => assert_eq!(x, y, "{key:?}"),
            }
        }
    }
}

/// Plan, scan every partition into `dir`, merge; returns (partition count, merged path).
fn fan_out(dir: &Path, raster: &Path, target_chunks: &str, scan_args: &[&str]) -> (usize, PathBuf) {
    let plan = dir.join(format!("plan-{target_chunks}.jsonl"));
    run_ok(&[
        "plan",
        s(raster),
        "--target-chunks",
        target_chunks,
        "-o",
        s(&plan),
    ]);
    let count = std::fs::read_to_string(&plan).unwrap().lines().count();
    let mut parts = Vec::new();
    for k in 0..count {
        let part = dir.join(format!("part-{target_chunks}-{k}.rh3"));
        let index = k.to_string();
        let spec = format!("@{}", s(&plan));
        let mut args = vec![
            "ipc",
            s(raster),
            "--partition",
            &spec,
            "--partition-index",
            &index,
        ];
        args.extend_from_slice(scan_args);
        args.extend_from_slice(&["-o", s(&part)]);
        run_ok(&args);
        parts.push(part);
    }
    // With several partitions some cells straddle an edge, so the merge has work to do.
    let partial_rows: usize = parts
        .iter()
        .flat_map(|p| read_part(p).1)
        .map(|b| match b.column_by_name("partial_state") {
            Some(c) => c.len() - c.null_count(),
            None => panic!("partition output lacks partial_state"),
        })
        .sum();
    assert_eq!(partial_rows > 0, count > 1, "{partial_rows} partial rows");
    let merged = dir.join(format!("merged-{target_chunks}.rh3"));
    let mut args = vec!["merge"];
    args.extend(parts.iter().map(|p| s(p)));
    args.extend_from_slice(&["-o", s(&merged)]);
    run_ok(&args);
    (count, merged)
}

#[test]
fn continuous_fan_out_matches_single_scan() {
    let dir = TempDir::new().unwrap();
    let raster = tiled_gradient(dir.path());
    let single = dir.path().join("single.rh3");
    run_ok(&["ipc", s(&raster), "-r", "8,9", "-o", s(&single)]);
    let (single_schema, single_batches) = read_ipc(&single);

    for target in ["1", "5", "1000"] {
        let (count, merged) = fan_out(dir.path(), &raster, target, &["-r", "8,9"]);
        assert_eq!(
            count > 1,
            target != "1000",
            "target {target}: {count} partitions"
        );
        let (schema, batches) = read_ipc(&merged);
        assert_eq!(schema, single_schema);
        assert_same_rows(&single_batches, &batches);
    }
}

#[test]
fn categorical_and_projected_fan_out_match_single_scan() {
    let dir = TempDir::new().unwrap();
    let raster = striped_classes(dir.path());
    let single = dir.path().join("single.rh3");
    run_ok(&["ipc", s(&raster), "-r", "9", "-c", "-o", s(&single)]);
    let (count, merged) = fan_out(dir.path(), &raster, "3", &["-r", "9", "-c"]);
    assert!(count > 1);
    let ((schema, batches), (single_schema, single_batches)) =
        (read_ipc(&merged), read_ipc(&single));
    assert_eq!(schema, single_schema);
    assert_same_rows(&single_batches, &batches);

    // A projection carries through the merge; `partial_state` may be listed.
    let raster = tiled_gradient(dir.path());
    let cols = "h3_index,resolution,mean,count,partial_state";
    let single = dir.path().join("single-proj.rh3");
    run_ok(&[
        "ipc",
        s(&raster),
        "-r",
        "8",
        "--columns",
        "h3_index,resolution,mean,count",
        "-o",
        s(&single),
    ]);
    let (count, merged) = fan_out(dir.path(), &raster, "4", &["-r", "8", "--columns", cols]);
    let ((schema, batches), (single_schema, single_batches)) =
        (read_ipc(&merged), read_ipc(&single));
    assert_eq!(schema, single_schema);
    assert_same_rows(&single_batches, &batches);

    // JSONL output of the merge has one line per cell.
    let parts: Vec<PathBuf> = (0..count)
        .map(|k| dir.path().join(format!("part-4-{k}.rh3")))
        .collect();
    let mut args = vec!["merge", "--jsonl"];
    args.extend(parts.iter().map(|p| s(p)));
    let text = String::from_utf8(run_ok(&args).stdout).unwrap();
    assert_eq!(text.lines().count(), rows(&single_batches).len());
    assert!(text.lines().all(|l| l.contains("\"mean\"")));
    assert!(!text.contains("partial_state"));
}

#[test]
fn a_spec_on_the_command_line_and_the_plan_on_stdout() {
    let dir = TempDir::new().unwrap();
    let raster = tiled_gradient(dir.path());
    let plan = run_ok(&["plan", s(&raster), "--target-chunks", "12"]);
    let plan = String::from_utf8(plan.stdout).unwrap();
    let specs: Vec<&str> = plan.lines().collect();
    assert!(specs.len() > 1);
    for (k, spec) in specs.iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(spec).unwrap();
        assert_eq!(v["index"], k);
        assert_eq!(v["count"], specs.len());
    }
    // A spec passed inline (as `xargs -d '\n' -I{}` would) or from a one-spec file.
    let inline = run_ok(&["ipc", s(&raster), "--partition", specs[1]]);
    let one = dir.path().join("one.json");
    std::fs::write(&one, specs[1]).unwrap();
    let from_file = run_ok(&["ipc", s(&raster), "--partition", &format!("@{}", s(&one))]);
    assert_eq!(inline.stdout, from_file.stdout);
    let schema = read_ipc_stream(&mut partition_stream(&inline.stdout))
        .unwrap()
        .0;
    assert_eq!(schema.fields.last().unwrap().name, "partial_state");
}

#[test]
fn misuse_is_rejected() {
    let dir = TempDir::new().unwrap();
    let raster = tiled_gradient(dir.path());
    let plan = dir.path().join("plan.jsonl");
    run_ok(&["plan", s(&raster), "--target-chunks", "12", "-o", s(&plan)]);
    let at_plan = format!("@{}", s(&plan));
    let r = s(&raster);

    run_err(&["plan", r, "--target-chunks", "0"], "positive integer");
    run_err(&["plan", r, "-r", "8"], "Unknown option");
    run_err(&["ipc", r, "--partition", &at_plan], "--partition-index");
    run_err(
        &["ipc", r, "--partition", &at_plan, "--partition-index", "9"],
        "out of range",
    );
    run_err(
        &["ipc", r, "--partition-index", "0"],
        "--partition @PLAN_FILE",
    );
    run_err(
        &["ipc", r, "--partition", "{\"index\":0}"],
        "invalid scan partition",
    );
    run_err(&["ipc", r, "--partition", "@/no/such/plan"], "cannot read");
    let p0 = ["--partition", &at_plan, "--partition-index", "0"];
    run_err(&[&["jsonl", r][..], &p0].concat(), "needs `ipc` output");
    run_err(&[&["ipc", r, "--limit", "5"][..], &p0].concat(), "--limit");
    run_err(
        &[&["ipc", r, "-c", "--format", "long"][..], &p0].concat(),
        "wide",
    );
    run_err(
        &[&["ipc", r, "--columns", "mean"][..], &p0].concat(),
        "h3_index",
    );
    run_err(
        &[&["parquet", r, "-o", "x.parquet"][..], &p0].concat(),
        "does not take --partition",
    );
    // A plan for one bbox is refused by a scan with another.
    run_err(
        &[&["ipc", r, "--bbox", "-100,39.95,-99.9,40"][..], &p0].concat(),
        "different mosaic",
    );

    run_err(&["merge"], "at least one partition file");
    run_err(&["merge", "/no/such/part.rh3"], "cannot open");
    let single = dir.path().join("single.rh3");
    run_ok(&["ipc", r, "-o", s(&single)]);
    run_err(&["merge", s(&single)], "not a partition file");
    std::fs::write(dir.path().join("junk.rh3"), b"not ipc").unwrap();
    run_err(
        &["merge", s(&dir.path().join("junk.rh3"))],
        "not a partition file",
    );

    // Partition `k` of `plan` scanned from `raster` with `extra` options.
    let scan = |raster: &str, plan: &str, k: usize, extra: &[&str], name: &str| {
        let out = dir.path().join(name);
        let (spec, index) = (format!("@{plan}"), k.to_string());
        let mut args = vec![
            "ipc",
            raster,
            "--partition",
            &spec,
            "--partition-index",
            &index,
        ];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-o", s(&out)]);
        run_ok(&args);
        out
    };
    let n = std::fs::read_to_string(&plan).unwrap().lines().count();
    assert!(n > 2);
    let full: Vec<PathBuf> = (0..n)
        .map(|k| scan(r, s(&plan), k, &[], &format!("full-{k}.rh3")))
        .collect();
    let merge = |files: &[&PathBuf], extra: &[&str], needle: &str| {
        let mut args = vec!["merge"];
        args.extend(files.iter().map(|p| s(p)));
        args.extend_from_slice(extra);
        run_err(&args, needle);
    };
    let all: Vec<&PathBuf> = full.iter().collect();
    run_ok(
        &[
            &["merge"][..],
            &all.iter().map(|p| s(p)).collect::<Vec<_>>(),
        ]
        .concat(),
    );

    // Every partition is required, once.
    merge(&all[1..], &[], "missing partition(s) 0 of");
    merge(
        &[&all[..], &[all[1]]].concat(),
        &[],
        "partition 1 is given twice",
    );
    // All partitions come from one scan: same options, columns, input and plan.
    let last = n - 1;

    let nodata = scan(r, s(&plan), last, &["--nodata", "5"], "nodata.rh3");
    merge(&swap(&all, &nodata), &[], "other options");
    let res = scan(r, s(&plan), last, &["-r", "9"], "res.rh3");
    merge(&swap(&all, &res), &[], "other options");
    let cols = scan(
        r,
        s(&plan),
        last,
        &["--columns", "h3_index,resolution,mean"],
        "cols.rh3",
    );
    merge(&swap(&all, &cols), &[], "other columns");
    let copy = dir.path().join("copy.tif");
    std::fs::copy(&raster, &copy).unwrap();
    let other_input = scan(s(&copy), s(&plan), last, &[], "copy.rh3");
    merge(&swap(&all, &other_input), &[], "other options");
    let plan2 = dir.path().join("plan2.jsonl");
    run_ok(&["plan", r, "--target-chunks", "1000", "-o", s(&plan2)]);
    let whole = scan(r, s(&plan2), 0, &[], "whole.rh3");
    merge(&[all[0], &whole], &[], "another plan");
    // Continuous partitions are not categorical.
    merge(&all, &["-c"], "not a categorical scan column");
}

/// `files` with its last entry replaced by `other`.
fn swap<'a>(files: &[&'a PathBuf], other: &'a PathBuf) -> Vec<&'a PathBuf> {
    let mut files = files.to_vec();
    *files.last_mut().unwrap() = other;
    files
}
