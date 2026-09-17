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

#[inline]
fn unit_xyz(ll: h3o::LatLng) -> [f64; 3] {
    let (la, lo) = (ll.lat_radians(), ll.lng_radians());
    [la.cos() * lo.cos(), la.cos() * lo.sin(), la.sin()]
}

/// Compute the exact southernmost latitude for an H3 cell.
///
/// H3 cell edges are gnomonic great-circle arcs. In the northern hemisphere, an edge's minimum
/// latitude is strictly attained at one of its vertex endpoints. In the southern hemisphere,
/// the geodesic arc bulges poleward (southward), so the minimum latitude can occur along the
/// interior of an edge. We densify edges with southern vertices across 8 interior points
/// to capture the true minimum latitude, with a 1e-9° (~0.1 mm) safety margin.
pub fn compute_cell_south_lat(cell_u64: u64) -> f64 {
    let Ok(cell) = CellIndex::try_from(cell_u64) else {
        return f64::NEG_INFINITY;
    };
    // A cell containing the south pole reaches -90° in its interior, well south of any
    // boundary vertex. Relevant for EPSG:3031 / south-polar rasters.
    if let Ok(pole) = h3o::LatLng::new(-90.0, 0.0) {
        if pole.to_cell(cell.resolution()) == cell {
            return -90.0;
        }
    }
    let b = cell.boundary();
    let n = b.len();
    if n == 0 {
        return f64::NEG_INFINITY;
    }
    let mut min_lat = f64::INFINITY;
    for i in 0..n {
        let (p, q) = (b[i], b[(i + 1) % n]);
        min_lat = min_lat.min(p.lat());
        // Southern hemisphere: a geodesic between two vertices bulges poleward (southward).
        if p.lat() < 0.0 || q.lat() < 0.0 {
            let (p3, q3) = (unit_xyz(p), unit_xyz(q));
            for k in 1..8 {
                let t = k as f64 / 8.0;
                let v = [
                    p3[0] * (1.0 - t) + q3[0] * t,
                    p3[1] * (1.0 - t) + q3[1] * t,
                    p3[2] * (1.0 - t) + q3[2] * t,
                ];
                let norm = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                min_lat = min_lat.min((v[2] / norm).asin().to_degrees());
            }
        }
    }
    min_lat - 1e-9
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
