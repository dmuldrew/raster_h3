//! Real-server PostgreSQL extension test. Requires Docker; builds the module in a Linux
//! container via `scripts/pg_e2e.sh` and loads it into `postgres:<major>`.
//!
//! `cargo test -p raster_h3 --test it pg_extension_e2e -- --ignored --nocapture`
//!
//! RASTER_H3_PG_VERSIONS picks the server majors (comma separated, default `16`);
//! RASTER_H3_PG_E2E_KEEP=1 leaves the containers running for manual `psql` sessions.
use crate::helpers;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use raster_h3::core::scalar_ops;
use raster_h3::pg::{PgCategoricalRow, PgCategoricalSrf, PgContinuousRow, PgContinuousSrf};

const CELL: u64 = 0x8828308281fffff;
const CRS: &str = "EPSG:4326";

/// Removes the server container when the test ends, including on assertion failure.
struct Server {
    major: String,
    container: String,
}

impl Server {
    fn start(major: &str) -> Self {
        let script = repo_root().join("scripts/pg_e2e.sh");
        let out = Command::new(&script)
            .arg(major)
            .stderr(Stdio::inherit())
            .output()
            .expect("run scripts/pg_e2e.sh (is Docker running?)");
        assert!(out.status.success(), "pg_e2e.sh {major} failed");
        let container = String::from_utf8(out.stdout).unwrap().trim().to_string();
        Self {
            major: major.to_string(),
            container,
        }
    }

    /// Copy a host file into the container, readable by the `postgres` server user.
    fn upload(&self, host: &Path, name: &str) -> String {
        let dest = format!("/tmp/{name}");
        run_ok(Command::new("docker").args([
            "cp",
            &host.to_string_lossy(),
            &format!("{}:{dest}", self.container),
        ]));
        run_ok(Command::new("docker").args(["exec", &self.container, "chmod", "644", &dest]));
        dest
    }

    /// Run `sql` in one psql session. With `stop_on_error`, any error fails the session.
    fn psql(&self, sql: &str, stop_on_error: bool) -> Output {
        let mut child = Command::new("docker")
            .args([
                "exec",
                "-i",
                &self.container,
                "psql",
                "-U",
                "postgres",
                "-X",
                "-q",
            ])
            .args(["-A", "-t", "-F", "|", "-v"])
            .arg(format!("ON_ERROR_STOP={}", u8::from(stop_on_error)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(sql.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Run `sql` and return its stdout lines, failing on any error.
    fn query(&self, sql: &str) -> Vec<String> {
        let out = self.psql(sql, true);
        assert!(
            out.status.success(),
            "PG {} query failed:\n{sql}\n{}",
            self.major,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn logs(&self) -> String {
        let out = Command::new("docker")
            .args(["logs", &self.container])
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if std::env::var_os("RASTER_H3_PG_E2E_KEEP").is_none() {
            let _ = Command::new(repo_root().join("scripts/pg_e2e.sh"))
                .args([&self.major, "--stop"])
                .status();
        }
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_ok(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn assert_float(actual: &str, expected: f64, what: &str) {
    let got: f64 = actual
        .parse()
        .unwrap_or_else(|_| panic!("{what}: '{actual}' is not a float"));
    let tol = 1e-12 * expected.abs().max(1.0);
    assert!(
        (got - expected).abs() <= tol,
        "{what}: got {got}, expected {expected}"
    );
}

fn assert_opt_float(actual: &str, expected: Option<f64>, what: &str) {
    match expected {
        None => assert_eq!(actual, "", "{what}: expected NULL"),
        Some(v) => assert_float(actual, v, what),
    }
}

fn sql_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn continuous_rows(
    path: &Path,
    res: u8,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
) -> Vec<PgContinuousRow> {
    let srf = PgContinuousSrf::open(
        &path.to_string_lossy(),
        vec![res],
        Some(CRS.into()),
        nodata,
        bbox,
    )
    .unwrap();
    let mut rows: Vec<_> = srf.collect::<raster_h3::core::error::Result<_>>().unwrap();
    rows.sort_by_key(|r| r.h3_index);
    rows
}

fn categorical_rows(path: &Path) -> Vec<PgCategoricalRow> {
    let srf = PgCategoricalSrf::open(
        &path.to_string_lossy(),
        vec![8],
        Some(CRS.into()),
        None,
        None,
    )
    .unwrap();
    let mut rows: Vec<_> = srf.collect::<raster_h3::core::error::Result<_>>().unwrap();
    rows.sort_by_key(|r| r.h3_index);
    rows
}

fn check_continuous(lines: &[String], expected: &[PgContinuousRow], label: &str) {
    assert!(!expected.is_empty(), "{label}: fixture produced no rows");
    assert_eq!(lines.len(), expected.len(), "{label}: row count");
    for (line, e) in lines.iter().zip(expected) {
        let f: Vec<&str> = line.split('|').collect();
        assert_eq!(f.len(), 10, "{label}: {line}");
        assert_eq!(f[0], e.h3_index.to_string(), "{label}: h3_index");
        assert_eq!(f[1], e.h3_hex, "{label}: h3_hex");
        assert_float(f[2], e.mean, "mean");
        assert_opt_float(f[3], e.stddev, "stddev");
        assert_float(f[4], e.count, "count");
        assert_float(f[5], e.min, "min");
        assert_float(f[6], e.max, "max");
        assert_float(f[7], e.sum, "sum");
        assert_eq!(f[8], e.resolution.to_string(), "{label}: resolution");
        assert_eq!(f[9], hex(&e.wkb), "{label}: wkb");
    }
}

fn check_server(major: &str, cont_tif: &Path, cat_tif: &Path) {
    let server = Server::start(major);
    let cont = sql_text(&server.upload(cont_tif, "continuous.tif"));
    let cat = sql_text(&server.upload(cat_tif, "categorical.tif"));
    println!("PG {major}: container {} ready", server.container);

    // Loading the library checks the magic block; CREATE EXTENSION resolves every symbol.
    assert_eq!(
        server.query(
            "CREATE EXTENSION raster_h3;
             SELECT extversion FROM pg_extension WHERE extname = 'raster_h3';"
        ),
        ["0.4.0"]
    );

    // Scalar functions on a known cell.
    let cell = CELL as i64;
    let cell_hex = scalar_ops::h3_to_string(CELL).unwrap();
    let lines = server.query(&format!(
        "SELECT h3_to_lat({cell}), h3_to_lng({cell}), h3_get_resolution({cell}),
                h3_is_valid({cell}), h3_is_valid('{cell_hex}'), h3_to_string({cell}),
                string_to_h3('{cell_hex}'), h3_cell_to_parent({cell}, 5),
                encode(h3_to_wkb({cell}), 'hex');"
    ));
    let f: Vec<&str> = lines[0].split('|').collect();
    assert_float(f[0], scalar_ops::h3_to_lat(CELL), "h3_to_lat");
    assert_float(f[1], scalar_ops::h3_to_lng(CELL), "h3_to_lng");
    assert_eq!(f[2], "8");
    assert_eq!(&f[3..5], ["t", "t"]);
    assert_eq!(f[5], cell_hex);
    assert_eq!(f[6], cell.to_string());
    let parent = scalar_ops::h3_cell_to_parent_u64(CELL, 5).unwrap();
    assert_eq!(f[7], (parent as i64).to_string());
    assert_eq!(f[8], hex(&scalar_ops::h3_to_wkb_u64(CELL).unwrap()));

    // Invalid input yields NULL / false rather than errors.
    assert_eq!(
        server.query(&format!(
            "SELECT h3_to_lat(0) IS NULL, h3_get_resolution(0) IS NULL, h3_is_valid(0),
                    h3_is_valid('zz'), string_to_h3('zz') IS NULL,
                    h3_to_string(NULL::bigint) IS NULL, h3_cell_to_parent({cell}, 12) IS NULL,
                    h3_to_wkb(0) IS NULL;"
        )),
        ["t|t|f|f|t|t|t|t"]
    );

    // Continuous SRF matches the in-process Rust iterator row for row.
    let cols = "h3_index, h3_hex, mean, stddev, count, min, max, sum, resolution, \
                encode(wkb, 'hex')";
    let lines = server.query(&format!(
        "SELECT {cols} FROM h3_raster_continuous_aggregate({cont}, 8, '{CRS}') ORDER BY 1;"
    ));
    check_continuous(
        &lines,
        &continuous_rows(cont_tif, 8, None, None),
        "continuous",
    );

    // nodata, bbox and resolution arguments reach the engine.
    let bbox = [-122.45, 37.79, -122.44, 37.80];
    let bbox_sql = format!("'{},{},{},{}'", bbox[0], bbox[1], bbox[2], bbox[3]);
    let lines = server.query(&format!(
        "SELECT {cols} FROM h3_raster_continuous_aggregate({cont}, 7, '{CRS}', 5.0, {bbox_sql})
         ORDER BY 1;"
    ));
    check_continuous(
        &lines,
        &continuous_rows(cont_tif, 7, Some(5.0), Some(bbox)),
        "continuous with options",
    );
    // Defaulted arguments (h3_res 8, no CRS override) still run.
    let lines = server.query(&format!(
        "SELECT count(*) > 0 FROM h3_raster_continuous_aggregate({cont});"
    ));
    assert_eq!(lines, ["t"]);

    // Categorical SRF parity.
    let expected = categorical_rows(cat_tif);
    assert!(!expected.is_empty());
    let lines = server.query(&format!(
        "SELECT h3_index, h3_hex, majority_class, majority_fraction, majority_count,
                unique_classes, total_count, histogram, resolution, shannon_entropy,
                encode(wkb, 'hex')
         FROM h3_raster_categorical_aggregate({cat}, 8, '{CRS}') ORDER BY 1;"
    ));
    assert_eq!(lines.len(), expected.len(), "categorical row count");
    for (line, e) in lines.iter().zip(&expected) {
        let f: Vec<&str> = line.split('|').collect();
        assert_eq!(f.len(), 11, "{line}");
        assert_eq!(f[0], e.h3_index.to_string());
        assert_eq!(f[1], e.h3_hex);
        assert_eq!(f[2], e.majority_class.to_string());
        assert_float(f[3], e.majority_fraction, "majority_fraction");
        assert_float(f[4], e.majority_count, "majority_count");
        assert_eq!(f[5], e.unique_classes.to_string());
        assert_float(f[6], e.total_count, "total_count");
        assert_eq!(f[7], e.histogram);
        assert_eq!(f[8], e.resolution.to_string());
        assert_float(f[9], e.shannon_entropy, "shannon_entropy");
        assert_eq!(f[10], hex(&e.wkb));
    }

    // Early termination: the executor abandons the SRF mid-stream, so the Rust state must
    // be released by the memory-context callback. Repeat to surface crashes or leaks.
    let mut sql = String::new();
    for _ in 0..25 {
        sql += &format!(
            "SELECT count(*) FROM (SELECT * FROM h3_raster_continuous_aggregate({cont}, 8, '{CRS}') \
             LIMIT 3) s;\n"
        );
        sql += &format!(
            "SELECT count(*) FROM (SELECT * FROM h3_raster_categorical_aggregate({cat}, 8, '{CRS}') \
             LIMIT 0) s;\n"
        );
    }
    let lines = server.query(&sql);
    assert_eq!(lines.len(), 50);
    assert!(lines.chunks(2).all(|p| p == ["3", "0"]), "{lines:?}");

    // Errors surface as SQL ERRORs and leave the session usable.
    for (call, needle) in [
        (
            format!("h3_raster_continuous_aggregate({cont}, 8, '{CRS}', NULL, '1,2,3')"),
            "bbox must be",
        ),
        (
            format!("h3_raster_continuous_aggregate({cont}, 16)"),
            "h3_res must be between 0 and 15",
        ),
        (
            "h3_raster_continuous_aggregate(NULL)".to_string(),
            "raster_path must not be NULL",
        ),
        (
            "h3_raster_categorical_aggregate('/nonexistent.tif')".to_string(),
            "ERROR",
        ),
    ] {
        let out = server.psql(
            &format!("SELECT count(*) FROM {call};\nSELECT 'alive';\n"),
            false,
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stderr.contains("ERROR") && stderr.contains(needle),
            "{call}: {stderr}"
        );
        assert_eq!(
            stdout.trim(),
            "alive",
            "{call}: session not usable after error"
        );
    }

    // No backend crashed along the way.
    let logs = server.logs();
    assert!(
        !logs.contains("terminated by signal") && !logs.contains("PANIC"),
        "PG {major} server log shows a crash:\n{logs}"
    );
    println!("PG {major}: all checks passed");
}

#[test]
#[ignore = "requires Docker; builds and loads the extension into postgres containers"]
fn pg_extension_in_real_server() {
    let versions = std::env::var("RASTER_H3_PG_VERSIONS").unwrap_or_else(|_| "16".into());

    let builder = helpers::TestGeoTiffBuilder::new(32, 32).with_geokeys(true);
    let (_cont_file, cont_tif) = builder.create_f32_tempfile(|c, r| (c + 2 * r) as f32);
    let (_cat_file, cat_tif) =
        builder.create_f32_tempfile(|c, r| (1 + c / 8 + 4 * (r / 16)) as f32);

    for major in versions.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        check_server(major, &cont_tif, &cat_tif);
    }
}
