//! DuckDB 2.0+ trusted custom extension repository end-to-end test. Run after
//! `cargo build --lib` with
//! `cargo test -p raster_h3 --test it duckdb_trusted_repository -- --ignored`.
//! DUCKDB_BIN and RASTER_H3_LIBRARY optionally select the CLI and built library; the
//! test is a no-op against DuckDB < 2.0, which has no `CREATE EXTENSION REPOSITORY`.
//! CI runs it in the DuckDB 2.0 step (see ci.yml).
//!
//! Unlike `duckdb_sql_smoke`, the CLI runs *without* `-unsigned`: the extension only
//! installs and loads if DuckDB accepts the signature `package_extension` wrote against
//! the key pinned from `.well-known/duckdb-extension-repo.json`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// `h3_to_string(599686042433355775)`; 599686042433355775 == 0x85283473fffffff.
const PROBE_SQL: &str = "SELECT h3_to_string(599686042433355775);";
const PROBE_CELL: &str = "85283473fffffff";

/// Forward slashes work for DuckDB paths on every platform and avoid escaping issues.
fn sql_path(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "''")
    )
}

fn output_text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn openssl(args: &[&str]) -> Output {
    let out = Command::new("openssl")
        .args(args)
        .output()
        .expect("openssl must be available");
    assert!(
        out.status.success(),
        "openssl failed: {}",
        output_text(&out)
    );
    out
}

fn gen_key(path: &Path) {
    openssl(&["genrsa", "-out", path.to_str().unwrap(), "2048"]);
}

/// SHA-256 of the DER public key, as DuckDB prints it for pinned keys.
fn key_fingerprint(private_key: &Path, temp: &Path) -> String {
    let der = temp.join(format!(
        "{}.pub.der",
        private_key.file_stem().unwrap().to_string_lossy()
    ));
    openssl(&[
        "pkey",
        "-in",
        private_key.to_str().unwrap(),
        "-pubout",
        "-outform",
        "DER",
        "-out",
        der.to_str().unwrap(),
    ]);
    let out = openssl(&["dgst", "-sha256", "-r", der.to_str().unwrap()]);
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_lowercase()
}

struct Harness {
    cli: String,
    library: PathBuf,
    platform: String,
    version_dir: String,
    temp: TempDir,
    home: PathBuf,
}

impl Harness {
    /// Returns `None` when the CLI predates DuckDB 2.0.
    fn new() -> Option<Self> {
        let cli = std::env::var("DUCKDB_BIN").unwrap_or_else(|_| "duckdb".into());
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut harness = Self {
            cli,
            library: std::env::var_os("RASTER_H3_LIBRARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    let exe = std::env::current_exe().unwrap();
                    exe.parent().unwrap().parent().unwrap().join(format!(
                        "{}raster_h3{}",
                        std::env::consts::DLL_PREFIX,
                        std::env::consts::DLL_SUFFIX
                    ))
                }),
            platform: String::new(),
            version_dir: String::new(),
            temp,
            home,
        };
        assert!(
            harness.library.exists(),
            "Build the cdylib first: cargo build --lib"
        );

        let probe = harness.run("SELECT version(); PRAGMA platform;");
        assert!(probe.status.success(), "{}", output_text(&probe));
        let stdout = String::from_utf8(probe.stdout).unwrap();
        let mut lines = stdout.lines().map(str::trim).filter(|l| !l.is_empty());
        // `version()` is the release tag (e.g. `v2.0.0`), which is also the
        // `${REVISION}` directory DuckDB requests from a repository.
        harness.version_dir = lines.next().unwrap().to_string();
        harness.platform = lines.next().unwrap().to_string();

        let major: u32 = harness
            .version_dir
            .trim_start_matches('v')
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        if major < 2 {
            eprintln!(
                "skipping: DuckDB {} has no trusted extension repositories",
                harness.version_dir
            );
            return None;
        }
        Some(harness)
    }

    /// Runs SQL in a fresh CLI process with signature checking enabled. HOME (and
    /// USERPROFILE on Windows) point at a scratch directory, which isolates both the
    /// pinned repository store (`~/.duckdb/extension_repositories`) and installed
    /// extensions (`~/.duckdb/extensions`) from the runner's real home.
    fn run(&self, sql: &str) -> Output {
        Command::new(&self.cli)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .args(["-batch", "-bail", "-csv", "-noheader", "-c", sql])
            .output()
            .expect("run DuckDB CLI")
    }

    /// Builds `<temp>/<name>/` with the manifest pinning `manifest_key` and the
    /// extension at `<version>/<platform>/raster_h3.duckdb_extension.gz`, signed with
    /// `signing_key` (or left unsigned).
    fn repository(&self, name: &str, manifest_key: &Path, signing_key: Option<&Path>) -> PathBuf {
        let repo = self.temp.path().join(name);
        let ext_dir = repo.join(&self.version_dir).join(&self.platform);
        std::fs::create_dir_all(&ext_dir).unwrap();

        let manifest = Command::new(env!("CARGO_BIN_EXE_package_extension"))
            .env_remove("DUCKDB_EXTENSION_SIGNING_KEY")
            .env_remove("DUCKDB_EXTENSION_PUBLIC_KEY")
            .arg("-k")
            .arg(manifest_key)
            .arg("-m")
            .arg(repo.join(".well-known").join("duckdb-extension-repo.json"))
            .output()
            .unwrap();
        assert!(manifest.status.success(), "{}", output_text(&manifest));

        let mut package = Command::new(env!("CARGO_BIN_EXE_package_extension"));
        package
            .env_remove("DUCKDB_EXTENSION_SIGNING_KEY")
            .env_remove("DUCKDB_EXTENSION_PUBLIC_KEY")
            .arg("-i")
            .arg(&self.library)
            .arg("-o")
            .arg(ext_dir.join("raster_h3.duckdb_extension.gz"))
            // Package with the C-API version tag matching the built library
            // (the v1.5.6 build is selected via RASTER_H3_LIBRARY).
            .args([
                "-p",
                &self.platform,
                "-d",
                &std::env::var("DUCKDB_VERSION").unwrap_or_else(|_| "v1.2.0".to_string()),
                "--gzip",
            ]);
        if let Some(key) = signing_key {
            package.arg("-k").arg(key);
        }
        let package = package.output().unwrap();
        assert!(package.status.success(), "{}", output_text(&package));
        repo
    }
}

#[test]
#[ignore = "requires a DuckDB 2.0+ CLI (DUCKDB_BIN) and openssl"]
fn duckdb_trusted_repository_installs_signed_extension() {
    let Some(h) = Harness::new() else {
        return;
    };
    let keys = h.temp.path().join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    let repo_key = keys.join("repo_key.pem");
    let other_key = keys.join("other_key.pem");
    gen_key(&repo_key);
    gen_key(&other_key);
    let fingerprint = key_fingerprint(&repo_key, &keys);

    // 1. Signed extension, key pinned from `.well-known/duckdb-extension-repo.json`:
    //    CREATE prints the fingerprint, INSTALL and LOAD verify the signature.
    let good = h.repository("good_repo", &repo_key, Some(&repo_key));
    let out = h.run(&format!(
        "SET allow_extension_repositories = 'allowed';
         CREATE EXTENSION REPOSITORY raster_h3_repo WITH PREFIX {};
         INSTALL raster_h3 FROM raster_h3_repo;
         LOAD raster_h3 FROM raster_h3_repo;
         {PROBE_SQL}",
        sql_path(&good)
    ));
    let text = output_text(&out);
    assert!(out.status.success(), "trusted install failed: {text}");
    assert!(
        text.contains(&format!("sha256:{fingerprint}")),
        "CREATE EXTENSION REPOSITORY did not pin the manifest key {fingerprint}: {text}"
    );
    assert!(text.contains(PROBE_CELL), "extension did not run: {text}");

    // 2. A later session loads from the pinned repository without opting in again
    //    and without allow_unsigned_extensions; the signature is re-checked on LOAD.
    let out = h.run(&format!("LOAD raster_h3 FROM raster_h3_repo; {PROBE_SQL}"));
    let text = output_text(&out);
    assert!(
        out.status.success(),
        "reload from trusted repo failed: {text}"
    );
    assert!(text.contains(PROBE_CELL), "extension did not run: {text}");

    // 3. Signed with a key the manifest does not pin: INSTALL must reject it.
    let wrong = h.repository("wrong_key_repo", &repo_key, Some(&other_key));
    let out = h.run(&format!(
        "SET allow_extension_repositories = 'allowed';
         CREATE EXTENSION REPOSITORY wrong_key_repo WITH PREFIX {};
         INSTALL raster_h3 FROM wrong_key_repo;",
        sql_path(&wrong)
    ));
    let text = output_text(&out);
    assert!(
        !out.status.success() && text.contains("valid signature"),
        "extension signed with an unpinned key was accepted: {text}"
    );

    // 4. Unsigned (zeroed signature slot) extension: INSTALL must reject it.
    let unsigned = h.repository("unsigned_repo", &repo_key, None);
    let out = h.run(&format!(
        "SET allow_extension_repositories = 'allowed';
         CREATE EXTENSION REPOSITORY unsigned_repo WITH PREFIX {};
         INSTALL raster_h3 FROM unsigned_repo;",
        sql_path(&unsigned)
    ));
    let text = output_text(&out);
    assert!(
        !out.status.success() && text.contains("valid signature"),
        "unsigned extension was accepted: {text}"
    );
}
