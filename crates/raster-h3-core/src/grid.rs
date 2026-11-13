//! Configurable Discrete Global Grid System (DGGS) abstraction supporting
//! Uber H3 hexagons ([`h3o`]) and A5 equal-area dodecahedral pentagons ([`a5`]).

use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;

use h3o::{CellIndex, LatLng, LatLngIndexer, Resolution};
use serde::{Deserialize, Serialize};

use crate::scalar_ops::H3_AREA_M2;

/// Maximum resolution index (`30`) across all supported grid systems, plus one (`0..=30`).
pub const MAX_GRID_RESOLUTIONS: usize = 31;

/// Supported Discrete Global Grid Systems (DGGS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GridKind {
    /// Uber H3 hexagonal grid (`0..=15`, aperture 7).
    #[default]
    H3,
    /// A5 equal-area dodecahedral pentagonal grid (`0..=30`, aperture 4 for `r >= 2`).
    A5,
}

impl GridKind {
    /// Parse a grid system identifier (case-insensitive: `"h3"` or `"a5"`).
    #[inline]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h3" => Some(Self::H3),
            "a5" => Some(Self::A5),
            _ => None,
        }
    }

    /// Canonical lowercase identifier (`"h3"` or `"a5"`).
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H3 => "h3",
            Self::A5 => "a5",
        }
    }

    /// Canonical 64-bit integer cell index column name (`"h3_index"` or `"a5_index"`).
    #[inline]
    pub const fn index_column_name(self) -> &'static str {
        match self {
            Self::H3 => "h3_index",
            Self::A5 => "a5_index",
        }
    }

    /// Canonical hexadecimal string cell index column name (`"h3_hex"` or `"a5_hex"`).
    #[inline]
    pub const fn hex_column_name(self) -> &'static str {
        match self {
            Self::H3 => "h3_hex",
            Self::A5 => "a5_hex",
        }
    }

    /// Maximum supported resolution level (`15` for H3, `30` for A5).
    #[inline]
    pub const fn max_resolution(self) -> u8 {
        match self {
            Self::H3 => 15,
            Self::A5 => 30,
        }
    }

    /// Average cell area in square meters at `res`.
    #[inline]
    pub fn area_m2(self, res: u8) -> f64 {
        match self {
            Self::H3 => H3_AREA_M2.get(res as usize).copied().unwrap_or(7.373e5),
            Self::A5 => a5::cell_area(i32::from(res.min(30))),
        }
    }

    /// Return `true` if `cell` is a valid, canonical 64-bit cell ID for this grid.
    #[inline]
    pub fn is_valid(self, cell: u64) -> bool {
        match self {
            Self::H3 => CellIndex::try_from(cell).is_ok(),
            Self::A5 => a5_is_valid_cell(cell),
        }
    }

    /// Extract the resolution (`0..=max_resolution`) of `cell`, or `None` if invalid.
    #[inline]
    pub fn resolution(self, cell: u64) -> Option<u8> {
        match self {
            Self::H3 => CellIndex::try_from(cell)
                .ok()
                .map(|idx| u8::from(idx.resolution())),
            Self::A5 => {
                if !a5_is_valid_cell(cell) {
                    return None;
                }
                u8::try_from(a5::get_resolution(cell)).ok()
            }
        }
    }

    /// Compute the ancestor cell of `cell` at `parent_res`, or `None` if invalid or
    /// `parent_res` is finer than `cell`'s resolution.
    #[inline]
    pub fn parent(self, cell: u64, parent_res: u8) -> Option<u64> {
        match self {
            Self::H3 => {
                let idx = CellIndex::try_from(cell).ok()?;
                let target = Resolution::try_from(parent_res).ok()?;
                idx.parent(target).map(u64::from)
            }
            Self::A5 => {
                let cur_res = self.resolution(cell)?;
                if parent_res > cur_res {
                    return None;
                }
                a5::cell_to_parent(cell, Some(i32::from(parent_res))).ok()
            }
        }
    }

    /// Immediate parent `(parent_res, parent_cell)` at resolution `res - 1`.
    #[inline]
    pub fn immediate_parent(self, cell: u64) -> Option<(u8, u64)> {
        let res = self.resolution(cell)?;
        if res == 0 {
            return None;
        }
        let parent_res = res - 1;
        let parent = self.parent(cell, parent_res)?;
        Some((parent_res, parent))
    }

    /// Append the immediate or descendant children of `cell` at `child_res` into `out`.
    #[inline]
    pub fn children_into(self, cell: u64, child_res: u8, out: &mut Vec<u64>) {
        out.clear();
        match self {
            Self::H3 => {
                if let (Ok(idx), Ok(res)) =
                    (CellIndex::try_from(cell), Resolution::try_from(child_res))
                {
                    out.extend(idx.children(res).map(u64::from));
                }
            }
            Self::A5 => {
                if self.is_valid(cell) {
                    if let Ok(children) = a5::cell_to_children(cell, Some(i32::from(child_res))) {
                        out.extend(children);
                        out.dedup();
                    }
                }
            }
        }
    }

    /// Expected number of immediate children (`parent_res -> parent_res + 1`) for `parent_cell`.
    #[inline]
    pub fn expected_immediate_children(self, parent_cell: u64, parent_res: u8) -> usize {
        match self {
            Self::H3 => match CellIndex::try_from(parent_cell) {
                Ok(p) if p.is_pentagon() => 6,
                _ => 7,
            },
            Self::A5 => {
                if parent_res == 0 {
                    5
                } else {
                    4
                }
            }
        }
    }

    /// Cell center in WGS84 degrees `(lat, lng)` with `lng` in `[-180, 180]`.
    #[inline]
    pub fn center_lat_lng(self, cell: u64) -> Option<(f64, f64)> {
        match self {
            Self::H3 => CellIndex::try_from(cell).ok().map(|idx| {
                let ll = LatLng::from(idx);
                (ll.lat(), ll.lng())
            }),
            Self::A5 => {
                if !a5_is_valid_cell(cell) {
                    return None;
                }
                let ll = a5::cell_to_lonlat(cell).ok()?;
                Some((ll.latitude(), wrap_lon_180(ll.longitude())))
            }
        }
    }

    /// Write the unclosed counter-clockwise boundary vertices `(lng, lat)` in degrees
    /// (`lng` in `[-180, 180]`) into `out` and return the vertex count (`<= 10`).
    #[inline]
    pub fn boundary_lng_lat(self, cell: u64, out: &mut [(f64, f64); 10]) -> usize {
        match self {
            Self::H3 => {
                let Ok(idx) = CellIndex::try_from(cell) else {
                    return 0;
                };
                let boundary = idx.boundary();
                let count = boundary.len().min(10);
                for (i, v) in boundary.iter().take(count).enumerate() {
                    out[i] = (v.lng(), v.lat());
                }
                count
            }
            Self::A5 => {
                if !a5_is_valid_cell(cell) {
                    return 0;
                }
                let Ok(verts) = a5::cell_to_boundary(
                    cell,
                    Some(a5::CellToBoundaryOptions {
                        closed_ring: false,
                        segments: Some(1),
                    }),
                ) else {
                    return 0;
                };
                // Leave room for one pole vertex to become two.
                let m = verts.len().min(9);
                let mut count = 0;
                let mut pole_expanded = false;
                for i in 0..m {
                    let lat = verts[i].latitude();
                    if !pole_expanded && lat.abs() >= 90.0 - 1e-9 {
                        // A pole has no longitude (A5 reports an arbitrary one). Run the
                        // ring along the pole between the neighbouring vertices' meridians
                        // so the lon/lat ring covers the cell.
                        let prev = wrap_lon_180(verts[(i + m - 1) % m].longitude());
                        let next = wrap_lon_180(verts[(i + 1) % m].longitude());
                        out[count] = (prev, lat);
                        out[count + 1] = (next, lat);
                        count += 2;
                        pole_expanded = true;
                    } else {
                        out[count] = (wrap_lon_180(verts[i].longitude()), lat);
                        count += 1;
                    }
                }
                count
            }
        }
    }

    /// Index a single WGS84 `(lat, lon)` coordinate in degrees at `res`.
    #[inline]
    pub fn cell_at(self, lat: f64, lon: f64, res: u8) -> Option<u64> {
        if !(-90.0..=90.0).contains(&lat) || !lon.is_finite() {
            return None;
        }
        match self {
            Self::H3 => {
                let r = Resolution::try_from(res).ok()?;
                let ll = LatLng::new(lat, lon).ok()?;
                Some(crate::aggregator::multi_horizon::profile::index(ll, r).into())
            }
            Self::A5 => {
                if res > 30 {
                    return None;
                }
                let pt = a5::core::coordinate_transforms::from_lon_lat(a5::LonLat::new(
                    wrap_lon_180(lon),
                    lat,
                ));
                crate::aggregator::multi_horizon::profile::index_a5(pt, res)
            }
        }
    }

    /// Return `true` if `cell` contains or touches the South Pole (`-90°`), requiring a
    /// conservative `-90.0` southern latitude bound.
    #[inline]
    pub fn is_south_pole_cell(self, cell: u64) -> bool {
        match self {
            Self::H3 => {
                static H3_SOUTH_POLE: OnceLock<[CellIndex; 16]> = OnceLock::new();
                let Ok(idx) = CellIndex::try_from(cell) else {
                    return false;
                };
                let cells = H3_SOUTH_POLE.get_or_init(|| {
                    core::array::from_fn(|r| {
                        LatLng::new(-90.0, 0.0)
                            .unwrap()
                            .to_cell(Resolution::try_from(r as u8).unwrap())
                    })
                });
                cells[u8::from(idx.resolution()) as usize] == idx
            }
            Self::A5 => cell == a5_pole_cells()[1],
        }
    }
}

impl fmt::Display for GridKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GridKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("Invalid grid system '{s}': expected 'h3' or 'a5'"))
    }
}

/// The resolution-0 A5 cells containing the north and south poles, in that order
/// (finer cells meet at a pole as a vertex).
pub fn a5_pole_cells() -> [u64; 2] {
    static A5_POLES: OnceLock<[u64; 2]> = OnceLock::new();
    *A5_POLES.get_or_init(|| {
        [
            a5::lonlat_to_cell(a5::LonLat::new(0.0, 90.0), 0).expect("north pole a5"),
            a5::lonlat_to_cell(a5::LonLat::new(0.0, -90.0), 0).expect("south pole a5"),
        ]
    })
}

/// Normalize a finite longitude (in degrees) into `[-180.0, 180.0]`.
#[inline(always)]
pub fn wrap_lon_180(lon: f64) -> f64 {
    if (-180.0..=180.0).contains(&lon) {
        return lon;
    }
    let wrapped = ((lon + 180.0) % 360.0 + 360.0) % 360.0 - 180.0;
    if wrapped == -180.0 && lon > 0.0 {
        180.0
    } else {
        wrapped
    }
}

/// Check whether `cell` is a valid, canonical A5 cell index at resolution `0..=30`.
#[inline]
pub fn a5_is_valid_cell(cell: u64) -> bool {
    thread_local! {
        /// The last cell this thread found valid (0 is never valid). Callers ask for a
        /// cell's centre, boundary and resolution in turn, each of which validates it.
        static LAST_VALID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    if cell == 0 {
        return false;
    }
    if LAST_VALID.get() == cell {
        return true;
    }
    let valid = a5_decodes_canonically(cell);
    if valid {
        LAST_VALID.set(cell);
    }
    valid
}

fn a5_decodes_canonically(cell: u64) -> bool {
    let Ok(parsed) = a5::core::serialization::deserialize(cell) else {
        return false;
    };
    if !(0..=a5::MAX_RESOLUTION).contains(&parsed.resolution) {
        return false;
    }
    a5::core::serialization::serialize(&parsed) == Ok(cell)
}

/// Static dispatch backend for the inner pixel scanline walker (`walker.rs`).
pub trait GridBackend: Copy + Send + Sync + 'static {
    /// Projected point representation shared across all resolutions of a sample.
    type Point: Copy;
    /// Per-thread/per-window indexer state.
    type Indexer;

    /// Construct a fresh indexer for the given sorted `resolutions`.
    fn new_indexer(resolutions: &[u8]) -> Self::Indexer;

    /// Validate `(lon, lat)` in degrees and convert to `Self::Point`.
    fn make_point(lon: f64, lat: f64) -> Option<Self::Point>;

    /// Index `point` across all `resolutions`, writing the resulting `u64` cell IDs to `out`.
    fn index_all(
        indexer: &mut Self::Indexer,
        point: Self::Point,
        resolutions: &[u8],
        out: &mut [u64],
    );
}

/// Static [`GridBackend`] implementation for Uber H3 (`h3o`).
#[derive(Clone, Copy, Debug, Default)]
pub struct H3Backend;

impl GridBackend for H3Backend {
    type Point = LatLng;
    type Indexer = (LatLngIndexer, Vec<Resolution>);

    #[inline]
    fn new_indexer(resolutions: &[u8]) -> Self::Indexer {
        let h3_res = resolutions
            .iter()
            .map(|&r| Resolution::try_from(r).expect("validated H3 resolution"))
            .collect();
        (LatLngIndexer::default(), h3_res)
    }

    #[inline(always)]
    fn make_point(lon: f64, lat: f64) -> Option<Self::Point> {
        LatLng::new(lat, lon).ok()
    }

    #[inline(always)]
    fn index_all(
        indexer: &mut Self::Indexer,
        point: Self::Point,
        _resolutions: &[u8],
        out: &mut [u64],
    ) {
        crate::aggregator::multi_horizon::profile::index_all(
            &mut indexer.0,
            point,
            &indexer.1,
            out,
        );
    }
}

/// Static [`GridBackend`] implementation for A5 pentagons (`a5`).
#[derive(Clone, Copy, Debug, Default)]
pub struct A5Backend;

impl GridBackend for A5Backend {
    type Point = a5::coordinate_systems::Spherical;
    type Indexer = ();

    #[inline]
    fn new_indexer(_resolutions: &[u8]) -> Self::Indexer {}

    #[inline(always)]
    fn make_point(lon: f64, lat: f64) -> Option<Self::Point> {
        if !lon.is_finite() || !(-90.0..=90.0).contains(&lat) {
            return None;
        }
        let norm_lon = wrap_lon_180(lon);
        Some(a5::core::coordinate_transforms::from_lon_lat(
            a5::LonLat::new(norm_lon, lat),
        ))
    }

    #[inline(always)]
    fn index_all(
        _indexer: &mut Self::Indexer,
        point: Self::Point,
        resolutions: &[u8],
        out: &mut [u64],
    ) {
        crate::aggregator::multi_horizon::profile::index_all_a5(point, resolutions, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_kind_basics() {
        assert_eq!(GridKind::parse("h3"), Some(GridKind::H3));
        assert_eq!(GridKind::parse("A5"), Some(GridKind::A5));
        assert_eq!(GridKind::parse("s2"), None);
        assert_eq!(GridKind::H3.max_resolution(), 15);
        assert_eq!(GridKind::A5.max_resolution(), 30);
        assert_eq!(GridKind::H3.index_column_name(), "h3_index");
        assert_eq!(GridKind::A5.index_column_name(), "a5_index");
    }

    #[test]
    fn test_a5_validity_and_hierarchy() {
        let cell = GridKind::A5.cell_at(37.7749, -122.4194, 10).unwrap();
        assert!(GridKind::A5.is_valid(cell));
        assert!(!GridKind::A5.is_valid(0));
        assert_eq!(GridKind::A5.resolution(cell), Some(10));

        let (parent_res, parent) = GridKind::A5.immediate_parent(cell).unwrap();
        assert_eq!(parent_res, 9);
        assert!(GridKind::A5.is_valid(parent));
        assert_eq!(GridKind::A5.resolution(parent), Some(9));

        let mut children = Vec::new();
        GridKind::A5.children_into(parent, 10, &mut children);
        assert_eq!(children.len(), 4);
        assert!(children.contains(&cell));

        let (lat, lng) = GridKind::A5.center_lat_lng(cell).unwrap();
        assert!((lat - 37.7749).abs() < 0.1);
        assert!((lng - (-122.4194)).abs() < 0.1);

        let mut boundary = [(0.0, 0.0); 10];
        let n = GridKind::A5.boundary_lng_lat(cell, &mut boundary);
        assert_eq!(n, 5);
    }

    #[test]
    fn a5_south_lat_bound_covers_curved_edges() {
        use crate::aggregator::horizon_streamer::compute_cell_south_lat_for;
        for res in 2u8..=5 {
            let mut lat = -88.0;
            while lat < 89.0 {
                let mut lon = -179.0;
                while lon < 180.0 {
                    let cell = GridKind::A5.cell_at(lat, lon, res).unwrap();
                    let dense = a5::cell_to_boundary(
                        cell,
                        Some(a5::CellToBoundaryOptions {
                            closed_ring: false,
                            segments: Some(32),
                        }),
                    )
                    .unwrap();
                    let true_south = dense
                        .iter()
                        .map(|p| p.latitude())
                        .fold(f64::INFINITY, f64::min);
                    let bound = compute_cell_south_lat_for(GridKind::A5, cell);
                    assert!(bound <= true_south, "res {res} cell {cell:x}");
                    lon += 9.7;
                }
                lat += 4.3;
            }
        }
    }

    #[test]
    fn a5_pole_vertex_is_spread_along_neighbour_meridians() {
        let mut buf = [0u8; crate::encoding::WKB_BUF_LEN];
        for lat in [89.999, -89.999] {
            for res in [1u8, 2, 5] {
                let cell = GridKind::A5.cell_at(lat, 100.0, res).unwrap();
                let mut verts = [(0.0, 0.0); 10];
                let n = GridKind::A5.boundary_lng_lat(cell, &mut verts);
                let verts = &verts[..n];
                assert_eq!(verts.iter().filter(|v| v.1.abs() == 90.0).count(), 2);
                let (w, e) = verts
                    .iter()
                    .fold((f64::MAX, f64::MIN), |(w, e), v| (w.min(v.0), e.max(v.0)));
                assert!(w <= 100.0 && 100.0 <= e && e - w < 180.0, "{verts:?}");
                // One plain polygon, not an antimeridian split.
                let len = crate::encoding::a5_index_to_wkb(cell, &mut buf).unwrap();
                assert_eq!(buf[1..5], 3u32.to_le_bytes());
                assert_eq!(len, 9 + 4 + 16 * (n + 1));
            }
        }
    }
}
