//! Vector-tile cell geometry: every exterior ring has positive area (MVT 2.1
//! §4.3.4.4), dateline cells stay intact, and pole cells are polar caps that
//! reach the map edge instead of rings jumping across the tile.

use h3o::{CellIndex, LatLng, Resolution};
use raster_h3_core::pmtiles::mvt::{MercatorPoint, MvtLayer, MvtValue};
use raster_h3_core::pmtiles::pyramid::{for_each_cell_tile_mercator, tile_xy_to_bbox};
use std::borrow::Cow;

use crate::pmtiles::mvt_decode as mvt_support;

type Props = Vec<(Cow<'static, str>, MvtValue)>;
type Ring = Vec<(i32, i32)>;

/// Surveyor's-formula area (×2) of a ring in tile coordinates (y down).
fn area2(ring: &[(i32, i32)]) -> i64 {
    (0..ring.len())
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            a.0 as i64 * b.1 as i64 - b.0 as i64 * a.1 as i64
        })
        .sum()
}

/// Whether no two non-adjacent edges properly cross.
fn is_simple(ring: &[(i32, i32)]) -> bool {
    let n = ring.len();
    let orient = |a: (i32, i32), b: (i32, i32), c: (i32, i32)| {
        ((b.0 - a.0) as i64 * (c.1 - a.1) as i64 - (b.1 - a.1) as i64 * (c.0 - a.0) as i64).signum()
    };
    for i in 0..n {
        for j in i + 2..n {
            if i == 0 && j == n - 1 {
                continue;
            }
            let (a, b, c, d) = (ring[i], ring[(i + 1) % n], ring[j], ring[(j + 1) % n]);
            if orient(a, b, c) * orient(a, b, d) < 0 && orient(c, d, a) * orient(c, d, b) < 0 {
                return false;
            }
        }
    }
    true
}

fn mercator(cell: CellIndex) -> (MercatorPoint, Vec<MercatorPoint>) {
    let c = LatLng::from(cell);
    let v = cell
        .boundary()
        .iter()
        .map(|p| MercatorPoint::from_lat_lng(p.lat(), p.lng()))
        .collect();
    (MercatorPoint::from_lat_lng(c.lat(), c.lng()), v)
}

/// Encode `cell` into every tile it touches at zoom `z` and decode the rings back.
fn tile_rings(cell: CellIndex, z: u8) -> Vec<((u32, u32), Vec<Ring>)> {
    let (center, verts) = mercator(cell);
    let mut out = Vec::new();
    for_each_cell_tile_mercator(center, &verts, z, |tx, ty| {
        let mut layer = MvtLayer::new("h3");
        layer.add_hexagon_mercator(u64::from(cell), &verts, z, tx, ty, Props::new());
        let features = mvt_support::decode(&layer.encode());
        assert_eq!(features.len(), 1);
        out.push(((tx, ty), features[0].rings.clone()));
    });
    out
}

fn crosses_antimeridian(cell: CellIndex) -> bool {
    let l: Vec<f64> = cell.boundary().iter().map(|v| v.lng()).collect();
    l.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
        - l.iter().cloned().fold(f64::INFINITY, f64::min)
        > 180.0
}

#[test]
fn every_feature_ring_has_positive_area() {
    let mut dateline = 0;
    for res in [Resolution::Zero, Resolution::One, Resolution::Two] {
        for cell in CellIndex::base_cells().flat_map(|c| c.children(res)) {
            let pole = [90.0, -90.0]
                .iter()
                .any(|&lat| LatLng::new(lat, 0.0).unwrap().to_cell(res) == cell);
            dateline += usize::from(crosses_antimeridian(cell) && !pole);
            for z in [0u8, 2] {
                for ((tx, ty), rings) in tile_rings(cell, z) {
                    for ring in &rings {
                        if area2(ring) == 0 {
                            // Cells wholly beyond Web Mercator's ±85.05° clamp flatten.
                            continue;
                        }
                        assert!(area2(ring) > 0, "{cell} z{z}/{tx}/{ty}: negative-area ring");
                        assert!(
                            is_simple(ring),
                            "{cell} z{z}/{tx}/{ty}: self-intersecting ring"
                        );
                    }
                }
            }
        }
    }
    assert!(dateline > 0);
}

#[test]
fn pole_cells_are_caps_reaching_the_map_edge() {
    for r in [0u8, 1] {
        let res = Resolution::try_from(r).unwrap();
        for (lat, edge) in [(90.0, 0), (-90.0, 4096)] {
            let cell = LatLng::new(lat, 0.0).unwrap().to_cell(res);
            let rings = tile_rings(cell, 0);
            assert_eq!(rings.len(), 1, "a pole cell spans the single z0 tile");
            let ring = &rings[0].1[0];
            assert!(
                area2(ring) > 0 && is_simple(ring),
                "res {r} lat {lat}: {ring:?}"
            );
            let xs = ring.iter().map(|p| p.0);
            assert_eq!(xs.clone().min(), Some(0), "cap starts at the west edge");
            assert_eq!(xs.max(), Some(4096), "cap ends at the east edge");
            assert!(
                ring.contains(&(0, edge)) && ring.contains(&(4096, edge)),
                "{ring:?}"
            );
        }
    }
}

#[test]
fn add_hexagon_builds_the_same_polar_cap() {
    let cell = LatLng::new(90.0, 0.0).unwrap().to_cell(Resolution::Zero);
    let verts: Vec<LatLng> = cell.boundary().iter().copied().collect();
    let [w, s, e, n] = tile_xy_to_bbox(0, 0, 0);
    let mut layer = MvtLayer::new("h3");
    layer.add_hexagon(u64::from(cell), &verts, w, e, s, n, Props::new());
    let ring = &mvt_support::decode(&layer.encode())[0].rings[0];
    assert!(area2(ring) > 0 && is_simple(ring), "{ring:?}");
    assert!(
        ring.contains(&(0, 0)) && ring.contains(&(4096, 0)),
        "{ring:?}"
    );
}
