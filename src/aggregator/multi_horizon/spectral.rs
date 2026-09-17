//! On-the-fly spectral index formula definitions and physical reflectance evaluations.
//!
//! Provides mathematical computation for standard remote sensing indices:
//! - NDVI: Normalized Difference Vegetation Index
//! - NDWI: Normalized Difference Water Index
//! - NBR:  Normalized Burn Ratio
//! - EVI:  Enhanced Vegetation Index

/// Supported on-the-fly spectral index formulas
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpectralFormula {
    /// Normalized Difference Vegetation Index: `(NIR - Red) / (NIR + Red)`.
    Ndvi {
        /// 1-indexed Near-Infrared (NIR) band.
        nir_band: usize,
        /// 1-indexed Red band.
        red_band: usize,
    },
    /// Normalized Difference Water Index: `(Green - NIR) / (Green + NIR)`.
    Ndwi {
        /// 1-indexed Green band.
        green_band: usize,
        /// 1-indexed Near-Infrared (NIR) band.
        nir_band: usize,
    },
    /// Normalized Burn Ratio: `(NIR - SWIR) / (NIR + SWIR)`.
    Nbr {
        /// 1-indexed Near-Infrared (NIR) band.
        nir_band: usize,
        /// 1-indexed Short-Wave Infrared (SWIR) band.
        swir_band: usize,
    },
    /// Enhanced Vegetation Index: `2.5 * (NIR - Red) / (NIR + 6 * Red - 7.5 * Blue + 1)`.
    Evi {
        /// 1-indexed Near-Infrared (NIR) band.
        nir_band: usize,
        /// 1-indexed Red band.
        red_band: usize,
        /// 1-indexed Blue band.
        blue_band: usize,
    },
}

impl SpectralFormula {
    pub fn parse(
        name: &str,
        nir: usize,
        red: usize,
        green: usize,
        blue: usize,
        swir: usize,
    ) -> Option<Self> {
        match name.to_lowercase().trim() {
            "ndvi" => Some(Self::Ndvi {
                nir_band: nir,
                red_band: red,
            }),
            "ndwi" => Some(Self::Ndwi {
                green_band: green,
                nir_band: nir,
            }),
            "nbr" => Some(Self::Nbr {
                nir_band: nir,
                swir_band: swir,
            }),
            "evi" => Some(Self::Evi {
                nir_band: nir,
                red_band: red,
                blue_band: blue,
            }),
            _ => None,
        }
    }

    /// Evaluate the spectral formula for given physical band reflectance values.
    /// Returns None if the denominator is within [-1e-12, 1e-12] (division-by-zero protection).
    pub fn compute(&self, nir: f64, red: f64, green: f64, blue: f64, swir: f64) -> Option<f64> {
        match *self {
            Self::Ndvi { .. } => {
                let denom = nir + red;
                if denom.abs() > 1e-12 {
                    Some((nir - red) / denom)
                } else {
                    None
                }
            }
            Self::Ndwi { .. } => {
                let denom = green + nir;
                if denom.abs() > 1e-12 {
                    Some((green - nir) / denom)
                } else {
                    None
                }
            }
            Self::Nbr { .. } => {
                let denom = nir + swir;
                if denom.abs() > 1e-12 {
                    Some((nir - swir) / denom)
                } else {
                    None
                }
            }
            Self::Evi { .. } => {
                let denom = nir + 6.0 * red - 7.5 * blue + 1.0;
                if denom.abs() > 1e-12 {
                    Some(2.5 * (nir - red) / denom)
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spectral_formula_parsing() {
        assert_eq!(
            SpectralFormula::parse("ndvi", 4, 3, 2, 1, 5),
            Some(SpectralFormula::Ndvi {
                nir_band: 4,
                red_band: 3
            })
        );
        assert_eq!(
            SpectralFormula::parse("  NDVI  ", 4, 3, 2, 1, 5),
            Some(SpectralFormula::Ndvi {
                nir_band: 4,
                red_band: 3
            })
        );
        assert_eq!(
            SpectralFormula::parse("ndwi", 4, 3, 2, 1, 5),
            Some(SpectralFormula::Ndwi {
                green_band: 2,
                nir_band: 4
            })
        );
        assert_eq!(
            SpectralFormula::parse("nbr", 4, 3, 2, 1, 5),
            Some(SpectralFormula::Nbr {
                nir_band: 4,
                swir_band: 5
            })
        );
        assert_eq!(
            SpectralFormula::parse("evi", 4, 3, 2, 1, 5),
            Some(SpectralFormula::Evi {
                nir_band: 4,
                red_band: 3,
                blue_band: 1
            })
        );
        assert_eq!(SpectralFormula::parse("invalid", 4, 3, 2, 1, 5), None);
    }

    #[test]
    fn test_spectral_formula_ndvi_computation_and_edge_cases() {
        let formula = SpectralFormula::Ndvi {
            nir_band: 4,
            red_band: 3,
        };

        // 1. Standard positive vegetation
        let ndvi = formula.compute(0.8, 0.2, 0.0, 0.0, 0.0).unwrap();
        assert!((ndvi - 0.6).abs() < 1e-12);

        // 2. Negative vegetation / water
        let ndvi_water = formula.compute(0.1, 0.5, 0.0, 0.0, 0.0).unwrap();
        assert!((ndvi_water - (-0.4 / 0.6)).abs() < 1e-12);

        // 3. Complete absorption / zero denominator
        assert_eq!(formula.compute(0.0, 0.0, 0.0, 0.0, 0.0), None);

        // 4. Denominator very close to zero (|denom| <= 1e-12)
        assert_eq!(formula.compute(1e-13, -1e-13, 0.0, 0.0, 0.0), None);
    }

    #[test]
    fn test_spectral_formula_ndwi_computation_and_edge_cases() {
        let formula = SpectralFormula::Ndwi {
            green_band: 2,
            nir_band: 4,
        };

        // 1. Standard water body
        let ndwi = formula.compute(0.1, 0.0, 0.4, 0.0, 0.0).unwrap();
        assert!((ndwi - (0.3 / 0.5)).abs() < 1e-12);

        // 2. Dense vegetation (negative NDWI)
        let ndwi_veg = formula.compute(0.8, 0.0, 0.2, 0.0, 0.0).unwrap();
        assert!((ndwi_veg - (-0.6 / 1.0)).abs() < 1e-12);

        // 3. Zero denominator
        assert_eq!(formula.compute(0.0, 0.0, 0.0, 0.0, 0.0), None);
    }

    #[test]
    fn test_spectral_formula_nbr_computation_and_edge_cases() {
        let formula = SpectralFormula::Nbr {
            nir_band: 4,
            swir_band: 5,
        };

        // 1. Healthy forest (high NIR, low SWIR)
        let nbr_healthy = formula.compute(0.7, 0.0, 0.0, 0.0, 0.2).unwrap();
        assert!((nbr_healthy - (0.5 / 0.9)).abs() < 1e-12);

        // 2. Burned scar (low NIR, high SWIR)
        let nbr_burned = formula.compute(0.2, 0.0, 0.0, 0.0, 0.6).unwrap();
        assert!((nbr_burned - (-0.4 / 0.8)).abs() < 1e-12);

        // 3. Zero denominator
        assert_eq!(formula.compute(0.0, 0.0, 0.0, 0.0, 0.0), None);
    }

    #[test]
    fn test_spectral_formula_evi_computation_and_edge_cases() {
        let formula = SpectralFormula::Evi {
            nir_band: 4,
            red_band: 3,
            blue_band: 1,
        };

        // 1. Standard EVI calculation: 2.5 * (NIR - Red) / (NIR + 6*Red - 7.5*Blue + 1)
        // NIR = 0.5, Red = 0.1, Blue = 0.05
        // denom = 0.5 + 0.6 - 0.375 + 1.0 = 1.725
        // numerator = 2.5 * 0.4 = 1.0
        // expected = 1.0 / 1.725
        let evi = formula.compute(0.5, 0.1, 0.0, 0.05, 0.0).unwrap();
        let expected = 1.0 / 1.725;
        assert!((evi - expected).abs() < 1e-12);

        // 2. Zero denominator singularity protection
        // E.g., denom = NIR + 6*Red - 7.5*Blue + 1.0 = 0
        // NIR = -1.0, Red = 0.0, Blue = 0.0 -> denom = -1 + 1 = 0
        assert_eq!(formula.compute(-1.0, 0.0, 0.0, 0.0, 0.0), None);
    }
}
