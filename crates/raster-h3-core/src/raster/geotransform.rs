use serde::{Deserialize, Serialize};

/// Affine Geotransform matrix representation:
/// x = c0 + col * a + row * b
/// y = f0 + col * d + row * e
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoTransform {
    /// Top-left X coordinate (origin x / easting / lon)
    pub c0: f64,
    /// Pixel width (x resolution)
    pub a: f64,
    /// Row rotation term (usually 0.0)
    pub b: f64,
    /// Top-left Y coordinate (origin y / northing / lat)
    pub f0: f64,
    /// Col rotation term (usually 0.0)
    pub d: f64,
    /// Pixel height (y resolution, usually negative for north-up rasters)
    pub e: f64,
}

impl Default for GeoTransform {
    fn default() -> Self {
        Self {
            c0: 0.0,
            a: 1.0,
            b: 0.0,
            f0: 0.0,
            d: 0.0,
            e: -1.0,
        }
    }
}

impl GeoTransform {
    /// Construct from GDAL-style 6-element array: [c0, a, b, f0, d, e]
    pub fn from_gdal_array(gt: [f64; 6]) -> Self {
        Self {
            c0: gt[0],
            a: gt[1],
            b: gt[2],
            f0: gt[3],
            d: gt[4],
            e: gt[5],
        }
    }

    /// Construct from standard GeoTIFF ModelTiepointTag and ModelPixelScaleTag
    pub fn from_tiepoint_and_scale(
        tiepoint: &[f64], // [i, j, k, x, y, z]
        scale: &[f64],    // [scale_x, scale_y, scale_z]
    ) -> Option<Self> {
        if tiepoint.len() < 6 || scale.len() < 2 || scale[0] == 0.0 || scale[1] == 0.0 {
            return None;
        }

        let i = tiepoint[0];
        let j = tiepoint[1];
        let x = tiepoint[3];
        let y = tiepoint[4];
        let sx = scale[0];
        let sy = scale[1];

        // x = x_tie - i * sx + col * sx
        // y = y_tie + j * sy - row * sy
        Some(Self {
            c0: x - i * sx,
            a: sx,
            b: 0.0,
            f0: y + j * sy,
            d: 0.0,
            e: -sy,
        })
    }

    /// Construct from GeoTIFF ModelTransformationTag (4x4 matrix, 16 elements)
    pub fn from_model_transformation(matrix: &[f64]) -> Option<Self> {
        if matrix.len() < 16 {
            return None;
        }
        Some(Self {
            c0: matrix[3],
            a: matrix[0],
            b: matrix[1],
            f0: matrix[7],
            d: matrix[4],
            e: matrix[5],
        })
    }

    /// Map pixel (col, row) directly to spatial coordinates (x, y)
    #[inline(always)]
    pub fn pixel_to_coord(&self, col: f64, row: f64) -> (f64, f64) {
        let x = self.c0 + col * self.a + row * self.b;
        let y = self.f0 + col * self.d + row * self.e;
        (x, y)
    }

    /// Map pixel centroid (col + 0.5, row + 0.5) to spatial coordinates (x, y)
    #[inline(always)]
    pub fn pixel_center_to_coord(&self, col: usize, row: usize) -> (f64, f64) {
        self.pixel_to_coord(col as f64 + 0.5, row as f64 + 0.5)
    }

    /// Whether the affine part is finite and non-singular. The singularity
    /// test is relative to the magnitude of the products forming the
    /// determinant, so it does not depend on coordinate units.
    #[inline]
    pub fn is_invertible(&self) -> bool {
        let det = self.a * self.e - self.b * self.d;
        let scale = (self.a * self.e).abs() + (self.b * self.d).abs();
        det.is_finite() && scale.is_finite() && det.abs() > scale * 1e-12
    }

    /// Whether every coefficient is finite and the affine part is invertible:
    /// the transform maps pixels to distinct, finite coordinates.
    pub fn is_valid(&self) -> bool {
        self.c0.is_finite() && self.f0.is_finite() && self.is_invertible()
    }

    /// Map spatial coordinates (x, y) back to pixel (col, row)
    ///
    /// Returns `None` when the affine part is singular or non-finite. The
    /// singularity test is relative to the magnitude of the products forming
    /// the determinant, so it is independent of coordinate units: a 1e-8
    /// degree geographic grid (|det| = 1e-16) is as invertible as a 10 m one.
    #[inline(always)]
    pub fn coord_to_pixel(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if !self.is_invertible() {
            return None;
        }
        let det = self.a * self.e - self.b * self.d;
        let dx = x - self.c0;
        let dy = y - self.f0;
        let col = (dx * self.e - dy * self.b) / det;
        let row = (dy * self.a - dx * self.d) / det;
        Some((col, row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_geotransform_pixel_to_coord() {
        let gt = GeoTransform {
            c0: 500000.0,
            a: 10.0,
            b: 0.0,
            f0: 4500000.0,
            d: 0.0,
            e: -10.0,
        };

        let (x0, y0) = gt.pixel_to_coord(0.0, 0.0);
        assert_eq!(x0, 500000.0);
        assert_eq!(y0, 4500000.0);

        let (xc, yc) = gt.pixel_center_to_coord(0, 0);
        assert_eq!(xc, 500005.0);
        assert_eq!(yc, 4499995.0);

        let (col, row) = gt.coord_to_pixel(500010.0, 4499990.0).unwrap();
        assert_eq!(col, 1.0);
        assert_eq!(row, 1.0);
    }

    #[test]
    fn test_coord_to_pixel_is_scale_independent() {
        // Geographic grid with 1e-8 degree pixels: |det| = 1e-16, still invertible.
        let tiny = GeoTransform {
            c0: 10.0,
            a: 1e-8,
            b: 0.0,
            f0: 50.0,
            d: 0.0,
            e: -1e-8,
        };
        let (x, y) = tiny.pixel_to_coord(3.25, 7.5);
        let (col, row) = tiny.coord_to_pixel(x, y).expect("tiny grid is invertible");
        assert!(
            (col - 3.25).abs() < 1e-3 && (row - 7.5).abs() < 1e-3,
            "({col}, {row})"
        );

        // Rotated, sheared grid at both geographic and projected scales.
        for s in [1e-9, 1e-4, 1.0, 1e4] {
            let gt = GeoTransform {
                c0: 1000.0 * s,
                a: 3.0 * s,
                b: 1.0 * s,
                f0: -500.0 * s,
                d: 0.5 * s,
                e: -2.0 * s,
            };
            let (x, y) = gt.pixel_to_coord(12.0, 34.0);
            let (col, row) = gt.coord_to_pixel(x, y).expect("invertible at every scale");
            assert!(
                (col - 12.0).abs() < 1e-6 && (row - 34.0).abs() < 1e-6,
                "s={s}"
            );
        }
    }

    #[test]
    fn test_coord_to_pixel_rejects_singular_and_non_finite() {
        let mk = |a: f64, b: f64, d: f64, e: f64| GeoTransform {
            c0: 0.0,
            a,
            b,
            f0: 0.0,
            d,
            e,
        };
        // Exactly collinear axes, at large and small magnitudes.
        assert!(mk(1e6, 2e6, 1e6, 2e6).coord_to_pixel(1.0, 1.0).is_none());
        assert!(mk(1e-9, 2e-9, 1e-9, 2e-9)
            .coord_to_pixel(1.0, 1.0)
            .is_none());
        // All-zero affine.
        assert!(mk(0.0, 0.0, 0.0, 0.0).coord_to_pixel(1.0, 1.0).is_none());
        // Nearly collinear: determinant lost to cancellation.
        assert!(mk(1.0, 1.0, 1.0, 1.0 + 1e-15)
            .coord_to_pixel(1.0, 1.0)
            .is_none());
        // Non-finite coefficients.
        assert!(mk(f64::NAN, 0.0, 0.0, -1.0)
            .coord_to_pixel(1.0, 1.0)
            .is_none());
        assert!(mk(f64::INFINITY, 0.0, 0.0, -1.0)
            .coord_to_pixel(1.0, 1.0)
            .is_none());
    }
}
