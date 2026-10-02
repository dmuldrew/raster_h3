//! Interval containment in the spherical polygon returned by h3o.
//!
//! IMPORTANT: this proves a statement about the supplied floating-point boundary
//! vertices, treated as exact spherical coordinates. h3o does not supply an
//! error enclosure relating these vertices to its indexing decisions. Therefore
//! this model is used ONLY for shadow verification, never to bypass indexing.
use crate::h3::{CellIndex, LatLng};

#[derive(Clone, Copy, Debug)]
struct I {
    lo: f64,
    hi: f64,
}
impl I {
    fn point(x: f64) -> Self {
        Self { lo: x, hi: x }
    }
    fn add(self, b: Self) -> Self {
        Self {
            lo: (self.lo + b.lo).next_down(),
            hi: (self.hi + b.hi).next_up(),
        }
    }
    fn neg(self) -> Self {
        Self {
            lo: -self.hi,
            hi: -self.lo,
        }
    }
    fn sub(self, b: Self) -> Self {
        self.add(b.neg())
    }
    fn mul(self, b: Self) -> Self {
        let v = [
            self.lo * b.lo,
            self.lo * b.hi,
            self.hi * b.lo,
            self.hi * b.hi,
        ];
        Self {
            lo: v.into_iter().fold(f64::INFINITY, f64::min).next_down(),
            hi: v.into_iter().fold(f64::NEG_INFINITY, f64::max).next_up(),
        }
    }
    fn div_positive(self, n: f64) -> Self {
        Self {
            lo: (self.lo / n).next_down(),
            hi: (self.hi / n).next_up(),
        }
    }
    fn abs_upper(self) -> f64 {
        self.lo.abs().max(self.hi.abs()).next_up()
    }
    fn widen(self, e: f64) -> Self {
        Self {
            lo: (self.lo - e).next_down(),
            hi: (self.hi + e).next_up(),
        }
    }
}

// Taylor polynomials evaluated with outward-rounded elementary operations.
// Sine: degree 39, remainder <= |x|^41/41!; cosine: degree 38,
// remainder <= |x|^40/40!. No libm accuracy assumption or empirical epsilon.
// Inputs are bounded by 4 radians, so every intermediate stays finite.
fn sin_cos(x: f64) -> Option<(I, I)> {
    if !x.is_finite() || x.abs() > 4.0 {
        return None;
    }
    let x = I::point(x);
    let x2 = x.mul(x);
    let (mut st, mut ct) = (x, I::point(1.0));
    let (mut s, mut c) = (st, ct);
    for k in 1..20 {
        st = st.mul(x2).neg().div_positive((2 * k * (2 * k + 1)) as f64);
        ct = ct.mul(x2).neg().div_positive(((2 * k - 1) * 2 * k) as f64);
        s = s.add(st);
        c = c.add(ct);
    }
    let sr = st.mul(x2).div_positive((40 * 41) as f64).abs_upper();
    let cr = ct.mul(x2).div_positive((39 * 40) as f64).abs_upper();
    Some((s.widen(sr), c.widen(cr)))
}

type V = [I; 3];
fn vector(ll: LatLng) -> Option<V> {
    let (sl, cl) = sin_cos(ll.lat_radians())?;
    let (so, co) = sin_cos(ll.lng_radians())?;
    Some([cl.mul(co), cl.mul(so), sl])
}
fn cross(a: V, b: V) -> V {
    [
        a[1].mul(b[2]).sub(a[2].mul(b[1])),
        a[2].mul(b[0]).sub(a[0].mul(b[2])),
        a[0].mul(b[1]).sub(a[1].mul(b[0])),
    ]
}
fn dot(a: V, b: V) -> I {
    a[0].mul(b[0]).add(a[1].mul(b[1])).add(a[2].mul(b[2]))
}

/// Six inward half-spaces for a verified convex, single-face polygon model.
pub(crate) struct EdgeModel {
    normals: [V; 6],
}
impl EdgeModel {
    pub(crate) fn new(cell: u64) -> Option<Self> {
        let cell = CellIndex::try_from(cell).ok()?;
        if !(4..=10).contains(&(cell.resolution() as u8))
            || cell.is_pentagon()
            || cell.icosahedron_faces().iter().count() != 1
        {
            return None;
        }
        let boundary = cell.boundary();
        if boundary.len() != 6 {
            return None;
        }
        let q = vector(LatLng::from(cell))?;
        let mut vertices = [[I::point(0.0); 3]; 6];
        for (v, ll) in vertices.iter_mut().zip(boundary.iter()) {
            *v = vector(*ll)?;
            // The model must lie in the open hemisphere around its interior q.
            if dot(q, *v).lo <= 0.0 {
                return None;
            }
        }
        let mut normals = [[I::point(0.0); 3]; 6];
        for k in 0..6 {
            let mut n = cross(vertices[k], vertices[(k + 1) % 6]);
            if dot(n, q).hi < 0.0 {
                n = n.map(I::neg);
            }
            if dot(n, q).lo <= 0.0 {
                return None;
            }
            for (j, v) in vertices.iter().enumerate() {
                if j != k && j != (k + 1) % 6 && dot(n, *v).lo <= 0.0 {
                    return None;
                }
            }
            normals[k] = n;
        }
        Some(Self { normals })
    }
    pub(crate) fn parallel(&self, lat: f64, lon: f64) -> Option<ParallelModel> {
        if !lat.is_finite() || lat.abs() >= 70.0 || !lon.is_finite() || lon.abs() > 175.0 {
            return None;
        }
        // These are the same rounded degree-to-radian conversions as LatLng::new.
        let (s, c) = sin_cos(lat.to_radians())?;
        let start = lon.to_radians();
        let (so, co) = sin_cos(start)?;
        let edges = self.normals.map(|n| {
            let a = n[0].mul(c);
            let b = n[1].mul(c);
            let c = n[2].mul(s);
            let at_start = a.mul(co).add(b.mul(so)).add(c);
            let curvature = (a.abs_upper() + b.abs_upper()).next_up();
            EdgeOnParallel {
                a,
                b,
                c,
                at_start,
                curvature,
            }
        });
        Some(ParallelModel { start, edges })
    }
}

#[derive(Clone, Copy)]
struct EdgeOnParallel {
    a: I,
    b: I,
    c: I,
    at_start: I,
    curvature: f64,
}
impl EdgeOnParallel {
    fn lower(&self, sin_end: I, cos_end: I, width: f64) -> f64 {
        let at_end = self.a.mul(cos_end).add(self.b.mul(sin_end)).add(self.c);
        // Linear interpolation error <= M*h^2/8 for |f''| <= M.
        // This covers ALL interior extrema, including tangency and re-entry.
        let error = I::point(self.curvature)
            .mul(I::point(width))
            .mul(I::point(width))
            .div_positive(8.0);
        (self.at_start.lo.min(at_end.lo) - error.hi).next_down()
    }
}
pub(crate) struct ParallelModel {
    start: f64,
    edges: [EdgeOnParallel; 6],
}
impl ParallelModel {
    pub(crate) fn contains_to(&self, longitude: f64) -> bool {
        if !longitude.is_finite() || longitude.abs() > 175.0 {
            return false;
        }
        let end = longitude.to_radians();
        let Some((s, c)) = sin_cos(end) else {
            return false;
        };
        let width = I::point(end).sub(I::point(self.start)).abs_upper();
        self.edges.iter().all(|edge| edge.lower(s, c, width) > 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trig_enclosures_include_reference_and_reject_invalid_arguments() {
        for k in -4000..=4000 {
            let x = k as f64 / 1000.0;
            let (s, c) = sin_cos(x).unwrap();
            assert!(s.lo <= x.sin() && x.sin() <= s.hi, "sin {x}: {s:?}");
            assert!(c.lo <= x.cos() && x.cos() <= c.hi, "cos {x}: {c:?}");
        }
        for x in [f64::NAN, f64::INFINITY, 4.1] {
            assert!(sin_cos(x).is_none());
        }
    }
    #[test]
    fn curvature_bound_rejects_positive_endpoints_with_negative_interior() {
        // f(x) = 0.999 - cos(x): positive at +/-0.1, negative at zero.
        let (s, c) = sin_cos(0.1).unwrap();
        let edge = EdgeOnParallel {
            a: I::point(-1.0),
            b: I::point(0.0),
            c: I::point(0.999),
            at_start: I::point(0.999).sub(c),
            curvature: 1.0,
        };
        assert!(edge.at_start.lo > 0.0);
        assert!(edge.lower(s, c, 0.2) < 0.0);
    }
    #[test]
    fn interior_segments_are_accepted_and_boundaries_are_not() {
        let cell = LatLng::new(37.8, -122.4)
            .unwrap()
            .to_cell(crate::h3::Resolution::Seven);
        let q = LatLng::from(cell);
        let model = EdgeModel::new(cell.into()).unwrap();
        let row = model.parallel(q.lat(), q.lng()).unwrap();
        assert!(row.contains_to(q.lng() + 1e-5));
        assert!(row.contains_to(q.lng() - 1e-5));
        assert!(!row.contains_to(q.lng() + 1.0));
        for v in cell.boundary().iter() {
            assert!(!model
                .parallel(v.lat(), v.lng())
                .unwrap()
                .contains_to(v.lng()));
        }
        assert!(EdgeModel::new(0).is_none());
        assert!(model.parallel(70.0, 0.0).is_none());
        assert!(!row.contains_to(f64::NAN));
    }
    #[test]
    fn accepted_models_agree_with_dense_h3_samples_across_regions() {
        let mut accepted = 0;
        for res in [
            crate::h3::Resolution::Four,
            crate::h3::Resolution::Seven,
            crate::h3::Resolution::Ten,
        ] {
            for lat in [-69.9, -45.0, 0.0, 45.0, 69.9] {
                for lon in [-174.9, -120.0, 0.0, 55.0, 174.9] {
                    let cell = LatLng::new(lat, lon).unwrap().to_cell(res);
                    let Some(model) = EdgeModel::new(cell.into()) else {
                        continue;
                    };
                    let q = LatLng::from(cell);
                    for dy in [-1e-6, 0.0, 1e-6] {
                        for dx in [-0.1, -0.001, -1e-6, 1e-6, 0.001, 0.1] {
                            let latitude = q.lat() + dy;
                            let Some(row) = model.parallel(latitude, q.lng()) else {
                                continue;
                            };
                            if !row.contains_to(q.lng() + dx) {
                                continue;
                            }
                            accepted += 1;
                            for k in 0..=128 {
                                let point =
                                    LatLng::new(latitude, q.lng() + dx * k as f64 / 128.0).unwrap();
                                assert_eq!(point.to_cell(res), cell, "{latitude} {}", point.lng());
                            }
                        }
                    }
                }
            }
        }
        assert!(accepted > 100, "test must exercise accepted proposals");
        let mut seams = 0;
        for p in crate::h3::Resolution::Seven.pentagons() {
            assert!(EdgeModel::new(p.into()).is_none());
            for neighbor in p.grid_disk::<Vec<_>>(2) {
                if neighbor.icosahedron_faces().iter().count() > 1 {
                    seams += 1;
                    assert!(EdgeModel::new(neighbor.into()).is_none());
                }
            }
        }
        assert!(seams > 0);
        let high = LatLng::new(37.8, -122.4)
            .unwrap()
            .to_cell(crate::h3::Resolution::Fifteen);
        assert!(EdgeModel::new(high.into()).is_none());
    }
}
