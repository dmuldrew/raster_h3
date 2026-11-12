//! WGS84 bounds of raster pixel rectangles, and whether they are certified.

use super::{wrap_lon, CrsTransformer};
use crate::raster::geotransform::GeoTransform;

impl CrsTransformer {
    /// Whether [`Self::transform_rect_bounds`] is a proven enclosure of the
    /// rectangle's image. Identity, Web Mercator and Albers bounds are
    /// analytic, built-in polar stereographic bounds use radial/angular extrema,
    /// and transverse Mercator bounds carry a Lipschitz certificate
    /// (see [`TmCertificate`](super::TmCertificate)); other PROJ bounds come from finitely many
    /// samples, which can miss interior extrema and singularities however
    /// densely they are placed.
    pub fn bounds_certified(&self) -> bool {
        !matches!(
            self,
            Self::Proj4 {
                tm: None,
                polar: None,
                ..
            }
        )
    }

    /// Bounds from [`Self::transform_rect_bounds`] that are safe to use to
    /// *reject* data (bbox pruning, mosaic overlap filtering). Data dropped by
    /// a rejection test never reaches the exact per-sample test, so
    /// uncertified bounds fail open to the whole globe and leave the decision
    /// to that exact test.
    pub fn rejection_bounds(&self, b: [f64; 4]) -> [f64; 4] {
        if self.bounds_certified() {
            b
        } else {
            [-180.0, -90.0, 180.0, 90.0]
        }
    }

    /// Polar stereographic inverse latitude is monotonic in radius, while
    /// longitude is atan2(x, +/-y) plus a constant. Enclose the affine image
    /// in a source-axis rectangle: radius is minimized at the point nearest
    /// the origin, maximized at a corner, and angular extrema occur at
    /// corners unless the rectangle contains the origin. This includes
    /// rotated/sheared rasters conservatively. Only the fixed WGS84 polar
    /// definitions may use this proof (no datum shift, offsets or axis swap).
    fn polar_rect_bounds(
        &self,
        gt: &GeoTransform,
        col: f64,
        row: f64,
        width: f64,
        height: f64,
    ) -> [f64; 4] {
        const GLOBE: [f64; 4] = [-180.0, -90.0, 180.0, 90.0];
        // proj4rs' inverse iteration stops at 1e-10 radians; this slack is
        // larger in degrees and also covers floating-point corner evaluation.
        const SLACK: f64 = 1e-7;
        let (mut xmin, mut xmax, mut ymin, mut ymax) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for (c, r) in [
            (col, row),
            (col + width, row),
            (col, row + height),
            (col + width, row + height),
        ] {
            let (x, y) = gt.pixel_to_coord(c, r);
            if !x.is_finite() || !y.is_finite() {
                return GLOBE;
            }
            xmin = xmin.min(x);
            xmax = xmax.max(x);
            ymin = ymin.min(y);
            ymax = ymax.max(y);
        }
        let closest = (0.0f64.clamp(xmin, xmax), 0.0f64.clamp(ymin, ymax));
        let mut bounds = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for (x, y) in [
            (xmin, ymin),
            (xmin, ymax),
            (xmax, ymin),
            (xmax, ymax),
            closest,
        ] {
            let Ok((lon, lat)) = self.transform_point(x, y) else {
                return GLOBE;
            };
            if !lon.is_finite() || !lat.is_finite() {
                return GLOBE;
            }
            let lon = wrap_lon(lon);
            bounds[0] = bounds[0].min(lon);
            bounds[2] = bounds[2].max(lon);
            bounds[1] = bounds[1].min(lat);
            bounds[3] = bounds[3].max(lat);
        }
        bounds[1] = (bounds[1] - SLACK).max(-90.0);
        bounds[3] = (bounds[3] + SLACK).min(90.0);
        if closest == (0.0, 0.0)
            || bounds[2] - bounds[0] >= 180.0
            || bounds[0] - SLACK <= -180.0
            || bounds[2] + SLACK >= 180.0
        {
            bounds[0] = -180.0;
            bounds[2] = 180.0;
        } else {
            bounds[0] -= SLACK;
            bounds[2] += SLACK;
        }
        bounds
    }

    /// Calculate WGS84 [min_lon, min_lat, max_lon, max_lat] spatial bounds for a raster pixel rectangle.
    ///
    /// For EPSG:4326 and EPSG:3857 (Web Mercator), latitude is affine or monotonic in y,
    /// so the 4 corners are mathematically exact extrema.
    ///
    /// For Albers Conic projections, parallels of latitude are circular arcs centered at the cone apex.
    /// Along each bounding edge, this computes the exact closed-form analytical critical point
    /// minimizing distance to the apex, capturing the true maximum latitude at the central meridian.
    ///
    /// For arbitrary PROJ4 projections, boundary edges are densified with interior samples
    /// and a small latitude headroom is applied. This is an estimate, not a certificate
    /// (see [`Self::bounds_certified`]); use [`Self::rejection_bounds`] before rejecting data.
    ///
    /// If any probe fails to project or yields a non-finite coordinate, the whole globe
    /// is returned.
    pub fn transform_rect_bounds(
        &self,
        gt: &GeoTransform,
        col: f64,
        row: f64,
        width: f64,
        height: f64,
    ) -> [f64; 4] {
        if let Self::Proj4 { tm: Some(tm), .. } = self {
            return self.tm_rect_bounds(tm, gt, col, row, width, height);
        }
        if let Self::Proj4 { polar: Some(_), .. } = self {
            return self.polar_rect_bounds(gt, col, row, width, height);
        }
        let p0 = gt.pixel_to_coord(col, row);
        let p1 = gt.pixel_to_coord(col + width, row);
        let p2 = gt.pixel_to_coord(col + width, row + height);
        let p3 = gt.pixel_to_coord(col, row + height);

        let corners = [p0, p1, p2, p3];

        // Any probe that fails or is non-finite means part of the rectangle is
        // outside the projection's domain or numerically unstable; the
        // surviving probes would then under-report the extent.
        let mut failed = false;
        let mut probe = |x: f64, y: f64| match self.transform_point(x, y) {
            Ok((lon, lat)) if lon.is_finite() && lat.is_finite() => Some((lon, lat)),
            _ => {
                failed = true;
                None
            }
        };

        let mut min_lon = f64::INFINITY;
        let mut min_lat = f64::INFINITY;
        let mut max_lon = f64::NEG_INFINITY;
        let mut max_lat = f64::NEG_INFINITY;

        for &(x, y) in &corners {
            if let Some((lon, lat)) = probe(x, y) {
                let w_lon = wrap_lon(lon);
                min_lon = min_lon.min(w_lon);
                max_lon = max_lon.max(w_lon);
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
            }
        }

        match self {
            Self::Wgs84Identity | Self::WebMercatorFast => {
                if gt.b != 0.0 || gt.d != 0.0 {
                    let edges = [
                        (p0, p1, 16usize),
                        (p1, p2, 16usize),
                        (p2, p3, 16usize),
                        (p3, p0, 16usize),
                    ];
                    for ((x1, y1), (x2, y2), steps) in edges {
                        let dx = x2 - x1;
                        let dy = y2 - y1;
                        for step in 1..steps {
                            let t = step as f64 / steps as f64;
                            let px = x1 + t * dx;
                            let py = y1 + t * dy;
                            if let Some((lon, lat)) = probe(px, py) {
                                let w_lon = wrap_lon(lon);
                                min_lon = min_lon.min(w_lon);
                                max_lon = max_lon.max(w_lon);
                                min_lat = min_lat.min(lat);
                                max_lat = max_lat.max(lat);
                            }
                        }
                    }
                }
            }
            Self::AlbersConic(albers) => {
                // Parallels are circular arcs centered at the cone apex (x_0, y_0 + rho0).
                let apex_x = albers.x_0;
                let apex_y = albers.y_0 + albers.rho0;

                // If the cone apex lies inside the pixel rectangle's bounding box,
                // the chunk contains the pole (North Pole for n >= 0, South Pole for n < 0).
                let min_x = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
                let max_x = corners
                    .iter()
                    .map(|p| p.0)
                    .fold(f64::NEG_INFINITY, f64::max);
                let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
                let max_y = corners
                    .iter()
                    .map(|p| p.1)
                    .fold(f64::NEG_INFINITY, f64::max);
                if apex_x >= min_x && apex_x <= max_x && apex_y >= min_y && apex_y <= max_y {
                    if albers.n >= 0.0 {
                        max_lat = 90.0;
                    } else {
                        min_lat = -90.0;
                    }
                    min_lon = -180.0;
                    max_lon = 180.0;
                }

                let edges = [(p0, p1), (p1, p2), (p2, p3), (p3, p0)];
                for ((x1, y1), (x2, y2)) in edges {
                    let dx = x2 - x1;
                    let dy = y2 - y1;
                    let denom = dx * dx + dy * dy;
                    if denom > 1e-12 {
                        let t = -(dx * (x1 - apex_x) + dy * (y1 - apex_y)) / denom;
                        if t > 0.0 && t < 1.0 {
                            let cx = x1 + t * dx;
                            let cy = y1 + t * dy;
                            if let Some((lon, lat)) = probe(cx, cy) {
                                let w_lon = wrap_lon(lon);
                                min_lon = min_lon.min(w_lon);
                                max_lon = max_lon.max(w_lon);
                                min_lat = min_lat.min(lat);
                                max_lat = max_lat.max(lat);
                            }
                        }
                    }
                }
            }
            Self::Proj4 { .. } => {
                // Densify boundary edges: 32 samples on horizontal edges, 16 on vertical edges
                let edges = [
                    (p0, p1, 32usize),
                    (p1, p2, 16usize),
                    (p2, p3, 32usize),
                    (p3, p0, 16usize),
                ];
                for ((x1, y1), (x2, y2), steps) in edges {
                    let dx = x2 - x1;
                    let dy = y2 - y1;
                    for step in 1..steps {
                        let t = step as f64 / steps as f64;
                        let px = x1 + t * dx;
                        let py = y1 + t * dy;
                        if let Some((lon, lat)) = probe(px, py) {
                            let w_lon = wrap_lon(lon);
                            min_lon = min_lon.min(w_lon);
                            max_lon = max_lon.max(w_lon);
                            min_lat = min_lat.min(lat);
                            max_lat = max_lat.max(lat);
                        }
                    }
                }

                // Densify interior with a 5x5 grid (including exact center at i=3, j=3)
                // to detect interior extrema (e.g., polar stereographic or LAEA projections)
                for i in 1..=5 {
                    let tx = i as f64 / 6.0;
                    for j in 1..=5 {
                        let ty = j as f64 / 6.0;
                        let px = p0.0 * (1.0 - tx) * (1.0 - ty)
                            + p1.0 * tx * (1.0 - ty)
                            + p2.0 * tx * ty
                            + p3.0 * (1.0 - tx) * ty;
                        let py = p0.1 * (1.0 - tx) * (1.0 - ty)
                            + p1.1 * tx * (1.0 - ty)
                            + p2.1 * tx * ty
                            + p3.1 * (1.0 - tx) * ty;
                        if let Some((lon, lat)) = probe(px, py) {
                            let w_lon = wrap_lon(lon);
                            min_lon = min_lon.min(w_lon);
                            max_lon = max_lon.max(w_lon);
                            min_lat = min_lat.min(lat);
                            max_lat = max_lat.max(lat);
                        }
                    }
                }

                // If any probe approaches the pole (within ~55km / 0.5°), clamp to full polar bounds
                if max_lat >= 89.5 {
                    max_lat = 90.0;
                    min_lon = -180.0;
                    max_lon = 180.0;
                }
                if min_lat <= -89.5 {
                    min_lat = -90.0;
                    min_lon = -180.0;
                    max_lon = 180.0;
                }

                // Add 0.0005° (~55m) safety margin to max_lat/min_lat for Proj4 projections
                if max_lat.is_finite() && max_lat < 90.0 {
                    max_lat = (max_lat + 0.0005).min(90.0);
                }
                if min_lat.is_finite() && min_lat > -90.0 {
                    min_lat = (min_lat - 0.0005).max(-90.0);
                }
            }
        }

        // Longitude is affine in pixel space for Wgs84Identity/WebMercatorFast and
        // monotone in polar angle around the cone apex for AlbersConic (when the apex
        // is outside the chunk), so unwrapped corner longitudes detect antimeridian
        // crossings even when wrapped corners span <= 180 degrees.
        if matches!(
            self,
            Self::Wgs84Identity | Self::WebMercatorFast | Self::AlbersConic(_)
        ) {
            let lons: Vec<f64> = corners
                .iter()
                .filter_map(|&(x, y)| self.transform_point(x, y).ok().map(|p| p.0))
                .collect();
            if lons.len() == corners.len() && lons.iter().all(|v| v.is_finite()) {
                let lo = lons.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = lons.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                if lo < -180.0 || hi > 180.0 {
                    (min_lon, max_lon) = (-180.0, 180.0);
                } else if matches!(self, Self::Wgs84Identity | Self::WebMercatorFast) {
                    // Per-sample bbox tests wrap every longitude, which can move
                    // an in-range value by an ulp. Wrapping is monotone, so the
                    // wrapped corners bound the wrapped samples, except that
                    // values within rounding below 180 wrap to -180. An edge at
                    // exactly 180 stays: samples lie strictly inside pixels.
                    let west = wrap_lon(lo);
                    let east = if hi == 180.0 { hi } else { wrap_lon(hi) };
                    (min_lon, max_lon) = if east < west || hi - lo > 180.0 {
                        (-180.0, 180.0)
                    } else {
                        (west, east)
                    };
                }
            }
        }

        // Handle rasters spanning across the antimeridian
        if max_lon - min_lon > 180.0 {
            min_lon = -180.0;
            max_lon = 180.0;
        }

        if failed || max_lat == f64::NEG_INFINITY {
            // Some probe failed to project. Return the whole globe rather than
            // bounds built from the survivors (or a plausible-looking `[0,0,0,0]`):
            // this keeps bbox pruning conservative and sorts the chunk first in the
            // north-to-south horizon order, so no cell can be evicted before the
            // chunk is processed.
            [-180.0, -90.0, 180.0, 90.0]
        } else {
            [min_lon, min_lat, max_lon, max_lat]
        }
    }
}
