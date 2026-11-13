//! Polygon fill for grids whose cells nest exactly inside their parents (S2, HEALPix).
//!
//! The fill walks the cell hierarchy from the base cells, descending only into cells
//! whose padded lon/lat box overlaps the polygon's box, and tests the cells at the
//! target resolution against the polygon. Tests are planar in lon/lat degrees, the
//! same model the cell geometry is published in (WKB, GeoParquet, vector tiles).

use geo_types::{MultiPolygon, Polygon};

use crate::grid::GridKind;
use crate::scalar_ops::{ContainmentMode, PolygonToCellsError, H3_POLYGON_TO_CELLS_MAX_CELLS};

/// `[west, south, east, north]` in degrees; `west..east` may leave `[-180, 180]`.
type LonLatBox = [f64; 4];

struct Ring {
    /// Unclosed vertices, `(lon, lat)` in degrees.
    points: Vec<(f64, f64)>,
}

impl Ring {
    fn new(coords: impl Iterator<Item = (f64, f64)>) -> Result<Self, PolygonToCellsError> {
        let mut points: Vec<(f64, f64)> = coords.collect();
        if points.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
            return Err(PolygonToCellsError::InvalidGeometry(
                "non-finite coordinates",
            ));
        }
        if points.iter().any(|p| !(-90.0..=90.0).contains(&p.1)) {
            return Err(PolygonToCellsError::InvalidGeometry(
                "latitude outside [-90, 90]",
            ));
        }
        if points.len() > 1 && points.first() == points.last() {
            points.pop();
        }
        Ok(Self { points })
    }

    fn edges(&self) -> impl Iterator<Item = ((f64, f64), (f64, f64))> + '_ {
        let n = self.points.len();
        (0..n).map(move |i| (self.points[i], self.points[(i + 1) % n]))
    }

    /// Even-odd ray cast.
    fn contains(&self, (x, y): (f64, f64)) -> bool {
        let mut inside = false;
        for ((x1, y1), (x2, y2)) in self.edges() {
            if (y1 > y) != (y2 > y) && x < x1 + (y - y1) / (y2 - y1) * (x2 - x1) {
                inside = !inside;
            }
        }
        inside
    }
}

struct Area {
    exterior: Ring,
    holes: Vec<Ring>,
    bbox: LonLatBox,
}

impl Area {
    fn new(polygon: &Polygon<f64>) -> Result<Option<Self>, PolygonToCellsError> {
        let exterior = Ring::new(polygon.exterior().coords().map(|c| (c.x, c.y)))?;
        if exterior.points.len() < 3 {
            return Ok(None);
        }
        let holes = polygon
            .interiors()
            .iter()
            .map(|ring| Ring::new(ring.coords().map(|c| (c.x, c.y))))
            .collect::<Result<Vec<_>, _>>()?;
        let bbox = bbox_of(&exterior.points);
        Ok(Some(Self {
            exterior,
            holes,
            bbox,
        }))
    }

    fn rings(&self) -> impl Iterator<Item = &Ring> {
        std::iter::once(&self.exterior).chain(&self.holes)
    }

    fn contains(&self, p: (f64, f64)) -> bool {
        self.exterior.contains(p) && !self.holes.iter().any(|hole| hole.contains(p))
    }

    /// Whether any polygon edge crosses any edge of `cell`.
    fn boundary_crosses(&self, cell: &Ring) -> bool {
        self.rings().any(|ring| {
            ring.edges()
                .any(|(a, b)| cell.edges().any(|(c, d)| segments_cross(a, b, c, d)))
        })
    }
}

fn bbox_of(points: &[(f64, f64)]) -> LonLatBox {
    let mut b = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for &(x, y) in points {
        b[0] = b[0].min(x);
        b[1] = b[1].min(y);
        b[2] = b[2].max(x);
        b[3] = b[3].max(y);
    }
    b
}

fn boxes_overlap(a: &LonLatBox, b: &LonLatBox) -> bool {
    a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3]
}

fn orient(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// Proper crossing or touching of segments `ab` and `cd`.
fn segments_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let (d1, d2) = (orient(c, d, a), orient(c, d, b));
    let (d3, d4) = (orient(a, b, c), orient(a, b, d));
    if (d1 > 0.0) != (d2 > 0.0) && (d3 > 0.0) != (d4 > 0.0) && d1 != d2 && d3 != d4 {
        return true;
    }
    let on = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
        r.0 >= p.0.min(q.0) && r.0 <= p.0.max(q.0) && r.1 >= p.1.min(q.1) && r.1 <= p.1.max(q.1)
    };
    (d1 == 0.0 && on(c, d, a))
        || (d2 == 0.0 && on(c, d, b))
        || (d3 == 0.0 && on(a, b, c))
        || (d4 == 0.0 && on(a, b, d))
}

/// Segments per cell side: coarse cells have strongly curved sides in lon/lat, so they
/// are traced; from level 5 (cells under about 3 degrees) the corners suffice.
fn side_segments(level: u8) -> u32 {
    1 << 5u8.saturating_sub(level)
}

/// A cell's boundary in lon/lat degrees, counter-clockwise, sides traced along their
/// true curve with `segments` steps each.
fn traced_boundary(grid: GridKind, cell: u64, segments: u32) -> Option<Vec<(f64, f64)>> {
    match grid {
        GridKind::S2 => {
            let id = s2::cellid::CellID(cell);
            if !id.is_valid() {
                return None;
            }
            let corners = s2::cell::Cell::from(id).vertices();
            let mut points = Vec::with_capacity(4 * segments as usize);
            for side in 0..4 {
                let (a, b) = (corners[side].0, corners[(side + 1) % 4].0);
                for step in 0..segments {
                    // A chord point pushed back to the sphere stays on the great circle.
                    let t = f64::from(step) / f64::from(segments);
                    let (x, y, z) = (
                        a.x + (b.x - a.x) * t,
                        a.y + (b.y - a.y) * t,
                        a.z + (b.z - a.z) * t,
                    );
                    points.push((y.atan2(x).to_degrees(), z.atan2(x.hypot(y)).to_degrees()));
                }
            }
            Some(points)
        }
        GridKind::Healpix => {
            let (depth, pixel) = crate::grid::healpix_decode(cell)?;
            let path = healpix::get(depth).path_along_cell_edge(
                pixel,
                healpix::dir::Cardinal::S,
                false,
                segments,
            );
            Some(
                path.iter()
                    .map(|p| (p.lon.to_degrees(), p.lat.to_degrees()))
                    .collect(),
            )
        }
        GridKind::H3 | GridKind::A5 => None,
    }
}

/// A cell's lon/lat ring for planar tests: sides traced, a pole vertex spread along the
/// pole, a pole in the interior closed over the pole, and longitudes unwrapped so
/// consecutive vertices are within 180 degrees.
fn cell_ring(grid: GridKind, cell: u64, level: u8) -> Option<Ring> {
    let traced = traced_boundary(grid, cell, side_segments(level))?;
    let n = traced.len();
    let mut points = Vec::with_capacity(n + 2);
    for i in 0..n {
        let (lon, lat) = traced[i];
        if lat.abs() >= 90.0 - 1e-9 {
            // A pole has no longitude: run along it between the neighbours' meridians.
            points.push((traced[(i + n - 1) % n].0, lat.clamp(-90.0, 90.0)));
            points.push((traced[(i + 1) % n].0, lat.clamp(-90.0, 90.0)));
        } else {
            points.push((lon, lat));
        }
    }
    for i in 1..points.len() {
        while points[i].0 - points[i - 1].0 > 180.0 {
            points[i].0 -= 360.0;
        }
        while points[i].0 - points[i - 1].0 < -180.0 {
            points[i].0 += 360.0;
        }
    }
    if let Some(north) = grid.interior_pole(cell) {
        // The boundary circles the pole once: close the ring over it.
        let pole = if north { 90.0 } else { -90.0 };
        let (first, last) = (points[0].0, points[points.len() - 1].0);
        points.push((last, pole));
        points.push((first, pole));
    }
    Some(Ring { points })
}

/// A box that holds every point of the cell and of its descendants. Edges are curves
/// in lon/lat, so the vertex box is padded by half the longest edge's lon/lat length
/// (no point of an edge is further than that from an endpoint), with the longitude
/// pad stretched by the convergence of the meridians.
fn padded_box(ring: &Ring) -> LonLatBox {
    let mut b = bbox_of(&ring.points);
    let pad = ring
        .edges()
        .map(|(p, q)| 0.5 * ((p.0 - q.0).abs() + (p.1 - q.1).abs()))
        .fold(0.0, f64::max);
    b[1] = (b[1] - pad).max(-90.0);
    b[3] = (b[3] + pad).min(90.0);
    let max_abs_lat = b[1].abs().max(b[3].abs());
    if max_abs_lat >= 85.0 {
        b[0] = -540.0;
        b[2] = 540.0;
    } else {
        let lon_pad = pad / max_abs_lat.to_radians().cos();
        b[0] -= lon_pad;
        b[2] += lon_pad;
    }
    b
}

/// Whole turns to try when laying a cell's ring over the polygon.
const TURNS: [f64; 3] = [0.0, -360.0, 360.0];

fn shifted(b: &LonLatBox, turn: f64) -> LonLatBox {
    [b[0] + turn, b[1], b[2] + turn, b[3]]
}

fn too_many(estimate: usize) -> PolygonToCellsError {
    PolygonToCellsError::TooManyCells {
        estimate,
        limit: H3_POLYGON_TO_CELLS_MAX_CELLS,
    }
}

/// Cells of `grid` at `res` whose area could fall inside `bbox`, as an upper estimate.
fn estimate_cells(grid: GridKind, res: u8, bbox: &LonLatBox) -> usize {
    const EARTH_RADIUS_M: f64 = 6_371_007.2;
    let lon_span = (bbox[2] - bbox[0]).clamp(0.0, 360.0).to_radians();
    let band = bbox[3].to_radians().sin() - bbox[1].to_radians().sin();
    let area = lon_span * band.max(0.0) * EARTH_RADIUS_M * EARTH_RADIUS_M;
    // Cells along the box's edge are visited too.
    let cell_area = grid.area_m2(res);
    // sqrt(area) undercounts the perimeter of long, thin boxes, even tending
    // to zero for a line that crosses millions of cells.
    let height = (bbox[3] - bbox[1]).to_radians() * EARTH_RADIUS_M;
    let nearest_equator = if bbox[1] <= 0.0 && bbox[3] >= 0.0 {
        0.0
    } else {
        bbox[1].abs().min(bbox[3].abs())
    };
    let width = lon_span * nearest_equator.to_radians().cos() * EARTH_RADIUS_M;
    (area / cell_area + 2.0 * (width + height) / cell_area.sqrt() + 16.0).ceil() as usize
}

/// Fill `multi` with cells of `grid` (S2 or HEALPix) at `res`, appending to `out`.
pub(crate) fn fill_nested_grid(
    grid: GridKind,
    multi: &MultiPolygon<f64>,
    res: u8,
    mode: ContainmentMode,
    out: &mut Vec<u64>,
) -> Result<(), PolygonToCellsError> {
    let mut base = Vec::new();
    match grid {
        GridKind::S2 => base.extend((0..6).map(|face| s2::cellid::CellID::from_face(face).0)),
        GridKind::Healpix => {
            base.extend((0..12).map(|pixel| crate::grid::healpix_encode(0, pixel)))
        }
        GridKind::H3 | GridKind::A5 => unreachable!("H3 and A5 have their own fills"),
    }
    let mut children = Vec::with_capacity(4);
    let mut stack = Vec::new();
    for polygon in &multi.0 {
        let Some(area) = Area::new(polygon)? else {
            continue;
        };
        let estimate = out
            .len()
            .saturating_add(estimate_cells(grid, res, &area.bbox));
        if estimate > H3_POLYGON_TO_CELLS_MAX_CELLS {
            return Err(too_many(estimate));
        }
        stack.clear();
        stack.extend(base.iter().map(|&cell| (cell, 0u8)));
        while let Some((cell, level)) = stack.pop() {
            let Some(mut ring) = cell_ring(grid, cell, level) else {
                continue;
            };
            if level < res {
                // A base cell can hold a pole or span most longitudes: always descend.
                let padded = padded_box(&ring);
                if level == 0
                    || TURNS
                        .iter()
                        .any(|&turn| boxes_overlap(&shifted(&padded, turn), &area.bbox))
                {
                    grid.children_into(cell, level + 1, &mut children);
                    stack.extend(children.iter().map(|&child| (child, level + 1)));
                }
                continue;
            }
            // Test the ring on every turn that lays it over the polygon.
            let tight = bbox_of(&ring.points);
            let mut turned = 0.0;
            let mut matches = false;
            for turn in TURNS {
                if !boxes_overlap(&shifted(&tight, turn), &area.bbox) {
                    continue;
                }
                for point in &mut ring.points {
                    point.0 += turn - turned;
                }
                turned = turn;
                if cell_matches(grid, cell, &ring, &area, mode) {
                    matches = true;
                    break;
                }
            }
            if matches {
                if out.len() >= H3_POLYGON_TO_CELLS_MAX_CELLS {
                    return Err(too_many(out.len() + 1));
                }
                out.push(cell);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(())
}

fn cell_matches(
    grid: GridKind,
    cell: u64,
    ring: &Ring,
    area: &Area,
    mode: ContainmentMode,
) -> bool {
    match mode {
        ContainmentMode::ContainsCentroid => {
            let Some((lat, mut lon)) = grid.center_lat_lng(cell) else {
                return false;
            };
            // Bring the centre onto the same turn as the (shifted) ring.
            let reference = ring.points[0].0;
            while lon - reference > 180.0 {
                lon -= 360.0;
            }
            while lon - reference < -180.0 {
                lon += 360.0;
            }
            area.contains((lon, lat))
        }
        ContainmentMode::ContainsBoundary => {
            ring.points.iter().all(|&p| area.contains(p))
                && !area.boundary_crosses(ring)
                // A hole lying wholly inside the cell.
                && !area.holes.iter().any(|hole| ring.contains(hole.points[0]))
        }
        // `IntersectsBoundary`, `Covers` and any mode h3o adds later: any overlap,
        // including a polygon that lies wholly inside the cell.
        _ => {
            ring.points.iter().any(|&p| area.contains(p))
                || ring.contains(area.exterior.points[0])
                || area.boundary_crosses(ring)
        }
    }
}
