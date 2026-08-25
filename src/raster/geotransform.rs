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
        if tiepoint.len() < 6 || scale.len() < 2 {
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

    /// Map spatial coordinates (x, y) back to pixel (col, row)
    #[inline(always)]
    pub fn coord_to_pixel(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let det = self.a * self.e - self.b * self.d;
        if det.abs() < 1e-15 {
            return None;
        }
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
}
