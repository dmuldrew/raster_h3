//! Real-engine SQL smoke test. Run after `cargo build --lib` with
//! `cargo test -p raster_h3 --test it duckdb_sql_smoke -- --ignored`.
//! DUCKDB_BIN and RASTER_H3_LIBRARY optionally select the CLI and built library.
//! CI runs it explicitly against the DuckDB CLI it downloads (see ci.yml).
use crate::helpers;

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn clean_stdout(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw).into_owned();
    // Strip ANSI escapes, then drop the CLI's Rosetta warning banner: it is
    // printed on every invocation when running under binary translation.
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out.lines()
        .filter(|l| {
            let t = l.trim();
            t != "WARNING:" && !t.contains("binary translation")
        })
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_cli(cli: &str, sql: &str) -> String {
    let result = Command::new(cli)
        .args(["-unsigned", "-batch", "-csv", "-noheader", "-c", sql])
        .output()
        .expect("run DuckDB CLI");
    let stdout = clean_stdout(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success() && !stderr.contains("Error:"),
        "DuckDB failed with status {:?}:\nstdout: {}\nstderr: {}",
        result.status.code(),
        stdout,
        stderr
    );
    stdout
}

struct DuckDbHarness {
    cli: String,
    extension: PathBuf,
    temp: TempDir,
}

impl DuckDbHarness {
    fn new() -> Self {
        let cli = std::env::var("DUCKDB_BIN").unwrap_or_else(|_| "duckdb".into());
        // The CLI may print warnings (e.g. a Rosetta notice) before the value;
        // the platform itself is the last line of plain identifier characters.
        // DUCKDB_PLATFORM overrides probing where the CLI output is unreliable.
        let platform = std::env::var("DUCKDB_PLATFORM")
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| {
                let platform_out = run_cli(&cli, "PRAGMA platform");
                platform_out
                    .lines()
                    .map(str::trim)
                    .rev()
                    .find(|line| {
                        !line.is_empty()
                            && line.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
                    .unwrap_or_else(|| {
                        panic!("PRAGMA platform did not report a platform:\n{platform_out}")
                    })
                    .to_string()
            });
        let library = std::env::var_os("RASTER_H3_LIBRARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let exe = std::env::current_exe().unwrap();
                exe.parent().unwrap().parent().unwrap().join(format!(
                    "{}raster_h3{}",
                    std::env::consts::DLL_PREFIX,
                    std::env::consts::DLL_SUFFIX
                ))
            });
        assert!(
            library.exists(),
            "Build the cdylib first: cargo build --lib"
        );
        let temp = tempfile::tempdir().unwrap();
        let extension = temp.path().join("raster_h3.duckdb_extension");
        let version = std::env::var("DUCKDB_VERSION").unwrap_or_else(|_| "v1.2.0".to_string());
        let result = Command::new(env!("CARGO_BIN_EXE_package_extension"))
            .arg("-i")
            .arg(library)
            .arg("-o")
            .arg(&extension)
            .args(["-p", platform.trim(), "-d", &version])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "extension packaging failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        Self {
            cli,
            extension,
            temp,
        }
    }

    fn query(&self, sql: &str) -> String {
        run_cli(
            &self.cli,
            &format!("LOAD {}; {sql}", quoted(&self.extension)),
        )
    }

    fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }
}

#[test]
#[ignore = "requires DuckDB CLI and a built cdylib (cargo build --lib)"]
fn real_sql_export_parameters_and_results() {
    let duckdb = DuckDbHarness::new();
    let (_file, tif) = helpers::TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(false)
        .create_constant_tempfile(42.0_f32);
    let parquet = duckdb.path("export.parquet");
    let pmtiles = duckdb.path("export.pmtiles");
    let categorical = duckdb.path("categorical.pmtiles");
    let sql = format!(
        r#"
        SELECT status = 'SUCCESS' AND total_hexagons > 0
        FROM h3_raster_to_parquet({tif}, {parquet}, crs := 'EPSG:4326',
            resolutions := '7,8', compact := false, geoparquet := true, workers := 2);
        SELECT count(DISTINCT (h3_index >> 52) & 15) = 2 AND sum(pixel_count) = 1152
            AND min(avg_value) = 42
        FROM read_parquet({parquet});
        SELECT status = 'SUCCESS' AND total_hexagons > 0
        FROM h3_raster_to_pmtiles({tif}, {pmtiles}, source_crs := 'EPSG:4326',
            resolution := 8, bbox := '-123,37,-122,38', overlap_rule := 'first',
            workers := 2, properties := 'mean,count');
        SELECT status = 'SUCCESS' AND total_hexagons > 0
        FROM h3_raster_to_pmtiles({tif}, {categorical}, crs := 'EPSG:4326',
            resolution := 8, categorical := true);
    "#,
        tif = quoted(&tif),
        parquet = quoted(&parquet),
        pmtiles = quoted(&pmtiles),
        categorical = quoted(&categorical)
    );
    assert_eq!(
        duckdb.query(&sql).lines().collect::<Vec<_>>(),
        vec!["true"; 4]
    );
    for path in [&pmtiles, &categorical] {
        assert_eq!(&std::fs::read(path).unwrap()[..7], b"PMTiles");
    }

    // h3_polygon_to_cells on a hand-built WKB POLYGON (BLOB works on every DuckDB version).
    let ring = [
        (-122.47, 37.73),
        (-122.37, 37.73),
        (-122.37, 37.81),
        (-122.47, 37.81),
        (-122.47, 37.73),
    ];
    let mut wkb = vec![1u8];
    wkb.extend(3u32.to_le_bytes());
    wkb.extend(1u32.to_le_bytes());
    wkb.extend((ring.len() as u32).to_le_bytes());
    for (x, y) in ring {
        wkb.extend(f64::to_le_bytes(x));
        wkb.extend(f64::to_le_bytes(y));
    }
    let hex: String = wkb.iter().map(|b| format!("{b:02x}")).collect();
    let sql = format!(
        r#"
        SELECT len(h3_polygon_to_cells(from_hex('{hex}'), 8)),
            len(h3_polygon_to_cells(from_hex('{hex}'), 8, 'contains_boundary')),
            len(h3_polygon_to_cells(from_hex('{hex}'), 8, 'intersects_boundary')),
            len(h3_polygon_to_cells(from_hex('{hex}'), 8, 'covers')),
            h3_polygon_to_cells(from_hex('{hex}'), 8, 'center')
                = h3_polygon_to_cells(from_hex('{hex}'), 8, 'contains_centroid'),
            h3_polygon_to_cells(from_hex('{hex}'), 8, 'full')
                = h3_polygon_to_cells(from_hex('{hex}'), 8, 'contains_boundary'),
            h3_polygon_to_cells(from_hex('{hex}'), 8, 'overlapping')
                = h3_polygon_to_cells(from_hex('{hex}'), 8, 'covers'),
            h3_polygon_to_cells(from_hex('00'), 8) IS NULL;
    "#
    );
    assert_eq!(
        duckdb.query(&sql).trim(),
        "102,79,128,128,true,true,true,true"
    );
}

#[test]
#[ignore = "requires DuckDB CLI and a built cdylib (cargo build --lib)"]
fn real_sql_raster_scans_and_parquet_parity() {
    let duckdb = DuckDbHarness::new();
    let (_constant_file, constant_tif) = helpers::TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(false)
        .create_constant_tempfile(42.0_f32);
    let parquet = duckdb.path("scan-parity.parquet");
    let sql = format!(
        r#"
        SELECT count(*) > 0 AND count(DISTINCT resolution) = 2
            AND bool_and(abs(mean - 42.0) < 1e-9)
        FROM h3_raster_continuous_aggregate({tif}, resolutions := '7,8',
            crs := 'EPSG:4326', compact := false, workers := 2);
        SELECT bool_and(pixel_total = 576)
        FROM (
            SELECT resolution, sum(count) AS pixel_total
            FROM h3_raster_continuous_aggregate({tif}, resolutions := '7,8',
                crs := 'EPSG:4326', compact := false, workers := 2)
            GROUP BY resolution
        );
        SELECT count(*) > 0 AND bool_and(abs(mean - 42.0) < 1e-9)
        FROM h3_raster_continuous({tif}, resolution := 8, crs := 'EPSG:4326');
        SELECT count(*) > 0 AND bool_and(abs(mean - 42.0) < 1e-9)
        FROM raster_h3({tif}, resolution := 8, crs := 'EPSG:4326');
        SELECT status = 'SUCCESS' AND total_hexagons > 0
        FROM h3_raster_to_parquet({tif}, {parquet}, crs := 'EPSG:4326',
            resolutions := '7,8', compact := false, workers := 2);
        WITH scan AS (
            SELECT h3_index, mean AS avg_value, count AS pixel_count,
                min AS min_value, max AS max_value, sum AS sum_value, resolution
            FROM h3_raster_continuous_aggregate({tif}, resolutions := '7,8',
                crs := 'EPSG:4326', compact := false, workers := 2)
        ),
        parquet AS (
            SELECT h3_index, avg_value, pixel_count, min_value, max_value, sum_value,
                CAST(((h3_index >> 52) & 15) AS UTINYINT) AS resolution
            FROM read_parquet({parquet})
        )
        SELECT NOT EXISTS (SELECT * FROM scan EXCEPT SELECT * FROM parquet)
            AND NOT EXISTS (SELECT * FROM parquet EXCEPT SELECT * FROM scan);
        "#,
        tif = quoted(&constant_tif),
        parquet = quoted(&parquet),
    );
    assert_eq!(
        duckdb.query(&sql).lines().collect::<Vec<_>>(),
        vec!["true"; 6]
    );

    let (_categories_file, categories_tif) = helpers::TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(false)
        .create_tempfile(|x, y| if (x + y) % 2 == 0 { 10_u16 } else { 20_u16 });
    let sql = format!(
        r#"
        SELECT count(*) > 0 AND bool_and(class_total = total_count)
        FROM (
            SELECT h3_index, resolution, total_count,
                sum(count) OVER (PARTITION BY h3_index, resolution) AS class_total
            FROM h3_raster_categorical_aggregate({tif}, resolutions := '7,8',
                format := 'long', crs := 'EPSG:4326', workers := 2)
        );
        SELECT count(*) > 0
        FROM h3_raster_categorical({tif}, resolution := 8, format := 'long',
            crs := 'EPSG:4326');
        "#,
        tif = quoted(&categories_tif),
    );
    assert_eq!(
        duckdb.query(&sql).lines().collect::<Vec<_>>(),
        vec!["true"; 2]
    );

    let (_projected_file, projected_tif) = helpers::TestGeoTiffBuilder::new(32, 32)
        .epsg(5070)
        .origin(1_618_000.0, 1_927_000.0)
        .pixel_size(200.0)
        .create_constant_tempfile(7.0_f32);
    let sql = format!(
        r#"
        SELECT count(*) > 0 AND bool_and(
            abs(h3_to_lat(h3_index) - 38.9072) < 0.5
            AND abs(h3_to_lng(h3_index) - (-77.0369)) < 0.5
        )
        FROM h3_raster_continuous_aggregate({tif}, resolution := 7, crs := 'EPSG:5070');
        "#,
        tif = quoted(&projected_tif),
    );
    assert_eq!(duckdb.query(&sql).trim(), "true");
}

#[test]
#[ignore = "requires DuckDB CLI and a built cdylib (cargo build --lib)"]
fn real_sql_parquet_to_pmtiles() {
    let duckdb = DuckDbHarness::new();
    let (_file, tif) = helpers::TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(false)
        .create_constant_tempfile(42.0_f32);
    let parquet = duckdb.path("pmtiles-input.parquet");
    let pmtiles = duckdb.path("from-parquet.pmtiles");
    let invalid_pmtiles = duckdb.path("missing-column.pmtiles");
    let sql = format!(
        r#"
        SELECT status = 'SUCCESS'
        FROM h3_raster_to_parquet({tif}, {parquet}, resolution := 7,
            crs := 'EPSG:4326', compact := false, workers := 2);
        SELECT status = 'SUCCESS' AND total_hexagons = (
            SELECT count(*) FROM read_parquet({parquet})
        )
        FROM h3_parquet_to_pmtiles({parquet}, {pmtiles},
            h3_column := 'h3_index', properties := 'avg_value,pixel_count');
        SELECT status LIKE 'ERROR:%missing_h3_column%' AND total_hexagons = 0
        FROM h3_parquet_to_pmtiles({parquet}, {invalid_pmtiles},
            h3_column := 'missing_h3_column');
        "#,
        tif = quoted(&tif),
        parquet = quoted(&parquet),
        pmtiles = quoted(&pmtiles),
        invalid_pmtiles = quoted(&invalid_pmtiles),
    );
    assert_eq!(
        duckdb.query(&sql).lines().collect::<Vec<_>>(),
        vec!["true"; 3]
    );
    let archive = std::fs::read(&pmtiles).unwrap();
    assert_eq!(&archive[..7], b"PMTiles");
    assert!(!invalid_pmtiles.exists());
}

#[test]
#[ignore = "requires DuckDB CLI and a built cdylib (cargo build --lib)"]
fn real_sql_h3_cell_to_geometry_alias() {
    let duckdb = DuckDbHarness::new();
    let version = duckdb.query("SELECT version()");
    let (spatial_setup, u64_wkb_eq, string_wkb_eq) = if version.starts_with("v1.2.") {
        (
            "",
            "hex(CAST(h3_cell_to_geometry(599686042433355775::UBIGINT) AS VARCHAR)) \
             = hex(CAST(h3_to_wkb(599686042433355775::UBIGINT) AS VARCHAR))",
            "hex(CAST(h3_cell_to_geometry('8828308281fffff') AS VARCHAR)) \
             = hex(CAST(h3_to_wkb('8828308281fffff') AS VARCHAR))",
        )
    } else {
        (
            "INSTALL spatial; LOAD spatial;",
            "ST_AsWKB(h3_cell_to_geometry(599686042433355775::UBIGINT)) \
             = h3_to_wkb(599686042433355775::UBIGINT)",
            "ST_AsWKB(h3_cell_to_geometry('8828308281fffff')) \
             = h3_to_wkb('8828308281fffff')",
        )
    };
    let sql = format!(
        r#"
        {spatial_setup}
        SELECT
            h3_cell_to_geometry(599686042433355775::UBIGINT) IS NOT NULL
            AND h3_cell_to_geometry(599686042433355775::UBIGINT)
                = h3_to_geometry(599686042433355775::UBIGINT)
            AND {u64_wkb_eq}
            AND h3_cell_to_geometry('8828308281fffff') IS NOT NULL
            AND h3_cell_to_geometry('8828308281fffff')
                = h3_to_geometry('8828308281fffff')
            AND {string_wkb_eq}
            AND h3_cell_to_geometry(0::UBIGINT) IS NULL
            AND h3_to_geometry(0::UBIGINT) IS NULL
            AND h3_cell_to_geometry('not-an-index') IS NULL
            AND h3_to_geometry('not-an-index') IS NULL;
    "#
    );
    assert_eq!(
        duckdb.query(&sql).trim(),
        "true",
        "DuckDB version: {version}"
    );
}

/// Like `query`, but returns `Err(stderr)` when DuckDB fails instead of
/// asserting — used for negative-path coverage.
/// Plan a raster, scan every partition with `partition :=`, merge the partial rows with
/// the merge aggregate, and compare the result with an unpartitioned scan, row for row.
fn assert_partitions_match_single_scan(
    duckdb: &DuckDbHarness,
    tif: &Path,
    function: &str,
    merge: &str,
    columns: &[&str],
    target: usize,
) -> usize {
    let path = quoted(tif);
    let specs: Vec<String> = duckdb
        .query(&format!(
            "SELECT spec FROM h3_raster_plan_partitions({path}, target_chunks := {target}) \
             ORDER BY partition_index"
        ))
        .lines()
        .map(|l| l.trim().trim_matches('"').replace("\"\"", "\""))
        .filter(|l| !l.is_empty())
        .collect();
    assert!(
        specs.len() > 1,
        "expected several partitions, got {specs:?}"
    );
    let parts = specs
        .iter()
        .map(|s| {
            format!("SELECT * FROM {function}({path}, resolutions := '8,9', partition := '{s}')")
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let cols = columns.join(", ");
    let fields = columns
        .iter()
        .map(|c| format!("cell.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Doubles are rounded: partial states merge in another order than the single scan.
    let rounded = columns
        .iter()
        .map(|c| match *c {
            "histogram" | "majority_class" | "unique_classes" => c.to_string(),
            _ => format!("round({c}, 9) AS {c}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let out = duckdb.query(&format!(
        r#"
        WITH parts AS ({parts}),
        partitioned AS (
            SELECT resolution, h3_index, {cols} FROM parts WHERE partial_state IS NULL
            UNION ALL
            SELECT resolution, h3_index, {fields} FROM (
                SELECT resolution, h3_index, {merge}(partial_state) AS cell
                FROM parts WHERE partial_state IS NOT NULL GROUP BY ALL)),
        p AS (SELECT resolution, h3_index, {rounded} FROM partitioned),
        s AS (SELECT resolution, h3_index, {rounded}
              FROM {function}({path}, resolutions := '8,9'))
        SELECT (SELECT count(*) FROM p) = (SELECT count(*) FROM s)
           AND (SELECT count(DISTINCT (resolution, h3_index)) FROM p) = (SELECT count(*) FROM s)
           AND (SELECT count(*) FROM (SELECT * FROM p EXCEPT ALL SELECT * FROM s)) = 0
           AND (SELECT count(*) FROM (SELECT * FROM s EXCEPT ALL SELECT * FROM p)) = 0
           AND (SELECT count(*) FROM parts WHERE partial_state IS NOT NULL) > 0;
        "#
    ));
    assert_eq!(
        out.trim(),
        "true",
        "{function}: partitioned result differs from the single scan"
    );
    specs.len()
}

#[test]
#[ignore = "requires DuckDB CLI and a built cdylib (cargo build --lib)"]
fn real_sql_partitioned_scans_merge_to_the_single_scan() {
    use helpers::{write_geotiff, Crs, Layout};
    let duckdb = DuckDbHarness::new();
    let sf = Crs::Geographic {
        lon: -122.45,
        lat: 37.80,
        pixel_deg: 0.0005,
    };
    let gradient = duckdb.path("partition-gradient.tif");
    write_geotiff(
        &gradient,
        (160, 120),
        Layout::Tiles(32, 32),
        sf,
        "-9999",
        |c, r| ((c * 7 + r * 3) % 50) as f32 + 1.0,
    );
    let classes = duckdb.path("partition-classes.tif");
    write_geotiff(&classes, (160, 120), Layout::Strips(8), sf, "0", |c, r| {
        ((c / 5 + r / 7) % 4 + 1) as u8
    });
    assert_partitions_match_single_scan(
        &duckdb,
        &gradient,
        "h3_raster_continuous_aggregate",
        "h3_merge_partials",
        &["mean", "stddev", "count", "min", "max", "sum"],
        4,
    );
    assert_partitions_match_single_scan(
        &duckdb,
        &classes,
        "h3_raster_categorical_aggregate",
        "h3_merge_categorical_partials",
        &[
            "majority_class",
            "majority_fraction",
            "unique_classes",
            "total_count",
            "histogram",
            "shannon_entropy",
        ],
        3,
    );

    // Misuse fails at bind time; a group of only NULL states merges to NULL.
    let spec = duckdb
        .query(&format!(
            "SELECT spec FROM h3_raster_plan_partitions({}, target_chunks := 4) LIMIT 1",
            quoted(&gradient)
        ))
        .trim()
        .trim_matches('"')
        .replace("\"\"", "\"");
    let ext = quoted(&duckdb.extension);
    for (sql, expected) in [
        (
            format!("SELECT * FROM h3_raster_continuous_aggregate({}, partition := '{{oops')", quoted(&gradient)),
            "Invalid partition parameter",
        ),
        (
            format!(
                "SELECT * FROM h3_raster_continuous_aggregate({}, partition := '{spec}', quantiles := 'p50')",
                quoted(&gradient)
            ),
            "quantiles",
        ),
        (
            format!(
                "SELECT * FROM h3_raster_categorical_aggregate({}, format := 'long', partition := '{spec}')",
                quoted(&classes)
            ),
            "wide categorical format",
        ),
        (
            format!("SELECT * FROM h3_raster_continuous_aggregate({}, partition := '{spec}')", quoted(&classes)),
            "different mosaic",
        ),
    ] {
        let err = try_query_cli(&duckdb.cli, &format!("LOAD {ext}; {sql}")).unwrap_err();
        assert!(err.contains(expected), "expected {expected:?} in: {err}");
    }
    assert_eq!(
        duckdb
            .query("SELECT h3_merge_partials(NULL::BLOB) IS NULL")
            .trim(),
        "true"
    );
}

fn try_query_cli(cli: &str, sql: &str) -> Result<String, String> {
    let result = Command::new(cli)
        .args(["-unsigned", "-batch", "-csv", "-noheader", "-c", sql])
        .output()
        .expect("run DuckDB CLI");
    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    if result.status.success() && !stderr.contains("Error:") {
        Ok(String::from_utf8(result.stdout).unwrap())
    } else {
        Err(format!(
            "{}{stderr}",
            String::from_utf8_lossy(&result.stdout)
        ))
    }
}

/// Coverage for the v1.5.6 C extension API build: session options, DuckDB
/// file-system raster reads, COPY output, and bind-time constant folding.
/// Runs only when the packaged library is the v1.5.6 build (signalled by
/// `DUCKDB_VERSION=v1.5.6`, the packaged C API version).
#[test]
#[ignore = "requires DuckDB CLI 2.0 and the v1.5.6-API cdylib"]
fn real_sql_v156_features() {
    if std::env::var("DUCKDB_VERSION").as_deref() != Ok("v1.5.6") {
        eprintln!("skipping: packaged library is not the v1.5.6-API build");
        return;
    }
    let duckdb = DuckDbHarness::new();

    // Session configuration options: SET + current_setting round-trip, and
    // a raster read routed through DuckDB's file system (local file here;
    // the same path covers httpfs/S3 secrets).
    let (_file, tif) = helpers::TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(false)
        .create_constant_tempfile(42.0_f32);
    let parquet = duckdb.path("fs_export.parquet");
    let sql = format!(
        r#"
        SET raster_h3_use_duckdb_fs = true;
        SET raster_h3_workers = 2;
        SET raster_h3_fetch_workers = 4;
        SET raster_h3_decode_budget_mb = 8;
        SELECT current_setting('raster_h3_use_duckdb_fs')::BOOLEAN = true
            AND current_setting('raster_h3_workers')::INTEGER = 2
            AND current_setting('raster_h3_fetch_workers')::INTEGER = 4
            AND current_setting('raster_h3_decode_budget_mb')::INTEGER = 8;
        SELECT status = 'SUCCESS' AND total_hexagons > 0
        FROM h3_raster_to_parquet({tif}, {parquet}, crs := 'EPSG:4326',
            resolutions := '7');
    "#,
        tif = quoted(&tif),
        parquet = quoted(&parquet)
    );
    assert_eq!(
        duckdb.query(&sql).lines().collect::<Vec<_>>(),
        vec!["true"; 2]
    );

    // COPY ... (FORMAT pmtiles): H3 index in the first column, remaining
    // columns become feature properties, output through DuckDB's file system.
    // The destination already holds a larger file: COPY must truncate it, not
    // leave stale bytes after the new archive.
    let pmtiles = duckdb.path("copy_out.pmtiles");
    std::fs::write(&pmtiles, vec![0xAB_u8; 4 << 20]).unwrap();
    let sql = format!(
        r#"
        COPY (
            SELECT h3_cell::UBIGINT AS c, 4.5::DOUBLE AS mean, 7::INTEGER AS count
            -- 85283473fffffff (res 5, zoom 8) and its res-4 parent (zoom 7)
            FROM (VALUES (599686042433355775::BIGINT), (595182446027210751::BIGINT)) t(h3_cell)
        ) TO {pmtiles} (FORMAT pmtiles, min_zoom 7, max_zoom 9);
    "#,
        pmtiles = quoted(&pmtiles)
    );
    duckdb.query(&sql);
    let bytes = std::fs::read(&pmtiles).unwrap();
    assert_eq!(&bytes[..7], b"PMTiles");
    // PMTiles v3 header: tile data is the last section, at offset 56 with its
    // length at 64, so the archive must end exactly where the tile data ends.
    let le_u64 = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    assert_eq!(
        bytes.len() as u64,
        le_u64(56) + le_u64(64),
        "stale bytes left after the archive"
    );
    // Both cells survive the zoom filter, so the archive is not empty.
    assert!(le_u64(72) > 0, "no tiles written: {} addressed", le_u64(72));

    // A non-NULL value that is not an H3 cell fails the COPY instead of being
    // dropped silently.
    let err = try_query_cli(
        &duckdb.cli,
        &format!(
            "LOAD {}; COPY (SELECT 1::UBIGINT c, 2.0 v) TO {} (FORMAT pmtiles);",
            quoted(&duckdb.extension),
            quoted(&duckdb.path("invalid_cell.pmtiles"))
        ),
    )
    .unwrap_err();
    assert!(
        err.contains("not a valid H3 cell index"),
        "unexpected error: {err}"
    );

    // Unknown COPY option names are rejected, not silently ignored.
    let err = try_query_cli(
        &duckdb.cli,
        &format!(
            "LOAD {}; COPY (SELECT 1::UBIGINT c, 2.0 v) TO {} (FORMAT pmtiles, bogus_opt 1);",
            quoted(&duckdb.extension),
            quoted(&duckdb.path("never.pmtiles"))
        ),
    )
    .unwrap_err();
    assert!(err.contains("BOGUS_OPT"), "unexpected error: {err}");

    // Bind-time folding: invalid constant resolution / mode fail at plan time.
    for (args, needle) in [
        ("from_hex('00'), 99", "outside 0..=15"),
        (
            "from_hex('00'), 8, 'no_such_mode'",
            "unknown containment mode",
        ),
    ] {
        let err = try_query_cli(
            &duckdb.cli,
            &format!(
                "LOAD {}; SELECT h3_polygon_to_cells({args});",
                quoted(&duckdb.extension)
            ),
        )
        .unwrap_err();
        assert!(err.contains(needle), "missing {needle} in: {err}");
    }

    // Bounded memory: ~23k res-9 cells with a 1 MiB budget spill several
    // sorted runs, and the merged archive matches the unspilled one byte for
    // byte. Spill files go to DuckDB's temp_directory, which only the
    // spilling COPY creates (as DuckDB itself does).
    let polygon = "010300000001000000050000000000000000a05ec00000000000c042400000000000805ec0\
                   0000000000c042400000000000805ec000000000000043400000000000a05ec0000000000000\
                   43400000000000a05ec00000000000c04240";
    let spill_dir = duckdb.path("duckdb_spill");
    let unspilled = duckdb.path("unspilled.pmtiles");
    let spilled = duckdb.path("spilled.pmtiles");
    let cells = format!(
        "SELECT cell, 'cell-' || cell::VARCHAR AS name, (cell % 1000)::DOUBLE AS v \
         FROM (SELECT unnest(h3_polygon_to_cells(from_hex('{polygon}'), 9)) AS cell)"
    );
    duckdb.query(&format!(
        r#"
        SET temp_directory = {spill_dir};
        COPY ({cells}) TO {unspilled} (FORMAT pmtiles);
    "#,
        spill_dir = quoted(&spill_dir),
        unspilled = quoted(&unspilled),
    ));
    assert!(
        !spill_dir.exists(),
        "a COPY that fits in memory created temp_directory"
    );
    duckdb.query(&format!(
        "SET temp_directory = {spill_dir}; \
         COPY ({cells}) TO {spilled} (FORMAT pmtiles, memory_budget_mb 1);",
        spill_dir = quoted(&spill_dir),
        spilled = quoted(&spilled),
    ));
    let unspilled_bytes = std::fs::read(&unspilled).unwrap();
    let spilled_bytes = std::fs::read(&spilled).unwrap();
    assert_eq!(&spilled_bytes[..7], b"PMTiles");
    assert!(
        spilled_bytes == unspilled_bytes,
        "spilled archive ({} bytes) differs from unspilled ({} bytes)",
        spilled_bytes.len(),
        unspilled_bytes.len()
    );
    assert!(
        spill_dir.is_dir(),
        "COPY did not use temp_directory {}",
        spill_dir.display()
    );

    // MEMORY_BUDGET_MB must be a positive integer.
    let err = try_query_cli(
        &duckdb.cli,
        &format!(
            "LOAD {}; COPY (SELECT 599686042433355775::UBIGINT c) TO {} (FORMAT pmtiles, memory_budget_mb 0);",
            quoted(&duckdb.extension),
            quoted(&duckdb.path("zero_budget.pmtiles"))
        ),
    )
    .unwrap_err();
    assert!(err.contains("MEMORY_BUDGET_MB"), "unexpected error: {err}");

    // Native GEOMETRY return type on the v1.5.6 build (no BLOB/alias fallback).
    let out = duckdb.query("SELECT typeof(h3_cell_to_geometry(599686042433355775::UBIGINT));");
    assert_eq!(out.trim(), "GEOMETRY");
}

#[test]
#[ignore = "requires DuckDB CLI and a built cdylib (cargo build --lib)"]
fn invalid_sampling_is_a_bind_error_in_all_raster_entry_points() {
    let duckdb = DuckDbHarness::new();
    let (_file, tif) = helpers::TestGeoTiffBuilder::new(4, 4).create_constant_tempfile(42.0_f32);
    let source = quoted(&tif);
    let parquet = quoted(&duckdb.path("invalid.parquet"));
    let pmtiles = quoted(&duckdb.path("invalid.pmtiles"));
    let queries = [
        format!("SELECT * FROM h3_raster_continuous_aggregate({source}, sampling := 'rgsss')"),
        format!("SELECT * FROM h3_raster_categorical_aggregate({source}, sampling := 'rgsss')"),
        format!("SELECT * FROM h3_raster_to_parquet({source}, {parquet}, sampling := 'rgsss')"),
        format!("SELECT * FROM h3_raster_to_pmtiles({source}, {pmtiles}, sampling := 'rgsss')"),
    ];
    for sql in queries {
        // The DuckDB CLI can exit 0 after a failed statement, so detect the
        // failure from stderr as the other negative-path tests do.
        let error = try_query_cli(
            &duckdb.cli,
            &format!("LOAD {}; {sql}", quoted(&duckdb.extension)),
        )
        .expect_err(&format!("{sql} unexpectedly succeeded"));
        assert!(error.contains("Invalid sampling"), "{sql}: {error}");
        assert!(error.contains("rgsss"), "{sql}: {error}");
    }
    assert!(!duckdb.path("invalid.parquet").exists());
    assert!(!duckdb.path("invalid.pmtiles").exists());
}
