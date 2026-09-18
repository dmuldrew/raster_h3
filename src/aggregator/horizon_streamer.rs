use h3o::CellIndex;

use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

const RAD_TO_DEG: f64 = 180.0 / std::f64::consts::PI;

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
/// H3 cell edges are gnomonic great-circle arcs on the unit sphere. In the northern hemisphere,
/// great-circle arcs between vertices bulge northward (away from the equator), so an edge's
/// minimum latitude is strictly attained at one of its vertex endpoints. In the southern hemisphere,
/// great-circle arcs bulge southward (toward the south pole), so the true minimum latitude can
/// occur along the interior of a southern boundary segment.
///
/// This function computes the exact closed-form analytic minimum latitude along every boundary
/// segment:
/// 1. Endpoint unit vectors $A, B$ define the great-circle normal $N = A \times B$.
/// 2. The candidate minimum latitude direction on the great circle is $V_{\min} = (N_z N_x, N_z N_y, -(N_x^2 + N_y^2))$.
/// 3. $V_{\min}$ is evaluated only when it lies strictly on the minor arc between $A$ and $B$.
/// 4. The candidate latitude $z_{\min} = -\sqrt{(N_x^2 + N_y^2)/\|N\|^2}$ is converted via $\arcsin$
///    with a mathematically derived numerical error tolerance $\text{tol} \propto \epsilon_{\text{mach}} / \cos\phi$.
/// 5. A cell containing the south pole is detected and assigned $-90.0^\circ$.
///
/// Invariant: `computed_south_lat <= true minimum latitude of the cell`.
pub fn compute_cell_south_lat(cell_u64: u64) -> f64 {
    let Ok(cell) = CellIndex::try_from(cell_u64) else {
        return -90.0;
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
        return -90.0;
    }
    let mut min_lat = f64::INFINITY;
    for i in 0..n {
        let (p, q) = (b[i], b[(i + 1) % n]);
        let p_lat = p.lat();
        let q_lat = q.lat();
        if p_lat <= -90.0 || q_lat <= -90.0 {
            return -90.0;
        }
        min_lat = min_lat.min(p_lat);

        // An interior minimum on a great-circle segment can only occur if at least one
        // vertex reaches the southern hemisphere or equator (great-circle arcs bulge
        // poleward, which in the northern hemisphere is northward, raising latitude).
        if p_lat <= 0.0 || q_lat <= 0.0 {
            let a = unit_xyz(p);
            let b_vec = unit_xyz(q);

            // Great-circle plane normal N = A x B
            let nx = a[1] * b_vec[2] - a[2] * b_vec[1];
            let ny = a[2] * b_vec[0] - a[0] * b_vec[2];
            let nz = a[0] * b_vec[1] - a[1] * b_vec[0];

            let n_xy2 = nx * nx + ny * ny;
            let n2 = n_xy2 + nz * nz;

            // Degenerate segment (endpoints coincident or antipodal) or equator-aligned segment
            if n2 > 1e-30 && n_xy2 > 1e-30 && n2.is_finite() {
                // Vector pointing toward the minimum-z extremum on the great circle:
                // V_min = (N_z * N_x, N_z * N_y, -(N_x^2 + N_y^2))
                let v_min = [nz * nx, nz * ny, -n_xy2];

                // Check if V_min lies on the open minor arc between A and B.
                // Using the planar orientation in the great-circle plane:
                // V_min = u * A + v * B with u > 0 and v > 0.
                let d = a[0] * b_vec[0] + a[1] * b_vec[1] + a[2] * b_vec[2];
                let v_dot_a = v_min[0] * a[0] + v_min[1] * a[1] + v_min[2] * a[2];
                let v_dot_b = v_min[0] * b_vec[0] + v_min[1] * b_vec[1] + v_min[2] * b_vec[2];

                let u = v_dot_b - d * v_dot_a;
                let v = v_dot_a - d * v_dot_b;

                if u > 0.0 && v > 0.0 {
                    let z_min = (-(n_xy2 / n2).sqrt()).clamp(-1.0, 0.0);
                    let phi_deg = z_min.asin().to_degrees();

                    // Numerical error bound:
                    // d(asin z)/dz = 1 / sqrt(1 - z^2) = sqrt(n2) / |nz|.
                    // Condition number increases near the pole as cos(lat) -> 0.
                    let cond = n2.sqrt() / nz.abs().max(1e-15);
                    let tol_deg = (RAD_TO_DEG * 32.0 * f64::EPSILON * cond).clamp(1e-13, 1e-7);

                    min_lat = min_lat.min(phi_deg - tol_deg);
                }
            }
        }
    }

    // Conservative endpoint floating-point tolerance:
    // H3 boundary vertex coordinates from h3o are in IEEE-754 f64 (~2 ULPs evaluation error).
    // Subtracting 1e-12° (~0.1 mm) guarantees computed_south_lat <= true minimum latitude.
    if !min_lat.is_finite() {
        -90.0
    } else {
        (min_lat - 1e-12).clamp(-90.0, 90.0)
    }
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
