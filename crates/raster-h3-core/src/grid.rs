//! Configurable Discrete Global Grid System (DGGS) abstraction supporting
//! Uber H3 hexagons ([`h3o`]), A5 equal-area dodecahedral pentagons ([`a5`]),
//! Google S2 cube-face quadrilaterals ([`s2`]) and HEALPix equal-area
//! quadrilaterals ([`healpix`], nested scheme, NUNIQ cell ids).

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
    /// Google S2 cube-face quadrilateral grid (levels `0..=30`, aperture 4).
    S2,
    /// HEALPix equal-area quadrilateral grid (depths `0..=29`, aperture 4), nested
    /// scheme. Cell ids are NUNIQ: `4 * 4^depth + nested_pixel`.
    Healpix,
}

impl GridKind {
    /// Every supported grid.
    pub const ALL: [Self; 4] = [Self::H3, Self::A5, Self::S2, Self::Healpix];

    /// The grid whose discriminant (`GridKind as u8`) is `index`; H3 if there is none.
    /// Lets a grid be a const generic argument.
    pub const fn from_index(index: u8) -> Self {
        match index {
            1 => Self::A5,
            2 => Self::S2,
            3 => Self::Healpix,
            _ => Self::H3,
        }
    }

    /// The accepted identifiers, for error messages.
    pub const EXPECTED: &'static str = "'h3', 'a5', 's2' or 'healpix'";

    /// Parse a grid system identifier (case-insensitive: `"h3"`, `"a5"`, `"s2"` or `"healpix"`).
    #[inline]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h3" => Some(Self::H3),
            "a5" => Some(Self::A5),
            "s2" => Some(Self::S2),
            "healpix" => Some(Self::Healpix),
            _ => None,
        }
    }

    /// Canonical lowercase identifier (`"h3"`, `"a5"`, `"s2"` or `"healpix"`).
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H3 => "h3",
            Self::A5 => "a5",
            Self::S2 => "s2",
            Self::Healpix => "healpix",
        }
    }

    /// Display name (`"H3"`, `"A5"`, `"S2"` or `"HEALPix"`).
    #[inline]
    pub const fn label(self) -> &'static str {
        match self {
            Self::H3 => "H3",
            Self::A5 => "A5",
            Self::S2 => "S2",
            Self::Healpix => "HEALPix",
        }
    }

    /// Canonical 64-bit integer cell index column name (`"h3_index"`, `"a5_index"`, ...).
    #[inline]
    pub const fn index_column_name(self) -> &'static str {
        match self {
            Self::H3 => "h3_index",
            Self::A5 => "a5_index",
            Self::S2 => "s2_index",
            Self::Healpix => "healpix_index",
        }
    }

    /// Canonical hexadecimal string cell index column name (`"h3_hex"`, `"a5_hex"`, ...).
    #[inline]
    pub const fn hex_column_name(self) -> &'static str {
        match self {
            Self::H3 => "h3_hex",
            Self::A5 => "a5_hex",
            Self::S2 => "s2_hex",
            Self::Healpix => "healpix_hex",
        }
    }

    /// The grid whose index or hex column is called `name` (case-insensitive), also
    /// accepting the bare identifier (`"s2"`).
    pub fn from_cell_column_name(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        Self::ALL.into_iter().find(|g| {
            name == g.index_column_name() || name == g.hex_column_name() || name == g.as_str()
        })
    }

    /// Maximum supported resolution level (`15` for H3, `30` for A5 and S2, `29` for HEALPix).
    #[inline]
    pub const fn max_resolution(self) -> u8 {
        match self {
            Self::H3 => 15,
            Self::A5 | Self::S2 => 30,
            Self::Healpix => 29,
        }
    }

    /// Average cell area in square meters at `res`.
    #[inline]
    pub fn area_m2(self, res: u8) -> f64 {
        let res = res.min(self.max_resolution());
        match self {
            Self::H3 => H3_AREA_M2.get(res as usize).copied().unwrap_or(7.373e5),
            Self::A5 => a5::cell_area(i32::from(res)),
            Self::S2 => EARTH_AREA_M2 / (6.0 * (1u64 << (2 * res)) as f64),
            Self::Healpix => EARTH_AREA_M2 / (12.0 * (1u64 << (2 * res)) as f64),
        }
    }

    /// Return `true` if `cell` is a valid, canonical 64-bit cell ID for this grid.
    #[inline]
    pub fn is_valid(self, cell: u64) -> bool {
        match self {
            Self::H3 => CellIndex::try_from(cell).is_ok(),
            Self::A5 => a5_is_valid_cell(cell),
            Self::S2 => S2CellId(cell).is_valid(),
            Self::Healpix => healpix_decode(cell).is_some(),
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
            Self::S2 => {
                let id = S2CellId(cell);
                id.is_valid().then(|| id.level() as u8)
            }
            Self::Healpix => healpix_decode(cell).map(|(depth, _)| depth),
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
            Self::S2 => {
                let id = S2CellId(cell);
                (id.is_valid() && u64::from(parent_res) <= id.level())
                    .then(|| id.parent(u64::from(parent_res)).0)
            }
            Self::Healpix => {
                let (depth, pixel) = healpix_decode(cell)?;
                (parent_res <= depth)
                    .then(|| healpix_encode(parent_res, pixel >> (2 * (depth - parent_res))))
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
            Self::S2 => {
                let id = S2CellId(cell);
                if id.is_valid()
                    && child_res <= self.max_resolution()
                    && u64::from(child_res) >= id.level()
                {
                    let end = id.child_end_at_level(u64::from(child_res));
                    let mut child = id.child_begin_at_level(u64::from(child_res));
                    while child != end {
                        out.push(child.0);
                        child = child.next();
                    }
                }
            }
            Self::Healpix => {
                if let Some((depth, pixel)) = healpix_decode(cell) {
                    if child_res >= depth && child_res <= self.max_resolution() {
                        let shift = 2 * (child_res - depth);
                        out.extend(
                            ((pixel << shift)..((pixel + 1) << shift))
                                .map(|child| healpix_encode(child_res, child)),
                        );
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
            Self::S2 | Self::Healpix => 4,
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
            Self::S2 => {
                let id = S2CellId(cell);
                if !id.is_valid() {
                    return None;
                }
                let ll = s2::latlng::LatLng::from(id);
                Some((ll.lat.deg().clamp(-90.0, 90.0), wrap_lon_180(ll.lng.deg())))
            }
            Self::Healpix => {
                let (depth, pixel) = healpix_decode(cell)?;
                let c = healpix::get(depth).center(pixel);
                Some((c.lat.to_degrees(), wrap_lon_180(c.lon.to_degrees())))
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
                let mut ring = [(0.0, 0.0); 9];
                let m = verts.len().min(9);
                for (slot, v) in ring.iter_mut().zip(&verts) {
                    *slot = (v.longitude(), v.latitude());
                }
                write_lng_lat_ring(&ring[..m], out)
            }
            Self::S2 => {
                let id = S2CellId(cell);
                if !id.is_valid() {
                    return 0;
                }
                // Counter-clockwise, as S2 defines them.
                let ring = s2::cell::Cell::from(id).vertices().map(|p| {
                    let ll = s2::latlng::LatLng::from(p);
                    (ll.lng.deg(), ll.lat.deg())
                });
                write_lng_lat_ring(&ring, out)
            }
            Self::Healpix => {
                let Some((depth, pixel)) = healpix_decode(cell) else {
                    return 0;
                };
                // South, east, north, west: counter-clockwise.
                let ring = healpix::get(depth)
                    .vertices(pixel)
                    .map(|v| (v.lon.to_degrees(), v.lat.to_degrees()));
                write_lng_lat_ring(&ring, out)
            }
        }
    }

    /// Whether cell edges are great-circle arcs, which lets latitude bounds assume an
    /// edge stays on its endpoints' side of the equator-ward bulge.
    #[inline]
    pub const fn has_geodesic_edges(self) -> bool {
        matches!(self, Self::H3 | Self::S2)
    }

    /// `Some(true)` if `cell` holds the north pole in its interior, `Some(false)` for
    /// the south pole, `None` otherwise (a pole on a vertex does not count).
    pub fn interior_pole(self, cell: u64) -> Option<bool> {
        let [north, south] = match self {
            Self::H3 => {
                let res = CellIndex::try_from(cell).ok()?.resolution();
                [90.0, -90.0]
                    .map(|lat| u64::from(LatLng::new(lat, 0.0).expect("pole").to_cell(res)))
            }
            Self::A5 => a5_pole_cells(),
            // The +z and -z cube faces; finer cells meet at the pole as a vertex.
            Self::S2 => [S2CellId::from_face(2).0, S2CellId::from_face(5).0],
            // Four base cells meet at each pole.
            Self::Healpix => return None,
        };
        if cell == north {
            Some(true)
        } else if cell == south {
            Some(false)
        } else {
            None
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
            Self::S2 | Self::Healpix => {
                if res > self.max_resolution() {
                    return None;
                }
                let mut cell = [0u64];
                match self {
                    Self::S2 => S2Backend::index_all(
                        &mut (),
                        S2Backend::make_point(lon, lat)?,
                        &[res],
                        &mut cell,
                    ),
                    _ => HealpixBackend::index_all(
                        &mut (),
                        HealpixBackend::make_point(lon, lat)?,
                        &[res],
                        &mut cell,
                    ),
                }
                Some(cell[0])
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
            Self::A5 | Self::S2 | Self::Healpix => self.interior_pole(cell) == Some(false),
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
        Self::parse(s)
            .ok_or_else(|| format!("Invalid grid system '{s}': expected {}", Self::EXPECTED))
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

/// Surface area of the authalic sphere in square meters.
const EARTH_AREA_M2: f64 = 5.100_656_217_240_886e14;

use s2::cellid::CellID as S2CellId;

/// Deepest HEALPix depth; a point is indexed once here and shifted to coarser depths.
const HEALPIX_MAX_DEPTH: u8 = healpix::MAX_DEPTH;

/// NUNIQ id of nested pixel `pixel` at `depth`: `4 * 4^depth + pixel`.
#[inline(always)]
pub const fn healpix_encode(depth: u8, pixel: u64) -> u64 {
    (4u64 << (2 * depth)) + pixel
}

/// `(depth, nested pixel)` of a NUNIQ id, or `None` if it is not one at depth `0..=29`.
#[inline]
pub const fn healpix_decode(uniq: u64) -> Option<(u8, u64)> {
    // Depth `d` ids fill `[4 * 4^d, 16 * 4^d)`.
    if uniq < 4 || uniq >= 16u64 << (2 * HEALPIX_MAX_DEPTH) {
        return None;
    }
    let depth = ((61 - uniq.leading_zeros()) >> 1) as u8;
    Some((depth, uniq - (4u64 << (2 * depth))))
}

/// Write an unclosed counter-clockwise `(lng, lat)` ring in degrees into `out`, with
/// longitudes wrapped to `[-180, 180]`, and return the vertex count.
///
/// A pole has no longitude (grids report an arbitrary one). A vertex on a pole is
/// replaced by two that run along it between the neighbouring vertices' meridians, so
/// the lon/lat ring covers the cell. `ring` holds at most 9 vertices.
fn write_lng_lat_ring(ring: &[(f64, f64)], out: &mut [(f64, f64); 10]) -> usize {
    let m = ring.len().min(9);
    let mut count = 0;
    let mut pole_expanded = false;
    for i in 0..m {
        let lat = ring[i].1.clamp(-90.0, 90.0);
        if !pole_expanded && lat.abs() >= 90.0 - 1e-9 {
            out[count] = (wrap_lon_180(ring[(i + m - 1) % m].0), lat);
            out[count + 1] = (wrap_lon_180(ring[(i + 1) % m].0), lat);
            count += 2;
            pole_expanded = true;
        } else {
            out[count] = (wrap_lon_180(ring[i].0), lat);
            count += 1;
        }
    }
    // A vertex on the antimeridian is at both -180 and 180. Put it on the side the
    // rest of the cell is on, so a cell that only touches the antimeridian is not
    // taken for one that crosses it.
    let side = |lng: f64| {
        if lng.abs() == 180.0 {
            0.0
        } else {
            lng.signum()
        }
    };
    let east = out[..count].iter().any(|v| side(v.0) > 0.0);
    let west = out[..count].iter().any(|v| side(v.0) < 0.0);
    if east != west {
        let edge = if east { 180.0 } else { -180.0 };
        for v in &mut out[..count] {
            if v.0.abs() == 180.0 {
                v.0 = edge;
            }
        }
    }
    count
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

/// Static [`GridBackend`] implementation for Google S2 (`s2`).
///
/// A sample is indexed once at the leaf level; every coarser level is a bit mask away.
#[derive(Clone, Copy, Debug, Default)]
pub struct S2Backend;

impl GridBackend for S2Backend {
    type Point = S2CellId;
    type Indexer = ();

    #[inline]
    fn new_indexer(_resolutions: &[u8]) -> Self::Indexer {}

    #[inline(always)]
    fn make_point(lon: f64, lat: f64) -> Option<Self::Point> {
        if !lon.is_finite() || !(-90.0..=90.0).contains(&lat) {
            return None;
        }
        Some(S2CellId::from(s2::latlng::LatLng::from_degrees(
            lat,
            wrap_lon_180(lon),
        )))
    }

    #[inline(always)]
    fn index_all(
        _indexer: &mut Self::Indexer,
        point: Self::Point,
        resolutions: &[u8],
        out: &mut [u64],
    ) {
        for (cell, &res) in out.iter_mut().zip(resolutions) {
            *cell = point.parent(u64::from(res)).0;
        }
    }
}

/// Static [`GridBackend`] implementation for HEALPix (`healpix`, nested scheme).
///
/// A sample is hashed once at the deepest depth; every coarser depth is a shift away.
#[derive(Clone, Copy, Debug, Default)]
pub struct HealpixBackend;

impl GridBackend for HealpixBackend {
    /// Nested pixel at the maximum HEALPix depth.
    type Point = u64;
    type Indexer = ();

    #[inline]
    fn new_indexer(_resolutions: &[u8]) -> Self::Indexer {}

    #[inline(always)]
    fn make_point(lon: f64, lat: f64) -> Option<Self::Point> {
        if !lon.is_finite() || !(-90.0..=90.0).contains(&lat) {
            return None;
        }
        Some(
            healpix::get(HEALPIX_MAX_DEPTH)
                .hash([lon.rem_euclid(360.0).to_radians(), lat.to_radians()]),
        )
    }

    #[inline(always)]
    fn index_all(
        _indexer: &mut Self::Indexer,
        point: Self::Point,
        resolutions: &[u8],
        out: &mut [u64],
    ) {
        for (cell, &res) in out.iter_mut().zip(resolutions) {
            *cell = healpix_encode(res, point >> (2 * (HEALPIX_MAX_DEPTH - res)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grid_kind_basics() {
        assert_eq!(GridKind::parse("h3"), Some(GridKind::H3));
        assert_eq!(GridKind::parse("A5"), Some(GridKind::A5));
        assert_eq!(GridKind::parse("s9"), None);
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

    const NESTED: [GridKind; 2] = [GridKind::S2, GridKind::Healpix];

    /// Sample points spread over the globe, poles and antimeridian included.
    fn sample_points() -> Vec<(f64, f64)> {
        let mut points = vec![(90.0, 0.0), (-90.0, 0.0), (0.0, 180.0), (0.0, -180.0)];
        let mut lat = -89.5;
        while lat < 90.0 {
            let mut lon = -179.5;
            while lon < 180.0 {
                points.push((lat, lon));
                lon += 13.7;
            }
            lat += 6.1;
        }
        points
    }

    #[test]
    fn nested_grids_parse_and_name_their_columns() {
        assert_eq!(GridKind::parse("S2"), Some(GridKind::S2));
        assert_eq!(GridKind::parse("healpix"), Some(GridKind::Healpix));
        assert_eq!(GridKind::S2.max_resolution(), 30);
        assert_eq!(GridKind::Healpix.max_resolution(), 29);
        assert_eq!(GridKind::S2.index_column_name(), "s2_index");
        assert_eq!(GridKind::Healpix.hex_column_name(), "healpix_hex");
        assert_eq!(
            GridKind::from_cell_column_name("HEALPIX_INDEX"),
            Some(GridKind::Healpix)
        );
        assert_eq!(GridKind::from_cell_column_name("mean"), None);
        assert_eq!(
            serde_json::to_string(&GridKind::Healpix).unwrap(),
            "\"healpix\""
        );
    }

    #[test]
    fn healpix_ids_are_nuniq() {
        // 4 * nside^2 + pixel.
        assert_eq!(healpix_encode(0, 0), 4);
        assert_eq!(healpix_encode(0, 11), 15);
        assert_eq!(healpix_encode(1, 0), 16);
        assert_eq!(healpix_encode(29, 0), 4 << 58);
        for (depth, pixel) in [(0u8, 0u64), (0, 11), (3, 767), (29, 12 * (1 << 58) - 1)] {
            assert_eq!(
                healpix_decode(healpix_encode(depth, pixel)),
                Some((depth, pixel))
            );
        }
        for bad in [0, 3, 1 << 62, u64::MAX] {
            assert_eq!(healpix_decode(bad), None);
            assert!(!GridKind::Healpix.is_valid(bad));
        }
        // Matches the healpix crate's own IVOA uniq encoding.
        assert_eq!(
            healpix::checked::to_uniq_ivoa(7, 1234),
            healpix_encode(7, 1234)
        );
    }

    #[test]
    fn s2_ids_match_the_reference_library() {
        let ll = s2::latlng::LatLng::from_degrees(37.7749, -122.4194);
        for level in [0u8, 1, 12, 30] {
            let expected = S2CellId::from(ll).parent(u64::from(level)).0;
            assert_eq!(
                GridKind::S2.cell_at(37.7749, -122.4194, level),
                Some(expected)
            );
        }
        assert!(!GridKind::S2.is_valid(0));
        assert!(!GridKind::S2.is_valid(u64::MAX));
        assert_eq!(GridKind::S2.cell_at(0.0, 0.0, 31), None);
    }

    #[test]
    fn nested_grid_hierarchy_round_trips() {
        for grid in NESTED {
            for (lat, lon) in sample_points() {
                for res in [0u8, 1, 5, 12, grid.max_resolution()] {
                    let cell = grid.cell_at(lat, lon, res).unwrap();
                    assert!(grid.is_valid(cell), "{grid} {lat} {lon} {res}");
                    assert_eq!(grid.resolution(cell), Some(res));
                    if res == 0 {
                        assert_eq!(grid.immediate_parent(cell), None);
                        continue;
                    }
                    let (parent_res, parent) = grid.immediate_parent(cell).unwrap();
                    assert_eq!(parent_res, res - 1);
                    assert_eq!(grid.cell_at(lat, lon, res - 1), Some(parent));
                    let mut children = Vec::new();
                    grid.children_into(parent, res, &mut children);
                    assert_eq!(
                        children.len(),
                        grid.expected_immediate_children(parent, parent_res)
                    );
                    assert!(children.contains(&cell));
                    assert_eq!(grid.parent(cell, res + 1), None);
                }
            }
        }
    }

    #[test]
    fn nested_grid_centres_index_back_to_their_cell() {
        for grid in NESTED {
            for (lat, lon) in sample_points() {
                for res in [0u8, 3, 9, 20] {
                    let cell = grid.cell_at(lat, lon, res).unwrap();
                    let (c_lat, c_lng) = grid.center_lat_lng(cell).unwrap();
                    assert!((-180.0..=180.0).contains(&c_lng));
                    assert_eq!(grid.cell_at(c_lat, c_lng, res), Some(cell), "{grid} {res}");
                }
            }
        }
    }

    #[test]
    fn nested_grid_south_bound_and_radius_cover_the_cell() {
        use crate::aggregator::horizon_streamer::compute_cell_south_lat_for;
        use crate::pmtiles::max_cell_radius_deg;
        for grid in NESTED {
            for res in 0u8..=6 {
                let radius = max_cell_radius_deg(grid, res);
                for (lat, lon) in sample_points() {
                    let cell = grid.cell_at(lat, lon, res).unwrap();
                    let south = compute_cell_south_lat_for(grid, cell);
                    assert!(south <= lat, "{grid} res {res}: {south} > {lat}");
                    let (c_lat, _) = grid.center_lat_lng(cell).unwrap();
                    assert!((lat - c_lat).abs() <= radius, "{grid} res {res}");
                    let mut verts = [(0.0, 0.0); 10];
                    let n = grid.boundary_lng_lat(cell, &mut verts);
                    assert!((4..=5).contains(&n));
                    for &(v_lng, v_lat) in &verts[..n] {
                        assert!((-180.0..=180.0).contains(&v_lng));
                        assert!(south <= v_lat && (v_lat - c_lat).abs() <= radius);
                    }
                }
            }
        }
    }

    #[test]
    fn nested_grid_pole_cells_are_plain_polygons() {
        let mut buf = [0u8; crate::encoding::WKB_BUF_LEN];
        for grid in NESTED {
            for lat in [89.9999, -89.9999] {
                for res in [1u8, 4, 9] {
                    let cell = grid.cell_at(lat, 100.0, res).unwrap();
                    let mut verts = [(0.0, 0.0); 10];
                    let n = grid.boundary_lng_lat(cell, &mut verts);
                    assert_eq!(n, 5, "{grid} res {res}");
                    assert_eq!(verts[..n].iter().filter(|v| v.1.abs() == 90.0).count(), 2);
                    let (w, e) = verts[..n]
                        .iter()
                        .fold((f64::MAX, f64::MIN), |(w, e), v| (w.min(v.0), e.max(v.0)));
                    assert!(
                        w <= 100.0 && 100.0 <= e && e - w < 180.0,
                        "{grid} {verts:?}"
                    );
                    crate::encoding::cell_index_to_wkb(grid, cell, &mut buf).unwrap();
                    assert_eq!(buf[1..5], 3u32.to_le_bytes());
                }
            }
        }
        // An S2 face holds the pole in its interior; HEALPix base cells meet at it.
        let north_face = GridKind::S2.cell_at(90.0, 0.0, 0).unwrap();
        assert_eq!(GridKind::S2.interior_pole(north_face), Some(true));
        assert!(GridKind::S2.is_south_pole_cell(GridKind::S2.cell_at(-90.0, 0.0, 0).unwrap()));
        let base = GridKind::Healpix.cell_at(90.0, 0.0, 0).unwrap();
        assert_eq!(GridKind::Healpix.interior_pole(base), None);
    }

    #[test]
    fn nested_backends_index_every_resolution_like_cell_at() {
        let resolutions = [0u8, 4, 11, 29];
        let mut cells = [0u64; 4];
        for (lat, lon) in sample_points() {
            let p = S2Backend::make_point(lon, lat).unwrap();
            S2Backend::index_all(&mut (), p, &resolutions, &mut cells);
            for (cell, res) in cells.iter().zip(resolutions) {
                assert_eq!(GridKind::S2.resolution(*cell), Some(res));
                assert_eq!(GridKind::S2.parent(cells[3], res), Some(*cell));
            }
            let p = HealpixBackend::make_point(lon, lat).unwrap();
            HealpixBackend::index_all(&mut (), p, &resolutions, &mut cells);
            for (cell, res) in cells.iter().zip(resolutions) {
                let direct =
                    healpix::get(res).hash([lon.rem_euclid(360.0).to_radians(), lat.to_radians()]);
                assert_eq!(
                    *cell,
                    healpix_encode(res, direct),
                    "{lat} {lon} depth {res}"
                );
            }
        }
        assert!(S2Backend::make_point(f64::NAN, 0.0).is_none());
        assert!(HealpixBackend::make_point(0.0, 91.0).is_none());
    }
}
