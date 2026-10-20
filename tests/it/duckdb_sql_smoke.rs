//! Real-engine SQL smoke test. Run after `cargo build --lib` with
//! `cargo test -p raster_h3 --test it duckdb_sql_smoke -- --ignored`.
//! DUCKDB_BIN and RASTER_H3_LIBRARY optionally select the CLI and built library.
//! CI runs it explicitly against the DuckDB CLI it downloads (see ci.yml).
use crate::helpers;

use std::path::Path;
use std::process::Command;

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

#[test]
#[ignore = "requires DuckDB CLI and a built cdylib (cargo build --lib)"]
fn real_sql_export_parameters_and_results() {
    let cli = std::env::var("DUCKDB_BIN").unwrap_or_else(|_| "duckdb".into());
    let query = |sql: &str| {
        let result = Command::new(&cli)
            .args(["-unsigned", "-batch", "-csv", "-noheader", "-c", sql])
            .output()
            .expect("run DuckDB CLI");
        assert!(
            result.status.success(),
            "DuckDB failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    let platform = query("PRAGMA platform");
    let library = std::env::var_os("RASTER_H3_LIBRARY")
        .map(std::path::PathBuf::from)
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
    let result = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .arg("-i")
        .arg(library)
        .arg("-o")
        .arg(&extension)
        .args(["-p", platform.trim(), "-d", "v1.2.0"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let (_file, tif) = helpers::TestGeoTiffBuilder::new(24, 24)
        .with_geokeys(false)
        .create_constant_tempfile(42.0_f32);
    let parquet = temp.path().join("export.parquet");
    let pmtiles = temp.path().join("export.pmtiles");
    let categorical = temp.path().join("categorical.pmtiles");
    let sql = format!(
        r#"
        LOAD {extension};
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
        extension = quoted(&extension),
        tif = quoted(&tif),
        parquet = quoted(&parquet),
        pmtiles = quoted(&pmtiles),
        categorical = quoted(&categorical)
    );
    assert_eq!(query(&sql).lines().collect::<Vec<_>>(), vec!["true"; 4]);
    for path in [&pmtiles, &categorical] {
        assert_eq!(&std::fs::read(path).unwrap()[..7], b"PMTiles");
    }
}
