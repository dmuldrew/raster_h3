//! Sufficient certificates for native indexing stages and exact address caching.
//!
//! `GuardedCellIndexer` evaluates the native projection of EVERY input, then
//! skips quantization and index construction only inside a rectangle certified
//! by `quantize_box`. It makes no error assumptions about platform libm and
//! never infers membership of unvisited geographic points from endpoints.

use super::{CoordIJK, FaceIJK, Vec2d, Vec3d};
use crate::h3::{face, CellIndex, Face, LatLng, Resolution};

/// A finite, closed interval of native floating-point values.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    lo: f64,
    hi: f64,
}
impl Bounds {
    /// Constructs an ordered finite interval; rejects NaNs and infinities.
    #[must_use]
    pub fn new(lo: f64, hi: f64) -> Option<Self> {
        (lo.is_finite() && hi.is_finite() && lo <= hi).then_some(Self { lo, hi })
    }
    fn point(x: f64) -> Self {
        Self { lo: x, hi: x }
    }
    fn add(self, b: Self) -> Self {
        Self {
            lo: (self.lo + b.lo).next_down(),
            hi: (self.hi + b.hi).next_up(),
        }
    }
    fn sub(self, b: Self) -> Self {
        self.add(Self {
            lo: -b.hi,
            hi: -b.lo,
        })
    }
    fn mul(self, b: Self) -> Self {
        let products = [
            self.lo * b.lo,
            self.lo * b.hi,
            self.hi * b.lo,
            self.hi * b.hi,
        ];
        Self {
            lo: products
                .into_iter()
                .fold(f64::INFINITY, f64::min)
                .next_down(),
            hi: products
                .into_iter()
                .fold(f64::NEG_INFINITY, f64::max)
                .next_up(),
        }
    }
    fn div(self, b: f64) -> Self {
        Self {
            lo: (self.lo / b).next_down(),
            hi: (self.hi / b).next_up(),
        }
    }
    fn abs(self) -> Self {
        Self {
            lo: if self.lo <= 0. && self.hi >= 0. {
                0.
            } else {
                self.lo.abs().min(self.hi.abs())
            },
            hi: self.lo.abs().max(self.hi.abs()),
        }
    }
    fn lt(self, b: Self) -> Option<bool> {
        if self.hi < b.lo {
            Some(true)
        } else if self.lo >= b.hi {
            Some(false)
        } else {
            None
        }
    }
    fn le(self, b: Self) -> Option<bool> {
        if self.hi <= b.lo {
            Some(true)
        } else if self.lo > b.hi {
            Some(false)
        } else {
            None
        }
    }
    #[allow(clippy::cast_possible_truncation)] // Restricted to small finite coordinates.
    fn integer(self) -> Option<i32> {
        let lo = self.lo as i32;
        (lo == self.hi as i32).then_some(lo)
    }
    fn small(self, limit: f64) -> bool {
        self.lo >= -limit && self.hi <= limit
    }
}
fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}
fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}
fn components(c: CoordIJK) -> [i32; 3] {
    [c.i(), c.j(), c.k()]
}

/// Certifies native Hex2d-to-IJK rounding throughout a rectangle.
///
/// Returns `None` on any unresolved branch, integer cast, or unsupported range.
/// Inputs are restricted to +/-1 million so the native integer tail cannot
/// overflow. This certifies only quantization, not geographic projection.
#[must_use]
pub fn quantize_box(x: Bounds, y: Bounds) -> Option<[i32; 3]> {
    if !x.small(1_000_000.) || !y.small(1_000_000.) {
        return None;
    }
    let x2 = y.abs().div(0.8660254037844386);
    let x1 = x.abs().add(x2.div(2.));
    let m1 = x1.integer()?;
    let m2 = x2.integer()?;
    let r1 = x1.sub(Bounds::point(f64::from(m1)));
    let r2 = x2.sub(Bounds::point(f64::from(m2)));
    let one_minus = Bounds::point(1.).sub(r1);
    let (mut i, mut j) = if r1.lt(Bounds::point(0.5))? {
        if r1.lt(Bounds::point(1. / 3.))? {
            (
                m1,
                m2 + i32::from(Bounds::point(1.).add(r1).div(2.).le(r2)?),
            )
        } else {
            (
                m1 + i32::from(and(one_minus.le(r2), r2.lt(r1.mul(Bounds::point(2.))))?),
                m2 + i32::from(one_minus.le(r2)?),
            )
        }
    } else if r1.lt(Bounds::point(2. / 3.))? {
        // The separate outward multiply/subtract encloses both the fused and
        // non-fused implementations of mul_add(2, r1, -1).
        (
            m1 + i32::from(or(
                r2.le(r1.mul(Bounds::point(2.)).sub(Bounds::point(1.))),
                one_minus.le(r2),
            )?),
            m2 + i32::from(one_minus.le(r2)?),
        )
    } else {
        (m1 + 1, m2 + i32::from(r1.div(2.).le(r2)?))
    };
    if x.lt(Bounds::point(0.))? {
        let offset = j % 2;
        let axis_i = (j + offset) / 2;
        let diff = i - axis_i;
        i -= 2 * diff + offset;
    }
    if y.lt(Bounds::point(0.))? {
        i -= (2 * j + 1) / 2;
        j = -j;
    }
    Some(components(CoordIJK::new(i, j, 0).normalize()))
}

/// Runs the unmodified scalar quantizer, for verification of certificates.
#[must_use]
pub fn quantize_point(x: f64, y: f64) -> Option<[i32; 3]> {
    if !x.is_finite() || !y.is_finite() || x.abs() > 1_000_000. || y.abs() > 1_000_000. {
        return None;
    }
    Some(components(CoordIJK::from(Vec2d::new(x, y))))
}

/// Certifies native closest-face selection for a box of Cartesian intermediates.
///
/// These must enclose the outputs of the native `Vec3d::from(LatLng)` stage.
/// A mathematical unit-vector enclosure alone does not establish that contract.
/// Strict distance separation rejects ties and uncertain face boundaries.
#[must_use]
pub fn closest_face_box(v: [Bounds; 3]) -> Option<Face> {
    if v.iter().any(|c| !c.small(2.)) {
        return None;
    }
    let distances = face::CENTER_POINT.map(|c| {
        let dx = v[0].sub(Bounds::point(c.x));
        let dy = v[1].sub(Bounds::point(c.y));
        let dz = v[2].sub(Bounds::point(c.z));
        // Outward operations enclose the nested fused native distance too.
        dx.mul(dx).add(dy.mul(dy).add(dz.mul(dz)))
    });
    let candidate = distances.iter().position(|d| {
        d.hi < 5.
            && distances
                .iter()
                .all(|other: &Bounds| core::ptr::eq(d, other) || d.hi < other.lo)
    })?;
    Some(Face::new_unchecked(candidate))
}

/// Certifies whether a specific face is strictly closest for a box of Cartesian intermediates,
/// early-terminating on the first face that cannot be proven further.
#[must_use]
pub fn is_face_strictly_closest(v: [Bounds; 3], face: Face) -> bool {
    if v.iter().any(|c| !c.small(2.)) {
        return false;
    }
    let face_idx = usize::from(face);
    let c0 = face::CENTER_POINT[face_idx];
    let dx0 = v[0].sub(Bounds::point(c0.x));
    let dy0 = v[1].sub(Bounds::point(c0.y));
    let dz0 = v[2].sub(Bounds::point(c0.z));
    let d0 = dx0.mul(dx0).add(dy0.mul(dy0).add(dz0.mul(dz0)));
    if d0.hi >= 5.0 {
        return false;
    }
    for (idx, &c) in face::CENTER_POINT.iter().enumerate() {
        if idx == face_idx {
            continue;
        }
        let dx = v[0].sub(Bounds::point(c.x));
        let dy = v[1].sub(Bounds::point(c.y));
        let dz = v[2].sub(Bounds::point(c.z));
        let dk = dx.mul(dx).add(dy.mul(dy).add(dz.mul(dz)));
        if d0.hi >= dk.lo {
            return false;
        }
    }
    true
}

/// Native intermediates for one point; useful only as a verification oracle.
#[derive(Clone, Copy, Debug)]
pub struct NativeTrace {
    /// Cartesian vector produced by native platform math.
    pub vector: [f64; 3],
    /// Face selected by the native indexer.
    pub face: Face,
    /// Native projected Hex2d coordinates.
    pub xy: [f64; 2],
    /// Native normalized quantized coordinates.
    pub ijk: [i32; 3],
}
/// Observes the existing projection without changing its arithmetic.
#[must_use]
pub fn trace(point: LatLng, resolution: Resolution) -> NativeTrace {
    let v = Vec3d::from(point);
    let (face, distance) = point.closest_face();
    let xy = point.to_vec2d(resolution, face, distance);
    NativeTrace {
        vector: [v.x, v.y, v.z],
        face,
        xy: [xy.x, xy.y],
        ijk: components(CoordIJK::from(xy)),
    }
}

/// A known cell against which to verify native intermediate rectangles.
///
/// This type deliberately provides no geographic-span or skip-token API.
#[derive(Clone, Copy, Debug)]
pub struct CellVerifier {
    cell: CellIndex,
    face: Face,
    ijk: [i32; 3],
}
impl CellVerifier {
    /// Creates a verifier for a single-face, non-pentagon cell at resolution 4–10.
    #[must_use]
    pub fn new(cell: CellIndex) -> Option<Self> {
        if !(Resolution::Four..=Resolution::Ten).contains(&cell.resolution())
            || cell.is_pentagon()
            || cell.icosahedron_faces().iter().count() != 1
        {
            return None;
        }
        let f = FaceIJK::from(cell);
        if f.to_cell(cell.resolution()) != cell {
            return None;
        }
        Some(Self {
            cell,
            face: f.face,
            ijk: components(f.coord),
        })
    }
    /// Returns the reference cell.
    #[must_use]
    pub const fn cell(self) -> CellIndex {
        self.cell
    }
    /// Returns the selected icosahedron face.
    #[must_use]
    pub const fn face(self) -> Face {
        self.face
    }
    /// Checks the integer tail conditional on a fixed native face and XY bounds.
    ///
    /// The caller must separately enclose the projection of all inputs and
    /// establish that they select this face. Endpoint sampling is insufficient.
    #[must_use]
    pub fn matches_projected_box(self, face: Face, x: Bounds, y: Bounds) -> bool {
        face == self.face && quantize_box(x, y) == Some(self.ijk)
    }
}

/// A sufficient certificate for one native quantizer rectangle. Private fields
/// prevent callers from manufacturing a certificate without `quantize_box`.
#[derive(Clone, Copy, Debug)]
struct ProjectedBox {
    x: Bounds,
    y: Bounds,
}
impl ProjectedBox {
    fn contains(self, xy: Vec2d) -> bool {
        // Ordered comparisons also reject NaNs and infinities.
        self.x.lo <= xy.x && xy.x <= self.x.hi && self.y.lo <= xy.y && xy.y <= self.y.hi
    }

    fn around(xy: Vec2d, ijk: [i32; 3]) -> Option<Self> {
        // These radii are proposal sizes, NOT numerical-error allowances.
        // Only the interval interpreter below can authorize a rectangle.
        // Bound construction work; failure simply leaves the exact path active.
        for exponent in 0..6 {
            let radius = 0.25 / f64::from(1u32 << exponent);
            let x = Bounds::new((xy.x - radius).next_down(), (xy.x + radius).next_up())?;
            let y = Bounds::new((xy.y - radius).next_down(), (xy.y + radius).next_up())?;
            if quantize_box(x, y) == Some(ijk) {
                return Some(Self { x, y });
            }
        }
        None
    }
}

/// Reuses a cell only after inspecting each input's actual native projection.
///
/// A rectangle is sufficient if abstract interpretation establishes one IJK
/// result throughout it. Every input still executes the same closest-face and
/// floating-point projection stages as `LatLng::to_cell`; membership uses the
/// resulting native XY values, not an approximation of geographic geometry.
/// On a rectangle miss we execute the native quantizer. The known integer tail
/// can also be reused on exact face/IJK equality; otherwise it runs unchanged.
///
/// This is NOT a certificate for unvisited geographic samples. Call `index`
/// for every pixel, including intermediate pixels in a proposed scanline span.
#[derive(Clone, Copy, Debug)]
pub struct GuardedCellIndexer {
    verifier: CellVerifier,
    rectangle: Option<ProjectedBox>,
}
impl GuardedCellIndexer {
    /// Uses the same conservative cell/resolution restrictions as `CellVerifier`.
    #[must_use]
    pub fn new(cell: CellIndex) -> Option<Self> {
        Some(Self {
            verifier: CellVerifier::new(cell)?,
            rectangle: None,
        })
    }

    /// Returns the exact native assignment and whether quantization was skipped.
    /// Resolution mismatches, face changes, and inconclusive certificates all
    /// take the native path. No caller-supplied membership assertion is trusted.
    #[must_use]
    pub fn index(&mut self, point: LatLng, resolution: Resolution) -> (CellIndex, bool) {
        let (face, xy) = point.to_face_xy(resolution);
        let same_context =
            resolution == self.verifier.cell.resolution() && face == self.verifier.face;
        if same_context
            && self
                .rectangle
                .is_some_and(|rectangle| rectangle.contains(xy))
        {
            return (self.verifier.cell, true);
        }

        let coord = CoordIJK::from(xy);
        if same_context && components(coord) == self.verifier.ijk {
            self.rectangle = ProjectedBox::around(xy, self.verifier.ijk);
            // CellVerifier::new proved this integer address maps to this cell.
            (self.verifier.cell, false)
        } else {
            (FaceIJK::new(face, coord).to_cell(resolution), false)
        }
    }
}

/// Reuses index construction for identical native face/grid addresses.
///
/// Every input still executes the unmodified native projection and quantization.
/// The cache key includes resolution, face, and all normalized IJK components.
/// A miss runs the same integer tail as `LatLng::to_cell`. No geometric bounds
/// or assumptions about transcendental accuracy are involved.
#[derive(Clone, Copy, Debug, Default)]
pub struct CachedIndexer {
    last: Option<(Resolution, FaceIJK, CellIndex)>,
}
impl CachedIndexer {
    /// Indexes a point and reports whether the integer tail was reused.
    #[must_use]
    pub fn index(&mut self, point: LatLng, resolution: Resolution) -> (CellIndex, bool) {
        let address = point.to_face_ijk(resolution);
        if let Some((old_resolution, old_address, cell)) = self.last {
            if resolution == old_resolution && address == old_address {
                return (cell, true);
            }
        }
        let cell = address.to_cell(resolution);
        self.last = Some((resolution, address, cell));
        (cell, false)
    }
}
