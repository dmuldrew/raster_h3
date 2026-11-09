//! Keys that decide when a horizon-map cell can no longer receive samples.
//!
//! Every chunk gets a *bound* and every cell a *key* in the same ordered
//! space. Larger values are reached earlier in the scan. A cell is complete
//! once its key exceeds the largest bound of any chunk not yet processed,
//! because every sample the cell could receive would have a value at least
//! its key.
//!
//! [`EvictionSpace::Latitude`] uses latitude: a cell's key is a lower bound of
//! its latitudes and a chunk's bound an upper bound of its samples' latitudes.
//! That is exact for rows that follow parallels (north-up WGS84 and Web
//! Mercator), but Albers rows curve across parallels: a CONUS row is about 3°
//! further south at its ends than at the central meridian, so cells at the
//! east and west edges stayed in memory for thousands of rows.
//!
//! [`EvictionSpace::Rows`] therefore uses the negated row coordinate of a
//! reference grid for mosaics whose tiles all use the same Albers
//! projection and lie on its canonical branch (inside the wedge that maps
//! to longitudes within 180 degrees of the central meridian). A chunk's bound is minus its first row; a cell's key is minus
//! a certified upper bound of the rows its points project to.

use h3o::{CellIndex, LatLng};

use crate::aggregator::horizon_streamer::compute_cell_south_lat;
use crate::crs::transformer::{wrap_lon, AlbersConicFast, CrsTransformer};
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::coordinates::eviction_north_bound;

/// Largest radius of curvature of the GRS80 ellipsoid used by
/// [`AlbersConicFast`] (`a / sqrt(1 - e²)`, at the poles), rounded up. A
/// curve of angular length θ on the unit sphere is at most this many metres
/// long when its latitudes and longitudes are read on the ellipsoid.
const MAX_RADIUS_OF_CURVATURE_M: f64 = 6_399_594.0;

/// Cells whose bounding cap reaches beyond this latitude keep the latitude
/// rule's behaviour of staying in memory, because the projection's scale
/// grows without bound towards the poles.
const MAX_CERTIFIED_LAT_DEG: f64 = 85.0;

/// Radians around [`RowSpace::k_argmin`] within which the minimum of the
/// parallel scale is assumed to lie.
const ARGMIN_TOLERANCE: f64 = 1e-4;

/// Eviction key and latitude summary of one cell, computed together because
/// both need the cell's geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellKeys {
    /// Ordering key compared against chunk bounds (see the module docs).
    pub key: f64,
    /// An upper bound of the cell's southernmost latitude, used to keep the
    /// published latitude horizon conservative while the cell is held. Equal
    /// to `key` in [`EvictionSpace::Latitude`].
    pub south_lat_upper: f64,
}

/// How cell keys and chunk bounds are measured for one stream.
#[derive(Debug, Clone)]
pub enum EvictionSpace {
    /// Keys are latitudes ([`compute_cell_south_lat`], [`eviction_north_bound`]).
    Latitude,
    /// Keys are negated rows of a reference grid in a shared Albers projection.
    Rows(Box<RowSpace>),
}

/// Row coordinates of a reference grid in an Albers projection.
#[derive(Debug, Clone)]
pub struct RowSpace {
    albers: AlbersConicFast,
    /// `row = row_x * x + row_y * y + row_0` for projected `(x, y)`.
    row_x: f64,
    row_y: f64,
    row_0: f64,
    /// Rows per projected metre in the steepest direction.
    rows_per_metre: f64,
    /// Rows added to every cell bound for floating-point error between the
    /// forward projection, its inverse and H3's point assignment.
    margin_rows: f64,
    /// Latitude (radians) where the parallel scale factor is smallest.
    k_argmin: f64,
}

impl EvictionSpace {
    /// The space for a mosaic: [`Self::Rows`] when every tile uses the same
    /// Albers projection and an invertible, finite geotransform, and lies
    /// entirely on the projection's canonical branch; otherwise
    /// [`Self::Latitude`].
    pub fn for_mosaic(mosaic: &MosaicReader) -> Self {
        let Some(first) = mosaic.tiles.first() else {
            return Self::Latitude;
        };
        let CrsTransformer::AlbersConic(albers) = &first.crs_transformer else {
            return Self::Latitude;
        };
        let same_projection = mosaic.tiles.iter().all(|tile| match &tile.crs_transformer {
            CrsTransformer::AlbersConic(other) => same_albers(albers, other),
            _ => false,
        });
        if !same_projection {
            return Self::Latitude;
        }
        if mosaic
            .tiles
            .iter()
            .any(|tile| !finite_invertible(&tile.reader.metadata.geotransform))
        {
            return Self::Latitude;
        }
        // Row keys project each cell onto the canonical branch of the
        // projection, so every sample must come from it too.
        if !mosaic.tiles.iter().all(|tile| {
            let meta = &tile.reader.metadata;
            on_canonical_branch(albers, &meta.geotransform, meta.width, meta.height)
        }) {
            return Self::Latitude;
        }
        RowSpace::new(*albers, &first.reader.metadata.geotransform)
            .map_or(Self::Latitude, |rows| Self::Rows(Box::new(rows)))
    }

    /// Whether keys are rows rather than latitudes.
    pub fn is_rows(&self) -> bool {
        matches!(self, Self::Rows(_))
    }

    /// Keys of `cell_u64`. Never larger than the value of any point of the
    /// cell; non-finite geometry yields `-inf`, so the cell stays until the end.
    pub fn cell_keys(&self, cell_u64: u64) -> CellKeys {
        match self {
            Self::Latitude => {
                let south = compute_cell_south_lat(cell_u64);
                CellKeys {
                    key: south,
                    south_lat_upper: south,
                }
            }
            Self::Rows(rows) => rows.cell_keys(cell_u64),
        }
    }

    /// Upper bound of the value of every sample in `chunk` of a tile with
    /// geotransform `gt` and transformer `crs`.
    pub fn chunk_bound(&self, chunk: &RasterChunk, gt: &GeoTransform, crs: &CrsTransformer) -> f64 {
        match self {
            Self::Latitude => eviction_north_bound(chunk, gt, crs),
            Self::Rows(rows) => rows.chunk_bound(chunk, gt),
        }
    }
}

fn same_albers(a: &AlbersConicFast, b: &AlbersConicFast) -> bool {
    let fields = |p: &AlbersConicFast| {
        [
            p.lat_origin_rad,
            p.lon_origin_rad,
            p.x_0,
            p.y_0,
            p.n,
            p.c,
            p.rho0,
            p.e,
            p.e2,
            p.a,
            p.qp,
        ]
        .map(f64::to_bits)
    };
    fields(a) == fields(b)
}

/// Whether every point of a `width` x `height` pixel grid lies on the
/// canonical branch of `albers`: its polar angle about the cone's apex,
/// `theta = n (lon - lon_0)`, is within `(-|n| pi, |n| pi)`.
///
/// The inverse projection reads `theta` from the full `(-pi, pi]` and
/// divides by `n`, so for `|n| < 1` points outside that wedge get
/// longitudes more than 180 degrees from `lon_0`. The forward projection
/// wraps such a longitude back by 360 degrees, onto a different place in
/// the plane, so the cells of those samples would be keyed by rows far from
/// the samples' own.
///
/// The grid is a parallelogram, hence convex. If it does not contain the
/// apex, it subtends less than a half turn from it, and its angular range
/// is spanned by its corners, read continuously around the angle of its
/// centre.
fn on_canonical_branch(
    albers: &AlbersConicFast,
    gt: &GeoTransform,
    width: u32,
    height: u32,
) -> bool {
    use std::f64::consts::PI;
    // Polar angle about the apex, as `AlbersInverse::transform_with_yp`
    // computes it.
    let angle = |x: f64, y: f64| {
        let x_p = x - albers.x_0;
        let y_p = albers.rho0 - (y - albers.y_0);
        if albers.n >= 0.0 {
            x_p.atan2(y_p)
        } else {
            (-x_p).atan2(-y_p)
        }
    };
    let (w, h) = (f64::from(width), f64::from(height));
    let corners =
        [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)].map(|(col, row)| gt.pixel_to_coord(col, row));
    // The apex lies inside the parallelogram or on its boundary iff no two
    // edges see it on opposite sides.
    let (apex_x, apex_y) = (albers.x_0, albers.y_0 + albers.rho0);
    let sides = (0..4).map(|i| {
        let (ax, ay) = corners[i];
        let (bx, by) = corners[(i + 1) % 4];
        (bx - ax) * (apex_y - ay) - (by - ay) * (apex_x - ax)
    });
    let (mut neg, mut pos) = (false, false);
    for side in sides {
        if !side.is_finite() {
            return false;
        }
        neg |= side < 0.0;
        pos |= side > 0.0;
    }
    if !(neg && pos) {
        return false;
    }
    let (cx, cy) = gt.pixel_to_coord(w / 2.0, h / 2.0);
    let centre = angle(cx, cy);
    let limit = albers.n.abs() * PI;
    corners.iter().all(|&(x, y)| {
        let delta = angle(x, y) - centre;
        // Continuous around the centre's angle: |delta| < pi after wrapping.
        let delta = delta - (2.0 * PI) * (delta / (2.0 * PI)).round();
        let theta = centre + delta;
        theta.is_finite() && theta.abs() < limit
    }) && centre.abs() < limit
}

fn finite_invertible(gt: &GeoTransform) -> bool {
    let det = gt.a * gt.e - gt.b * gt.d;
    [gt.a, gt.b, gt.c0, gt.d, gt.e, gt.f0, det]
        .iter()
        .all(|v| v.is_finite())
        && det != 0.0
}

impl RowSpace {
    fn new(albers: AlbersConicFast, gt: &GeoTransform) -> Option<Self> {
        // x = c0 + a col + b row, y = f0 + d col + e row, so
        // row = (a (y - f0) - d (x - c0)) / det.
        let det = gt.a * gt.e - gt.b * gt.d;
        let (row_x, row_y) = (-gt.d / det, gt.a / det);
        let row_0 = (gt.d * gt.c0 - gt.a * gt.f0) / det;
        let rows_per_metre = row_x.hypot(row_y);
        let space = Self {
            albers,
            row_x,
            row_y,
            row_0,
            rows_per_metre,
            margin_rows: 1.0 + rows_per_metre,
            k_argmin: k_argmin(&albers)?,
        };
        let finite = [row_x, row_y, row_0, rows_per_metre, space.margin_rows]
            .iter()
            .all(|v| v.is_finite());
        (finite && albers.n != 0.0 && albers.n.is_finite()).then_some(space)
    }

    fn row(&self, x: f64, y: f64) -> f64 {
        self.row_x * x + self.row_y * y + self.row_0
    }

    fn chunk_bound(&self, chunk: &RasterChunk, gt: &GeoTransform) -> f64 {
        // Samples lie in the chunk's pixel rectangle, a parallelogram in
        // projected coordinates, where the linear row attains its minimum
        // at a corner.
        let cols = [
            chunk.col_offset as f64,
            chunk.col_offset as f64 + chunk.width as f64,
        ];
        let rows = [
            chunk.row_offset as f64,
            chunk.row_offset as f64 + chunk.height as f64,
        ];
        let mut first_row = f64::INFINITY;
        for col in cols {
            for row in rows {
                let (x, y) = gt.pixel_to_coord(col, row);
                first_row = first_row.min(self.row(x, y));
            }
        }
        if first_row.is_finite() {
            (-first_row).next_up()
        } else {
            f64::INFINITY
        }
    }

    fn cell_keys(&self, cell_u64: u64) -> CellKeys {
        let never = CellKeys {
            key: f64::NEG_INFINITY,
            south_lat_upper: 90.0,
        };
        let Ok(cell) = CellIndex::try_from(cell_u64) else {
            return never;
        };
        let center = LatLng::from(cell);
        let boundary = cell.boundary();
        if boundary.is_empty() {
            return never;
        }
        // The cell lies in the spherical cap around its centre through its
        // farthest vertex: its edges are geodesics, so it is inside the
        // vertices' convex hull. The relative and absolute slack covers H3
        // assigning points within rounding distance of the boundary.
        let radius = boundary
            .iter()
            .map(|v| center.distance_rads(*v))
            .fold(0.0_f64, f64::max);
        let radius = radius * (1.0 + 1e-6) + 1e-9;
        let (lat_c, lng_c) = (center.lat(), center.lng());
        let south_lat_upper = lat_c.next_up();
        let lat_lo = lat_c.to_radians() - radius;
        let lat_hi = lat_c.to_radians() + radius;
        let lat_limit = MAX_CERTIFIED_LAT_DEG.to_radians();
        if !radius.is_finite() || lat_lo <= -lat_limit || lat_hi >= lat_limit {
            return CellKeys {
                key: f64::NEG_INFINITY,
                south_lat_upper,
            };
        }
        // Stay clear of the projection's cut at the antimeridian of lon_0,
        // where it is discontinuous. Within the cap, longitude differs from
        // the centre's by at most asin(sin r / cos lat_max) < 2 r / cos lat_max.
        let cos_max = lat_lo.abs().max(lat_hi.abs()).cos();
        let lng_reach = (2.0 * radius / cos_max).to_degrees();
        let from_cut = 180.0 - wrap_lon(lng_c - self.albers.lon_origin_rad.to_degrees()).abs();
        if lng_reach.is_nan() || from_cut.is_nan() || lng_reach >= from_cut - 1.0 {
            return CellKeys {
                key: f64::NEG_INFINITY,
                south_lat_upper,
            };
        }
        // Any point of the cell joins the centre by a geodesic of angular
        // length <= radius inside the cap. Read on the ellipsoid that path is
        // at most radius * R_max metres long, and the projection stretches
        // lengths by at most the largest principal scale on the cap.
        let scale = self.max_scale(lat_lo, lat_hi);
        let (x, y) = self.albers.project_point(lng_c, lat_c);
        let reach_m = radius * MAX_RADIUS_OF_CURVATURE_M * scale * (1.0 + 1e-9);
        let last_row = self.row(x, y) + reach_m * self.rows_per_metre + self.margin_rows;
        if !last_row.is_finite() {
            return CellKeys {
                key: f64::NEG_INFINITY,
                south_lat_upper,
            };
        }
        CellKeys {
            key: (-last_row).next_down(),
            south_lat_upper,
        }
    }

    /// Largest principal scale factor of the projection on latitudes
    /// `[lat_lo, lat_hi]` (radians). Albers is equal-area, so the meridian
    /// scale is the reciprocal of the parallel scale `k`, which depends only
    /// on latitude and has a single minimum at `k_argmin`.
    fn max_scale(&self, lat_lo: f64, lat_hi: f64) -> f64 {
        let (k_lo, k_hi) = (
            parallel_scale(&self.albers, lat_lo),
            parallel_scale(&self.albers, lat_hi),
        );
        // `k_argmin` is located to about 1e-7 rad, where k is flat, so also
        // count the minimum for intervals that end just short of it: that can
        // only lower `k_min`, which is conservative.
        let near_argmin =
            (lat_lo - ARGMIN_TOLERANCE..=lat_hi + ARGMIN_TOLERANCE).contains(&self.k_argmin);
        let mut k_min = k_lo.min(k_hi);
        if near_argmin {
            k_min = k_min.min(parallel_scale(&self.albers, self.k_argmin));
        }
        let s = k_lo.max(k_hi).max(1.0 / k_min);
        // Rounding in k and the location of its minimum.
        if s.is_finite() {
            s * (1.0 + 1e-6)
        } else {
            f64::INFINITY
        }
    }
}

/// Parallel scale factor `k = n ρ / (a m)` of an ellipsoidal Albers
/// projection at latitude `phi` (radians).
pub(crate) fn parallel_scale(p: &AlbersConicFast, phi: f64) -> f64 {
    let sin_phi = phi.sin();
    let e_sin = p.e * sin_phi;
    let q = (1.0 - p.e2)
        * (sin_phi / (1.0 - e_sin * e_sin)
            - (1.0 / (2.0 * p.e)) * ((1.0 - e_sin) / (1.0 + e_sin)).ln());
    let m = phi.cos() / (1.0 - p.e2 * sin_phi * sin_phi).sqrt();
    ((p.c - p.n * q).max(0.0).sqrt() / m).abs()
}

/// Latitude (radians) of the minimum of [`parallel_scale`], by golden-section
/// search over the certified latitude range.
fn k_argmin(p: &AlbersConicFast) -> Option<f64> {
    let limit = MAX_CERTIFIED_LAT_DEG.to_radians();
    let (mut lo, mut hi) = (-limit, limit);
    let ratio = (5.0_f64.sqrt() - 1.0) / 2.0;
    for _ in 0..200 {
        let m1 = hi - ratio * (hi - lo);
        let m2 = lo + ratio * (hi - lo);
        if parallel_scale(p, m1) < parallel_scale(p, m2) {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    let argmin = 0.5 * (lo + hi);
    argmin.is_finite().then_some(argmin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn albers_definitions() -> [AlbersConicFast; 3] {
        [
            AlbersConicFast::epsg_5070(),
            // EPSG:3338 (Alaska Albers).
            AlbersConicFast::new(55.0, 65.0, 50.0, -154.0),
            // A southern-hemisphere cone.
            AlbersConicFast::new(-18.0, -36.0, 0.0, 132.0),
        ]
    }

    /// `max_scale` relies on k having one minimum; check it decreases then
    /// increases on a dense grid for each definition.
    #[test]
    fn parallel_scale_is_unimodal() {
        for p in albers_definitions() {
            let argmin = k_argmin(&p).unwrap();
            let limit = MAX_CERTIFIED_LAT_DEG.to_radians();
            let steps = 20_000;
            let mut prev = parallel_scale(&p, -limit);
            for i in 1..=steps {
                let phi = -limit + 2.0 * limit * i as f64 / steps as f64;
                let k = parallel_scale(&p, phi);
                if phi < argmin - ARGMIN_TOLERANCE {
                    assert!(
                        k <= prev * (1.0 + 1e-12),
                        "k rises before its minimum at {phi}"
                    );
                } else if phi > argmin + ARGMIN_TOLERANCE {
                    assert!(
                        k >= prev * (1.0 - 1e-12),
                        "k falls after its minimum at {phi}"
                    );
                }
                prev = k;
            }
        }
    }

    /// Points of a cell to test against its key: centre, vertices, points
    /// along each geodesic edge and the centres of descendants.
    fn points_in_cell(cell: CellIndex) -> Vec<LatLng> {
        let mut points = vec![LatLng::from(cell)];
        let boundary: Vec<LatLng> = cell.boundary().iter().copied().collect();
        for (i, &p) in boundary.iter().enumerate() {
            let q = boundary[(i + 1) % boundary.len()];
            points.push(p);
            // Normalised linear interpolation of unit vectors stays on the
            // geodesic between p and q.
            let v = |ll: LatLng| {
                let (lat, lng) = (ll.lat_radians(), ll.lng_radians());
                [lat.cos() * lng.cos(), lat.cos() * lng.sin(), lat.sin()]
            };
            let (a, b) = (v(p), v(q));
            for t in 1..8 {
                let t = t as f64 / 8.0;
                let w = [0, 1, 2].map(|j| a[j] * (1.0 - t) + b[j] * t);
                let norm = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
                let w = w.map(|c| c / norm);
                let lat = w[2].asin().to_degrees();
                let lng = w[1].atan2(w[0]).to_degrees();
                points.push(LatLng::new(lat, lng).unwrap());
            }
        }
        let res = u8::from(cell.resolution());
        if res + 2 <= 15 {
            let child = h3o::Resolution::try_from(res + 2).unwrap();
            points.extend(cell.children(child).map(LatLng::from));
        }
        points
    }

    /// Every point of a cell projects to a row no later than the cell's key
    /// says, and the key is not loose by more than the cell's diameter plus
    /// the margin. Covers resolutions 0-15, north-up and rotated grids, and
    /// three cones.
    #[test]
    fn row_keys_bound_every_point_of_the_cell() {
        let grids = [
            // North-up 30 m (the CONUS layout).
            GeoTransform {
                c0: -2_362_395.0,
                a: 30.0,
                b: 0.0,
                f0: 3_267_405.0,
                d: 0.0,
                e: -30.0,
            },
            // Rotated by about 17 degrees and anisotropic.
            GeoTransform {
                c0: 0.0,
                a: 28.7,
                b: 8.9,
                f0: 2_000_000.0,
                d: 8.9,
                e: -31.2,
            },
            // Rows running west to east from the south (row index grows north).
            GeoTransform {
                c0: -2_000_000.0,
                a: 0.0,
                b: 100.0,
                f0: 0.0,
                d: 100.0,
                e: 0.0,
            },
        ];
        let centres = [
            (
                albers_definitions()[0],
                [(38.0, -96.0), (47.0, -124.0), (25.0, -70.0), (49.5, -67.0)],
            ),
            (
                albers_definitions()[1],
                [
                    (64.0, -150.0),
                    (58.0, -170.0),
                    (70.0, -141.0),
                    (55.0, -131.0),
                ],
            ),
            (
                albers_definitions()[2],
                [
                    (-25.0, 132.0),
                    (-40.0, 115.0),
                    (-12.0, 150.0),
                    (-30.0, 160.0),
                ],
            ),
        ];
        let mut checked = 0usize;
        let mut finite = 0usize;
        let mut cells = 0usize;
        for (albers, places) in centres {
            for gt in &grids {
                let space = RowSpace::new(albers, gt).unwrap();
                for (lat, lng) in places {
                    for res in 0..=15u8 {
                        let r = h3o::Resolution::try_from(res).unwrap();
                        let origin = LatLng::new(lat, lng).unwrap().to_cell(r);
                        // The cell and its neighbours.
                        for cell in origin.grid_disk::<Vec<_>>(1) {
                            cells += 1;
                            let keys = space.cell_keys(u64::from(cell));
                            let points = points_in_cell(cell);
                            let south = points.iter().map(|p| p.lat()).fold(90.0, f64::min);
                            assert!(keys.south_lat_upper >= south, "south bound below the cell");
                            if !keys.key.is_finite() {
                                assert_eq!(keys.key, f64::NEG_INFINITY);
                                continue;
                            }
                            finite += 1;
                            let bound = -keys.key;
                            let mut max_row = f64::NEG_INFINITY;
                            let mut min_row = f64::INFINITY;
                            for p in points {
                                let (x, y) = albers.project_point(p.lng(), p.lat());
                                let row = space.row(x, y);
                                max_row = max_row.max(row);
                                min_row = min_row.min(row);
                                checked += 1;
                            }
                            assert!(
                                max_row <= bound,
                                "res {res} cell {cell}: point at row {max_row} past bound {bound}"
                            );
                            // Loose by at most twice the cell's row extent plus margins.
                            let extent = max_row - min_row;
                            assert!(
                                bound - max_row <= 2.5 * extent + 2.0 * space.margin_rows + 1.0,
                                "res {res} cell {cell}: bound {bound} loose against {max_row} (extent {extent})"
                            );
                        }
                    }
                }
            }
        }
        assert!(checked > 300_000, "only {checked} points checked");
        assert!(
            finite * 10 >= cells * 9,
            "only {finite} of {cells} cells certified"
        );
    }

    /// k equals 1 on the standard parallels of a two-parallel cone.
    #[test]
    fn canonical_branch_accepts_conus_and_rejects_grids_past_the_cut() {
        let conus = AlbersConicFast::epsg_5070();
        let grid =
            |c0: f64, f0: f64, a: f64, b: f64, d: f64, e: f64| GeoTransform { c0, f0, a, b, d, e };
        // BP_CONUS.tif: 156,335 x 101,538 pixels of 30 m.
        let bp_conus = grid(-2_361_915.0, 3_177_435.0, 30.0, 0.0, 0.0, -30.0);
        assert!(on_canonical_branch(&conus, &bp_conus, 156_335, 101_538));
        // The same grid rotated by 30 degrees.
        let (s, c) = 30f64.to_radians().sin_cos();
        let rotated = grid(
            -2_361_915.0,
            3_177_435.0,
            30.0 * c,
            30.0 * s,
            30.0 * s,
            -30.0 * c,
        );
        assert!(on_canonical_branch(&conus, &rotated, 156_335, 101_538));

        let apex_y = conus.y_0 + conus.rho0;
        // Directly above the apex, where the inverse gives lon_0 + 300 degrees.
        let above = grid(-6000.0, apex_y + conus.rho0, 30.0, 0.0, 0.0, 30.0);
        assert!(!on_canonical_branch(&conus, &above, 400, 120));
        // Containing the apex, or touching it with an edge or a corner.
        let around = grid(-1000.0, apex_y + 1000.0, 10.0, 0.0, 0.0, -10.0);
        assert!(!on_canonical_branch(&conus, &around, 200, 200));
        let edge = grid(-1000.0, apex_y, 10.0, 0.0, 0.0, -10.0);
        assert!(!on_canonical_branch(&conus, &edge, 200, 200));
        let corner = grid(0.0, apex_y, 10.0, 0.0, 0.0, -10.0);
        assert!(!on_canonical_branch(&conus, &corner, 200, 200));
        // Beside the apex, polar angle 90 degrees: inside CONUS's 108 degree
        // half-wedge (n ~ 0.6) ...
        let beside = grid(1_000_000.0, apex_y + 500.0, 10.0, 0.0, 0.0, -10.0);
        assert!(on_canonical_branch(&conus, &beside, 100, 100));
        // ... but not for a flatter cone whose half-wedge is narrower.
        let flat = AlbersConicFast::new(20.0, 30.0, 10.0, -96.0);
        assert!(flat.n < 0.5);
        let flat_apex = flat.y_0 + flat.rho0;
        let beside_flat = grid(1_000_000.0, flat_apex + 500.0, 10.0, 0.0, 0.0, -10.0);
        assert!(!on_canonical_branch(&flat, &beside_flat, 100, 100));

        // A southern cone mirrors it: its apex is below the origin.
        let south = AlbersConicFast::new(-45.5, -29.5, -23.0, 135.0);
        assert!(south.n < 0.0);
        let south_apex = south.y_0 + south.rho0;
        let australia = grid(-2_000_000.0, -1_000_000.0, 30.0, 0.0, 0.0, -30.0);
        assert!(on_canonical_branch(&south, &australia, 100_000, 60_000));
        let below = grid(-6000.0, south_apex + south.rho0, 30.0, 0.0, 0.0, -30.0);
        assert!(!on_canonical_branch(&south, &below, 400, 120));
    }

    #[test]
    fn parallel_scale_is_one_on_standard_parallels() {
        let p = AlbersConicFast::epsg_5070();
        for lat in [29.5_f64, 45.5] {
            assert!((parallel_scale(&p, lat.to_radians()) - 1.0).abs() < 1e-12);
        }
    }
}
