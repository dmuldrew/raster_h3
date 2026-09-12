//! PMTiles v3 Vector Hexagon Tile Generation Module
//!
//! Provides zero-intermediate-file streaming aggregation of GeoTIFF rasters
//! into Mapbox Vector Tile (MVT) format and single-file PMTiles v3 archives.

pub mod features;
pub mod mvt;
pub mod parquet_tiler;
pub mod pyramid;
pub mod tiler;
pub mod writer;

pub use features::{H3Feature, PmtilesExportSummary, ResolutionAccumulatorStats};
pub use mvt::{FeatureProperties, MercatorPoint, MvtFeature, MvtLayer, MvtValue};
pub use parquet_tiler::{process_parquet_to_pmtiles, RowGroupExtent};
pub use pyramid::{
    cell_boundary_mercator, cell_tile_range, cell_tile_range_mercator, h3_res_for_zoom,
    h3_res_to_zoom, lon_lat_to_tile_xy, max_hex_radius_deg, mercator_to_tile_xy,
    tile_xy_to_bbox, zoom_to_h3_res, zooms_for_h3_res,
};
pub use tiler::H3PmtilesTiler;
pub use writer::{zxy_to_tile_id, PmtilesWriter, TilePayload};

