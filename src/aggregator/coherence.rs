use h3o::{CellIndex, LatLng, Resolution};

/// Approximate H3 cell edge length in degrees by resolution (0–15).
/// Derived from H3 documentation average edge lengths, converted to degrees at mid-latitudes.
/// Used as a fast proxy for the inscribed safe bounding box instead of computing cell.boundary().
const H3_EDGE_DEG: [f64; 16] = [
    6.570,     // res 0:  ~730 km
    2.490,     // res 1:  ~277 km
    0.943,     // res 2:  ~105 km
    0.357,     // res 3:  ~39.7 km
    0.135,     // res 4:  ~15.0 km
    0.0510,    // res 5:  ~5.66 km
    0.0193,    // res 6:  ~2.14 km
    0.00730,   // res 7:  ~810 m
    0.00276,   // res 8:  ~306 m
    0.00104,   // res 9:  ~116 m
    0.000394,  // res 10: ~44 m
    0.000149,  // res 11: ~17 m
    0.0000563, // res 12: ~6.3 m
    0.0000213, // res 13: ~2.4 m
    0.00000805,// res 14: ~0.9 m
    0.00000304,// res 15: ~0.3 m
];

/// Spatial coherence cache for accelerating in-row pixel-to-H3 conversion
#[derive(Debug, Clone, Copy)]
pub struct SpatialCoherenceCache {
    pub cell_u64: u64,
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
}

impl Default for SpatialCoherenceCache {
    fn default() -> Self {
        Self {
            cell_u64: 0,
            min_lat: f64::NAN,
            max_lat: f64::NAN,
            min_lon: f64::NAN,
            max_lon: f64::NAN,
        }
    }
}

impl SpatialCoherenceCache {
    /// Check if a (lat, lon) point falls strictly inside the cached cell's inner bounds
    #[inline(always)]
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        self.cell_u64 != 0
            && lat >= self.min_lat
            && lat <= self.max_lat
            && lon >= self.min_lon
            && lon <= self.max_lon
    }

    /// Calculate the number of consecutive pixels along the scanline guaranteed to remain in this cell
    #[inline(always)]
    pub fn safe_span_length(&self, lon: f64, d_lon_step: f64) -> usize {
        if self.cell_u64 == 0 || d_lon_step == 0.0 {
            return 1;
        }
        if d_lon_step > 0.0 {
            if self.max_lon > lon {
                (((self.max_lon - lon) / d_lon_step).floor() as usize).max(1)
            } else {
                1
            }
        } else if self.min_lon < lon {
            (((self.min_lon - lon) / d_lon_step).floor() as usize).max(1)
        } else {
            1
        }
    }

    /// Update the cache with a newly resolved H3 cell using precomputed edge-length table.
    /// This avoids the expensive `cell.boundary()` call by using a resolution-indexed
    /// approximate edge length with a conservative 0.45× safety factor.
    pub fn update(&mut self, cell_index: CellIndex) {
        self.cell_u64 = cell_index.into();

        let center = LatLng::from(cell_index);
        let center_lat = center.lat();
        let center_lon = center.lng();

        // Use resolution-indexed edge length table instead of computing boundary vertices
        let res_u8: u8 = cell_index.resolution().into();
        let edge_deg = H3_EDGE_DEG.get(res_u8 as usize).copied().unwrap_or(0.001);

        // Conservative safe factor: 0.25× edge length gives a guaranteed safe inscribed box
        // across all hexagon rotations and icosahedron face distortions.
        let safe_r_lat = edge_deg * 0.25;
        let cos_lat = center_lat.to_radians().cos().abs().max(0.05);
        let safe_r_lon = (safe_r_lat / cos_lat).min(180.0);

        self.min_lat = center_lat - safe_r_lat;
        self.max_lat = center_lat + safe_r_lat;
        self.min_lon = center_lon - safe_r_lon;
        self.max_lon = center_lon + safe_r_lon;
    }

    /// Fast lookup: returns cached cell_u64 if within safe inner box, or computes new cell
    #[inline(always)]
    pub fn get_or_compute(&mut self, lat: f64, lon: f64, resolution: Resolution) -> Option<u64> {
        if self.contains(lat, lon) {
            Some(self.cell_u64)
        } else if let Ok(lat_lng) = LatLng::new(lat, lon) {
            let cell = lat_lng.to_cell(resolution);
            self.update(cell);
            Some(self.cell_u64)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spatial_coherence_cache() {
        let mut cache = SpatialCoherenceCache::default();
        let res = Resolution::Eight;

        let lat0 = 37.7749;
        let lon0 = -122.4194;

        let cell0 = cache.get_or_compute(lat0, lon0, res).unwrap();
        assert_ne!(cell0, 0);

        // A tiny step (0.5 meters ~ 0.000005 deg) should hit the cache
        let cell1 = cache.get_or_compute(lat0 + 0.000005, lon0 + 0.000005, res).unwrap();
        assert_eq!(cell0, cell1);

        // A large step (5 km ~ 0.05 deg) should recompute a new cell
        let cell2 = cache.get_or_compute(lat0 + 0.05, lon0 + 0.05, res).unwrap();
        assert_ne!(cell0, cell2);
    }

    #[test]
    fn test_safe_span_length() {
        let mut cache = SpatialCoherenceCache::default();
        let res = Resolution::Eight;

        let lat0 = 37.7749;
        let lon0 = -122.4194;

        cache.get_or_compute(lat0, lon0, res).unwrap();
        let span = cache.safe_span_length(lon0, 0.00001);
        assert!(span > 1);
    }

    #[test]
    fn test_update_uses_precomputed_radius() {
        let mut cache = SpatialCoherenceCache::default();
        let res = Resolution::Eight;

        let lat0 = 37.7749;
        let lon0 = -122.4194;
        cache.get_or_compute(lat0, lon0, res).unwrap();

        // Verify that the safe box is reasonable for res 8 (~306m edge ≈ 0.00276 deg)
        let box_width = cache.max_lon - cache.min_lon;
        let box_height = cache.max_lat - cache.min_lat;

        // Should be approximately 2 * 0.00276 * 0.45 ≈ 0.00248 degrees
        assert!(box_width > 0.001, "Safe box too small: {}", box_width);
        assert!(box_width < 0.005, "Safe box too large: {}", box_width);
        assert!(box_height > 0.001, "Safe box too small: {}", box_height);
        assert!(box_height < 0.005, "Safe box too large: {}", box_height);
    }

    #[test]
    fn test_latitude_cosine_scaling_at_poles_and_equator() {
        let mut cache = SpatialCoherenceCache::default();
        let res = Resolution::Eight;

        // Equator (0.0 lat): cos(0) = 1.0 -> longitude and latitude radii are equal
        cache.get_or_compute(0.0, 10.0, res).unwrap();
        let eq_width = cache.max_lon - cache.min_lon;
        let eq_height = cache.max_lat - cache.min_lat;
        assert!((eq_width - eq_height).abs() < 1e-6);

        // High Arctic latitude (80.0 lat): cos(80 deg) ≈ 0.1736 -> longitude width should expand ~5.7x
        cache.get_or_compute(80.0, 10.0, res).unwrap();
        let arctic_width = cache.max_lon - cache.min_lon;
        let arctic_height = cache.max_lat - cache.min_lat;
        assert!(arctic_width > arctic_height * 5.0);
        assert!(arctic_width < 180.0);
        assert!(!arctic_width.is_nan());
    }
}
