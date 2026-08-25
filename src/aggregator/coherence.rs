use h3o::{CellIndex, LatLng, Resolution};

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

    /// Update the cache with a newly resolved H3 cell and calculate its safe inner bounds
    pub fn update(&mut self, cell_index: CellIndex) {
        self.cell_u64 = cell_index.into();

        // Calculate cell center and approximate inner inscribed bounding box
        let center = LatLng::from(cell_index);
        let center_lat = center.lat();
        let center_lon = center.lng();

        // Find distance to closest vertex to establish safe inradius
        let boundary = cell_index.boundary();
        let mut min_d_lat = f64::INFINITY;
        let mut min_d_lon = f64::INFINITY;

        for vertex in boundary.iter() {
            let d_lat = (vertex.lat() - center_lat).abs();
            let d_lon = (vertex.lng() - center_lon).abs();
            if d_lat > 0.0 && d_lat < min_d_lat {
                min_d_lat = d_lat;
            }
            if d_lon > 0.0 && d_lon < min_d_lon {
                min_d_lon = d_lon;
            }
        }

        // Conservative safe factor (0.75 of minimum distance to edge)
        let safe_lat = if min_d_lat.is_finite() { min_d_lat * 0.75 } else { 0.0001 };
        let safe_lon = if min_d_lon.is_finite() { min_d_lon * 0.75 } else { 0.0001 };

        self.min_lat = center_lat - safe_lat;
        self.max_lat = center_lat + safe_lat;
        self.min_lon = center_lon - safe_lon;
        self.max_lon = center_lon + safe_lon;
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
}
