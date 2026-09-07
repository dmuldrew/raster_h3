use serde::{Deserialize, Serialize};

/// Represents a single sub-pixel sample offset within the pixel unit box [0.0, 1.0] x [0.0, 1.0]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SamplePoint {
    pub dx: f64,     // X offset in [0.0, 1.0] (0.5 is pixel center)
    pub dy: f64,     // Y offset in [0.0, 1.0] (0.5 is pixel center)
    pub weight: f64, // Proportional weight (e.g. 0.2)
}

/// Collection of sub-pixel sample points and weights
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SamplingPattern {
    pub points: Vec<SamplePoint>,
}

impl Default for SamplingPattern {
    fn default() -> Self {
        Self::center()
    }
}

impl SamplingPattern {
    /// 1. Single-point centroid sampling (default: maximum raw throughput)
    pub fn center() -> Self {
        Self {
            points: vec![SamplePoint {
                dx: 0.5,
                dy: 0.5,
                weight: 1.0,
            }],
        }
    }

    /// 2. Rotated Grid Super-Sampling (RGSS / 4-point rotated: optimal anti-aliasing efficiency)
    /// Rotates coordinates by ~26.6 degrees so no two points share the same X or Y axis.
    pub fn rgss() -> Self {
        Self {
            points: vec![
                SamplePoint { dx: 0.375, dy: 0.125, weight: 0.25 },
                SamplePoint { dx: 0.875, dy: 0.375, weight: 0.25 },
                SamplePoint { dx: 0.125, dy: 0.625, weight: 0.25 },
                SamplePoint { dx: 0.625, dy: 0.875, weight: 0.25 },
            ],
        }
    }

    /// 3. 5-point quincunx sampling (center + 4 diagonal corner insets)
    pub fn five_point() -> Self {
        Self {
            points: vec![
                SamplePoint { dx: 0.5, dy: 0.5, weight: 0.2 },
                SamplePoint { dx: 0.2, dy: 0.2, weight: 0.2 },
                SamplePoint { dx: 0.8, dy: 0.2, weight: 0.2 },
                SamplePoint { dx: 0.2, dy: 0.8, weight: 0.2 },
                SamplePoint { dx: 0.8, dy: 0.8, weight: 0.2 },
            ],
        }
    }

    /// 4. Gaussian Center-Weighted 5-point sampling (Point Spread Function / optical sensor modeling)
    /// Center has 50% weight; 4 cross points have 12.5% weight each.
    pub fn gaussian_five_point() -> Self {
        Self {
            points: vec![
                SamplePoint { dx: 0.5, dy: 0.5, weight: 0.50 },
                SamplePoint { dx: 0.2, dy: 0.5, weight: 0.125 },
                SamplePoint { dx: 0.8, dy: 0.5, weight: 0.125 },
                SamplePoint { dx: 0.5, dy: 0.2, weight: 0.125 },
                SamplePoint { dx: 0.5, dy: 0.8, weight: 0.125 },
            ],
        }
    }

    /// 5. 7-point Hexagonal Lattice sampling (inscribed regular hexagon matching H3 geometry)
    pub fn hex_seven_point() -> Self {
        let r = 0.35; // Radius from pixel center
        let w = 1.0 / 7.0;
        let mut points = Vec::with_capacity(7);
        points.push(SamplePoint { dx: 0.5, dy: 0.5, weight: w });

        for i in 0..6 {
            let angle = (i as f64) * std::f64::consts::PI / 3.0;
            let dx = 0.5 + r * angle.cos();
            let dy = 0.5 + r * angle.sin();
            points.push(SamplePoint { dx, dy, weight: w });
        }
        Self { points }
    }

    /// 6. 8-Rooks Stratified sampling (Latin Hypercube sampling: anti-clumping on diagonal edges)
    pub fn eight_rooks() -> Self {
        // Non-attacking rook permutation on 8x8 subgrid
        let rook_cols = [0, 4, 1, 5, 2, 6, 3, 7];
        let w = 1.0 / 8.0;
        let points = (0..8)
            .map(|row| {
                let col = rook_cols[row];
                let dx = (col as f64 + 0.5) / 8.0;
                let dy = (row as f64 + 0.5) / 8.0;
                SamplePoint { dx, dy, weight: w }
            })
            .collect();
        Self { points }
    }

    /// 7. 9-point regular 3x3 grid sampling
    pub fn nine_point() -> Self {
        let mut points = Vec::with_capacity(9);
        let offsets = [0.2, 0.5, 0.8];
        let w = 1.0 / 9.0;
        for &dy in &offsets {
            for &dx in &offsets {
                points.push(SamplePoint { dx, dy, weight: w });
            }
        }
        Self { points }
    }

    /// 8. 16-point regular 4x4 dense grid sampling (ideal for coarse pixel -> fine H3 cells)
    pub fn sixteen_point() -> Self {
        let mut points = Vec::with_capacity(16);
        let offsets = [0.125, 0.375, 0.625, 0.875];
        let w = 1.0 / 16.0;
        for &dy in &offsets {
            for &dx in &offsets {
                points.push(SamplePoint { dx, dy, weight: w });
            }
        }
        Self { points }
    }

    /// Parse pattern from string identifier
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "rgss" | "rotated4" | "4point_rotated" | "rotated" | "4" => Self::rgss(),
            "5point" | "5_point" | "quincunx" | "5" => Self::five_point(),
            "gaussian" | "weighted5" | "psf" => Self::gaussian_five_point(),
            "hex" | "hex7" | "7point" | "7point_hex" | "7" => Self::hex_seven_point(),
            "8rooks" | "rooks8" | "stratified8" | "8" => Self::eight_rooks(),
            "9point" | "9_point" | "3x3" | "grid3x3" | "9" => Self::nine_point(),
            "16point" | "16_point" | "4x4" | "grid4x4" | "16" => Self::sixteen_point(),
            _ => Self::center(),
        }
    }

    /// Whether this is single-point center sampling
    #[inline(always)]
    pub fn is_single_point(&self) -> bool {
        self.points.len() == 1
    }

    /// Returns (min_dx, max_dx) across all sample points in pattern
    #[inline(always)]
    pub fn dx_bounds(&self) -> (f64, f64) {
        let mut min_dx = 1.0f64;
        let mut max_dx = 0.0f64;
        for p in &self.points {
            if p.dx < min_dx { min_dx = p.dx; }
            if p.dx > max_dx { max_dx = p.dx; }
        }
        (min_dx, max_dx)
    }

    /// Returns (min_dy, max_dy) across all sample points in pattern
    #[inline(always)]
    pub fn dy_bounds(&self) -> (f64, f64) {
        let mut min_dy = 1.0f64;
        let mut max_dy = 0.0f64;
        for p in &self.points {
            if p.dy < min_dy { min_dy = p.dy; }
            if p.dy > max_dy { max_dy = p.dy; }
        }
        (min_dy, max_dy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_sampling_presets_weights() {
        let presets = [
            ("center", SamplingPattern::center(), 1),
            ("rgss", SamplingPattern::rgss(), 4),
            ("5point", SamplingPattern::five_point(), 5),
            ("gaussian", SamplingPattern::gaussian_five_point(), 5),
            ("hex", SamplingPattern::hex_seven_point(), 7),
            ("8rooks", SamplingPattern::eight_rooks(), 8),
            ("9point", SamplingPattern::nine_point(), 9),
            ("16point", SamplingPattern::sixteen_point(), 16),
        ];

        for (name, pattern, expected_len) in presets {
            assert_eq!(pattern.points.len(), expected_len, "Failed len for {}", name);
            let sum_w: f64 = pattern.points.iter().map(|p| p.weight).sum();
            assert!((sum_w - 1.0).abs() < 1e-9, "Weights did not sum to 1 for {}", name);
            for p in &pattern.points {
                assert!(p.dx >= 0.0 && p.dx <= 1.0, "dx out of bounds for {}", name);
                assert!(p.dy >= 0.0 && p.dy <= 1.0, "dy out of bounds for {}", name);
                assert!(p.weight > 0.0 && p.weight <= 1.0, "weight out of bounds for {}", name);
            }
        }
    }

    #[test]
    fn test_sampling_parser() {
        assert_eq!(SamplingPattern::parse("rgss").points.len(), 4);
        assert_eq!(SamplingPattern::parse("hex").points.len(), 7);
        assert_eq!(SamplingPattern::parse("gaussian").points.len(), 5);
        assert_eq!(SamplingPattern::parse("8rooks").points.len(), 8);
        assert_eq!(SamplingPattern::parse("16point").points.len(), 16);
        assert_eq!(SamplingPattern::parse("unknown_preset").points.len(), 1);
    }
}
