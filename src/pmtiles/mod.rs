//! PMTiles v3 Vector Hexagon Tile Generation Module
//!
//! Provides zero-intermediate-file streaming aggregation of GeoTIFF rasters
//! into Mapbox Vector Tile (MVT) format and single-file PMTiles v3 archives.

pub mod mvt;
pub mod writer;
pub mod tiler;

pub use mvt::{MvtFeature, MvtLayer, MvtValue, MercatorPoint};
pub use writer::{PmtilesWriter, TilePayload, zxy_to_tile_id};
pub use tiler::{H3PmtilesTiler, h3_res_to_zoom, lon_lat_to_tile_xy, tile_xy_to_bbox};
