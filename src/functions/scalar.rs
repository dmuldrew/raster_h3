use h3o::CellIndex;
use std::ffi::c_char;

use crate::ffi::*;

/// Scalar function: h3_to_string(UBIGINT) -> VARCHAR
pub unsafe extern "C" fn scalar_h3_to_string(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    let count = duckdb_data_chunk_get_size(input);
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    let p_in = duckdb_vector_get_data(v_in) as *const u64;

    for i in 0..count {
        let cell_u64 = *p_in.add(i as usize);
        let s = format!("{:x}", cell_u64);
        duckdb_vector_assign_string_element_len(
            output,
            i,
            s.as_ptr() as *const c_char,
            s.len() as idx_t,
        );
    }
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

/// Scalar function: h3_get_resolution(UBIGINT) -> BIGINT
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
        if let Ok(cell) = CellIndex::try_from(cell_u64) {
            *p_out.add(i as usize) = u8::from(cell.resolution()) as i64;
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

    // 1. h3_to_string
    let fn_str = duckdb_create_scalar_function();
    let name_str = to_c_string("h3_to_string");
    duckdb_scalar_function_set_name(fn_str, name_str.as_ptr());
    duckdb_scalar_function_add_parameter(fn_str, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_str, type_varchar);
    duckdb_scalar_function_set_function(fn_str, scalar_h3_to_string);
    duckdb_register_scalar_function(con, fn_str);
    let mut fn_str_mut = fn_str;
    duckdb_destroy_scalar_function(&mut fn_str_mut);

    // 2. h3_to_lat
    let fn_lat = duckdb_create_scalar_function();
    let name_lat = to_c_string("h3_to_lat");
    duckdb_scalar_function_set_name(fn_lat, name_lat.as_ptr());
    duckdb_scalar_function_add_parameter(fn_lat, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_lat, type_double);
    duckdb_scalar_function_set_function(fn_lat, scalar_h3_to_lat);
    duckdb_register_scalar_function(con, fn_lat);
    let mut fn_lat_mut = fn_lat;
    duckdb_destroy_scalar_function(&mut fn_lat_mut);

    // 3. h3_to_lng
    let fn_lng = duckdb_create_scalar_function();
    let name_lng = to_c_string("h3_to_lng");
    duckdb_scalar_function_set_name(fn_lng, name_lng.as_ptr());
    duckdb_scalar_function_add_parameter(fn_lng, type_ubigint);
    duckdb_scalar_function_set_return_type(fn_lng, type_double);
    duckdb_scalar_function_set_function(fn_lng, scalar_h3_to_lng);
    duckdb_register_scalar_function(con, fn_lng);
    let mut fn_lng_mut = fn_lng;
    duckdb_destroy_scalar_function(&mut fn_lng_mut);

    // 4. h3_get_resolution
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
