//! Guards the C symbols exported by the root cdylib, which CI packages as
//! `raster_h3.duckdb_extension`. Only the DuckDB entry points belong there; the
//! Python and PostgreSQL adapters are opt-in features precisely so their
//! `#[no_mangle]` functions are not exported from the DuckDB extension.

use std::path::PathBuf;
use std::process::Command;

/// The exact set of non-underscore-prefixed dynamic symbols the extension defines.
const DUCKDB_ENTRY_POINTS: [&str; 3] = [
    "raster_h3_init",
    "raster_h3_init_c_api",
    "raster_h3_version",
];

fn cdylib_path() -> PathBuf {
    // `cargo test` builds the lib target (cdylib + rlib) into the same `deps/`
    // directory as this test binary; only `cargo build` copies it up a level, so
    // the `deps/` copy is the one built by the current invocation.
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().join(format!(
        "{}raster_h3{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

#[test]
fn cdylib_exports_only_duckdb_entry_points() {
    let library = cdylib_path();
    assert!(
        library.exists(),
        "cdylib not found at {}; build it with cargo build --lib",
        library.display()
    );
    let output = match Command::new("nm")
        .args(["-D", "--defined-only", "--format=posix"])
        .arg(&library)
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping: `nm` is not installed");
            return;
        }
        Err(e) => panic!("failed to run nm: {e}"),
    };
    assert!(
        output.status.success(),
        "nm failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut exported: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        // Leading-underscore names are toolchain/runtime symbols, not our API.
        .filter(|name| !name.starts_with('_'))
        .map(str::to_owned)
        .collect();
    exported.sort();
    exported.dedup();
    assert_eq!(exported, DUCKDB_ENTRY_POINTS, "unexpected exported symbols");
}
