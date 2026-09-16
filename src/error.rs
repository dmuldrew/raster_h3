//! Crate-wide error type and [`Result`] alias for `raster_h3`.
//!
//! This module defines [`RasterH3Error`](crate::error::RasterH3Error), representing all recoverable errors that can
//! occur during raster decoding, coordinate reprojection, H3 aggregation, and DuckDB
//! extension lifecycle management.

use thiserror::Error;

/// Comprehensive error enumeration for the `raster_h3` crate.
#[derive(Error, Debug)]
pub enum RasterH3Error {
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

    /// DuckDB C-FFI or extension lifecycle error.
    #[error("DuckDB extension error: {0}")]
    DuckDbError(String),
}

/// Specialized [`Result`] type alias using [`RasterH3Error`].
pub type Result<T> = std::result::Result<T, RasterH3Error>;
