//! Web Mercator tile pyramid coordinate math and bounding boxes for PMTiles v3.

use crate::pmtiles::mvt::MercatorPoint;
use h3o::{CellIndex, LatLng};

/// Convert WGS84 (lon, lat) to Web Mercator tile coordinates (x, y) at zoom z
pub fn lon_lat_to_tile_xy(lon: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = (1u32 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor().max(0.0).min(n - 1.0) as u32;

    let lat_clamped = lat.max(-85.05112878).min(85.05112878);
    let lat_rad = lat_clamped.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor()
        .max(0.0)
        .min(n - 1.0) as u32;

    (x, y)
}

/// Convert a normalized MercatorPoint to tile coordinates (x, y) at zoom z using cheap bitshifts
#[inline(always)]
pub fn mercator_to_tile_xy(pt: MercatorPoint, z: u8) -> (u32, u32) {
    let n = (1u32 << z) as f64;
    let tx = (pt.x * n).floor().max(0.0).min(n - 1.0) as u32;
    let ty = (pt.y * n).floor().max(0.0).min(n - 1.0) as u32;
    (tx, ty)
}

/// Compute WGS84 bounding box [min_lon, min_lat, max_lon, max_lat] for tile (z, x, y)
pub fn tile_xy_to_bbox(z: u8, x: u32, y: u32) -> [f64; 4] {
    let n = (1u32 << z) as f64;
    let min_lon = (x as f64) / n * 360.0 - 180.0;
    let max_lon = ((x + 1) as f64) / n * 360.0 - 180.0;

    let lat_rad_max = ((std::f64::consts::PI * (1.0 - 2.0 * (y as f64) / n)).sinh()).atan();
    let lat_rad_min = ((std::f64::consts::PI * (1.0 - 2.0 * ((y + 1) as f64) / n)).sinh()).atan();

    let max_lat = lat_rad_max.to_degrees();
    let min_lat = lat_rad_min.to_degrees();

    [min_lon, min_lat, max_lon, max_lat]
}

/// Determine the range of tile coordinates (min_tx..=max_tx, min_ty..=max_ty)
/// intersected by an H3 cell's boundary vertices and center at zoom level z.
/// Ensures boundary-spanning hexagons are emitted into all overlapping tiles.
pub fn cell_tile_range(center: LatLng, vertices: &[LatLng], z: u8) -> (u32, u32, u32, u32) {
    let (mut min_tx, mut min_ty) = lon_lat_to_tile_xy(center.lng(), center.lat(), z);
    let mut max_tx = min_tx;
    let mut max_ty = min_ty;

    for v in vertices {
        let (tx, ty) = lon_lat_to_tile_xy(v.lng(), v.lat(), z);
        min_tx = min_tx.min(tx);
        max_tx = max_tx.max(tx);
        min_ty = min_ty.min(ty);
        max_ty = max_ty.max(ty);
    }

    (min_tx, max_tx, min_ty, max_ty)
}

/// Determine the tile coordinate range for an H3 cell with precalculated normalized Mercator points
#[inline(always)]
pub fn cell_tile_range_mercator(
    center: MercatorPoint,
    vertices: &[MercatorPoint],
    z: u8,
) -> (u32, u32, u32, u32) {
    let (mut min_tx, mut min_ty) = mercator_to_tile_xy(center, z);
    let mut max_tx = min_tx;
    let mut max_ty = min_ty;

    for &v in vertices {
        let (tx, ty) = mercator_to_tile_xy(v, z);
        min_tx = min_tx.min(tx);
        max_tx = max_tx.max(tx);
        min_ty = min_ty.min(ty);
        max_ty = max_ty.max(ty);
    }

    (min_tx, max_tx, min_ty, max_ty)
}

/// Compute boundary vertices for an H3 cell directly into a stack-allocated array (zero heap allocations)
#[inline(always)]
pub fn cell_boundary_mercator(cell: CellIndex) -> ([MercatorPoint; 8], usize) {
    let mut arr = [MercatorPoint { x: 0.0, y: 0.0 }; 8];
    let mut count = 0;
    for v in cell.boundary().iter() {
        if count < 8 {
            arr[count] = MercatorPoint::from_lat_lng(v.lat(), v.lng());
            count += 1;
        }
    }
    (arr, count)
}

/// Default mapping from H3 resolution to Web Mercator zoom level (scaling ratio ~1.4037)
pub fn h3_res_to_zoom(res: u8) -> u8 {
    match res {
        0 => 0,
        1 => 2,
        2 => 4,
        3 => 5,
        4 => 7,
        5 => 8,
        6 => 10,
        7 => 11,
        8 => 13,
        9 => 14,
        10 => 16,
        11 => 17,
        12 => 19,
        13 => 20,
        14 => 21,
        15 => 23,
        _ => 24,
    }
}

/// Determine the optimal H3 resolution for a given Web Mercator zoom level
pub fn h3_res_for_zoom(zoom: u8) -> u8 {
    match zoom {
        0 | 1 => 0,
        2 | 3 => 1,
        4 => 2,
        5 => 3,
        6 | 7 => 4,
        8 | 9 => 5,
        10 => 6,
        11 | 12 => 7,
        13 => 8,
        14 => 9,
        15 | 16 => 10,
        17 => 11,
        18 | 19 => 12,
        20 => 13,
        21 | 22 => 14,
        _ => 15,
    }
}

/// Alias for `h3_res_for_zoom`
pub use h3_res_for_zoom as zoom_to_h3_res;

/// Map an H3 resolution to the continuous range of Web Mercator zoom levels it covers
pub fn zooms_for_h3_res(res: u8, min_res: u8) -> Vec<u8> {
    let standard_zooms: Vec<u8> = match res {
        0 => vec![0, 1],
        1 => vec![2, 3],
        2 => vec![4],
        3 => vec![5],
        4 => vec![6, 7],
        5 => vec![8, 9],
        6 => vec![10],
        7 => vec![11, 12],
        8 => vec![13],
        9 => vec![14],
        10 => vec![15, 16],
        11 => vec![17],
        12 => vec![18, 19],
        13 => vec![20],
        14 => vec![21, 22],
        _ => vec![23, 24],
    };

    if res == min_res {
        let max_z = standard_zooms.iter().copied().max().unwrap_or(0);
        (0..=max_z).collect()
    } else {
        standard_zooms
    }
}

/// Estimate maximum radius (in WGS84 degrees) of an H3 hexagon at resolution res
pub fn max_hex_radius_deg(res: u8) -> f64 {
    match res {
        0 => 12.5,
        1 => 5.0,
        2 => 1.7,
        3 => 0.65,
        4 => 0.25,
        5 => 0.10,
        6 => 0.04,
        7 => 0.015,
        8 => 0.006,
        9 => 0.0025,
        10 => 0.001,
        11 => 0.0004,
        12 => 0.00015,
        13 => 0.00006,
        14 => 0.000025,
        _ => 0.00001,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lon_lat_to_tile_xy() {
        let (x, y) = lon_lat_to_tile_xy(0.0, 0.0, 0);
        assert_eq!((x, y), (0, 0));

        let (x, y) = lon_lat_to_tile_xy(0.0, 0.0, 1);
        assert_eq!((x, y), (1, 1));
    }

    #[test]
    fn test_tile_xy_to_bbox() {
        let bbox = tile_xy_to_bbox(0, 0, 0);
        assert_eq!(bbox[0], -180.0);
        assert_eq!(bbox[2], 180.0);
        assert!((bbox[1] - (-85.05112878)).abs() < 1e-4);
        assert!((bbox[3] - 85.05112878).abs() < 1e-4);
    }

    #[test]
    fn test_h3_res_zoom_mapping() {
        for res in 0..=15 {
            let z = h3_res_to_zoom(res);
            assert!(z <= 24);
        }
    }
}
