//! raster-h3-pg integration tests, linked into a single test binary.
//!
//! The `fmgr` suite needs `--features pg-extension` (and `RASTER_H3_PG_MAJOR` or
//! `pg_config`); it links against the in-process server stand-ins in `fmgr/server.rs`.

#[path = "../../../raster-h3-core/tests/it/helpers.rs"]
mod helpers;

#[cfg(feature = "pg-extension")]
mod fmgr;
mod srf;
