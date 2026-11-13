use crate::crs::transformer::CrsTransformer;
use crate::grid::GridKind;
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
pub fn compute_cell_south_lat(cell_u64: u64) -> f64 {
    compute_cell_south_lat_for(GridKind::H3, cell_u64)
}

pub fn compute_cell_south_lat_for(grid: GridKind, cell_u64: u64) -> f64 {
    if grid.is_south_pole_cell(cell_u64) {
        return -90.0;
    }
    let mut verts = [(0.0, 0.0); 10];
    let n = grid.boundary_lng_lat(cell_u64, &mut verts);
    if n == 0 {
        return -90.0;
    }
    let mut south = 90.0_f64;
    for i in 0..n {
        let (p_lng, p_lat) = verts[i];
        let (q_lng, q_lat) = verts[(i + 1) % n];
        if grid == GridKind::A5 && (p_lat <= -89.0 || q_lat <= -89.0) {
            return -90.0;
        }
        let mut edge_south = p_lat.min(q_lat).next_down();
        // A5 edges are not great-circle arcs, so they can dip below both endpoints in
        // either hemisphere; pad every A5 edge.
        if grid == GridKind::A5 || p_lat <= 0.0 || q_lat <= 0.0 {
            // Longitude differences are bounded above by both the direct and wrapped routes.
            let direct_lo = (p_lng - q_lng).abs();
            let direct_hi = direct_lo.next_up();
            let wrapped_hi = (360.0_f64 - direct_lo.next_down()).next_up();
            let delta_lon = direct_hi.min(wrapped_hi);
            let delta_lat = (p_lat - q_lat).abs().next_up();
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
    // Uncertified estimates are discarded by rejection_bounds anyway.
    // Avoid projecting 121 probes just to test the whole globe.
    if !transformer.bounds_certified() {
        return crate::aggregator::multi_horizon::coordinates::rect_intersects_bbox(
            &[-180.0, -90.0, 180.0, 90.0],
            bbox,
        );
    }
    let bounds = transformer.transform_rect_bounds(
        gt,
        chunk.col_offset as f64,
        chunk.row_offset as f64,
        chunk.width as f64,
        chunk.height as f64,
    );
    bounds_intersect_bbox(transformer, bounds, bbox)
}

/// [`chunk_intersects_bbox`] for bounds the caller already computed with
/// [`CrsTransformer::transform_rect_bounds`].
pub(crate) fn bounds_intersect_bbox(
    transformer: &CrsTransformer,
    bounds: [f64; 4],
    bbox: &[f64; 4],
) -> bool {
    if !bounds[0].is_finite() {
        return true;
    }
    crate::aggregator::multi_horizon::coordinates::rect_intersects_bbox(
        &transformer.rejection_bounds(bounds),
        bbox,
    )
}
