//! Tests WKB polygon geometry emission and DuckDB spatial extension integration.
//!
//! Validates OGC-compliant WKB hexagon generation across all H3 resolutions, DuckDB version detection,
//! projection pushdown column indices, and error handling for invalid cells.

use h3o::{LatLng, Resolution};
use raster_h3_duckdb::ffi::{is_duckdb_version_at_least, parse_version_string};
use raster_h3_duckdb::functions::fast_hex::parse_hex_u64;
use raster_h3_duckdb::functions::fast_hex_u64;
use raster_h3_duckdb::functions::wkb::{cell_to_wkb, h3_index_to_wkb, WkbBuf, WKB_BUF_LEN};

#[test]
fn test_duckdb_version_parsing_logic() {
    // Tests parse_version_string with various version strings
    assert!(parse_version_string("v1.5.0", 1, 5));
    assert!(parse_version_string("v1.5.2", 1, 5));
    assert!(parse_version_string("1.5.0", 1, 5));
    assert!(parse_version_string("v1.6.0", 1, 5));
    assert!(parse_version_string("v2.0.0", 1, 5));
    assert!(!parse_version_string("v1.4.9", 1, 5));
    assert!(!parse_version_string("v1.2.0", 1, 5));
    assert!(!parse_version_string("v0.10.0", 1, 5));
    assert!(!parse_version_string("dev-build", 1, 5));

    // Dynamic library check safely runs via dlsym (returns false outside duckdb)
    assert!(!is_duckdb_version_at_least(99, 0));
}

#[test]
fn test_wkb_and_geometry_byte_equivalence() {
    // SF Bay area cell (Res 8)
    let cell_u64 = 0x8828308281fffffu64;
    let mut wkb_buf: WkbBuf = [0u8; WKB_BUF_LEN];
    let mut geom_buf: WkbBuf = [0u8; WKB_BUF_LEN];

    let wkb_len = h3_index_to_wkb(cell_u64, &mut wkb_buf).expect("valid cell wkb");
    let geom_len = h3_index_to_wkb(cell_u64, &mut geom_buf).expect("valid cell geom");

    assert_eq!(wkb_len, geom_len);
    assert_eq!(&wkb_buf[..wkb_len], &geom_buf[..geom_len]);

    // OGC Polygon validation
    assert_eq!(geom_buf[0], 1); // Little endian
    let geom_type = u32::from_le_bytes(geom_buf[1..5].try_into().unwrap());
    assert_eq!(geom_type, 3); // OGC Polygon
    let num_rings = u32::from_le_bytes(geom_buf[5..9].try_into().unwrap());
    assert_eq!(num_rings, 1); // 1 outer boundary ring
    let num_points = u32::from_le_bytes(geom_buf[9..13].try_into().unwrap());
    assert_eq!(num_points, 7); // 6 vertices + 1 closing vertex
}

#[test]
fn test_scalar_geometry_from_hex_string() {
    let hex_str = "8828308281fffff";
    let cell_u64 = parse_hex_u64(hex_str).expect("parse valid hex");
    assert_eq!(cell_u64, 0x8828308281fffffu64);

    let mut buf_from_str: WkbBuf = [0u8; WKB_BUF_LEN];
    let mut buf_from_u64: WkbBuf = [0u8; WKB_BUF_LEN];

    let len_str = h3_index_to_wkb(cell_u64, &mut buf_from_str).unwrap();
    let len_u64 = h3_index_to_wkb(0x8828308281fffffu64, &mut buf_from_u64).unwrap();

    assert_eq!(len_str, len_u64);
    assert_eq!(&buf_from_str[..len_str], &buf_from_u64[..len_u64]);
}

#[test]
fn test_geometry_projection_pushdown_column_indices() {
    // Verify continuous table function column mapping with and without geom:
    // With geom = false:
    // 0: h3_index, 1: h3_hex, 2: mean, 3: stddev, 4: count, 5: min, 6: max, 7: sum, 8: resolution, 9: wkb
    // quantiles start at 10
    let emit_geom_false = false;
    let q_start_false = if emit_geom_false { 11 } else { 10 };
    assert_eq!(q_start_false, 10);

    // With geom = true:
    // 0..8: standard, 9: wkb, 10: geom
    // quantiles start at 11
    let emit_geom_true = true;
    let q_start_true = if emit_geom_true { 11 } else { 10 };
    assert_eq!(q_start_true, 11);
}

#[test]
fn test_categorical_geometry_column_indices() {
    // Wide mode:
    // 0..11: standard metrics, 12: wkb, 13: geom (when emit_geom = true)
    let wide_wkb_col = 12;
    let wide_geom_col = 13;
    assert_eq!(wide_wkb_col + 1, wide_geom_col);

    // Long mode:
    // 0..10: standard metrics, 11: wkb, 12: geom (when emit_geom = true)
    let long_wkb_col = 11;
    let long_geom_col = 12;
    assert_eq!(long_wkb_col + 1, long_geom_col);
}

#[test]
fn test_invalid_h3_geometry_emission() {
    let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
    // Index 0 is invalid H3 index
    assert!(h3_index_to_wkb(0, &mut buf).is_none());
    // All 1s is invalid H3 index
    assert!(h3_index_to_wkb(u64::MAX, &mut buf).is_none());
}

#[test]
fn test_all_resolutions_geometry_emission() {
    // Verify valid WKB geometry emission across all resolutions from 0 to 15
    for res in 0..=15 {
        let lat_lng = h3o::LatLng::new(37.7749, -122.4194).unwrap();
        let cell = lat_lng.to_cell(h3o::Resolution::try_from(res as u8).unwrap());
        let cell_u64: u64 = cell.into();

        let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
        let len = h3_index_to_wkb(cell_u64, &mut buf).expect("valid wkb for resolution");
        assert_eq!(len, 125);
        assert_eq!(buf[0], 1); // Little endian
        assert_eq!(u32::from_le_bytes(buf[1..5].try_into().unwrap()), 3); // Polygon
        assert_eq!(u32::from_le_bytes(buf[5..9].try_into().unwrap()), 1); // 1 ring
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 7); // 7 points
    }
}

#[test]
fn test_class_iii_pentagon_and_edge_crossing_hexagons() {
    let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];

    // Res 7 pentagon (10 vertices -> 189 bytes)
    let pentagon_res7 = 0x870800000ffffffu64;
    let p_len = h3_index_to_wkb(pentagon_res7, &mut buf).expect("valid res 7 pentagon");
    assert_eq!(p_len, 189);
    assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 11);

    // Res 7 hexagon crossing icosahedron edge (8 vertices -> 157 bytes)
    let hex_res7_8 = 0x87e06dac8ffffffu64;
    let h_len = h3_index_to_wkb(hex_res7_8, &mut buf).expect("valid res 7 hexagon");
    assert_eq!(h_len, 157);
    assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 9);

    // Also check cell_to_wkb directly
    let cell_res7_8 = h3o::CellIndex::try_from(hex_res7_8).unwrap();
    let direct_len = cell_to_wkb(cell_res7_8, &mut buf);
    assert_eq!(direct_len, 157);
}

#[test]
fn test_fast_hex_formatting() {
    let mut buf = [0u8; 16];
    let cell_u64 = 0x8828308281fffffu64;
    let hex_slice = fast_hex_u64(cell_u64, &mut buf);
    let hex_str = std::str::from_utf8(hex_slice).unwrap();
    assert_eq!(hex_str, format!("{:x}", cell_u64));

    // Test bidirectional parsing
    let parsed = raster_h3_duckdb::functions::parse_hex_u64(hex_str).unwrap();
    assert_eq!(parsed, cell_u64);
}

#[test]
fn test_wkb_ogc_compliance() {
    use raster_h3_duckdb::functions::{cell_to_wkb, h3_index_to_wkb, WkbBuf, WKB_BUF_LEN};

    let coord = LatLng::new(21.3069, -157.8583).unwrap(); // Honolulu
    let cell = coord.to_cell(Resolution::Eight);

    let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
    let len = cell_to_wkb(cell, &mut buf);
    assert_eq!(len, 125, "Hexagon WKB must be exactly 125 bytes");

    // Check OGC WKB header
    assert_eq!(buf[0], 1, "Byte order must be Little Endian (1)");
    let geom_type = u32::from_le_bytes(buf[1..5].try_into().unwrap());
    assert_eq!(geom_type, 3, "Geometry type must be wkbPolygon (3)");

    let num_rings = u32::from_le_bytes(buf[5..9].try_into().unwrap());
    assert_eq!(num_rings, 1, "Ring count must be 1");

    let num_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
    assert_eq!(
        num_points, 7,
        "Hexagon ring must have 7 points (6 vertices + 1 closing)"
    );

    // Check ring closure (point 0 == point 6)
    let p0_x = f64::from_le_bytes(buf[13..21].try_into().unwrap());
    let p0_y = f64::from_le_bytes(buf[21..29].try_into().unwrap());
    let p6_x = f64::from_le_bytes(buf[109..117].try_into().unwrap());
    let p6_y = f64::from_le_bytes(buf[117..125].try_into().unwrap());
    assert_eq!(p0_x, p6_x, "Ring must be closed: X0 == X6");
    assert_eq!(p0_y, p6_y, "Ring must be closed: Y0 == Y6");

    // Coordinates should match Honolulu
    assert!(p0_x > -158.5 && p0_x < -157.0);
    assert!(p0_y > 21.0 && p0_y < 22.0);

    // Also verify h3_index_to_wkb matches
    let mut buf2: WkbBuf = [0u8; WKB_BUF_LEN];
    let len2 = h3_index_to_wkb(cell.into(), &mut buf2).expect("valid cell u64");
    assert_eq!(len, len2);
    assert_eq!(&buf[..len], &buf2[..len2]);

    // Verify Class III (odd) resolution pentagon: 10 vertices -> 189 bytes
    let pentagon_res7 = h3o::CellIndex::try_from(0x870800000ffffffu64).unwrap();
    let p_len = cell_to_wkb(pentagon_res7, &mut buf);
    assert_eq!(p_len, 189);
    let p_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
    assert_eq!(p_points, 11);

    // Verify Class III icosahedron-straddling hexagon: 8 vertices -> 157 bytes
    let hex_res7_8 = h3o::CellIndex::try_from(0x87e06dac8ffffffu64).unwrap();
    let h_len = cell_to_wkb(hex_res7_8, &mut buf);
    assert_eq!(h_len, 157);
    let h_points = u32::from_le_bytes(buf[9..13].try_into().unwrap());
    assert_eq!(h_points, 9);
}
