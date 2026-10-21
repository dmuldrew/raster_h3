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

fn run_cli(cli: &str, sql: &str) -> String {
    let result = Command::new(cli)
        .args(["-unsigned", "-batch", "-csv", "-noheader", "-c", sql])
        .output()
        .expect("run DuckDB CLI");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success() && !stderr.contains("Error:"),
        "DuckDB failed with status {:?}:\nstdout: {}\nstderr: {}",
        result.status.code(),
        String::from_utf8_lossy(&result.stdout),
        stderr
    );
    String::from_utf8(result.stdout).unwrap()
}

struct DuckDbHarness {
    cli: String,
    extension: PathBuf,
    temp: TempDir,
}

impl DuckDbHarness {
    fn new() -> Self {
        let cli = std::env::var("DUCKDB_BIN").unwrap_or_else(|_| "duckdb".into());
        let platform = run_cli(&cli, "PRAGMA platform");
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
