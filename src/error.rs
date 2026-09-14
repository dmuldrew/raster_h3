use thiserror::Error;

#[derive(Error, Debug)]
pub enum RasterH3Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TIFF decode error: {0}")]
    Tiff(#[from] tiff::TiffError),

    #[error("Invalid GeoTIFF metadata: {0}")]
    InvalidMetadata(String),

    #[error("CRS not detected: {0}")]
    CrsNotDetected(String),

    #[error("CRS projection failed: {0}")]
    CrsProjectionFailed(String),

    #[error("CRS transformation error: {0}")]
    CrsError(String),

    #[error("PROJ error: {0}")]
    Proj(#[from] proj4rs::errors::Error),

    #[error("H3 conversion error: {0}")]
    H3Error(String),

    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("DuckDB extension error: {0}")]
    DuckDbError(String),
}

pub type Result<T> = std::result::Result<T, RasterH3Error>;
