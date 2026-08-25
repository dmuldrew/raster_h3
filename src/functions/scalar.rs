use h3o::CellIndex;
use std::ffi::c_char;

use crate::ffi::*;
use crate::functions::fast_hex::{fast_hex_u64, parse_hex_u64};

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

/// Register scalar functions with DuckDB
pub unsafe fn register_scalar_functions(con: duckdb_connection) -> Result<(), String> {
    let type_ubigint = duckdb_create_logical_type(DuckDBType::UBigInt);
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);

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

    // Cleanup types
    let mut type_ubigint_mut = type_ubigint;
    duckdb_destroy_logical_type(&mut type_ubigint_mut);
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);

    Ok(())
}
