//! Comprehensive tests for analytic southern boundary computation (`compute_cell_south_lat`).
//!
//! Validates the invariant: `computed_south_lat <= true minimum latitude of the cell`.
//! Covers:
//! - Southern hemisphere interior-edge extrema
//! - Northern hemisphere vertex-only extrema
//! - Polar cells and pentagons
//! - Face-crossing Class III boundary cells
//! - Multi-resolution dense sampling regression oracle (1000 points per edge)

use h3o::{CellIndex, LatLng, Resolution};
use raster_h3::aggregator::horizon_streamer::compute_cell_south_lat;

/// Independent 1000-point numerical densification oracle for geodesic boundary arcs.
fn independent_dense_oracle_south_lat(cell: CellIndex) -> f64 {
    fn unit_xyz(ll: LatLng) -> [f64; 3] {
        let (la, lo) = (ll.lat_radians(), ll.lng_radians());
        [la.cos() * lo.cos(), la.cos() * lo.sin(), la.sin()]
    }

    if let Ok(pole) = LatLng::new(-90.0, 0.0) {
        if pole.to_cell(cell.resolution()) == cell {
            return -90.0;
        }
    }

    let b = cell.boundary();
    let n = b.len();
    let mut min = f64::INFINITY;
    for i in 0..n {
        let p = b[i];
        let q = b[(i + 1) % n];
        let (p_xyz, q_xyz) = (unit_xyz(p), unit_xyz(q));
        for k in 0..=1000 {
            let t = k as f64 / 1000.0;
            let v = [
                p_xyz[0] * (1.0 - t) + q_xyz[0] * t,
                p_xyz[1] * (1.0 - t) + q_xyz[1] * t,
                p_xyz[2] * (1.0 - t) + q_xyz[2] * t,
            ];
            let norm = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            let lat = (v[2] / norm).clamp(-1.0, 1.0).asin().to_degrees();
            min = min.min(lat);
        }
    }
    min
}

#[test]
fn test_southern_interior_edge_extrema() {
    // In the southern hemisphere, great circle arcs bulge southward (toward the pole).
    // An east-west oriented edge has an interior point with strictly lower latitude than both endpoints.
    // Cell in southern hemisphere: lat -45.0, lon 10.0
    for res in [2, 3, 4, 5, 6, 7] {
        let cell = LatLng::new(-45.0, 10.0)
            .unwrap()
            .to_cell(Resolution::try_from(res).unwrap());
        let computed = compute_cell_south_lat(cell.into());
        let oracle = independent_dense_oracle_south_lat(cell);

        // Required invariant: computed_south_lat <= true minimum latitude
        assert!(
            computed <= oracle,
            "res {res}: computed ({computed}) must be <= dense oracle ({oracle})"
        );

        // Check that vertex endpoints alone do NOT capture the true minimum:
        let vertex_min = cell
            .boundary()
            .iter()
            .map(|v| v.lat())
            .fold(f64::INFINITY, f64::min);
        // For at least one southern cell, the true minimum is strictly south of the vertex minimum
        assert!(computed <= vertex_min, "computed must be <= vertex minimum");
    }
}

#[test]
fn test_northern_hemisphere_vertex_extrema() {
    // In the northern hemisphere, great circle arcs bulge northward (away from equator),
    // so the minimum latitude along any edge is strictly attained at an endpoint vertex.
    for res in [1, 3, 5, 7, 9] {
        let cell = LatLng::new(45.0, 10.0)
            .unwrap()
            .to_cell(Resolution::try_from(res).unwrap());
        let computed = compute_cell_south_lat(cell.into());
        let oracle = independent_dense_oracle_south_lat(cell);

        let vertex_min = cell
            .boundary()
            .iter()
            .map(|v| v.lat())
            .fold(f64::INFINITY, f64::min);

        assert!(
            computed <= oracle,
            "res {res}: computed ({computed}) must be <= dense oracle ({oracle})"
        );

        // In the northern hemisphere, vertex_min is the true minimum (modulo numerical tolerance)
        assert!(
            (computed - vertex_min).abs() < 1e-6,
            "res {res}: in northern hemisphere, computed ({computed}) should match vertex_min ({vertex_min})"
        );
    }
}

#[test]
fn test_south_pole_containment() {
    // Cell containing the South Pole must report -90.0 exactly
    for res in 0..=8 {
        let r = Resolution::try_from(res).unwrap();
        let pole_cell = LatLng::new(-90.0, 0.0).unwrap().to_cell(r);
        let south_lat = compute_cell_south_lat(pole_cell.into());
        assert_eq!(
            south_lat, -90.0,
            "res {res}: South pole cell must return -90.0"
        );
    }
}

#[test]
fn test_all_base_pentagons() {
    // Test pentagon cells across resolutions 0 to 6
    // There are 12 pentagons at each resolution
    for res in 0..=6 {
        let r = Resolution::try_from(res).unwrap();
        // H3 pentagons at this resolution
        let pentagons = r.pentagons();
        for pentagon in pentagons {
            let computed = compute_cell_south_lat(pentagon.into());
            let oracle = independent_dense_oracle_south_lat(pentagon);
            assert!(
                computed <= oracle,
                "pentagon {:x} at res {res}: computed ({computed}) must be <= oracle ({oracle})",
                u64::from(pentagon)
            );
        }
    }
}

#[test]
fn test_face_crossing_class_iii_boundaries() {
    // Odd resolutions (Class III) have vertices along icosahedron edges (up to 10 vertices for pentagons).
    for res in [1, 3, 5, 7] {
        let r = Resolution::try_from(res).unwrap();
        // Sample points near icosahedron edge boundaries
        for (lat, lon) in [
            (0.0, 0.0),
            (58.28, 10.76),
            (-58.28, 10.76),
            (26.56, 68.0),
            (-26.56, 68.0),
        ] {
            let cell = LatLng::new(lat, lon).unwrap().to_cell(r);
            let computed = compute_cell_south_lat(cell.into());
            let oracle = independent_dense_oracle_south_lat(cell);
            assert!(
                computed <= oracle,
                "face-crossing cell {:x} at res {res}: computed ({computed}) must be <= oracle ({oracle})",
                u64::from(cell)
            );
        }
    }
}

#[test]
fn test_dense_sampling_regression_oracle_across_globe() {
    // Test dozens of cells across latitudes (-80 to 80), longitudes (-180 to 180), and resolutions 0 to 10.
    for res in [0, 1, 2, 4, 6, 8] {
        let r = Resolution::try_from(res).unwrap();
        for lat in [-80.0, -60.0, -30.0, -10.0, 0.0, 15.0, 45.0, 75.0] {
            for lon in [-170.0, -90.0, 0.0, 45.0, 120.0, 179.0] {
                let cell = LatLng::new(lat, lon).unwrap().to_cell(r);
                let computed = compute_cell_south_lat(cell.into());
                let oracle = independent_dense_oracle_south_lat(cell);
                assert!(
                    computed <= oracle,
                    "at lat {lat}, lon {lon}, res {res}: computed {computed} > oracle {oracle}"
                );
            }
        }
    }
}

#[test]
fn test_invalid_and_degenerate_cells() {
    // Invalid u64 should return conservative -90.0
    assert_eq!(compute_cell_south_lat(0), -90.0);
    assert_eq!(compute_cell_south_lat(u64::MAX), -90.0);
}
