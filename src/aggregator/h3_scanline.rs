//! Exact H3 indexing of a WGS84 point.
use h3o::{LatLng, Resolution};

#[inline(always)]
pub fn cell_at(lat: f64, lon: f64, res: Resolution) -> Option<u64> {
    if !(-90.0..=90.0).contains(&lat) {
        return None;
    }
    LatLng::new(lat, lon)
        .ok()
        .map(|ll| crate::aggregator::multi_horizon::profile::index(ll, res).into())
}
