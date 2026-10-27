//! Crate-wide error type and [`Result`] alias for `raster_h3`.
//!
//! This module defines [`RasterH3Error`](crate::error::RasterH3Error), representing all recoverable errors that can
//! occur during raster decoding, coordinate reprojection, H3 aggregation, and DuckDB
//! extension lifecycle management.

use thiserror::Error;

/// Comprehensive error enumeration for the `raster_h3` crate.
#[derive(Error, Debug)]
pub enum RasterH3Error {
    /// A terminal failure while reading or aggregating a raster stream.
    #[error("Raster stream failed: {0}")]
    StreamFailed(String),

    /// File system I/O failures (open, read, mmap).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// TIFF/GeoTIFF decoder errors (corrupt headers, unsupported compression).
    #[error("TIFF decode error: {0}")]
    Tiff(#[from] tiff::TiffError),

    /// Missing or malformed GeoTIFF metadata (geotransform, dimensions).
    #[error("Invalid GeoTIFF metadata: {0}")]
    InvalidMetadata(String),

    /// No CRS found in raster metadata and none explicitly provided.
    #[error("CRS not detected: {0}")]
    CrsNotDetected(String),

    /// Coordinate transformation produced invalid results.
    #[error("CRS projection failed: {0}")]
    CrsProjectionFailed(String),

    /// General CRS configuration or parsing error.
    #[error("CRS transformation error: {0}")]
    CrsError(String),

    /// EPSG code not recognized by built-in fast paths or PROJ4 fallback.
    #[error("Unsupported EPSG code {code}: {detail}")]
    UnsupportedEpsg {
        /// EPSG spatial reference identifier code.
        code: u32,
        /// Detail explaining why the EPSG code cannot be resolved.
        detail: String,
    },

    /// Error from the proj4rs coordinate transformation library.
    #[error("PROJ error: {0}")]
    Proj(#[from] proj4rs::errors::Error),

    /// H3 cell indexing or resolution error.
    #[error("H3 conversion error: {0}")]
    H3Error(String),

    /// Invalid user-supplied parameter value.
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Strict transform mode: valid sample points could not be transformed to
    /// WGS84 (the count covers the work done before the stream stopped).
    #[error(
        "strict_transforms: {0} valid sample point(s) could not be transformed to WGS84 \
         (unsupported CRS, or points outside its valid domain); check the raster's CRS \
         or pass an explicit crs"
    )]
    TransformFailures(u64),

    /// DuckDB C-FFI or extension lifecycle error.
    #[error("DuckDB extension error: {0}")]
    DuckDbError(String),
}

/// Specialized [`Result`] type alias using [`RasterH3Error`].
pub type Result<T> = std::result::Result<T, RasterH3Error>;

/// Extracts a human-readable panic message from a panic payload while ensuring
/// payload disposal does not escape with a secondary panic across FFI or thread boundaries.
pub fn safe_panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    let msg_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic payload".to_string()
        }
    }));

    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
    {
        std::mem::forget(secondary);
    }

    msg_res.unwrap_or_else(|secondary| {
        std::mem::forget(secondary);
        "Unknown panic (payload downcast failed)".to_string()
    })
}

/// Runs `f`, returning `on_panic` instead of unwinding if it panics.
///
/// For `extern "C"` entry points: a panic that reaches the C ABI aborts the host process
/// (DuckDB, PostgreSQL backend, Python interpreter).
pub fn catch_unwind_or<T>(on_panic: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        safe_panic_payload_to_string(payload);
        on_panic
    })
}

/// Extracts a human-readable panic message from a panic payload.
pub fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    safe_panic_payload_to_string(payload)
}
