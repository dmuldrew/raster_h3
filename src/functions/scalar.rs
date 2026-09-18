use h3o::{CellIndex, LatLng, Resolution};
use std::ffi::c_char;

#[cfg(test)]
use crate::encoding::WKB_BUF_LEN;
use crate::encoding::{fast_hex_u64, h3_index_to_wkb, parse_hex_u64, WkbBuf};
use crate::ffi::*;

// =========================================================================
// Generic Zero-Cost Scalar Execution Kernels
// =========================================================================

#[inline(always)]
unsafe fn unary_scalar_kernel<T: Copy, R: Copy, F: Fn(T) -> R>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    if input.is_null() || output.is_null() {
        return;
    }
    let count = duckdb_data_chunk_get_size(input);
    if count == 0 {
        return;
    }
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    if v_in.is_null() {
        return;
    }
    let p_in = duckdb_vector_get_data(v_in) as *const T;
    let p_out = duckdb_vector_get_data(output) as *mut R;
    if p_in.is_null() || p_out.is_null() {
        return;
    }
    let val_in = duckdb_vector_get_validity(v_in);

    for i in 0..count {
        if !duckdb_validity_is_valid(val_in, i) {
            duckdb_vector_set_row_invalid(output, i);
            continue;
        }
        let in_val = *p_in.add(i as usize);
        *p_out.add(i as usize) = op(in_val);
    }
}

#[inline(always)]
unsafe fn unary_str_to_scalar_kernel<R: Copy, F: Fn(&str) -> R>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    if input.is_null() || output.is_null() {
        return;
    }
    let count = duckdb_data_chunk_get_size(input);
    if count == 0 {
        return;
    }
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    if v_in.is_null() {
        return;
    }
    let p_out = duckdb_vector_get_data(output) as *mut R;
    let str_ptr = duckdb_vector_get_data(v_in) as *const duckdb_string_t;
    if p_out.is_null() || str_ptr.is_null() {
        return;
    }
    let val_in = duckdb_vector_get_validity(v_in);

    for i in 0..count {
        if !duckdb_validity_is_valid(val_in, i) {
            duckdb_vector_set_row_invalid(output, i);
            continue;
        }
        let d_str = &*str_ptr.add(i as usize);
        *p_out.add(i as usize) = op(d_str.as_str());
    }
}

#[inline(always)]
unsafe fn unary_to_str_kernel<T: Copy, const N: usize, F: Fn(T, &mut [u8; N]) -> Option<&[u8]>>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    if input.is_null() || output.is_null() {
        return;
    }
    let count = duckdb_data_chunk_get_size(input);
    if count == 0 {
        return;
    }
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    if v_in.is_null() {
        return;
    }
    let p_in = duckdb_vector_get_data(v_in) as *const T;
    if p_in.is_null() {
        return;
    }
    let val_in = duckdb_vector_get_validity(v_in);
    let mut buf = [0u8; N];

    for i in 0..count {
        if !duckdb_validity_is_valid(val_in, i) {
            duckdb_vector_set_row_invalid(output, i);
            continue;
        }
        let in_val = *p_in.add(i as usize);
        if let Some(bytes) = op(in_val, &mut buf) {
            duckdb_vector_assign_string_element_len(
                output,
                i,
                bytes.as_ptr() as *const c_char,
                bytes.len() as idx_t,
            );
        } else {
            duckdb_vector_set_row_invalid(output, i);
        }
    }
}

#[inline(always)]
unsafe fn unary_str_to_str_kernel<const N: usize, F>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) where
    F: for<'a> Fn(&str, &'a mut [u8; N]) -> Option<&'a [u8]>,
{
    if input.is_null() || output.is_null() {
        return;
    }
    let count = duckdb_data_chunk_get_size(input);
    if count == 0 {
        return;
    }
    let v_in = duckdb_data_chunk_get_vector(input, 0);
    if v_in.is_null() {
        return;
    }
    let str_ptr = duckdb_vector_get_data(v_in) as *const duckdb_string_t;
    if str_ptr.is_null() {
        return;
    }
    let val_in = duckdb_vector_get_validity(v_in);
    let mut buf = [0u8; N];

    for i in 0..count {
        if !duckdb_validity_is_valid(val_in, i) {
            duckdb_vector_set_row_invalid(output, i);
            continue;
        }
        let d_str = &*str_ptr.add(i as usize);
        if let Some(bytes) = op(d_str.as_str(), &mut buf) {
            duckdb_vector_assign_string_element_len(
                output,
                i,
                bytes.as_ptr() as *const c_char,
                bytes.len() as idx_t,
            );
        } else {
            duckdb_vector_set_row_invalid(output, i);
        }
    }
}

#[inline(always)]
unsafe fn binary_scalar_kernel<T1: Copy, T2: Copy, R: Copy, F: Fn(T1, T2) -> R>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    if input.is_null() || output.is_null() {
        return;
    }
    let count = duckdb_data_chunk_get_size(input);
    if count == 0 {
        return;
    }
    let v1 = duckdb_data_chunk_get_vector(input, 0);
    let v2 = duckdb_data_chunk_get_vector(input, 1);
    if v1.is_null() || v2.is_null() {
        return;
    }
    let p1 = duckdb_vector_get_data(v1) as *const T1;
    let p2 = duckdb_vector_get_data(v2) as *const T2;
    let p_out = duckdb_vector_get_data(output) as *mut R;
    if p1.is_null() || p2.is_null() || p_out.is_null() {
        return;
    }
    let val1 = duckdb_vector_get_validity(v1);
    let val2 = duckdb_vector_get_validity(v2);

    for i in 0..count {
        if !duckdb_validity_is_valid(val1, i) || !duckdb_validity_is_valid(val2, i) {
            duckdb_vector_set_row_invalid(output, i);
            continue;
        }
        let v1_val = *p1.add(i as usize);
        let v2_val = *p2.add(i as usize);
        *p_out.add(i as usize) = op(v1_val, v2_val);
    }
}

#[inline(always)]
unsafe fn binary_str_scalar_to_str_kernel<T2: Copy, const N: usize, F>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) where
    F: for<'a> Fn(&str, T2, &'a mut [u8; N]) -> Option<&'a [u8]>,
{
    if input.is_null() || output.is_null() {
        return;
    }
    let count = duckdb_data_chunk_get_size(input);
    if count == 0 {
        return;
    }
    let v1 = duckdb_data_chunk_get_vector(input, 0);
    let v2 = duckdb_data_chunk_get_vector(input, 1);
    if v1.is_null() || v2.is_null() {
        return;
    }
    let str_ptr = duckdb_vector_get_data(v1) as *const duckdb_string_t;
    let p2 = duckdb_vector_get_data(v2) as *const T2;
    if str_ptr.is_null() || p2.is_null() {
        return;
    }
    let val1 = duckdb_vector_get_validity(v1);
    let val2 = duckdb_vector_get_validity(v2);
    let mut buf = [0u8; N];

    for i in 0..count {
        if !duckdb_validity_is_valid(val1, i) || !duckdb_validity_is_valid(val2, i) {
            duckdb_vector_set_row_invalid(output, i);
            continue;
        }
        let d_str = &*str_ptr.add(i as usize);
        let v2_val = *p2.add(i as usize);
        if let Some(bytes) = op(d_str.as_str(), v2_val, &mut buf) {
            duckdb_vector_assign_string_element_len(
                output,
                i,
                bytes.as_ptr() as *const c_char,
                bytes.len() as idx_t,
            );
        } else {
            duckdb_vector_set_row_invalid(output, i);
        }
    }
}

// =========================================================================
// Scalar Function Implementations
// =========================================================================

/// Scalar function: h3_to_string(UBIGINT) -> VARCHAR (Zero-allocation)
pub unsafe extern "C" fn scalar_h3_to_string(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_to_str_kernel(input, output, |cell_u64, buf: &mut [u8; 16]| {
            Some(fast_hex_u64(cell_u64, buf))
        });
    });
}

/// Scalar function: string_to_h3(VARCHAR) -> UBIGINT (Zero-allocation hex parsing)
pub unsafe extern "C" fn scalar_string_to_h3(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_str_to_scalar_kernel(input, output, |s| parse_hex_u64(s).unwrap_or(0));
    });
}

/// Scalar function: h3_to_lat(UBIGINT) -> DOUBLE
pub unsafe extern "C" fn scalar_h3_to_lat(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_scalar_kernel(input, output, |cell_u64: u64| {
            CellIndex::try_from(cell_u64)
                .map(|c| LatLng::from(c).lat())
                .unwrap_or(f64::NAN)
        });
    });
}

/// Scalar function: h3_to_lng(UBIGINT) -> DOUBLE
pub unsafe extern "C" fn scalar_h3_to_lng(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_scalar_kernel(input, output, |cell_u64: u64| {
            CellIndex::try_from(cell_u64)
                .map(|c| LatLng::from(c).lng())
                .unwrap_or(f64::NAN)
        });
    });
}

/// Scalar function: h3_get_resolution(UBIGINT) -> BIGINT (1-cycle bitshift)
pub unsafe extern "C" fn scalar_h3_get_resolution(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_scalar_kernel(input, output, |cell_u64: u64| {
            let res = (cell_u64 >> 52) & 0x0F;
            if res <= 15 && cell_u64 != 0 {
                res as i64
            } else {
                -1
            }
        });
    });
}

/// Scalar function: h3_is_valid(UBIGINT) -> BOOLEAN
pub unsafe extern "C" fn scalar_h3_is_valid_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_scalar_kernel(input, output, |cell_u64: u64| {
            CellIndex::try_from(cell_u64).is_ok()
        });
    });
}

/// Scalar function: h3_is_valid(VARCHAR) -> BOOLEAN
pub unsafe extern "C" fn scalar_h3_is_valid_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_str_to_scalar_kernel(input, output, |s| {
            parse_hex_u64(s)
                .map(|u| CellIndex::try_from(u).is_ok())
                .unwrap_or(false)
        });
    });
}

/// Scalar function: h3_to_wkb(UBIGINT) -> BLOB (Zero-allocation WKB polygon encoder)
pub unsafe extern "C" fn scalar_h3_to_wkb_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_to_str_kernel(input, output, |cell_u64: u64, buf: &mut WkbBuf| {
            h3_index_to_wkb(cell_u64, buf).map(|len| &buf[..len])
        });
    });
}

/// Scalar function: h3_to_wkb(VARCHAR) -> BLOB (Zero-allocation WKB polygon encoder)
pub unsafe extern "C" fn scalar_h3_to_wkb_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        unary_str_to_str_kernel(input, output, |s, buf: &mut WkbBuf| {
            parse_hex_u64(s)
                .and_then(|u| h3_index_to_wkb(u, buf))
                .map(|len| &buf[..len])
        });
    });
}

/// Scalar function: h3_to_geometry(UBIGINT) -> GEOMETRY (Zero-allocation WKB polygon encoder)
pub unsafe extern "C" fn scalar_h3_to_geometry_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        scalar_h3_to_wkb_u64(info, input, output);
    });
}

/// Scalar function: h3_to_geometry(VARCHAR) -> GEOMETRY (Zero-allocation WKB polygon encoder)
pub unsafe extern "C" fn scalar_h3_to_geometry_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        scalar_h3_to_wkb_str(info, input, output);
    });
}

/// Scalar function: h3_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT
pub unsafe extern "C" fn scalar_h3_cell_to_parent_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        binary_scalar_kernel(input, output, |cell_u64: u64, parent_res_i64: i64| {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                if (0..=15).contains(&parent_res_i64) {
                    if let Ok(target_res) = Resolution::try_from(parent_res_i64 as u8) {
                        if let Some(parent) = cell.parent(target_res) {
                            return parent.into();
                        }
                    }
                }
            }
            0
        });
    });
}

/// Scalar function: h3_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR
pub unsafe extern "C" fn scalar_h3_cell_to_parent_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    ffi_scalar_guard(info, || {
        binary_str_scalar_to_str_kernel(
            input,
            output,
            |s, parent_res_i64: i64, buf: &mut [u8; 16]| {
                let cell_opt = parse_hex_u64(s).and_then(|u| CellIndex::try_from(u).ok());
                let res_opt = if (0..=15).contains(&parent_res_i64) {
                    Resolution::try_from(parent_res_i64 as u8).ok()
                } else {
                    None
                };
                if let (Some(cell), Some(target_res)) = (cell_opt, res_opt) {
                    if let Some(parent) = cell.parent(target_res) {
                        return Some(fast_hex_u64(parent.into(), buf));
                    }
                }
                None
            },
        );
    });
}

#[inline]
unsafe fn register_unary_scalar_fn(
    con: duckdb_connection,
    name: &str,
    param_type: duckdb_logical_type,
    return_type: duckdb_logical_type,
    func: duckdb_scalar_function_t,
) {
    let scalar_fn = duckdb_create_scalar_function();
    let c_name = to_c_string(name);
    duckdb_scalar_function_set_name(scalar_fn, c_name.as_ptr());
    duckdb_scalar_function_add_parameter(scalar_fn, param_type);
    duckdb_scalar_function_set_return_type(scalar_fn, return_type);
    duckdb_scalar_function_set_function(scalar_fn, func);
    duckdb_register_scalar_function(con, scalar_fn);
    let mut scalar_fn_mut = scalar_fn;
    duckdb_destroy_scalar_function(&mut scalar_fn_mut);
}

#[inline]
unsafe fn register_binary_scalar_fn(
    con: duckdb_connection,
    name: &str,
    param1_type: duckdb_logical_type,
    param2_type: duckdb_logical_type,
    return_type: duckdb_logical_type,
    func: duckdb_scalar_function_t,
) {
    let scalar_fn = duckdb_create_scalar_function();
    let c_name = to_c_string(name);
    duckdb_scalar_function_set_name(scalar_fn, c_name.as_ptr());
    duckdb_scalar_function_add_parameter(scalar_fn, param1_type);
    duckdb_scalar_function_add_parameter(scalar_fn, param2_type);
    duckdb_scalar_function_set_return_type(scalar_fn, return_type);
    duckdb_scalar_function_set_function(scalar_fn, func);
    duckdb_register_scalar_function(con, scalar_fn);
    let mut scalar_fn_mut = scalar_fn;
    duckdb_destroy_scalar_function(&mut scalar_fn_mut);
}

/// Register scalar functions with DuckDB
pub unsafe fn register_scalar_functions(con: duckdb_connection) -> Result<(), String> {
    let type_ubigint = duckdb_create_logical_type(DuckDBType::UBigInt);
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    let type_bool = duckdb_create_logical_type(DuckDBType::Boolean);
    let type_blob = duckdb_create_logical_type(DuckDBType::Blob);
    let type_geom = crate::ffi::create_geometry_logical_type();

    // 1. h3_to_string(UBIGINT) -> VARCHAR
    register_unary_scalar_fn(
        con,
        "h3_to_string",
        type_ubigint,
        type_varchar,
        scalar_h3_to_string,
    );

    // 2. string_to_h3(VARCHAR) -> UBIGINT
    register_unary_scalar_fn(
        con,
        "string_to_h3",
        type_varchar,
        type_ubigint,
        scalar_string_to_h3,
    );

    // 3. h3_to_lat(UBIGINT) -> DOUBLE
    register_unary_scalar_fn(
        con,
        "h3_to_lat",
        type_ubigint,
        type_double,
        scalar_h3_to_lat,
    );

    // 4. h3_to_lng(UBIGINT) -> DOUBLE
    register_unary_scalar_fn(
        con,
        "h3_to_lng",
        type_ubigint,
        type_double,
        scalar_h3_to_lng,
    );

    // 5. h3_get_resolution(UBIGINT) -> BIGINT
    register_unary_scalar_fn(
        con,
        "h3_get_resolution",
        type_ubigint,
        type_bigint,
        scalar_h3_get_resolution,
    );

    // 6. h3_is_valid(UBIGINT) -> BOOLEAN
    register_unary_scalar_fn(
        con,
        "h3_is_valid",
        type_ubigint,
        type_bool,
        scalar_h3_is_valid_u64,
    );

    // 7. h3_is_valid(VARCHAR) -> BOOLEAN
    register_unary_scalar_fn(
        con,
        "h3_is_valid",
        type_varchar,
        type_bool,
        scalar_h3_is_valid_str,
    );

    // 8. h3_to_wkb(UBIGINT) -> BLOB
    register_unary_scalar_fn(
        con,
        "h3_to_wkb",
        type_ubigint,
        type_blob,
        scalar_h3_to_wkb_u64,
    );

    // 9. h3_to_wkb(VARCHAR) -> BLOB
    register_unary_scalar_fn(
        con,
        "h3_to_wkb",
        type_varchar,
        type_blob,
        scalar_h3_to_wkb_str,
    );

    // 10. h3_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT
    register_binary_scalar_fn(
        con,
        "h3_cell_to_parent",
        type_ubigint,
        type_bigint,
        type_ubigint,
        scalar_h3_cell_to_parent_u64,
    );

    // 11. h3_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR
    register_binary_scalar_fn(
        con,
        "h3_cell_to_parent",
        type_varchar,
        type_bigint,
        type_varchar,
        scalar_h3_cell_to_parent_str,
    );

    // 12. h3_to_geometry(UBIGINT) -> GEOMETRY
    register_unary_scalar_fn(
        con,
        "h3_to_geometry",
        type_ubigint,
        type_geom,
        scalar_h3_to_geometry_u64,
    );

    // 13. h3_to_geometry(VARCHAR) -> GEOMETRY
    register_unary_scalar_fn(
        con,
        "h3_to_geometry",
        type_varchar,
        type_geom,
        scalar_h3_to_geometry_str,
    );

    // 14. h3_cell_to_geometry(UBIGINT) -> GEOMETRY (standard alias)
    register_unary_scalar_fn(
        con,
        "h3_cell_to_geometry",
        type_ubigint,
        type_geom,
        scalar_h3_to_geometry_u64,
    );

    // 15. h3_cell_to_geometry(VARCHAR) -> GEOMETRY (standard alias)
    register_unary_scalar_fn(
        con,
        "h3_cell_to_geometry",
        type_varchar,
        type_geom,
        scalar_h3_to_geometry_str,
    );

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
    let mut type_geom_mut = type_geom;
    duckdb_destroy_logical_type(&mut type_geom_mut);

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
        let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
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

    #[test]
    fn test_duckdb_string_t_inlined_and_pointer() {
        use crate::ffi::duckdb_c::*;

        // 1. Empty string (0 bytes)
        let empty_s = duckdb_string_t {
            inlined: DuckDbStringInlined {
                length: 0,
                inlined: [0u8; 12],
            },
        };
        assert_eq!(unsafe { empty_s.as_str() }, "");

        // 2. Short inline string (4 bytes)
        let mut inlined4 = [0u8; 12];
        inlined4[..4].copy_from_slice(b"test");
        let s4 = duckdb_string_t {
            inlined: DuckDbStringInlined {
                length: 4,
                inlined: inlined4,
            },
        };
        assert_eq!(unsafe { s4.as_str() }, "test");

        // 3. Medium inline string (8 bytes) - previously triggered slice bounds check panic!
        let mut inlined8 = [0u8; 12];
        inlined8[..8].copy_from_slice(b"12345678");
        let s8 = duckdb_string_t {
            inlined: DuckDbStringInlined {
                length: 8,
                inlined: inlined8,
            },
        };
        assert_eq!(unsafe { s8.as_str() }, "12345678");

        // 4. Max inline string (12 bytes)
        let mut inlined12 = [0u8; 12];
        inlined12.copy_from_slice(b"123456789012");
        let s12 = duckdb_string_t {
            inlined: DuckDbStringInlined {
                length: 12,
                inlined: inlined12,
            },
        };
        assert_eq!(unsafe { s12.as_str() }, "123456789012");

        // 5. Pointer string (15 bytes, standard H3 hex index string)
        let h3_str = b"8828308281fffff\0";
        let s15 = duckdb_string_t {
            pointer: DuckDbStringPointer {
                length: 15,
                prefix: [h3_str[0], h3_str[1], h3_str[2], h3_str[3]],
                ptr: h3_str.as_ptr() as *const std::os::raw::c_char,
            },
        };
        assert_eq!(unsafe { s15.as_str() }, "8828308281fffff");
    }
}
