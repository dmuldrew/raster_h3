//! # `raster-h3-core`
//!
//! Pure-Rust core engine for aggregating geospatial raster data (GeoTIFF, Cloud-Optimized GeoTIFFs)
//! into Uber H3 hexagonal grid cells, emitting Arrow C Data/Stream (`ArrowArrayStream`),
//! columnar `RecordBatch`es, Parquet/GeoParquet, and PMTiles v3 vector tile pyramids.

/// Scanline accumulation and statistical aggregation of raster pixels into H3 hexagonal cells.
pub mod aggregator;
/// Columnar `RecordBatch` representation, streaming readers, Arrow C Data/Stream interface, and IPC.
pub mod columnar;
/// Coordinate reference system (CRS) detection, parsing, and reprojection.
pub mod crs;
/// Shared zero-allocation hexadecimal formatting and WKB geometry encoding utilities.
pub mod encoding;
/// Crate-wide error handling types and result alias.
pub mod error;
/// Native Parquet and GeoParquet streaming export.
pub mod parquet;
/// Direct PMTiles v3 vector tile export with MVT encoding.
pub mod pmtiles;
/// GeoTIFF file decoding, metadata extraction, and chunked raster I/O.
pub mod raster;
/// Engine-neutral H3 scalar and utility operations.
pub mod scalar_ops;
/// Cross-format dataset transcoding pipelines (e.g. Parquet to PMTiles).
pub mod transcode;

pub use h3o;
pub use h3o::{CellIndex, LatLng, Resolution};
