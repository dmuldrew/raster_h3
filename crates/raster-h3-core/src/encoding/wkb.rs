//! Well-Known Binary (WKB) geometry serialization for H3 cells.

use h3o::CellIndex;
use std::sync::OnceLock;

/// The north and south pole cells at each of the 16 resolutions.
fn pole_cells(res: h3o::Resolution) -> [CellIndex; 2] {
    static POLES: OnceLock<[[CellIndex; 2]; 16]> = OnceLock::new();
    POLES.get_or_init(|| {
        let north = h3o::LatLng::new(90.0, 0.0).expect("valid");
        let south = h3o::LatLng::new(-90.0, 0.0).expect("valid");
        std::array::from_fn(|i| {
            let res = h3o::Resolution::try_from(i as u8).expect("resolution 0..=15");
            [north.to_cell(res), south.to_cell(res)]
        })
    })[u8::from(res) as usize]
}

/// Most boundary vertices of any H3 cell (class III pentagons).
const MAX_CELL_VERTICES: usize = 10;

/// Most points in one written ring, closing point included. A dateline piece
/// holds at most two points per input edge, and a polar cap adds two crossing
/// points and two pole corners to the cell's vertices.
const MAX_RING_POINTS: usize = 2 * MAX_CELL_VERTICES + 3;

/// Bytes of one single-ring WKB Polygon with `points` points: byte order,
/// type, ring count, point count, then 16 bytes per point.
const fn polygon_bytes(points: usize) -> usize {
    1 + 4 + 4 + 4 + 16 * points
}

/// Capacity of a stack-allocated H3 WKB geometry: a MultiPolygon header plus
/// two maximal pieces, the largest output (a cell split at the dateline).
/// Ordinary cells use 109–189 bytes, as before.
pub const WKB_BUF_LEN: usize = 1 + 4 + 4 + 2 * polygon_bytes(MAX_RING_POINTS);

/// Fixed-capacity stack buffer for zero-allocation OGC 2D Polygon WKB geometry serialization.
pub type WkbBuf = [u8; WKB_BUF_LEN];

const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOLYGON: u32 = 6;

/// A ring of (longitude, latitude) points, without the closing point.
#[derive(Clone, Copy)]
struct Ring {
    pts: [(f64, f64); MAX_RING_POINTS],
    len: usize,
}

impl Ring {
    fn new() -> Self {
        Self {
            pts: [(0.0, 0.0); MAX_RING_POINTS],
            len: 0,
        }
    }

    /// Append a point, skipping an exact repeat of the previous one.
    fn push(&mut self, p: (f64, f64)) {
        if self.len > 0 && self.pts[self.len - 1] == p {
            return;
        }
        self.pts[self.len] = p;
        self.len += 1;
    }

    fn points(&self) -> &[(f64, f64)] {
        let pts = &self.pts[..self.len];
        // Drop a trailing copy of the first point; the writer closes the ring.
        match pts {
            [first, .., last] if pts.len() > 1 && first == last => &pts[..pts.len() - 1],
            _ => pts,
        }
    }

    /// Whether the ring encloses area (at least three distinct points).
    fn is_area(&self) -> bool {
        self.points().len() >= 3
    }
}

/// Little-endian WKB writer over a caller's buffer.
struct WkbWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl WkbWriter<'_> {
    fn put(&mut self, bytes: &[u8]) {
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
    }

    fn header(&mut self, geometry_type: u32, count: u32) {
        self.put(&[1]);
        self.put(&geometry_type.to_le_bytes());
        self.put(&count.to_le_bytes());
    }

    /// Write a single-ring Polygon, closing the ring.
    fn polygon(&mut self, ring: &Ring) {
        let pts = ring.points();
        self.header(WKB_POLYGON, 1);
        self.put(&((pts.len() + 1) as u32).to_le_bytes());
        for &(lng, lat) in pts.iter().chain(&pts[..1]) {
            self.put(&lng.to_le_bytes());
            self.put(&lat.to_le_bytes());
        }
    }
}

/// Keep the part of `ring` on one side of the meridian `lng = 180`
/// (Sutherland–Hodgman against a single half-plane, planar edges).
fn clip_at_180(ring: &[(f64, f64)], keep_east: bool) -> Ring {
    let inside = |p: (f64, f64)| {
        if keep_east {
            p.0 >= 180.0
        } else {
            p.0 <= 180.0
        }
    };
    let crossing = |a: (f64, f64), b: (f64, f64)| {
        let t = (180.0 - a.0) / (b.0 - a.0);
        (180.0, a.1 + t * (b.1 - a.1))
    };
    let mut out = Ring::new();
    for (i, &cur) in ring.iter().enumerate() {
        let prev = ring[(i + ring.len() - 1) % ring.len()];
        match (inside(prev), inside(cur)) {
            (true, true) => out.push(cur),
            (true, false) => out.push(crossing(prev, cur)),
            (false, true) => {
                out.push(crossing(prev, cur));
                out.push(cur);
            }
            (false, false) => {}
        }
    }
    out
}

/// Planar polygon for a cell containing a pole: the cell's ring unrolled from
/// -180° to 180°, closed along both meridians and the pole's edge.
/// Counter-clockwise, like every other ring written here.
fn polar_cap(lngs: &[f64], lats: &[f64], north: bool) -> Ring {
    let n = lngs.len();
    let wrap = |d: f64| {
        if d > 180.0 {
            d - 360.0
        } else if d <= -180.0 {
            d + 360.0
        } else {
            d
        }
    };
    // Visit vertices eastward (increasing longitude).
    let turn: f64 = (0..n).map(|i| wrap(lngs[(i + 1) % n] - lngs[i])).sum();
    let order = |i: usize| if turn >= 0.0 { i } else { n - 1 - i };
    // The eastward edge that crosses the antimeridian starts at `k`.
    let k = (0..n)
        .find(|&i| {
            let a = lngs[order(i)];
            a + wrap(lngs[order((i + 1) % n)] - a) > 180.0
        })
        .unwrap_or(n - 1);
    let (a, b) = (order(k), order((k + 1) % n));
    let b_lng = lngs[a] + wrap(lngs[b] - lngs[a]);
    let t = (180.0 - lngs[a]) / (b_lng - lngs[a]);
    let cross_lat = lats[a] + t * (lats[b] - lats[a]);
    let pole = if north { 90.0 } else { -90.0 };

    let mut ring = Ring::new();
    ring.push((-180.0, cross_lat));
    for step in 1..=n {
        let i = order((k + step) % n);
        // Vertices exactly on the antimeridian sit on the cap's meridian edges.
        let lng = match lngs[i] {
            l if step == 1 && l == 180.0 => -180.0,
            l if step == n && l == -180.0 => 180.0,
            l => l,
        };
        ring.push((lng, lats[i]));
    }
    ring.push((180.0, cross_lat));
    ring.push((180.0, pole));
    ring.push((-180.0, pole));
    if !north {
        // The south cap above runs clockwise; reverse it.
        let len = ring.len;
        ring.pts[..len].reverse();
    }
    ring
}

/// Convert an H3 cell into OGC 2D Well-Known Binary in WGS84 longitude/latitude,
/// directly into a stack-allocated buffer (zero heap allocations).
///
/// Returns the number of bytes written to `buf`. Every geometry is valid for
/// planar GIS tools (DuckDB Spatial, GeoPandas/Shapely, GDAL) with longitudes
/// in [-180, 180]:
///
/// - Most cells are a `Polygon` of the cell's 5–10 vertices (109–189 bytes),
///   exactly as H3 returns them.
/// - **Antimeridian.** A cell crossing ±180° is clipped at the meridian into a
///   `MultiPolygon` of its eastern and western parts. Points on the cut are
///   interpolated linearly in longitude/latitude, matching `"edges": "planar"`.
/// - **Poles.** The two cells containing a pole encircle it, which no simple
///   ring in longitude/latitude can do. They are written as polar-cap
///   `Polygon`s: the ring unrolled from -180° to 180° and closed along both
///   meridians and the pole's latitude (±90°).
///
/// Ring orientation is preserved for ordinary and split cells (counter-clockwise
/// as H3 returns them); polar caps are written counter-clockwise.
#[inline]
pub fn cell_to_wkb(cell: CellIndex, buf: &mut WkbBuf) -> usize {
    let boundary = cell.boundary();
    let n = boundary.len();
    debug_assert!(n <= MAX_CELL_VERTICES);

    let mut lngs = [0.0f64; MAX_CELL_VERTICES];
    let mut lats = [0.0f64; MAX_CELL_VERTICES];
    let mut min_lng = f64::INFINITY;
    let mut max_lng = f64::NEG_INFINITY;
    for (i, v) in boundary.iter().enumerate() {
        lngs[i] = v.lng();
        lats[i] = v.lat();
        min_lng = min_lng.min(lngs[i]);
        max_lng = max_lng.max(lngs[i]);
    }
    let (lngs, lats) = (&mut lngs[..n], &lats[..n]);
    let mut out = WkbWriter { buf, pos: 0 };

    if max_lng - min_lng <= 180.0 {
        // Common case: H3's ring as is, written without an intermediate copy.
        out.header(WKB_POLYGON, 1);
        out.put(&((n + 1) as u32).to_le_bytes());
        for i in (0..n).chain([0]) {
            out.put(&lngs[i].to_le_bytes());
            out.put(&lats[i].to_le_bytes());
        }
        return out.pos;
    }

    let poles = pole_cells(cell.resolution());
    if poles.contains(&cell) {
        out.polygon(&polar_cap(lngs, lats, cell == poles[0]));
        return out.pos;
    }

    // Unwrap so successive vertices differ by less than 180°, then shift the
    // ring to straddle +180° from below and cut it there.
    for i in 1..n {
        while lngs[i] - lngs[i - 1] > 180.0 {
            lngs[i] -= 360.0;
        }
        while lngs[i] - lngs[i - 1] < -180.0 {
            lngs[i] += 360.0;
        }
    }
    let shift = if lngs.iter().any(|&l| l < -180.0) {
        360.0
    } else {
        0.0
    };
    let mut ring = Ring::new();
    for i in 0..n {
        ring.push((lngs[i] + shift, lats[i]));
    }
    let west = clip_at_180(ring.points(), false);
    let mut east = clip_at_180(ring.points(), true);
    for p in &mut east.pts[..east.len] {
        p.0 -= 360.0;
    }
    match (west.is_area(), east.is_area()) {
        (true, true) => {
            out.header(WKB_MULTIPOLYGON, 2);
            out.polygon(&west);
            out.polygon(&east);
        }
        (true, false) => out.polygon(&west),
        (false, true) => out.polygon(&east),
        // Unreachable for real cells; keep the unwrapped ring rather than nothing.
        (false, false) => out.polygon(&ring),
    }
    out.pos
}

/// Grow `bounds` (`[min_x, min_y, max_x, max_y]`) to cover every point of a
/// little-endian `Polygon` or `MultiPolygon` as written by [`cell_to_wkb`].
/// Other or truncated input leaves `bounds` unchanged past the readable part.
pub fn extend_wkb_bounds(wkb: &[u8], bounds: &mut [f64; 4]) {
    let u32_at = |pos: usize| {
        wkb.get(pos..pos + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
    };
    let f64_at = |pos: usize| {
        wkb.get(pos..pos + 8)
            .map(|b| f64::from_le_bytes(b.try_into().expect("8 bytes")))
    };
    // Visit the polygon starting at `pos`; returns the offset just past it.
    let polygon = |mut pos: usize, bounds: &mut [f64; 4]| -> Option<usize> {
        if wkb.get(pos) != Some(&1) || u32_at(pos + 1)? != WKB_POLYGON {
            return None;
        }
        let rings = u32_at(pos + 5)?;
        pos += 9;
        for _ in 0..rings {
            let points = u32_at(pos)? as usize;
            pos += 4;
            for _ in 0..points {
                let (x, y) = (f64_at(pos)?, f64_at(pos + 8)?);
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x);
                bounds[3] = bounds[3].max(y);
                pos += 16;
            }
        }
        Some(pos)
    };
    match (wkb.first(), u32_at(1)) {
        (Some(1), Some(WKB_POLYGON)) => {
            polygon(0, bounds);
        }
        (Some(1), Some(WKB_MULTIPOLYGON)) => {
            let Some(parts) = u32_at(5) else { return };
            let mut pos = 9;
            for _ in 0..parts {
                match polygon(pos, bounds) {
                    Some(next) => pos = next,
                    None => return,
                }
            }
        }
        _ => {}
    }
}

/// Convert a raw 64-bit H3 index to standard 2D WKB polygon bytes.
/// Returns `None` if the index is not a valid H3 cell.
#[inline]
pub fn h3_index_to_wkb(h3_index: u64, buf: &mut WkbBuf) -> Option<usize> {
    let cell = CellIndex::try_from(h3_index).ok()?;
    Some(cell_to_wkb(cell, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cell_to_wkb_hexagon() {
        // San Francisco cell at res 8
        let coord = h3o::LatLng::new(37.7749, -122.4194).expect("valid latlng");
        let cell = coord.to_cell(h3o::Resolution::Eight);
        assert!(!cell.is_pentagon());

        let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
        let len = cell_to_wkb(cell, &mut buf);

        // Hexagon: 1 + 4 + 4 + 4 + 7*16 = 125 bytes
        assert_eq!(len, 125);

        // Byte order: Little Endian
        assert_eq!(buf[0], 1);

        // Geom type: 3 (Polygon)
        let geom_type = u32::from_le_bytes(buf[1..5].try_into().unwrap());
        assert_eq!(geom_type, 3);

        // Num rings: 1
        let num_rings = u32::from_le_bytes(buf[5..9].try_into().unwrap());
        assert_eq!(num_rings, 1);

        // Num points: 7 (6 vertices + 1 closing)
        let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(num_points, 7);

        // Verify first and last points match (closed ring)
        let first_x = f64::from_le_bytes(buf[13..21].try_into().unwrap());
        let first_y = f64::from_le_bytes(buf[21..29].try_into().unwrap());
        let last_x = f64::from_le_bytes(buf[109..117].try_into().unwrap());
        let last_y = f64::from_le_bytes(buf[117..125].try_into().unwrap());

        assert_eq!(first_x, last_x);
        assert_eq!(first_y, last_y);

        // Verify coordinates are reasonable for San Francisco
        assert!(first_x < -120.0 && first_x > -125.0);
        assert!(first_y > 35.0 && first_y < 40.0);
    }

    #[test]
    fn test_cell_to_wkb_pentagon() {
        // Find a pentagon base cell
        let pentagon = CellIndex::base_cells()
            .find(|c| c.is_pentagon())
            .expect("pentagon base cell");
        assert!(pentagon.is_pentagon());

        let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
        let len = cell_to_wkb(pentagon, &mut buf);

        // Pentagon: 1 + 4 + 4 + 4 + 6*16 = 109 bytes
        assert_eq!(len, 109);

        let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(num_points, 6);

        // Closed ring
        let first_x = f64::from_le_bytes(buf[13..21].try_into().unwrap());
        let first_y = f64::from_le_bytes(buf[21..29].try_into().unwrap());
        let last_x = f64::from_le_bytes(buf[93..101].try_into().unwrap());
        let last_y = f64::from_le_bytes(buf[101..109].try_into().unwrap());

        assert_eq!(first_x, last_x);
        assert_eq!(first_y, last_y);
    }

    #[test]
    fn test_cell_to_wkb_class_iii_pentagon_and_hexagons() {
        // Res 7 pentagon: 10 vertices -> 11 ring points -> 189 bytes
        let pentagon_res7 =
            CellIndex::try_from(0x870800000ffffffu64).expect("valid res 7 pentagon");
        assert!(pentagon_res7.is_pentagon());
        assert_eq!(pentagon_res7.boundary().len(), 10);
        let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
        let len = cell_to_wkb(pentagon_res7, &mut buf);
        assert_eq!(len, 189);
        let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(num_points, 11);
        let first_x = f64::from_le_bytes(buf[13..21].try_into().unwrap());
        let first_y = f64::from_le_bytes(buf[21..29].try_into().unwrap());
        let last_x = f64::from_le_bytes(buf[173..181].try_into().unwrap());
        let last_y = f64::from_le_bytes(buf[181..189].try_into().unwrap());
        assert_eq!(first_x, last_x);
        assert_eq!(first_y, last_y);

        // Res 7 hexagon crossing icosahedron edge: 8 vertices -> 9 points -> 157 bytes
        let hex_res7 = CellIndex::try_from(0x87e06dac8ffffffu64).expect("valid res 7 hexagon");
        assert_eq!(hex_res7.boundary().len(), 8);
        let len = cell_to_wkb(hex_res7, &mut buf);
        assert_eq!(len, 157);
        let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(num_points, 9);

        // Res 9 hexagon crossing icosahedron edge (Patagonia): 8 vertices -> 157 bytes
        let hex_res9 = CellIndex::try_from(0x89e8cb91b83ffffu64).expect("valid res 9 hexagon");
        assert_eq!(hex_res9.boundary().len(), 8);
        let len = cell_to_wkb(hex_res9, &mut buf);
        assert_eq!(len, 157);

        // Res 13 hexagon crossing icosahedron edge: 8 vertices -> 157 bytes
        let hex_res13_8 = CellIndex::try_from(0x8d30cb31c61da3fu64).expect("valid res 13 hexagon");
        assert_eq!(hex_res13_8.boundary().len(), 8);
        let len = cell_to_wkb(hex_res13_8, &mut buf);
        assert_eq!(len, 157);

        // Res 13 hexagon crossing icosahedron edge (Sakhalin): 7 vertices -> 141 bytes
        let hex_res13_7 = CellIndex::try_from(0x8d2e9b66e59113fu64).expect("valid res 13 hexagon");
        assert_eq!(hex_res13_7.boundary().len(), 7);
        let len = cell_to_wkb(hex_res13_7, &mut buf);
        assert_eq!(len, 141);
    }

    use crate::encoding::wkb_to_multipolygon;
    use geo_types::{LineString, MultiPolygon};

    fn encode(cell: CellIndex) -> MultiPolygon<f64> {
        let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
        let len = cell_to_wkb(cell, &mut buf);
        assert!(len <= WKB_BUF_LEN);
        wkb_to_multipolygon(&buf[..len]).expect("valid WKB")
    }

    /// Shoelace area in degree²; positive for counter-clockwise rings.
    fn signed_area(ring: &LineString<f64>) -> f64 {
        ring.0
            .windows(2)
            .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
            .sum::<f64>()
            / 2.0
    }

    /// Whether no two non-adjacent edges of the closed ring properly cross.
    fn is_simple(ring: &LineString<f64>) -> bool {
        let p = &ring.0;
        let segs = p.len() - 1;
        let orient =
            |a: geo_types::Coord<f64>, b: geo_types::Coord<f64>, c: geo_types::Coord<f64>| {
                ((b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)).signum()
            };
        for i in 0..segs {
            for j in i + 2..segs {
                if i == 0 && j == segs - 1 {
                    continue;
                }
                let (a, b, c, d) = (p[i], p[i + 1], p[j], p[j + 1]);
                let (o1, o2) = (orient(a, b, c), orient(a, b, d));
                let (o3, o4) = (orient(c, d, a), orient(c, d, b));
                if o1 * o2 < 0.0 && o3 * o4 < 0.0 {
                    return false;
                }
            }
        }
        true
    }

    /// The cell's ring unwrapped so successive longitudes differ by < 180°.
    fn unwrapped_area(cell: CellIndex) -> f64 {
        let mut pts: Vec<(f64, f64)> = cell.boundary().iter().map(|v| (v.lng(), v.lat())).collect();
        for i in 1..pts.len() {
            while pts[i].0 - pts[i - 1].0 > 180.0 {
                pts[i].0 -= 360.0;
            }
            while pts[i].0 - pts[i - 1].0 < -180.0 {
                pts[i].0 += 360.0;
            }
        }
        pts.push(pts[0]);
        signed_area(&LineString::from(pts))
    }

    fn crosses_antimeridian(cell: CellIndex) -> bool {
        let lngs: Vec<f64> = cell.boundary().iter().map(|v| v.lng()).collect();
        let span = lngs.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - lngs.iter().cloned().fold(f64::INFINITY, f64::min);
        span > 180.0
    }

    #[test]
    fn every_cell_is_planar_valid_at_resolutions_0_to_2() {
        let (mut split, mut caps) = (0, 0);
        for res in [
            h3o::Resolution::Zero,
            h3o::Resolution::One,
            h3o::Resolution::Two,
        ] {
            for cell in CellIndex::base_cells().flat_map(|c| c.children(res)) {
                let geometry = encode(cell);
                for polygon in &geometry.0 {
                    assert!(polygon.interiors().is_empty());
                    let ring = polygon.exterior();
                    assert!(ring.0.len() >= 4, "{cell}: degenerate ring");
                    assert_eq!(ring.0.first(), ring.0.last(), "{cell}: open ring");
                    for c in &ring.0 {
                        assert!((-180.0..=180.0).contains(&c.x), "{cell}: lng {}", c.x);
                        assert!((-90.0..=90.0).contains(&c.y), "{cell}: lat {}", c.y);
                    }
                    assert!(
                        signed_area(ring) > 0.0,
                        "{cell}: ring is not counter-clockwise"
                    );
                    assert!(is_simple(ring), "{cell}: ring self-intersects");
                }

                let is_pole = pole_cells(res).contains(&cell);
                if is_pole {
                    caps += 1;
                    assert_eq!(geometry.0.len(), 1);
                    let pole = if cell == pole_cells(res)[0] {
                        90.0
                    } else {
                        -90.0
                    };
                    let ring = geometry.0[0].exterior();
                    assert!(ring.0.iter().any(|c| c.x == -180.0 && c.y == pole));
                    assert!(ring.0.iter().any(|c| c.x == 180.0 && c.y == pole));
                    // Every H3 vertex appears in the cap.
                    for v in cell.boundary().iter() {
                        assert!(
                            ring.0.iter().any(|c| (c.y - v.lat()).abs() < 1e-12
                                && ((c.x - v.lng()).abs() < 1e-12
                                    || (c.x.abs() == 180.0 && v.lng().abs() == 180.0))),
                            "{cell}: vertex {v:?} missing from cap"
                        );
                    }
                } else if crosses_antimeridian(cell) {
                    split += 1;
                    assert_eq!(geometry.0.len(), 2, "{cell}: expected east and west parts");
                    let area: f64 = geometry.0.iter().map(|p| signed_area(p.exterior())).sum();
                    let expected = unwrapped_area(cell);
                    assert!(
                        (area - expected).abs() < 1e-9 * expected.max(1.0),
                        "{cell}: {area} vs {expected}"
                    );
                    // Both parts meet on the antimeridian.
                    for polygon in &geometry.0 {
                        assert!(polygon.exterior().0.iter().any(|c| c.x.abs() == 180.0));
                    }
                } else {
                    // Ordinary cells are H3's ring, unchanged.
                    let ring = geometry.0[0].exterior();
                    assert_eq!(geometry.0.len(), 1);
                    let expected: Vec<(f64, f64)> =
                        cell.boundary().iter().map(|v| (v.lng(), v.lat())).collect();
                    let got: Vec<(f64, f64)> = ring.0[..ring.0.len() - 1]
                        .iter()
                        .map(|c| (c.x, c.y))
                        .collect();
                    assert_eq!(got, expected, "{cell}");
                }
            }
        }
        assert_eq!(caps, 6, "two pole cells per resolution");
        assert!(split > 0, "no antimeridian cells exercised");
    }

    #[test]
    fn antimeridian_cell_is_a_multipolygon_at_fine_resolution() {
        let cell = h3o::LatLng::new(-18.0, 180.0)
            .unwrap()
            .to_cell(h3o::Resolution::Five);
        assert!(crosses_antimeridian(cell));
        let geometry = encode(cell);
        assert_eq!(geometry.0.len(), 2);
        let (west, east) = (&geometry.0[0], &geometry.0[1]);
        assert!(west.exterior().0.iter().all(|c| c.x > 0.0 && c.x <= 180.0));
        assert!(east.exterior().0.iter().all(|c| c.x < 0.0 && c.x >= -180.0));
        let area: f64 = geometry.0.iter().map(|p| signed_area(p.exterior())).sum();
        assert!((area - unwrapped_area(cell)).abs() < 1e-12);
    }

    #[test]
    fn pole_caps_at_every_resolution() {
        for r in 0..=15u8 {
            let res = h3o::Resolution::try_from(r).unwrap();
            for (cell, pole) in pole_cells(res).into_iter().zip([90.0, -90.0]) {
                let geometry = encode(cell);
                assert_eq!(geometry.0.len(), 1);
                let ring = geometry.0[0].exterior();
                assert!(
                    signed_area(ring) > 0.0,
                    "res {r}: cap is not counter-clockwise"
                );
                assert!(is_simple(ring), "res {r}: cap self-intersects");
                let lngs = ring.0.iter().map(|c| c.x);
                assert_eq!(lngs.clone().fold(f64::INFINITY, f64::min), -180.0);
                assert_eq!(lngs.fold(f64::NEG_INFINITY, f64::max), 180.0);
                assert!(ring.0.iter().any(|c| c.y == pole));
            }
        }
    }

    #[test]
    fn test_pole_cells_cache_matches_direct_indexing() {
        for res in h3o::Resolution::range(h3o::Resolution::Zero, h3o::Resolution::Fifteen) {
            let [n, s] = pole_cells(res);
            assert_eq!(n, h3o::LatLng::new(90.0, 0.0).unwrap().to_cell(res));
            assert_eq!(s, h3o::LatLng::new(-90.0, 0.0).unwrap().to_cell(res));
        }
    }

    #[test]
    fn wkb_bounds_cover_polygons_and_multipolygons() {
        for cell in [
            h3o::LatLng::new(37.7749, -122.4194)
                .unwrap()
                .to_cell(h3o::Resolution::Eight),
            h3o::LatLng::new(-18.0, 180.0)
                .unwrap()
                .to_cell(h3o::Resolution::Five),
            h3o::LatLng::new(90.0, 0.0)
                .unwrap()
                .to_cell(h3o::Resolution::Zero),
        ] {
            let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
            let len = cell_to_wkb(cell, &mut buf);
            let mut bounds = [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ];
            extend_wkb_bounds(&buf[..len], &mut bounds);
            let mut want = [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ];
            for polygon in &encode(cell).0 {
                for c in &polygon.exterior().0 {
                    want = [
                        want[0].min(c.x),
                        want[1].min(c.y),
                        want[2].max(c.x),
                        want[3].max(c.y),
                    ];
                }
            }
            assert_eq!(bounds, want, "{cell}");
            // Truncated or foreign input never panics.
            for cut in 0..len {
                extend_wkb_bounds(&buf[..cut], &mut bounds);
            }
            extend_wkb_bounds(&[0, 3, 0, 0, 0], &mut bounds);
        }
    }

    #[test]
    fn test_h3_index_to_wkb_invalid() {
        let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
        assert!(h3_index_to_wkb(0, &mut buf).is_none());
        assert!(h3_index_to_wkb(0xffffffffffffffffu64, &mut buf).is_none());
    }
}
