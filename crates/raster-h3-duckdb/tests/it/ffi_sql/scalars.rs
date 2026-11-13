// =============================================================================
// 2. Scalar Function Execution & NULL Propagation Tests
// =============================================================================

use std::ptr;

use raster_h3_duckdb::ffi::duckdb_c::*;
use raster_h3_duckdb::functions::scalar::*;

use super::mock_api::{duckdb_validity_set_row_invalid, install_mock_api};
use super::{MockDataChunk, MockVector};

#[test]
fn test_all_scalar_functions_and_null_propagation() {
    install_mock_api();
    let valid_cell: u64 = 0x8828308281fffff; // Res 8 SF
    let invalid_cell: u64 = 0;

    // 1. h3_to_string & string_to_h3 with NULL row at index 1
    let mut chunk_u64 = MockDataChunk::new(1, 4);
    chunk_u64.size = 3;
    // SAFETY: mock vector buffers are 8-aligned `u64` storage with two slots per row and
    // capacity 4, so writing `u64`/`i64` rows 0..3 is in bounds; the validity mask covers
    // that capacity.
    unsafe {
        let data = chunk_u64.vectors[0].data.as_mut_ptr();
        *data.add(0) = valid_cell;
        *data.add(1) = valid_cell;
        *data.add(2) = invalid_cell;
        chunk_u64.vectors[0].has_validity = true;
        duckdb_validity_set_row_invalid(chunk_u64.vectors[0].validity.as_mut_ptr(), 1);
    }

    let mut out_str = MockVector::new(4);
    // SAFETY: the handles point to live mock chunks/vectors whose columns have the types
    // each callback is registered for, and the mock accepts a null `info`.
    unsafe {
        scalar_h3_to_string(
            ptr::null_mut(),
            &mut chunk_u64 as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_str as *mut MockVector as duckdb_vector,
        );
    }
    assert!(out_str.is_valid(0));
    assert!(
        !out_str.is_valid(1),
        "NULL input must propagate to NULL output"
    );
    assert_eq!(
        std::str::from_utf8(&out_str.strings[0]).unwrap(),
        "8828308281fffff"
    );

    // 2. h3_to_lat, h3_to_lng, h3_get_resolution, h3_is_valid(u64), h3_to_wkb(u64), h3_to_geometry(u64)
    let mut out_lat = MockVector::new(4);
    let mut out_lng = MockVector::new(4);
    let mut out_res = MockVector::new(4);
    let mut out_valid = MockVector::new(4);
    let mut out_wkb = MockVector::new(4);
    let mut out_geom = MockVector::new(4);
    let mut out_cell_geom = MockVector::new(4);
    // SAFETY: the handles point to live mock chunks/vectors whose columns have the types
    // each callback is registered for, and the mock accepts a null `info`.
    unsafe {
        let in_ptr = &mut chunk_u64 as *mut MockDataChunk as duckdb_data_chunk;
        scalar_h3_to_lat(
            ptr::null_mut(),
            in_ptr,
            &mut out_lat as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_lng(
            ptr::null_mut(),
            in_ptr,
            &mut out_lng as *mut MockVector as duckdb_vector,
        );
        scalar_h3_get_resolution(
            ptr::null_mut(),
            in_ptr,
            &mut out_res as *mut MockVector as duckdb_vector,
        );
        scalar_h3_is_valid_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_valid as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_wkb_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_wkb as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_geometry_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_geom as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_geometry_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_cell_geom as *mut MockVector as duckdb_vector,
        );
    }
    // SAFETY: the callbacks above wrote values of these columns' declared types for rows
    // `0..size` into 8-aligned buffers with room for at least that many rows.
    unsafe {
        let lats = out_lat.data.as_ptr() as *const f64;
        let lngs = out_lng.data.as_ptr() as *const f64;
        let res = out_res.data.as_ptr() as *const i64;
        let valid = out_valid.data.as_ptr() as *const bool;
        assert!((*lats.add(0) - 37.77).abs() < 0.1);
        assert!((*lats.add(2)).is_nan());
        assert!((*lngs.add(0) - (-122.42)).abs() < 0.1);
        assert!((*lngs.add(2)).is_nan());
        assert_eq!(*res.add(0), 8);
        assert_eq!(*res.add(2), -1);
        assert!(*valid.add(0));
        assert!(!*valid.add(2));
        assert_eq!(out_wkb.strings[0].len(), 125);
        assert!(
            !out_wkb.is_valid(2),
            "Invalid H3 cell must produce NULL WKB"
        );
        assert_eq!(out_geom.strings[0], out_wkb.strings[0]);
        assert_eq!(out_cell_geom.strings[0], out_geom.strings[0]);
        assert!(!out_cell_geom.is_valid(2));
    }

    // 3. String scalar functions: string_to_h3, h3_is_valid(VARCHAR), h3_to_wkb(VARCHAR), h3_to_geometry(VARCHAR)
    let mut chunk_str = MockDataChunk::new(1, 4);
    chunk_str.size = 2;
    chunk_str.vectors[0].set_str_input(0, "8828308281fffff");
    chunk_str.vectors[0].set_str_input(1, "invalid_hex");

    let mut out_u64 = MockVector::new(4);
    let mut out_valid_s = MockVector::new(4);
    let mut out_wkb_s = MockVector::new(4);
    let mut out_geom_s = MockVector::new(4);
    let mut out_cell_geom_s = MockVector::new(4);
    // SAFETY: the handles point to live mock chunks/vectors whose columns have the types
    // each callback is registered for, and the mock accepts a null `info`.
    unsafe {
        let in_ptr = &mut chunk_str as *mut MockDataChunk as duckdb_data_chunk;
        scalar_string_to_h3(
            ptr::null_mut(),
            in_ptr,
            &mut out_u64 as *mut MockVector as duckdb_vector,
        );
        scalar_h3_is_valid_str(
            ptr::null_mut(),
            in_ptr,
            &mut out_valid_s as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_wkb_str(
            ptr::null_mut(),
            in_ptr,
            &mut out_wkb_s as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_geometry_str(
            ptr::null_mut(),
            in_ptr,
            &mut out_geom_s as *mut MockVector as duckdb_vector,
        );
        scalar_h3_to_geometry_str(
            ptr::null_mut(),
            in_ptr,
            &mut out_cell_geom_s as *mut MockVector as duckdb_vector,
        );
        let parsed = out_u64.data.as_ptr();
        let valid_s = out_valid_s.data.as_ptr() as *const bool;
        assert_eq!(*parsed.add(0), valid_cell);
        assert_eq!(*parsed.add(1), 0);
        assert!(*valid_s.add(0));
        assert!(!*valid_s.add(1));
        assert_eq!(out_wkb_s.strings[0].len(), 125);
        assert!(!out_wkb_s.is_valid(1));
        assert_eq!(out_geom_s.strings[0].len(), 125);
        assert_eq!(out_cell_geom_s.strings[0], out_geom_s.strings[0]);
        assert!(!out_cell_geom_s.is_valid(1));
    }

    // 4. Binary scalar functions: h3_cell_to_parent(u64, i64) and h3_cell_to_parent(VARCHAR, i64)
    let mut chunk_bin_u64 = MockDataChunk::new(2, 4);
    chunk_bin_u64.size = 2;
    // SAFETY: mock vector buffers are 8-aligned `u64` storage with two slots per row and
    // capacity 4, so writing `u64`/`i64` rows 0..3 is in bounds; the validity mask covers
    // that capacity.
    unsafe {
        *chunk_bin_u64.vectors[0].data.as_mut_ptr().add(0) = valid_cell;
        *chunk_bin_u64.vectors[0].data.as_mut_ptr().add(1) = valid_cell;
        *(chunk_bin_u64.vectors[1].data.as_mut_ptr() as *mut i64).add(0) = 7;
        *(chunk_bin_u64.vectors[1].data.as_mut_ptr() as *mut i64).add(1) = 9; // invalid coarser > 8
    }
    let mut out_parent_u64 = MockVector::new(4);
    // SAFETY: the handles point to live mock chunks/vectors whose columns have the types
    // each callback is registered for, and the mock accepts a null `info`.
    unsafe {
        scalar_h3_cell_to_parent_u64(
            ptr::null_mut(),
            &mut chunk_bin_u64 as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_parent_u64 as *mut MockVector as duckdb_vector,
        );
        let p = out_parent_u64.data.as_ptr();
        let expected_parent: u64 = h3o::CellIndex::try_from(valid_cell)
            .unwrap()
            .parent(h3o::Resolution::Seven)
            .unwrap()
            .into();
        assert_eq!(*p.add(0), expected_parent);
        assert_eq!(*p.add(1), 0);
    }

    let mut chunk_bin_str = MockDataChunk::new(2, 4);
    chunk_bin_str.size = 2;
    chunk_bin_str.vectors[0].set_str_input(0, "8828308281fffff");
    chunk_bin_str.vectors[0].set_str_input(1, "8828308281fffff");
    // SAFETY: mock vector buffers are 8-aligned `u64` storage with two slots per row and
    // capacity 4, so writing `u64`/`i64` rows 0..3 is in bounds; the validity mask covers
    // that capacity.
    unsafe {
        *(chunk_bin_str.vectors[1].data.as_mut_ptr() as *mut i64).add(0) = 7;
        *(chunk_bin_str.vectors[1].data.as_mut_ptr() as *mut i64).add(1) = 9;
    }
    let mut out_parent_str = MockVector::new(4);
    // SAFETY: the handles point to live mock chunks/vectors whose columns have the types
    // each callback is registered for, and the mock accepts a null `info`.
    unsafe {
        scalar_h3_cell_to_parent_str(
            ptr::null_mut(),
            &mut chunk_bin_str as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_parent_str as *mut MockVector as duckdb_vector,
        );
        assert!(out_parent_str.is_valid(0));
        assert!(!out_parent_str.is_valid(1));
        assert!(std::str::from_utf8(&out_parent_str.strings[0])
            .unwrap()
            .starts_with("8728308"));
    }

    // 5. A5 scalar functions: a5_lonlat_to_cell, a5_to_string, string_to_a5, a5_to_lat, a5_to_lng,
    //    a5_get_resolution, a5_is_valid, a5_to_wkb, a5_to_geometry, a5_cell_to_parent
    let mut chunk_a5_lonlat = MockDataChunk::new(3, 4);
    chunk_a5_lonlat.size = 3;
    // SAFETY: mock vector buffers are 8-aligned `u64` storage with capacity 4.
    unsafe {
        *(chunk_a5_lonlat.vectors[0].data.as_mut_ptr() as *mut f64).add(0) = -122.4194;
        *(chunk_a5_lonlat.vectors[1].data.as_mut_ptr() as *mut f64).add(0) = 37.7749;
        *(chunk_a5_lonlat.vectors[2].data.as_mut_ptr() as *mut i64).add(0) = 8;
        // Row 1: invalid resolution 31 -> NULL
        *(chunk_a5_lonlat.vectors[0].data.as_mut_ptr() as *mut f64).add(1) = -122.4194;
        *(chunk_a5_lonlat.vectors[1].data.as_mut_ptr() as *mut f64).add(1) = 37.7749;
        *(chunk_a5_lonlat.vectors[2].data.as_mut_ptr() as *mut i64).add(1) = 31;
        // Row 2: NULL input -> NULL
        chunk_a5_lonlat.vectors[0].has_validity = true;
        duckdb_validity_set_row_invalid(chunk_a5_lonlat.vectors[0].validity.as_mut_ptr(), 2);
    }
    let mut out_a5_cell = MockVector::new(4);
    // SAFETY: live mock chunk/vector matching `scalar_a5_lonlat_to_cell` layout.
    let valid_a5_cell = unsafe {
        scalar_a5_lonlat_to_cell(
            ptr::null_mut(),
            &mut chunk_a5_lonlat as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_a5_cell as *mut MockVector as duckdb_vector,
        );
        assert!(out_a5_cell.is_valid(0));
        assert!(!out_a5_cell.is_valid(1));
        assert!(!out_a5_cell.is_valid(2));
        *out_a5_cell.data.as_ptr()
    };
    assert_ne!(valid_a5_cell, 0);

    let mut chunk_a5_u64 = MockDataChunk::new(1, 4);
    chunk_a5_u64.size = 2;
    // SAFETY: mock vector buffer with capacity 4.
    unsafe {
        *chunk_a5_u64.vectors[0].data.as_mut_ptr().add(0) = valid_a5_cell;
        *chunk_a5_u64.vectors[0].data.as_mut_ptr().add(1) = 0u64; // invalid A5 cell
    }
    let mut out_a5_str = MockVector::new(4);
    let mut out_a5_lat = MockVector::new(4);
    let mut out_a5_lng = MockVector::new(4);
    let mut out_a5_res = MockVector::new(4);
    let mut out_a5_valid = MockVector::new(4);
    let mut out_a5_wkb = MockVector::new(4);
    let mut out_a5_geom = MockVector::new(4);
    // SAFETY: live mock chunks/vectors matching registered A5 scalar types.
    unsafe {
        let in_ptr = &mut chunk_a5_u64 as *mut MockDataChunk as duckdb_data_chunk;
        scalar_a5_to_string(
            ptr::null_mut(),
            in_ptr,
            &mut out_a5_str as *mut MockVector as duckdb_vector,
        );
        scalar_a5_to_lat(
            ptr::null_mut(),
            in_ptr,
            &mut out_a5_lat as *mut MockVector as duckdb_vector,
        );
        scalar_a5_to_lng(
            ptr::null_mut(),
            in_ptr,
            &mut out_a5_lng as *mut MockVector as duckdb_vector,
        );
        scalar_a5_get_resolution(
            ptr::null_mut(),
            in_ptr,
            &mut out_a5_res as *mut MockVector as duckdb_vector,
        );
        scalar_a5_is_valid_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_a5_valid as *mut MockVector as duckdb_vector,
        );
        scalar_a5_to_wkb_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_a5_wkb as *mut MockVector as duckdb_vector,
        );
        scalar_a5_to_geometry_u64(
            ptr::null_mut(),
            in_ptr,
            &mut out_a5_geom as *mut MockVector as duckdb_vector,
        );
        let lats = out_a5_lat.data.as_ptr() as *const f64;
        let lngs = out_a5_lng.data.as_ptr() as *const f64;
        let res = out_a5_res.data.as_ptr() as *const i64;
        let valid = out_a5_valid.data.as_ptr() as *const bool;
        assert!((*lats.add(0) - 37.77).abs() < 1.0);
        assert!((*lats.add(1)).is_nan());
        assert!((*lngs.add(0) - (-122.42)).abs() < 1.0);
        assert!((*lngs.add(1)).is_nan());
        assert_eq!(*res.add(0), 8);
        assert_eq!(*res.add(1), -1);
        assert!(*valid.add(0));
        assert!(!*valid.add(1));
        assert_eq!(out_a5_wkb.strings[0].len(), 109); // pentagon WKB (6 vertices = 109 bytes)
        assert!(!out_a5_wkb.is_valid(1));
        assert_eq!(out_a5_geom.strings[0], out_a5_wkb.strings[0]);
    }

    let a5_hex = std::str::from_utf8(&out_a5_str.strings[0])
        .unwrap()
        .to_string();
    let mut chunk_a5_parent = MockDataChunk::new(2, 4);
    chunk_a5_parent.size = 2;
    chunk_a5_parent.vectors[0].set_str_input(0, &a5_hex);
    chunk_a5_parent.vectors[0].set_str_input(1, &a5_hex);
    // SAFETY: mock vector buffer with capacity 4.
    unsafe {
        *(chunk_a5_parent.vectors[1].data.as_mut_ptr() as *mut i64).add(0) = 6;
        *(chunk_a5_parent.vectors[1].data.as_mut_ptr() as *mut i64).add(1) = 9; // invalid coarser > 8
    }
    let mut out_a5_parent_str = MockVector::new(4);
    // SAFETY: live mock chunk/vector matching `scalar_a5_cell_to_parent_str` layout.
    unsafe {
        scalar_a5_cell_to_parent_str(
            ptr::null_mut(),
            &mut chunk_a5_parent as *mut MockDataChunk as duckdb_data_chunk,
            &mut out_a5_parent_str as *mut MockVector as duckdb_vector,
        );
        assert!(out_a5_parent_str.is_valid(0));
        assert!(!out_a5_parent_str.is_valid(1));
    }
}

#[test]
fn test_s2_and_healpix_scalar_functions() {
    use raster_h3_duckdb::grid::GridKind;
    install_mock_api();

    fn run<const G: u8>() {
        let grid = GridKind::from_index(G);
        let expected = grid.cell_at(37.7749, -122.4194, 8).unwrap();

        let mut lonlat = MockDataChunk::new(3, 4);
        lonlat.size = 3;
        // SAFETY: mock vector buffers are 8-aligned `u64` storage with capacity 4.
        unsafe {
            for row in 0..2 {
                *(lonlat.vectors[0].data.as_mut_ptr() as *mut f64).add(row) = -122.4194;
                *(lonlat.vectors[1].data.as_mut_ptr() as *mut f64).add(row) = 37.7749;
            }
            *(lonlat.vectors[2].data.as_mut_ptr() as *mut i64).add(0) = 8;
            // Row 1: a resolution past the grid's range -> NULL. Row 2: NULL input.
            *(lonlat.vectors[2].data.as_mut_ptr() as *mut i64).add(1) = 31;
            lonlat.vectors[0].has_validity = true;
            duckdb_validity_set_row_invalid(lonlat.vectors[0].validity.as_mut_ptr(), 2);
        }
        let mut out_cell = MockVector::new(4);
        // SAFETY: live mock chunk/vector matching `(DOUBLE, DOUBLE, BIGINT) -> UBIGINT`.
        unsafe {
            scalar_grid_lonlat_to_cell::<G>(
                ptr::null_mut(),
                &mut lonlat as *mut MockDataChunk as duckdb_data_chunk,
                &mut out_cell as *mut MockVector as duckdb_vector,
            );
            assert_eq!(*out_cell.data.as_ptr(), expected, "{grid}");
        }
        assert!(out_cell.is_valid(0));
        assert_eq!(out_cell.is_valid(1), grid.max_resolution() >= 31);
        assert!(!out_cell.is_valid(2));

        let mut cells = MockDataChunk::new(1, 4);
        cells.size = 2;
        // SAFETY: mock vector buffer with capacity 4.
        unsafe {
            *cells.vectors[0].data.as_mut_ptr().add(0) = expected;
            *cells.vectors[0].data.as_mut_ptr().add(1) = 0u64; // no grid has cell 0
        }
        let [mut hex, mut lat, mut lng, mut res, mut valid, mut wkb] =
            std::array::from_fn(|_| MockVector::new(4));
        // SAFETY: live mock chunks/vectors matching the registered scalar types.
        unsafe {
            let input = &mut cells as *mut MockDataChunk as duckdb_data_chunk;
            let out = |v: &mut MockVector| v as *mut MockVector as duckdb_vector;
            scalar_h3_to_string(ptr::null_mut(), input, out(&mut hex));
            scalar_grid_to_lat::<G>(ptr::null_mut(), input, out(&mut lat));
            scalar_grid_to_lng::<G>(ptr::null_mut(), input, out(&mut lng));
            scalar_grid_get_resolution::<G>(ptr::null_mut(), input, out(&mut res));
            scalar_grid_is_valid_u64::<G>(ptr::null_mut(), input, out(&mut valid));
            scalar_grid_to_wkb_u64::<G>(ptr::null_mut(), input, out(&mut wkb));

            let lats = lat.data.as_ptr() as *const f64;
            let lngs = lng.data.as_ptr() as *const f64;
            assert!((*lats - 37.7749).abs() < 1.0 && (*lats.add(1)).is_nan());
            assert!((*lngs + 122.4194).abs() < 1.0 && (*lngs.add(1)).is_nan());
            assert_eq!(*(res.data.as_ptr() as *const i64), 8);
            assert_eq!(*(res.data.as_ptr() as *const i64).add(1), -1);
            assert!(*(valid.data.as_ptr() as *const bool));
            assert!(!*(valid.data.as_ptr() as *const bool).add(1));
        }
        assert_eq!(wkb.strings[0].len(), 93); // four corners and the closing point
        assert!(!wkb.is_valid(1));

        // The hex overloads agree with the integer ones.
        let hex = std::str::from_utf8(&hex.strings[0]).unwrap().to_string();
        assert_eq!(hex, format!("{expected:x}"));
        let mut hexes = MockDataChunk::new(2, 4);
        hexes.size = 2;
        hexes.vectors[0].set_str_input(0, &hex);
        hexes.vectors[0].set_str_input(1, &hex);
        // SAFETY: mock vector buffer with capacity 4.
        unsafe {
            *(hexes.vectors[1].data.as_mut_ptr() as *mut i64).add(0) = 6;
            *(hexes.vectors[1].data.as_mut_ptr() as *mut i64).add(1) = 9; // finer than the cell
        }
        let [mut parent, mut valid_str, mut wkb_str] = std::array::from_fn(|_| MockVector::new(4));
        // SAFETY: live mock chunks/vectors matching the registered scalar types.
        unsafe {
            let input = &mut hexes as *mut MockDataChunk as duckdb_data_chunk;
            let out = |v: &mut MockVector| v as *mut MockVector as duckdb_vector;
            scalar_grid_cell_to_parent_str::<G>(ptr::null_mut(), input, out(&mut parent));
            scalar_grid_is_valid_str::<G>(ptr::null_mut(), input, out(&mut valid_str));
            scalar_grid_to_wkb_str::<G>(ptr::null_mut(), input, out(&mut wkb_str));
            assert!(*(valid_str.data.as_ptr() as *const bool));
        }
        assert_eq!(
            std::str::from_utf8(&parent.strings[0]).unwrap(),
            format!("{:x}", grid.parent(expected, 6).unwrap())
        );
        assert!(!parent.is_valid(1));
        assert_eq!(wkb_str.strings[0], wkb.strings[0]);
    }

    run::<{ GridKind::S2 as u8 }>();
    run::<{ GridKind::Healpix as u8 }>();
}
