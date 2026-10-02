use crate::h3::CellIndex;

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

/// Conservative lower latitude bound for the spherical polygon returned by h3o.
///
/// Latitude is 1-Lipschitz in angular distance. Every point on a minor great-circle
/// edge is at most half that edge's length from one endpoint. The path consisting
/// of a meridian followed by a parallel has length at most |delta_lat|+|delta_lon|,
/// so `min(endpoint_latitudes) - (|delta_lat|+|delta_lon|)/2` is a lower bound.
/// Unlike a stationary-point test this needs no ill-conditioned cross product,
/// inverse trigonometry, or floating-point arc-membership decision. Bounds are
/// deliberately loose; spilling handles any additional retained cells.
///
/// Basic arithmetic is rounded outward with next_up/next_down. This bound is
/// relative to h3o's supplied spherical boundary, not a survey/datum accuracy claim.
/// Northern edges have their minimum at an endpoint. The south pole must be
/// checked separately because it can be in the polygon interior.
static SOUTH_POLE_CELLS: std::sync::OnceLock<[CellIndex; 16]> = std::sync::OnceLock::new();

#[inline]
fn is_south_pole_cell(cell: CellIndex) -> bool {
    let cells = SOUTH_POLE_CELLS.get_or_init(|| {
        core::array::from_fn(|r| {
            crate::h3::LatLng::new(-90.0, 0.0)
                .unwrap()
                .to_cell(crate::h3::Resolution::try_from(r as u8).unwrap())
        })
    });
    cells[u8::from(cell.resolution()) as usize] == cell
}

pub fn compute_cell_south_lat(cell_u64: u64) -> f64 {
    let Ok(cell) = CellIndex::try_from(cell_u64) else {
        return -90.0;
    };
    if is_south_pole_cell(cell) {
        return -90.0;
    }
    let boundary = cell.boundary();
    if boundary.is_empty() {
        return -90.0;
    }
    let mut south = 90.0_f64;
    for i in 0..boundary.len() {
        let p = boundary[i];
        let q = boundary[(i + 1) % boundary.len()];
        let mut edge_south = p.lat().min(q.lat()).next_down();
        if p.lat() <= 0.0 || q.lat() <= 0.0 {
            // Longitude differences are bounded above by both the direct and wrapped routes.
            let direct_lo = (p.lng() - q.lng()).abs();
            let direct_hi = direct_lo.next_up();
            let wrapped_hi = (360.0_f64 - direct_lo.next_down()).next_up();
            let delta_lon = direct_hi.min(wrapped_hi);
            let delta_lat = (p.lat() - q.lat()).abs().next_up();
            let half_length = ((delta_lat + delta_lon).next_up() * 0.5).next_up();
            edge_south = (edge_south - half_length).next_down();
        }
        if !edge_south.is_finite() {
            return -90.0;
        }
        south = south.min(edge_south);
    }
    south.max(-90.0)
}

pub use crate::aggregator::nodata::{
    is_chunk_all_nodata, is_decoding_result_all_nodata, NodataCast,
};

/// Check if a 2D chunk intersects the given [min_lon, min_lat, max_lon, max_lat] bounding box.
/// Supports antimeridian-crossing bounding boxes where `min_lon > max_lon`.
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

    let lat_intersects = c_min_lat <= b_max_lat && c_max_lat >= b_min_lat;
    if !lat_intersects {
        return false;
    }

    if b_min_lon <= b_max_lon {
        c_min_lon <= b_max_lon && c_max_lon >= b_min_lon
    } else {
        // Antimeridian-crossing bounding box (e.g. Fiji [178, -20, -178, -15])
        (c_min_lon <= 180.0 && c_max_lon >= b_min_lon)
            || (c_min_lon <= b_max_lon && c_max_lon >= -180.0)
    }
}
