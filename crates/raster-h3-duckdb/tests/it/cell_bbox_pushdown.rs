//! H3 cell WGS84 bbox pushdown (`cell_wgs84_bbox`) soundness at high latitudes and the antimeridian.

use crate::helpers;
use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;

#[test]
fn test_h3_cell_and_hex_bbox_pushdown_high_latitude_and_antimeridian() {
    use h3o::{LatLng, Resolution};
    use raster_h3_core::aggregator::multi_horizon::coordinates::is_point_in_bbox;
    use raster_h3_duckdb::functions::bind_utils::cell_wgs84_bbox;

    // Check high-latitude, polar, and antimeridian-straddling cells across resolutions
    let test_points = [
        (0.0, 0.0),
        (37.7749, -122.4194),
        (72.5, -156.0),  // High-latitude Alaska (where lon span >> lat radius)
        (86.0, 45.0),    // Near North Pole
        (-86.0, -120.0), // Near South Pole
        (90.0, 0.0),     // Exact North Pole
        (-90.0, 0.0),    // Exact South Pole
        (-18.0, 180.0),  // Antimeridian crossing (Fiji)
        (52.0, 179.95),  // Antimeridian crossing (Aleutians)
    ];

    for &(lat, lon) in &test_points {
        for r in [0, 1, 2, 5, 8] {
            let res = Resolution::try_from(r).unwrap();
            let cell = LatLng::new(lat, lon).unwrap().to_cell(res);
            let bbox = cell_wgs84_bbox(cell);

            let center: LatLng = cell.into();
            assert!(
                is_point_in_bbox(center.lng(), center.lat(), Some(bbox)),
                "Cell center ({}, {}) not in bbox {:?} for cell {:x}",
                center.lng(),
                center.lat(),
                bbox,
                u64::from(cell)
            );
            for v in cell.boundary().iter() {
                assert!(
                    is_point_in_bbox(v.lng(), v.lat(), Some(bbox)),
                    "Cell vertex ({}, {}) not in bbox {:?} for cell {:x} at lat={lat}, lon={lon}, res={r}",
                    v.lng(),
                    v.lat(),
                    bbox,
                    u64::from(cell)
                );
            }
            // Normalized chord interpolation follows the same great-circle arc.
            // Vertices alone do not exercise latitude extrema inside an edge.
            let boundary = cell.boundary();
            let xyz = |p: h3o::LatLng| {
                let (lat, lon) = (p.lat().to_radians(), p.lng().to_radians());
                [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
            };
            for i in 0..boundary.len() {
                let a = xyz(boundary[i]);
                let b = xyz(boundary[(i + 1) % boundary.len()]);
                for step in 1..16 {
                    let t = step as f64 / 16.0;
                    let v = [
                        a[0] * (1.0 - t) + b[0] * t,
                        a[1] * (1.0 - t) + b[1] * t,
                        a[2] * (1.0 - t) + b[2] * t,
                    ];
                    let lat = v[2].atan2(v[0].hypot(v[1])).to_degrees();
                    let lon = v[1].atan2(v[0]).to_degrees();
                    assert!(
                        is_point_in_bbox(lon, lat, Some(bbox)),
                        "edge sample outside {bbox:?}: {lat}, {lon}"
                    );
                }
            }
        }
    }

    // Confirm the bbox uses the crossing representation.
    let fiji_cell = LatLng::new(-18.0, 180.0).unwrap().to_cell(Resolution::Five);
    let fiji_bbox = cell_wgs84_bbox(fiji_cell);
    assert!(
        fiji_bbox[0] > fiji_bbox[2],
        "Antimeridian cell bbox must have min_lon > max_lon: {fiji_bbox:?}"
    );
}

#[test]
fn cell_bbox_pushdown_preserves_raster_samples() {
    use h3o::{LatLng, Resolution};
    use raster_h3_duckdb::functions::bind_utils::cell_wgs84_bbox;
    for (lat, lon) in [(72.5, -156.0), (-18.0, 180.0)] {
        let cell = LatLng::new(lat, lon).unwrap().to_cell(Resolution::Five);
        let (_file, path) = helpers::TestGeoTiffBuilder::new(200, 200)
            .origin(lon - 0.4, lat + 0.4)
            .pixel_size(0.004)
            .create_f32_tempfile(|c, r| (c + 3 * r + 1) as f32);
        let collect = |bbox| {
            let mut config = MultiResolutionConfig::new(vec![5]);
            config.bbox = bbox;
            let reader = GeoTiffStreamReader::open(&path).unwrap();
            let mut stream = MultiScanHorizonStreamer::new(reader, &config).unwrap();
            let mut matched = Vec::new();
            loop {
                let batch = stream.fetch_next_batch(17).unwrap();
                if batch.is_empty() {
                    break;
                }
                for rec in batch {
                    if rec.h3_index == u64::from(cell) {
                        matched.push(rec.accumulator);
                    }
                }
            }
            assert_eq!(matched.len(), 1);
            matched.pop().unwrap()
        };
        let reference = collect(None);
        let filtered = collect(Some(cell_wgs84_bbox(cell)));
        assert!(reference.count > 0.0);
        assert_eq!(filtered.count, reference.count);
        assert_eq!(filtered.sum, reference.sum);
        assert_eq!(filtered.min, reference.min);
        assert_eq!(filtered.max, reference.max);
    }
}
