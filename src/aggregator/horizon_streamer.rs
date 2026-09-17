use h3o::CellIndex;

use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

/// Priority queue entry for H3 cell eviction ordered by southernmost latitude
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HexEvictionEntry {
    pub south_lat: f64,
    pub cell_u64: u64,
}

impl Eq for HexEvictionEntry {}

impl Ord for HexEvictionEntry {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap: highest south_lat (northernmost southern boundary) is popped first
        self.south_lat.total_cmp(&other.south_lat)
    }
}

impl PartialOrd for HexEvictionEntry {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Compute the conservative southernmost latitude for an H3 cell (center latitude minus max circumradius in degrees).
/// This guarantees that any cell with south_lat > lat_horizon lies strictly north of lat_horizon,
/// ensuring zero premature evictions and zero double-counting across batch boundaries.
#[inline(always)]
pub fn compute_cell_south_lat(cell_u64: u64) -> f64 {
    if let Ok(cell) = CellIndex::try_from(cell_u64) {
        let center_lat = h3o::LatLng::from(cell).lat();
        let r = crate::pmtiles::tiler::max_hex_radius_deg(cell.resolution().into());
        center_lat - r
    } else {
        f64::NEG_INFINITY
    }
}

pub use crate::aggregator::nodata::{
    is_chunk_all_nodata, is_decoding_result_all_nodata, NodataCast,
};

/// Check if a 2D chunk intersects the given [min_lon, min_lat, max_lon, max_lat] bounding box
pub fn chunk_intersects_bbox(
    chunk: &RasterChunk,
    gt: &GeoTransform,
    transformer: &CrsTransformer,
    bbox: &[f64; 4],
) -> bool {
    let [b_min_lon, b_min_lat, b_max_lon, b_max_lat] = *bbox;

    let [c_min_lon, c_min_lat, c_max_lon, c_max_lat] = transformer.transform_rect_bounds(
        gt,
        chunk.col_offset as f64,
        chunk.row_offset as f64,
        chunk.width as f64,
        chunk.height as f64,
    );

    if !c_min_lon.is_finite() {
        return true;
    }

    c_min_lon <= b_max_lon
        && c_max_lon >= b_min_lon
        && c_min_lat <= b_max_lat
        && c_max_lat >= b_min_lat
}
