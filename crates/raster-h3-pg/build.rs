//! Detects the target PostgreSQL major version when the `pg-extension` feature is enabled.
//!
//! The magic block layout and the `ERROR` elevel differ across majors, so the loadable
//! module must be compiled for the server it is installed into. The version comes from
//! `RASTER_H3_PG_MAJOR` when set, otherwise from `$PG_CONFIG --version` (default `pg_config`).

use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(pg_magic, values(\"v13\", \"v15\", \"v18\"))");
    println!("cargo::rerun-if-env-changed=RASTER_H3_PG_MAJOR");
    println!("cargo::rerun-if-env-changed=PG_CONFIG");

    if std::env::var_os("CARGO_FEATURE_PG_EXTENSION").is_none() {
        return;
    }

    let major = match std::env::var("RASTER_H3_PG_MAJOR") {
        Ok(v) => v
            .trim()
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("RASTER_H3_PG_MAJOR must be an integer, got '{v}'")),
        Err(_) => detect_from_pg_config(),
    };

    if !(13..=18).contains(&major) {
        panic!("raster-h3-pg supports PostgreSQL 13 through 18, found {major}");
    }

    let magic = match major {
        13 | 14 => "v13",
        15..=17 => "v15",
        _ => "v18",
    };
    // elog.h inserted WARNING_CLIENT_ONLY before ERROR in PostgreSQL 14.
    let error_level = if major >= 14 { 21 } else { 20 };

    println!("cargo::rustc-cfg=pg_magic=\"{magic}\"");
    println!("cargo::rustc-env=RASTER_H3_PG_MAJOR={major}");
    println!("cargo::rustc-env=RASTER_H3_PG_ERROR_LEVEL={error_level}");
}

fn detect_from_pg_config() -> u32 {
    let pg_config = std::env::var("PG_CONFIG").unwrap_or_else(|_| "pg_config".to_string());
    let output = Command::new(&pg_config)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "pg-extension feature needs the target PostgreSQL version: run with \
                 RASTER_H3_PG_MAJOR=<major> or put pg_config on PATH ({pg_config}: {e})"
            )
        });
    let text = String::from_utf8_lossy(&output.stdout);
    // e.g. "PostgreSQL 16.4 (Homebrew)" or "PostgreSQL 18beta1"
    text.split_whitespace()
        .nth(1)
        .and_then(|v| {
            let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        })
        .unwrap_or_else(|| panic!("could not parse `{pg_config} --version` output: {text}"))
}
