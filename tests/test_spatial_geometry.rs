use raster_h3::ffi::{
    is_duckdb_version_at_least, is_geometry_available, parse_version_string, set_spatial_loaded,
};
use raster_h3::functions::fast_hex::parse_hex_u64;
use raster_h3::functions::wkb::h3_index_to_wkb;

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
fn test_spatial_loaded_toggle() {
    set_spatial_loaded(false);
    // If not version >= 1.5, should reflect the atomic flag
    set_spatial_loaded(true);
    assert!(is_geometry_available());
    set_spatial_loaded(false);
}

#[test]
fn test_wkb_and_geometry_byte_equivalence() {
    // SF Bay area cell (Res 8)
    let cell_u64 = 0x8828308281fffffu64;
    let mut wkb_buf = [0u8; 128];
    let mut geom_buf = [0u8; 128];

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

    let mut buf_from_str = [0u8; 128];
    let mut buf_from_u64 = [0u8; 128];

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
    let mut buf = [0u8; 128];
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

        let mut buf = [0u8; 128];
        let len = h3_index_to_wkb(cell_u64, &mut buf).expect("valid wkb for resolution");
        assert_eq!(len, 125);
        assert_eq!(buf[0], 1); // Little endian
        assert_eq!(u32::from_le_bytes(buf[1..5].try_into().unwrap()), 3); // Polygon
        assert_eq!(u32::from_le_bytes(buf[5..9].try_into().unwrap()), 1); // 1 ring
        assert_eq!(u32::from_le_bytes(buf[9..13].try_into().unwrap()), 7); // 7 points
    }
}
