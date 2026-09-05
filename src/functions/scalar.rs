use h3o::CellIndex;
use std::ffi::c_char;

use crate::ffi::*;
use crate::functions::fast_hex::{fast_hex_u64, parse_hex_u64};
use crate::functions::wkb::h3_index_to_wkb;

/// Scalar function: h3_to_string(UBIGINT) -> VARCHAR (Zero-allocation)
pub unsafe extern "C" fn scalar_h3_to_string(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_in = duckdb_vector_get_data(v_in) as *const u64;

    let mut hex_buf = [0u8; 16];

    for i in 0..count {
        let cell_u64 = *p_in.add(i as usize);
        let hex_slice = fast_hex_u64(cell_u64, &mut hex_buf);
        duckdb_vector_assign_string_element_len(
            output,
            i,
            hex_slice.as_ptr() as *const c_char,
            hex_slice.len() as idx_t,
        );
    }
}

/// Scalar function: string_to_h3(VARCHAR) -> UBIGINT (Zero-allocation hex parsing)
pub unsafe extern "C" fn scalar_string_to_h3(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_out = duckdb_vector_get_data(output) as *mut u64;

    for i in 0..count {
        // DuckDB string struct layout (16 bytes: length (4), prefix (4), pointer/inline (8))
        let str_ptr = duckdb_vector_get_data(v_in) as *const duckdb_string_t;
        let d_str = &*str_ptr.add(i as usize);

        let s = if d_str.length <= 12 {
            let bytes = &d_str.prefix[..d_str.length as usize];
            std::str::from_utf8(bytes).unwrap_or("")
        } else {
            let ptr = d_str.ptr as *const u8;
            if ptr.is_null() {
                ""
            } else {
                let slice = std::slice::from_raw_parts(ptr, d_str.length as usize);
                std::str::from_utf8(slice).unwrap_or("")
            }
        };

        *p_out.add(i as usize) = parse_hex_u64(s).unwrap_or(0);
    }
}

#[repr(C)]
struct duckdb_string_t {
    length: u32,
    prefix: [u8; 4],
    ptr: *const c_char,
}

/// Scalar function: h3_to_lat(UBIGINT) -> DOUBLE
pub unsafe extern "C" fn scalar_h3_to_lat(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_in = duckdb_vector_get_data(v_in) as *const u64;
    let p_out = duckdb_vector_get_data(output) as *mut f64;

    for i in 0..count {
        let cell_u64 = *p_in.add(i as usize);
        if let Ok(cell) = CellIndex::try_from(cell_u64) {
            let lat_lng = h3o::LatLng::from(cell);
            *p_out.add(i as usize) = lat_lng.lat();
        } else {
            *p_out.add(i as usize) = f64::NAN;
        }
    }
}

/// Scalar function: h3_to_lng(UBIGINT) -> DOUBLE
pub unsafe extern "C" fn scalar_h3_to_lng(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_in = duckdb_vector_get_data(v_in) as *const u64;
    let p_out = duckdb_vector_get_data(output) as *mut f64;

    for i in 0..count {
        let cell_u64 = *p_in.add(i as usize);
        if let Ok(cell) = CellIndex::try_from(cell_u64) {
            let lat_lng = h3o::LatLng::from(cell);
            *p_out.add(i as usize) = lat_lng.lng();
        } else {
            *p_out.add(i as usize) = f64::NAN;
        }
    }
}

/// Scalar function: h3_get_resolution(UBIGINT) -> BIGINT (1-cycle bitshift)
pub unsafe extern "C" fn scalar_h3_get_resolution(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_in = duckdb_vector_get_data(v_in) as *const u64;
    let p_out = duckdb_vector_get_data(output) as *mut i64;

    for i in 0..count {
        let cell_u64 = *p_in.add(i as usize);
        // In H3 index bit architecture, resolution is in bits 52..56
        let res = (cell_u64 >> 52) & 0x0F;
        if res <= 15 && cell_u64 != 0 {
            *p_out.add(i as usize) = res as i64;
        } else {
            *p_out.add(i as usize) = -1;
        }
    }
}

/// Scalar function: h3_is_valid(UBIGINT) -> BOOLEAN
pub unsafe extern "C" fn scalar_h3_is_valid_u64(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_in = duckdb_vector_get_data(v_in) as *const u64;
    let p_out = duckdb_vector_get_data(output) as *mut bool;

    for i in 0..count {
        let cell_u64 = *p_in.add(i as usize);
        *p_out.add(i as usize) = CellIndex::try_from(cell_u64).is_ok();
    }
}

/// Scalar function: h3_is_valid(VARCHAR) -> BOOLEAN
pub unsafe extern "C" fn scalar_h3_is_valid_str(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_out = duckdb_vector_get_data(output) as *mut bool;

    for i in 0..count {
        let str_ptr = duckdb_vector_get_data(v_in) as *const duckdb_string_t;
        let d_str = &*str_ptr.add(i as usize);

        let s = if d_str.length <= 12 {
            let bytes = &d_str.prefix[..d_str.length as usize];
            std::str::from_utf8(bytes).unwrap_or("")
        } else {
            let ptr = d_str.ptr as *const u8;
            if ptr.is_null() {
                ""
            } else {
                let slice = std::slice::from_raw_parts(ptr, d_str.length as usize);
                std::str::from_utf8(slice).unwrap_or("")
            }
        };

        let is_valid = match parse_hex_u64(s) {
            Some(u) => CellIndex::try_from(u).is_ok(),
            None => false,
        };
        *p_out.add(i as usize) = is_valid;
    }
}

/// Scalar function: h3_to_wkb(UBIGINT) -> BLOB (Zero-allocation WKB polygon encoder)
pub unsafe extern "C" fn scalar_h3_to_wkb_u64(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_in = duckdb_vector_get_data(v_in) as *const u64;

    let mut wkb_buf = [0u8; 128];

    for i in 0..count {
        let cell_u64 = *p_in.add(i as usize);
        if let Some(wkb_len) = h3_index_to_wkb(cell_u64, &mut wkb_buf) {
            duckdb_vector_assign_string_element_len(
                output,
                i,
                wkb_buf.as_ptr() as *const c_char,
                wkb_len as idx_t,
            );
        } else {
            duckdb_vector_assign_string_element_len(output, i, std::ptr::null(), 0);
        }
    }
}

/// Scalar function: h3_to_wkb(VARCHAR) -> BLOB (Zero-allocation WKB polygon encoder)
pub unsafe extern "C" fn scalar_h3_to_wkb_str(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);

    let mut wkb_buf = [0u8; 128];

    for i in 0..count {
        let str_ptr = duckdb_vector_get_data(v_in) as *const duckdb_string_t;
        let d_str = &*str_ptr.add(i as usize);

        let s = if d_str.length <= 12 {
            let bytes = &d_str.prefix[..d_str.length as usize];
            std::str::from_utf8(bytes).unwrap_or("")
        } else {
            let ptr = d_str.ptr as *const u8;
            if ptr.is_null() {
                ""
            } else {
                let slice = std::slice::from_raw_parts(ptr, d_str.length as usize);
                std::str::from_utf8(slice).unwrap_or("")
            }
        };

        let cell_u64_opt = parse_hex_u64(s);
        let wkb_len_opt = cell_u64_opt.and_then(|u| h3_index_to_wkb(u, &mut wkb_buf));

        if let Some(wkb_len) = wkb_len_opt {
            duckdb_vector_assign_string_element_len(
                output,
                i,
                wkb_buf.as_ptr() as *const c_char,
                wkb_len as idx_t,
            );
        } else {
            duckdb_vector_assign_string_element_len(output, i, std::ptr::null(), 0);
        }
    }
}

/// Scalar function: h3_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT
pub unsafe extern "C" fn scalar_h3_cell_to_parent_u64(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_cell = duckdb_data_chunk_get_vector(input, 0);
    let v_res = duckdb_data_chunk_get_vector(input, 1);
    let p_cell = duckdb_vector_get_data(v_cell) as *const u64;
    let p_res = duckdb_vector_get_data(v_res) as *const i64;
    let p_out = duckdb_vector_get_data(output) as *mut u64;

    for i in 0..count {
        let cell_u64 = *p_cell.add(i as usize);
        let parent_res_i64 = *p_res.add(i as usize);
        if let Ok(cell) = CellIndex::try_from(cell_u64) {
            if parent_res_i64 >= 0 && parent_res_i64 <= 15 {
                if let Ok(target_res) = h3o::Resolution::try_from(parent_res_i64 as u8) {
                    if let Some(parent) = cell.parent(target_res) {
                        *p_out.add(i as usize) = parent.into();
                        continue;
                    }
                }
            }
        }
        *p_out.add(i as usize) = 0;
    }
}

/// Scalar function: h3_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR
pub unsafe extern "C" fn scalar_h3_cell_to_parent_str(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_cell = duckdb_data_chunk_get_vector(input, 0);
    let v_res = duckdb_data_chunk_get_vector(input, 1);
    let p_res = duckdb_vector_get_data(v_res) as *const i64;

    let mut hex_buf = [0u8; 16];

    for i in 0..count {
        let str_ptr = duckdb_vector_get_data(v_cell) as *const duckdb_string_t;
        let d_str = &*str_ptr.add(i as usize);

        let s = if d_str.length <= 12 {
            let bytes = &d_str.prefix[..d_str.length as usize];
            std::str::from_utf8(bytes).unwrap_or("")
        } else {
            let ptr = d_str.ptr as *const u8;
            if ptr.is_null() {
                ""
            } else {
                let slice = std::slice::from_raw_parts(ptr, d_str.length as usize);
                std::str::from_utf8(slice).unwrap_or("")
            }
        };

        let parent_res_i64 = *p_res.add(i as usize);
        let cell_opt = parse_hex_u64(s).and_then(|u| CellIndex::try_from(u).ok());
        let res_opt = if parent_res_i64 >= 0 && parent_res_i64 <= 15 {
            h3o::Resolution::try_from(parent_res_i64 as u8).ok()
        } else {
            None
        };

        if let (Some(cell), Some(target_res)) = (cell_opt, res_opt) {
            if let Some(parent) = cell.parent(target_res) {
                let hex_slice = fast_hex_u64(parent.into(), &mut hex_buf);
                duckdb_vector_assign_string_element_len(
                    output,
                    i,
                    hex_slice.as_ptr() as *const c_char,
                    hex_slice.len() as idx_t,
                );
                continue;
            }
        }
        duckdb_vector_assign_string_element_len(output, i, std::ptr::null(), 0);
    }
}

/// Register scalar functions with DuckDB
pub unsafe fn register_scalar_functions(con: duckdb_connection) -> Result<(), String> {
    let type_ubigint = duckdb_create_logical_type(DuckDBType::UBigInt);
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    let type_bool = duckdb_create_logical_type(DuckDBType::Boolean);
    let type_blob = duckdb_create_logical_type(DuckDBType::Blob);

    // 1. h3_to_string(UBIGINT) -> VARCHAR
    let fn_str = duckdb_create_scalar_function();
    let name_str = to_c_string("h3_to_string");
    duckdb_scalar_function_set_name(fn_str, name_str.as_ptr());
    duckdb_scalar_function_add_parameter(fn_str, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_str, type_varchar);
    duckdb_scalar_function_set_function(fn_str, scalar_h3_to_string);
    duckdb_register_scalar_function(con, fn_str);
    let mut fn_str_mut = fn_str;
    duckdb_destroy_scalar_function(&mut fn_str_mut);

    // 2. string_to_h3(VARCHAR) -> UBIGINT
    let fn_s2h = duckdb_create_scalar_function();
    let name_s2h = to_c_string("string_to_h3");
    duckdb_scalar_function_set_name(fn_s2h, name_s2h.as_ptr());
    duckdb_scalar_function_add_parameter(fn_s2h, type_varchar);
    duckdb_scalar_function_set_return_type(fn_s2h, type_ubigint);
    duckdb_scalar_function_set_function(fn_s2h, scalar_string_to_h3);
    duckdb_register_scalar_function(con, fn_s2h);
    let mut fn_s2h_mut = fn_s2h;
    duckdb_destroy_scalar_function(&mut fn_s2h_mut);

    // 3. h3_to_lat(UBIGINT) -> DOUBLE
    let fn_lat = duckdb_create_scalar_function();
    let name_lat = to_c_string("h3_to_lat");
    duckdb_scalar_function_set_name(fn_lat, name_lat.as_ptr());
    duckdb_scalar_function_add_parameter(fn_lat, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_lat, type_double);
    duckdb_scalar_function_set_function(fn_lat, scalar_h3_to_lat);
    duckdb_register_scalar_function(con, fn_lat);
    let mut fn_lat_mut = fn_lat;
    duckdb_destroy_scalar_function(&mut fn_lat_mut);

    // 4. h3_to_lng(UBIGINT) -> DOUBLE
    let fn_lng = duckdb_create_scalar_function();
    let name_lng = to_c_string("h3_to_lng");
    duckdb_scalar_function_set_name(fn_lng, name_lng.as_ptr());
    duckdb_scalar_function_add_parameter(fn_lng, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_lng, type_double);
    duckdb_scalar_function_set_function(fn_lng, scalar_h3_to_lng);
    duckdb_register_scalar_function(con, fn_lng);
    let mut fn_lng_mut = fn_lng;
    duckdb_destroy_scalar_function(&mut fn_lng_mut);

    // 5. h3_get_resolution(UBIGINT) -> BIGINT
    let fn_res = duckdb_create_scalar_function();
    let name_res = to_c_string("h3_get_resolution");
    duckdb_scalar_function_set_name(fn_res, name_res.as_ptr());
    duckdb_scalar_function_add_parameter(fn_res, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_res, type_bigint);
    duckdb_scalar_function_set_function(fn_res, scalar_h3_get_resolution);
    duckdb_register_scalar_function(con, fn_res);
    let mut fn_res_mut = fn_res;
    duckdb_destroy_scalar_function(&mut fn_res_mut);

    // 6. h3_is_valid(UBIGINT) -> BOOLEAN
    let fn_valid_u64 = duckdb_create_scalar_function();
    let name_valid = to_c_string("h3_is_valid");
    duckdb_scalar_function_set_name(fn_valid_u64, name_valid.as_ptr());
    duckdb_scalar_function_add_parameter(fn_valid_u64, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_valid_u64, type_bool);
    duckdb_scalar_function_set_function(fn_valid_u64, scalar_h3_is_valid_u64);
    duckdb_register_scalar_function(con, fn_valid_u64);
    let mut fn_valid_u64_mut = fn_valid_u64;
    duckdb_destroy_scalar_function(&mut fn_valid_u64_mut);

    // 7. h3_is_valid(VARCHAR) -> BOOLEAN
    let fn_valid_str = duckdb_create_scalar_function();
    duckdb_scalar_function_set_name(fn_valid_str, name_valid.as_ptr());
    duckdb_scalar_function_add_parameter(fn_valid_str, type_varchar);
    duckdb_scalar_function_set_return_type(fn_valid_str, type_bool);
    duckdb_scalar_function_set_function(fn_valid_str, scalar_h3_is_valid_str);
    duckdb_register_scalar_function(con, fn_valid_str);
    let mut fn_valid_str_mut = fn_valid_str;
    duckdb_destroy_scalar_function(&mut fn_valid_str_mut);

    // 8. h3_to_wkb(UBIGINT) -> BLOB
    let fn_wkb_u64 = duckdb_create_scalar_function();
    let name_wkb = to_c_string("h3_to_wkb");
    duckdb_scalar_function_set_name(fn_wkb_u64, name_wkb.as_ptr());
    duckdb_scalar_function_add_parameter(fn_wkb_u64, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_wkb_u64, type_blob);
    duckdb_scalar_function_set_function(fn_wkb_u64, scalar_h3_to_wkb_u64);
    duckdb_register_scalar_function(con, fn_wkb_u64);
    let mut fn_wkb_u64_mut = fn_wkb_u64;
    duckdb_destroy_scalar_function(&mut fn_wkb_u64_mut);

    // 9. h3_to_wkb(VARCHAR) -> BLOB
    let fn_wkb_str = duckdb_create_scalar_function();
    duckdb_scalar_function_set_name(fn_wkb_str, name_wkb.as_ptr());
    duckdb_scalar_function_add_parameter(fn_wkb_str, type_varchar);
    duckdb_scalar_function_set_return_type(fn_wkb_str, type_blob);
    duckdb_scalar_function_set_function(fn_wkb_str, scalar_h3_to_wkb_str);
    duckdb_register_scalar_function(con, fn_wkb_str);
    let mut fn_wkb_str_mut = fn_wkb_str;
    duckdb_destroy_scalar_function(&mut fn_wkb_str_mut);

    // 10. h3_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT
    let fn_parent_u64 = duckdb_create_scalar_function();
    let name_parent = to_c_string("h3_cell_to_parent");
    duckdb_scalar_function_set_name(fn_parent_u64, name_parent.as_ptr());
    duckdb_scalar_function_add_parameter(fn_parent_u64, type_ubigint);
    duckdb_scalar_function_add_parameter(fn_parent_u64, type_bigint);
    duckdb_scalar_function_set_return_type(fn_parent_u64, type_ubigint);
    duckdb_scalar_function_set_function(fn_parent_u64, scalar_h3_cell_to_parent_u64);
    duckdb_register_scalar_function(con, fn_parent_u64);
    let mut fn_parent_u64_mut = fn_parent_u64;
    duckdb_destroy_scalar_function(&mut fn_parent_u64_mut);

    // 11. h3_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR
    let fn_parent_str = duckdb_create_scalar_function();
    duckdb_scalar_function_set_name(fn_parent_str, name_parent.as_ptr());
    duckdb_scalar_function_add_parameter(fn_parent_str, type_varchar);
    duckdb_scalar_function_add_parameter(fn_parent_str, type_bigint);
    duckdb_scalar_function_set_return_type(fn_parent_str, type_varchar);
    duckdb_scalar_function_set_function(fn_parent_str, scalar_h3_cell_to_parent_str);
    duckdb_register_scalar_function(con, fn_parent_str);
    let mut fn_parent_str_mut = fn_parent_str;
    duckdb_destroy_scalar_function(&mut fn_parent_str_mut);

    // Cleanup types
    let mut type_ubigint_mut = type_ubigint;
    duckdb_destroy_logical_type(&mut type_ubigint_mut);
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_bool_mut = type_bool;
    duckdb_destroy_logical_type(&mut type_bool_mut);
    let mut type_blob_mut = type_blob;
    duckdb_destroy_logical_type(&mut type_blob_mut);


    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_h3_is_valid_logic() {
        // Valid H3 resolution 8 cell (San Francisco)
        let valid_u64 = 0x8828308281fffffu64;
        assert!(CellIndex::try_from(valid_u64).is_ok());

        // Invalid: 0 is never a valid H3 index
        assert!(CellIndex::try_from(0u64).is_err());

        // Invalid: Corrupted reserved bits / mode
        assert!(CellIndex::try_from(0xFFFFFFFFFFFFFFFFu64).is_err());

        // Invalid: Out-of-bounds base cell (122 > 121)
        let invalid_base_cell = 0x88f8308281fffffu64;
        assert!(CellIndex::try_from(invalid_base_cell).is_err());

        // String parsing test
        let valid_str = "8828308281fffff";
        let parsed = parse_hex_u64(valid_str).unwrap();
        assert!(CellIndex::try_from(parsed).is_ok());

        let invalid_str = "not_a_hex_string";
        assert!(parse_hex_u64(invalid_str).is_none());
    }

    #[test]
    fn test_h3_to_wkb_logic() {
        let valid_u64 = 0x8828308281fffffu64;
        let mut buf = [0u8; 128];
        let len = h3_index_to_wkb(valid_u64, &mut buf).expect("valid wkb");
        assert_eq!(len, 125);
        assert_eq!(buf[0], 1); // little endian
        assert_eq!(u32::from_le_bytes(buf[1..5].try_into().unwrap()), 3); // polygon

        assert!(h3_index_to_wkb(0, &mut buf).is_none());
    }

    #[test]
    fn test_h3_cell_to_parent_logic() {
        let valid_u64 = 0x8828308281fffffu64; // Res 8
        let cell = CellIndex::try_from(valid_u64).unwrap();
        assert_eq!(cell.resolution(), h3o::Resolution::Eight);

        let parent_res7 = cell.parent(h3o::Resolution::Seven).unwrap();
        assert_eq!(parent_res7.resolution(), h3o::Resolution::Seven);

        let parent_res6 = cell.parent(h3o::Resolution::Six).unwrap();
        assert_eq!(parent_res6.resolution(), h3o::Resolution::Six);

        // Child cannot have parent at higher resolution
        assert!(cell.parent(h3o::Resolution::Nine).is_none());
    }
}

