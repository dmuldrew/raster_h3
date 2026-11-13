use h3o::{CellIndex, Resolution};
use std::ffi::c_char;

#[cfg(test)]
use crate::encoding::WKB_BUF_LEN;
use crate::encoding::{
    a5_index_to_wkb, cell_index_to_wkb, fast_hex_u64, h3_index_to_wkb, parse_hex_u64, WkbBuf,
};
use crate::ffi::*;
use crate::grid::GridKind;
use crate::scalar_ops;

// =========================================================================
// Generic Zero-Cost Scalar Execution Kernels
// =========================================================================

/// # Safety
/// `input` must be null or a live, flat DuckDB data chunk whose argument vectors have the
/// physical layout of the kernel's input type parameters (or `duckdb_string_t` for
/// VARCHAR/BLOB), and `output` must be null or a live, writable vector of the result type,
/// with capacity for `duckdb_data_chunk_get_size(input)` rows.
#[inline(always)]
unsafe fn unary_scalar_kernel<T: Copy, R: Copy, F: Fn(T) -> R>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    // SAFETY: chunk and vector handles are live and their element types match `T` -> `R`
    // (caller contract). DuckDB flattens scalar-function inputs before calling the C
    // callback, and every data/validity buffer has at least `count` slots, so each `add(i)`
    // with `i < count` is in bounds and aligned.
    unsafe {
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
}

/// # Safety
/// `input` must be null or a live, flat DuckDB data chunk whose argument vectors have the
/// physical layout of the kernel's input type parameters (or `duckdb_string_t` for
/// VARCHAR/BLOB), and `output` must be null or a live, writable vector of the result type,
/// with capacity for `duckdb_data_chunk_get_size(input)` rows.
#[inline(always)]
unsafe fn unary_str_to_scalar_kernel<R: Copy, F: Fn(&str) -> R>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    // SAFETY: chunk and vector handles are live and their element types match
    // `duckdb_string_t` -> `R` (caller contract). DuckDB flattens scalar-function inputs
    // before calling the C callback, and every data/validity buffer has at least `count`
    // slots, so each `add(i)` with `i < count` is in bounds and aligned. Strings borrowed
    // from the input vector are only used within the iteration; `assign_string_element_len`
    // copies the bytes out of `buf`.
    unsafe {
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
}

/// # Safety
/// `input` must be null or a live, flat DuckDB data chunk whose argument vectors have the
/// physical layout of the kernel's input type parameters (or `duckdb_string_t` for
/// VARCHAR/BLOB), and `output` must be null or a live, writable vector of the result type,
/// with capacity for `duckdb_data_chunk_get_size(input)` rows.
#[inline(always)]
unsafe fn unary_to_str_kernel<T: Copy, const N: usize, F: Fn(T, &mut [u8; N]) -> Option<&[u8]>>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    // SAFETY: chunk and vector handles are live and their element types match `T` ->
    // VARCHAR/BLOB` (caller contract). DuckDB flattens scalar-function inputs before
    // calling the C callback, and every data/validity buffer has at least `count` slots, so
    // each `add(i)` with `i < count` is in bounds and aligned. Strings borrowed from the
    // input vector are only used within the iteration; `assign_string_element_len` copies
    // the bytes out of `buf`.
    unsafe {
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
}

/// # Safety
/// `input` must be null or a live, flat DuckDB data chunk whose argument vectors have the
/// physical layout of the kernel's input type parameters (or `duckdb_string_t` for
/// VARCHAR/BLOB), and `output` must be null or a live, writable vector of the result type,
/// with capacity for `duckdb_data_chunk_get_size(input)` rows.
#[inline(always)]
unsafe fn unary_str_to_str_kernel<const N: usize, F>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) where
    F: for<'a> Fn(&str, &'a mut [u8; N]) -> Option<&'a [u8]>,
{
    // SAFETY: chunk and vector handles are live and their element types match
    // `duckdb_string_t` -> VARCHAR/BLOB` (caller contract). DuckDB flattens scalar-function
    // inputs before calling the C callback, and every data/validity buffer has at least
    // `count` slots, so each `add(i)` with `i < count` is in bounds and aligned. Strings
    // borrowed from the input vector are only used within the iteration;
    // `assign_string_element_len` copies the bytes out of `buf`.
    unsafe {
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
}

/// # Safety
/// `input` must be null or a live, flat DuckDB data chunk whose argument vectors have the
/// physical layout of the kernel's input type parameters (or `duckdb_string_t` for
/// VARCHAR/BLOB), and `output` must be null or a live, writable vector of the result type,
/// with capacity for `duckdb_data_chunk_get_size(input)` rows.
#[inline(always)]
unsafe fn binary_scalar_kernel<T1: Copy, T2: Copy, R: Copy, F: Fn(T1, T2) -> R>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) {
    // SAFETY: chunk and vector handles are live and their element types match `(T1, T2)` ->
    // `R` (caller contract). DuckDB flattens scalar-function inputs before calling the C
    // callback, and every data/validity buffer has at least `count` slots, so each `add(i)`
    // with `i < count` is in bounds and aligned.
    unsafe {
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
}

/// # Safety
/// `input` must be null or a live, flat DuckDB data chunk whose argument vectors have the
/// physical layout of the kernel's input type parameters (or `duckdb_string_t` for
/// VARCHAR/BLOB), and `output` must be null or a live, writable vector of the result type,
/// with capacity for `duckdb_data_chunk_get_size(input)` rows.
#[inline(always)]
unsafe fn binary_str_scalar_to_str_kernel<T2: Copy, const N: usize, F>(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    op: F,
) where
    F: for<'a> Fn(&str, T2, &'a mut [u8; N]) -> Option<&'a [u8]>,
{
    // SAFETY: chunk and vector handles are live and their element types match
    // `(duckdb_string_t, T2)` -> VARCHAR` (caller contract). DuckDB flattens scalar-
    // function inputs before calling the C callback, and every data/validity buffer has at
    // least `count` slots, so each `add(i)` with `i < count` is in bounds and aligned.
    // Strings borrowed from the input vector are only used within the iteration;
    // `assign_string_element_len` copies the bytes out of `buf`.
    unsafe {
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
}

// =========================================================================
// Scalar Function Implementations
// =========================================================================

/// Scalar function: h3_to_string(UBIGINT) -> VARCHAR (Zero-allocation)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_to_string(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_to_string(UBIGINT) -> VARCHAR`, so
    // column 0 holds `u64`, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_to_str_kernel(input, output, |cell_u64, buf: &mut [u8; 16]| {
                Some(fast_hex_u64(cell_u64, buf))
            });
        });
    }
}

/// Scalar function: string_to_h3(VARCHAR) -> UBIGINT (Zero-allocation hex parsing)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_string_to_h3(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `string_to_h3(VARCHAR) -> UBIGINT`, so
    // column 0 holds `duckdb_string_t` and the output holds `u64`, which is what the kernel
    // requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_str_to_scalar_kernel(input, output, |s| parse_hex_u64(s).unwrap_or(0));
        });
    }
}

/// Scalar function: h3_to_lat(UBIGINT) -> DOUBLE
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_to_lat(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_to_lat(UBIGINT) -> DOUBLE`, so input
    // is `u64` and output `f64`, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, scalar_ops::h3_to_lat);
        });
    }
}

/// Scalar function: h3_to_lng(UBIGINT) -> DOUBLE
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_to_lng(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_to_lng(UBIGINT) -> DOUBLE`, so input
    // is `u64` and output `f64`, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, scalar_ops::h3_to_lng);
        });
    }
}

/// Scalar function: h3_get_resolution(UBIGINT) -> BIGINT (1-cycle bitshift)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_get_resolution(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_get_resolution(UBIGINT) -> BIGINT`, so
    // input is `u64` and output `i64`, which is what the kernel requires.
    unsafe {
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
}

/// Scalar function: h3_is_valid(UBIGINT) -> BOOLEAN
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_is_valid_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_is_valid(UBIGINT) -> BOOLEAN`, so
    // input is `u64` and output is DuckDB's 1-byte bool, written as Rust `bool` (0/1),
    // which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, scalar_ops::h3_is_valid_u64);
        });
    }
}

/// Scalar function: h3_is_valid(VARCHAR) -> BOOLEAN
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_is_valid_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_is_valid(VARCHAR) -> BOOLEAN`, so
    // input is `duckdb_string_t` and output is a 1-byte bool, which is what the kernel
    // requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_str_to_scalar_kernel(input, output, scalar_ops::h3_is_valid_str);
        });
    }
}

/// Scalar function: h3_to_wkb(UBIGINT) -> BLOB (Zero-allocation WKB polygon encoder)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_to_wkb_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `(UBIGINT) -> BLOB/GEOMETRY` (directly or
    // via `scalar_h3_to_geometry_u64`), so input is `u64` and output is a string-like
    // vector, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_to_str_kernel(input, output, |cell_u64: u64, buf: &mut WkbBuf| {
                h3_index_to_wkb(cell_u64, buf).map(|len| &buf[..len])
            });
        });
    }
}

/// Scalar function: h3_to_wkb(VARCHAR) -> BLOB (Zero-allocation WKB polygon encoder)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_to_wkb_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `(VARCHAR) -> BLOB/GEOMETRY` (directly or
    // via `scalar_h3_to_geometry_str`), so input is `duckdb_string_t` and output is a
    // string-like vector, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_str_to_str_kernel(input, output, |s, buf: &mut WkbBuf| {
                parse_hex_u64(s)
                    .and_then(|u| h3_index_to_wkb(u, buf))
                    .map(|len| &buf[..len])
            });
        });
    }
}

/// Scalar function: h3_to_geometry(UBIGINT) -> GEOMETRY (Zero-allocation WKB polygon encoder)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_to_geometry_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: forwards DuckDB's live handles unchanged to `scalar_h3_to_wkb_u64`, whose
    // contract (`(UBIGINT) -> GEOMETRY`, a BLOB-backed type) is identical.
    unsafe {
        ffi_scalar_guard(info, || {
            scalar_h3_to_wkb_u64(info, input, output);
        });
    }
}

/// Scalar function: h3_to_geometry(VARCHAR) -> GEOMETRY (Zero-allocation WKB polygon encoder)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_to_geometry_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: forwards DuckDB's live handles unchanged to `scalar_h3_to_wkb_str`, whose
    // contract (`(VARCHAR) -> GEOMETRY`, a BLOB-backed type) is identical.
    unsafe {
        ffi_scalar_guard(info, || {
            scalar_h3_to_wkb_str(info, input, output);
        });
    }
}

/// Scalar function: h3_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_cell_to_parent_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_cell_to_parent(UBIGINT, BIGINT) ->
    // UBIGINT`, so the columns hold `u64`/`i64` and the output `u64`, which is what the
    // kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            binary_scalar_kernel(input, output, |cell_u64: u64, parent_res_i64: i64| {
                scalar_ops::h3_cell_to_parent_u64(cell_u64, parent_res_i64).unwrap_or(0)
            });
        });
    }
}

/// Scalar function: h3_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_cell_to_parent_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_cell_to_parent(VARCHAR, BIGINT) ->
    // VARCHAR`, so the columns hold `duckdb_string_t`/`i64`, which is what the kernel
    // requires.
    unsafe {
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
}

/// Scalar function: a5_to_string(UBIGINT) -> VARCHAR
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_to_string(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: forwarded to `scalar_h3_to_string` (`(UBIGINT) -> VARCHAR` hex formatting).
    unsafe {
        scalar_h3_to_string(info, input, output);
    }
}

/// Scalar function: string_to_a5(VARCHAR) -> UBIGINT
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_string_to_a5(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: forwarded to `scalar_string_to_h3` (`(VARCHAR) -> UBIGINT` hex parsing).
    unsafe {
        scalar_string_to_h3(info, input, output);
    }
}

/// Scalar function: a5_to_lat(UBIGINT) -> DOUBLE
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_to_lat(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_to_lat(UBIGINT) -> DOUBLE`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, scalar_ops::a5_to_lat);
        });
    }
}

/// Scalar function: a5_to_lng(UBIGINT) -> DOUBLE
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_to_lng(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_to_lng(UBIGINT) -> DOUBLE`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, scalar_ops::a5_to_lng);
        });
    }
}

/// Scalar function: a5_get_resolution(UBIGINT) -> BIGINT
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_get_resolution(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_get_resolution(UBIGINT) -> BIGINT`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, scalar_ops::a5_get_resolution);
        });
    }
}

/// Scalar function: a5_is_valid(UBIGINT) -> BOOLEAN
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_is_valid_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_is_valid(UBIGINT) -> BOOLEAN`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, scalar_ops::a5_is_valid_u64);
        });
    }
}

/// Scalar function: a5_is_valid(VARCHAR) -> BOOLEAN
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_is_valid_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_is_valid(VARCHAR) -> BOOLEAN`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_str_to_scalar_kernel(input, output, scalar_ops::a5_is_valid_str);
        });
    }
}

/// Scalar function: a5_to_wkb(UBIGINT) -> BLOB
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_to_wkb_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `(UBIGINT) -> BLOB/GEOMETRY`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_to_str_kernel(input, output, |cell_u64: u64, buf: &mut WkbBuf| {
                a5_index_to_wkb(cell_u64, buf).map(|len| &buf[..len])
            });
        });
    }
}

/// Scalar function: a5_to_wkb(VARCHAR) -> BLOB
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_to_wkb_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `(VARCHAR) -> BLOB/GEOMETRY`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_str_to_str_kernel(input, output, |s, buf: &mut WkbBuf| {
                parse_hex_u64(s)
                    .and_then(|u| a5_index_to_wkb(u, buf))
                    .map(|len| &buf[..len])
            });
        });
    }
}

/// Scalar function: a5_to_geometry(UBIGINT) -> GEOMETRY
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_to_geometry_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: forwards DuckDB's live handles unchanged to `scalar_a5_to_wkb_u64`.
    unsafe {
        ffi_scalar_guard(info, || {
            scalar_a5_to_wkb_u64(info, input, output);
        });
    }
}

/// Scalar function: a5_to_geometry(VARCHAR) -> GEOMETRY
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_to_geometry_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: forwards DuckDB's live handles unchanged to `scalar_a5_to_wkb_str`.
    unsafe {
        ffi_scalar_guard(info, || {
            scalar_a5_to_wkb_str(info, input, output);
        });
    }
}

/// Scalar function: a5_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_cell_to_parent_u64(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT`.
    unsafe {
        ffi_scalar_guard(info, || {
            binary_scalar_kernel(input, output, |cell_u64: u64, parent_res_i64: i64| {
                scalar_ops::a5_cell_to_parent_u64(cell_u64, parent_res_i64).unwrap_or(0)
            });
        });
    }
}

/// Scalar function: a5_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_cell_to_parent_str(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR`.
    unsafe {
        ffi_scalar_guard(info, || {
            binary_str_scalar_to_str_kernel(
                input,
                output,
                |s, parent_res_i64: i64, buf: &mut [u8; 16]| {
                    let cell = parse_hex_u64(s)?;
                    let parent = scalar_ops::a5_cell_to_parent_u64(cell, parent_res_i64)?;
                    Some(fast_hex_u64(parent, buf))
                },
            );
        });
    }
}

/// Shared body of `<grid>_lonlat_to_cell(DOUBLE, DOUBLE, BIGINT) -> UBIGINT`; rows with
/// a NULL argument or that `index` rejects yield NULL.
///
/// # Safety
/// `input` and `output` must be live handles from a DuckDB scalar callback, with
/// `input` holding `(DOUBLE, DOUBLE, BIGINT)` and `output` holding `UBIGINT`.
unsafe fn lonlat_to_cell_kernel(
    input: duckdb_data_chunk,
    output: duckdb_vector,
    index: impl Fn(f64, f64, i64) -> Option<u64>,
) {
    // SAFETY: handles are live and typed per the caller contract; every buffer holds at
    // least `count` rows and null pointers are rejected before use.
    unsafe {
        if input.is_null() || output.is_null() {
            return;
        }
        let count = duckdb_data_chunk_get_size(input);
        if count == 0 {
            return;
        }
        let v_lon = duckdb_data_chunk_get_vector(input, 0);
        let v_lat = duckdb_data_chunk_get_vector(input, 1);
        let v_res = duckdb_data_chunk_get_vector(input, 2);
        if v_lon.is_null() || v_lat.is_null() || v_res.is_null() {
            return;
        }
        let p_lon = duckdb_vector_get_data(v_lon) as *const f64;
        let p_lat = duckdb_vector_get_data(v_lat) as *const f64;
        let p_res = duckdb_vector_get_data(v_res) as *const i64;
        let p_out = duckdb_vector_get_data(output) as *mut u64;
        if p_lon.is_null() || p_lat.is_null() || p_res.is_null() || p_out.is_null() {
            return;
        }
        let val_lon = duckdb_vector_get_validity(v_lon);
        let val_lat = duckdb_vector_get_validity(v_lat);
        let val_res = duckdb_vector_get_validity(v_res);

        for i in 0..count {
            if !duckdb_validity_is_valid(val_lon, i)
                || !duckdb_validity_is_valid(val_lat, i)
                || !duckdb_validity_is_valid(val_res, i)
            {
                duckdb_vector_set_row_invalid(output, i);
                continue;
            }
            let lon = *p_lon.add(i as usize);
            let lat = *p_lat.add(i as usize);
            let res = *p_res.add(i as usize);
            match index(lon, lat, res) {
                Some(cell) => *p_out.add(i as usize) = cell,
                None => duckdb_vector_set_row_invalid(output, i),
            }
        }
    }
}

/// Scalar function: a5_lonlat_to_cell(DOUBLE, DOUBLE, BIGINT) -> UBIGINT
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding `(DOUBLE, DOUBLE, BIGINT)` and
/// `output` holding `UBIGINT`.
pub unsafe extern "C" fn scalar_a5_lonlat_to_cell(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). Registered only as `a5_lonlat_to_cell(DOUBLE, DOUBLE, BIGINT) -> UBIGINT`.
    unsafe {
        ffi_scalar_guard(info, || {
            lonlat_to_cell_kernel(input, output, scalar_ops::a5_lonlat_to_cell);
        });
    }
}

// =========================================================================
// Grid-generic scalar functions (S2, HEALPix)
//
// `GRID` is the grid's discriminant (`GridKind as u8`), so each grid gets its own
// monomorphized C callback.
// =========================================================================

/// Scalar function: `<grid>_to_lat(UBIGINT) -> DOUBLE`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_to_lat<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(UBIGINT) -> DOUBLE`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, |cell: u64| {
                scalar_ops::grid_to_lat(GridKind::from_index(GRID), cell)
            });
        });
    }
}

/// Scalar function: `<grid>_to_lng(UBIGINT) -> DOUBLE`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_to_lng<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(UBIGINT) -> DOUBLE`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, |cell: u64| {
                scalar_ops::grid_to_lng(GridKind::from_index(GRID), cell)
            });
        });
    }
}

/// Scalar function: `<grid>_get_resolution(UBIGINT) -> BIGINT`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_get_resolution<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(UBIGINT) -> BIGINT`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, |cell: u64| {
                scalar_ops::grid_get_resolution(GridKind::from_index(GRID), cell)
            });
        });
    }
}

/// Scalar function: `<grid>_is_valid(UBIGINT) -> BOOLEAN`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_is_valid_u64<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(UBIGINT) -> BOOLEAN`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_scalar_kernel(input, output, |cell: u64| {
                GridKind::from_index(GRID).is_valid(cell)
            });
        });
    }
}

/// Scalar function: `<grid>_is_valid(VARCHAR) -> BOOLEAN`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_is_valid_str<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(VARCHAR) -> BOOLEAN`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_str_to_scalar_kernel(input, output, |hex: &str| {
                scalar_ops::grid_is_valid_str(GridKind::from_index(GRID), hex)
            });
        });
    }
}

/// Scalar function: `<grid>_to_wkb(UBIGINT) -> BLOB` and `<grid>_to_geometry(UBIGINT) -> GEOMETRY`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_to_wkb_u64<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(UBIGINT) -> BLOB/GEOMETRY`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_to_str_kernel(input, output, |cell: u64, buf: &mut WkbBuf| {
                cell_index_to_wkb(GridKind::from_index(GRID), cell, buf).map(|len| &buf[..len])
            });
        });
    }
}

/// Scalar function: `<grid>_to_wkb(VARCHAR) -> BLOB` and `<grid>_to_geometry(VARCHAR) -> GEOMETRY`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_to_wkb_str<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(VARCHAR) -> BLOB/GEOMETRY`.
    unsafe {
        ffi_scalar_guard(info, || {
            unary_str_to_str_kernel(input, output, |s, buf: &mut WkbBuf| {
                parse_hex_u64(s)
                    .and_then(|cell| cell_index_to_wkb(GridKind::from_index(GRID), cell, buf))
                    .map(|len| &buf[..len])
            });
        });
    }
}

/// Scalar function: `<grid>_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_cell_to_parent_u64<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(UBIGINT, BIGINT) -> UBIGINT`.
    unsafe {
        ffi_scalar_guard(info, || {
            binary_scalar_kernel(input, output, |cell: u64, parent_res: i64| {
                scalar_ops::grid_cell_to_parent_u64(GridKind::from_index(GRID), cell, parent_res)
                    .unwrap_or(0)
            });
        });
    }
}

/// Scalar function: `<grid>_cell_to_parent(VARCHAR, BIGINT) -> VARCHAR`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_cell_to_parent_str<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(VARCHAR, BIGINT) -> VARCHAR`.
    unsafe {
        ffi_scalar_guard(info, || {
            binary_str_scalar_to_str_kernel(
                input,
                output,
                |s, parent_res: i64, buf: &mut [u8; 16]| {
                    let cell = parse_hex_u64(s)?;
                    let parent = scalar_ops::grid_cell_to_parent_u64(
                        GridKind::from_index(GRID),
                        cell,
                        parent_res,
                    )?;
                    Some(fast_hex_u64(parent, buf))
                },
            );
        });
    }
}

/// Scalar function: `<grid>_lonlat_to_cell(DOUBLE, DOUBLE, BIGINT) -> UBIGINT`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_lonlat_to_cell<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: live handles, registered only as `(DOUBLE, DOUBLE, BIGINT) -> UBIGINT`.
    unsafe {
        ffi_scalar_guard(info, || {
            lonlat_to_cell_kernel(input, output, |lon, lat, res| {
                scalar_ops::grid_lonlat_to_cell(GridKind::from_index(GRID), lon, lat, res)
            });
        });
    }
}

/// Scalar function: `<grid>_polygon_to_cells(BLOB | GEOMETRY, BIGINT) -> UBIGINT[]`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_polygon_to_cells<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: registered only as `(BLOB|GEOMETRY, BIGINT) -> UBIGINT[]`.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel_for(GridKind::from_index(GRID), info, input, output, false);
        });
    }
}

/// Scalar function: `<grid>_polygon_to_cells(BLOB | GEOMETRY, BIGINT, VARCHAR) -> UBIGINT[]`
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this scalar
/// callback, typed as declared.
pub unsafe extern "C" fn scalar_grid_polygon_to_cells_mode<const GRID: u8>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: registered only as `(BLOB|GEOMETRY, BIGINT, VARCHAR) -> UBIGINT[]`.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel_for(GridKind::from_index(GRID), info, input, output, true);
        });
    }
}

/// Bind callback for `<grid>_polygon_to_cells`.
#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe extern "C" fn grid_polygon_cells_bind<const GRID: u8>(info: duckdb_bind_info) {
    // SAFETY: `info` is the live scalar-bind handle.
    unsafe { polygon_cells_bind_for(info, GridKind::from_index(GRID)) };
}

/// Owns a scalar-bind expression and destroys it on drop.
#[cfg(feature = "duckdb-api-v1_5_6")]
struct ExpressionGuard(duckdb_expression);

#[cfg(feature = "duckdb-api-v1_5_6")]
impl Drop for ExpressionGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned expression handle, destroyed once.
            unsafe { duckdb_destroy_expression(&mut self.0) };
        }
    }
}

/// Owns a client context and destroys it on drop.
#[cfg(feature = "duckdb-api-v1_5_6")]
struct ClientContextGuard(duckdb_client_context);

#[cfg(feature = "duckdb-api-v1_5_6")]
impl Drop for ClientContextGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned client-context handle, destroyed once.
            unsafe { duckdb_destroy_client_context(&mut self.0) };
        }
    }
}

/// Owns a `duckdb_value` and destroys it on drop.
#[cfg(feature = "duckdb-api-v1_5_6")]
struct ValueGuard(duckdb_value);

#[cfg(feature = "duckdb-api-v1_5_6")]
impl Drop for ValueGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned value handle, destroyed once.
            unsafe { duckdb_destroy_value(&mut self.0) };
        }
    }
}

/// Fold scalar-bind argument `index` to a constant if DuckDB can.
///
/// Returns `Ok(None)` when the argument is absent, not foldable, or folds to
/// NULL (the kernel turns NULL arguments into NULL rows), and `Err` with
/// DuckDB's message when folding itself fails.
///
/// # Safety
/// `info` must be the live scalar-bind handle and `ctx` its client context.
#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe fn fold_constant_argument(
    info: duckdb_bind_info,
    ctx: duckdb_client_context,
    index: idx_t,
) -> Result<Option<ValueGuard>, String> {
    // SAFETY: `info`/`ctx` are live (caller contract); the expression, error
    // data and value are owned here and released by their guards.
    unsafe {
        if index >= duckdb_scalar_function_bind_get_argument_count(info) {
            return Ok(None);
        }
        let expr = ExpressionGuard(duckdb_scalar_function_bind_get_argument(info, index));
        if expr.0.is_null() || !duckdb_expression_is_foldable(expr.0) {
            return Ok(None);
        }
        let mut val: duckdb_value = std::ptr::null_mut();
        let err = ErrorData::from_raw(duckdb_expression_fold(ctx, expr.0, &mut val));
        let val = ValueGuard(val);
        if err.has_error() {
            return Err(err.message());
        }
        if val.0.is_null() || duckdb_is_null_value(val.0) {
            return Ok(None);
        }
        Ok(Some(val))
    }
}

/// Shared bind callback for `h3_polygon_to_cells` / `a5_polygon_to_cells`.
#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe fn polygon_cells_bind_for(info: duckdb_bind_info, grid: GridKind) {
    let fn_name = format!("{grid}_polygon_to_cells");
    let max_res = i64::from(grid.max_resolution());
    let set_error = |msg: String| {
        let c = to_c_string(&format!("{fn_name}: {msg}"));
        // SAFETY: `info` is the live scalar-bind handle; DuckDB copies the message.
        unsafe { duckdb_scalar_function_bind_set_error(info, c.as_ptr()) };
    };
    let run = || -> Result<(), String> {
        let mut raw_ctx: duckdb_client_context = std::ptr::null_mut();
        // SAFETY: `info` is the live scalar-bind handle; `raw_ctx` is written
        // with an owned context (or left null).
        unsafe { duckdb_scalar_function_get_client_context(info, &mut raw_ctx) };
        let ctx = ClientContextGuard(raw_ctx);
        if ctx.0.is_null() {
            return Ok(());
        }

        // Argument 1: resolution.
        // SAFETY: live bind handle and its client context.
        if let Some(val) = unsafe { fold_constant_argument(info, ctx.0, 1) }? {
            // SAFETY: `val` is a live non-NULL value.
            let res = unsafe { duckdb_get_int64(val.0) };
            if !(0..=max_res).contains(&res) {
                return Err(format!("resolution {res} is outside 0..={max_res}"));
            }
        }

        // Argument 2: containment mode.
        // SAFETY: live bind handle and its client context.
        if let Some(val) = unsafe { fold_constant_argument(info, ctx.0, 2) }? {
            // SAFETY: `val` is a live non-NULL value; the returned string is
            // DuckDB-allocated and freed by `from_duckdb_string`.
            let name = unsafe { from_duckdb_string(duckdb_get_varchar(val.0)) };
            if let Some(name) = name {
                if scalar_ops::parse_containment_mode(&name).is_none() {
                    return Err(format!("unknown containment mode `{name}`"));
                }
            }
        }
        Ok(())
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => set_error(msg),
        Err(payload) => set_error(format!(
            "panic in bind: {}",
            safe_panic_payload_to_string(payload)
        )),
    }
}

/// Bind callback for `h3_polygon_to_cells`: folds constant `resolution` and
/// `containment` arguments and rejects invalid values at plan time instead of
/// failing per row.
#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe extern "C" fn polygon_cells_bind(info: duckdb_bind_info) {
    // SAFETY: `info` is the live scalar-bind handle.
    unsafe { polygon_cells_bind_for(info, GridKind::H3) };
}

/// Bind callback for `a5_polygon_to_cells`.
#[cfg(feature = "duckdb-api-v1_5_6")]
unsafe extern "C" fn a5_polygon_cells_bind(info: duckdb_bind_info) {
    // SAFETY: `info` is the live scalar-bind handle.
    unsafe { polygon_cells_bind_for(info, GridKind::A5) };
}

/// Shared body of `{h3,a5}_polygon_to_cells(geom, res[, containment])` -> `UBIGINT[]`.
///
/// Rows with a NULL argument, invalid WKB or an out-of-range resolution yield NULL. An
/// unknown containment mode or a polygon over the cell limit fails the query, since retrying
/// row by row would not help.
///
/// # Safety
/// `info`, `input` and `output` must be live handles from a DuckDB scalar callback; `input`
/// must hold (BLOB|GEOMETRY, BIGINT[, VARCHAR when `has_mode`]) columns and `output` must
/// be a `UBIGINT[]` list vector.
unsafe fn polygon_to_cells_kernel_for(
    grid: GridKind,
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
    has_mode: bool,
) {
    let fn_name = format!("{grid}_polygon_to_cells");
    // SAFETY: handles are live and typed per the caller contract: column 0 is BLOB/GEOMETRY
    // (`duckdb_string_t`), column 1 BIGINT (`i64`), optional column 2 VARCHAR, and `output`
    // a `UBIGINT[]` list vector. Input/entry buffers hold at least `count` rows, so
    // `add(i)` is in bounds. The child buffer is re-fetched after every successful
    // `duckdb_list_vector_reserve(new_total)`, which guarantees room for `new_total`
    // `u64`s, so copying `cells.len()` values at offset `total` stays in bounds; `cells` is
    // a separate Rust allocation, so the copy cannot overlap. Error messages are NUL-
    // terminated and copied by DuckDB.
    unsafe {
        if input.is_null() || output.is_null() {
            return;
        }
        let count = duckdb_data_chunk_get_size(input);
        if count == 0 {
            return;
        }
        let v_geom = duckdb_data_chunk_get_vector(input, 0);
        let v_res = duckdb_data_chunk_get_vector(input, 1);
        let v_mode = if has_mode {
            duckdb_data_chunk_get_vector(input, 2)
        } else {
            std::ptr::null_mut()
        };
        if v_geom.is_null() || v_res.is_null() || (has_mode && v_mode.is_null()) {
            return;
        }
        let p_geom = duckdb_vector_get_data(v_geom) as *const duckdb_string_t;
        let p_res = duckdb_vector_get_data(v_res) as *const i64;
        let p_mode = if has_mode {
            duckdb_vector_get_data(v_mode) as *const duckdb_string_t
        } else {
            std::ptr::null()
        };
        let entries = duckdb_vector_get_data(output) as *mut duckdb_list_entry;
        let child = duckdb_list_vector_get_child(output);
        if p_geom.is_null() || p_res.is_null() || (has_mode && p_mode.is_null()) {
            return;
        }
        if entries.is_null() || child.is_null() {
            return;
        }
        let val_geom = duckdb_vector_get_validity(v_geom);
        let val_res = duckdb_vector_get_validity(v_res);
        let val_mode = if has_mode {
            duckdb_vector_get_validity(v_mode)
        } else {
            std::ptr::null_mut()
        };

        // One buffer reused for every row of the chunk; its contents are
        // copied into the list child vector below.
        let mut cells: Vec<u64> = Vec::new();
        let mut total: u64 = 0;
        for i in 0..count {
            *entries.add(i as usize) = duckdb_list_entry {
                offset: total,
                length: 0,
            };
            if !duckdb_validity_is_valid(val_geom, i)
                || !duckdb_validity_is_valid(val_res, i)
                || (has_mode && !duckdb_validity_is_valid(val_mode, i))
            {
                duckdb_vector_set_row_invalid(output, i);
                continue;
            }
            let mode = if has_mode {
                let name = (*p_mode.add(i as usize)).as_str();
                match scalar_ops::parse_containment_mode(name) {
                    Some(mode) => mode,
                    None => {
                        let msg = to_c_string(&format!(
                            "{fn_name}: unknown containment mode '{name}' (expected one of \
                             contains_centroid, contains_boundary, intersects_boundary, covers, \
                             center, full, overlapping)"
                        ));
                        duckdb_scalar_function_set_error(info, msg.as_ptr());
                        return;
                    }
                }
            } else {
                scalar_ops::ContainmentMode::ContainsCentroid
            };
            let wkb = (*p_geom.add(i as usize)).as_bytes();
            let res_val = *p_res.add(i as usize);
            let fill_res =
                scalar_ops::grid_polygon_to_cells_wkb_into(grid, wkb, res_val, mode, &mut cells);
            match fill_res {
                Ok(()) => {}
                Err(err @ scalar_ops::PolygonToCellsError::TooManyCells { .. }) => {
                    let msg = to_c_string(&format!("{fn_name}: {err}"));
                    duckdb_scalar_function_set_error(info, msg.as_ptr());
                    return;
                }
                Err(_) => {
                    duckdb_vector_set_row_invalid(output, i);
                    continue;
                }
            }
            let new_total = total + cells.len() as u64;
            if duckdb_list_vector_reserve(output, new_total) != DuckDBState::Success {
                let msg = to_c_string(&format!("{fn_name}: failed to reserve list storage"));
                duckdb_scalar_function_set_error(info, msg.as_ptr());
                return;
            }
            // Reserving may reallocate the child buffer, so fetch it after every reserve.
            let p_child = duckdb_vector_get_data(child) as *mut u64;
            if !cells.is_empty() {
                std::ptr::copy_nonoverlapping(
                    cells.as_ptr(),
                    p_child.add(total as usize),
                    cells.len(),
                );
            }
            duckdb_list_vector_set_size(output, new_total);
            (*entries.add(i as usize)).length = cells.len() as u64;
            total = new_total;
        }
    }
}

/// Scalar function: h3_polygon_to_cells(BLOB | GEOMETRY, BIGINT) -> UBIGINT[]
/// (containment mode `contains_centroid`)
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_polygon_to_cells(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_polygon_to_cells(BLOB|GEOMETRY,
    // BIGINT) -> UBIGINT[]`, so the layout matches `polygon_to_cells_kernel_for` with `has_mode
    // = false`, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel_for(GridKind::H3, info, input, output, false);
        });
    }
}

/// Scalar function: h3_polygon_to_cells(BLOB | GEOMETRY, BIGINT, VARCHAR) -> UBIGINT[]
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_h3_polygon_to_cells_mode(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: DuckDB invokes this callback with live `info`/`input`/`output` handles
    // (contract above). It is registered only as `h3_polygon_to_cells(BLOB|GEOMETRY,
    // BIGINT, VARCHAR) -> UBIGINT[]`, so the layout matches `polygon_to_cells_kernel_for` with
    // `has_mode = true`, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel_for(GridKind::H3, info, input, output, true);
        });
    }
}

/// Scalar function: a5_polygon_to_cells(BLOB | GEOMETRY, BIGINT) -> UBIGINT[]
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_polygon_to_cells(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: registered only as `a5_polygon_to_cells(BLOB|GEOMETRY, BIGINT) -> UBIGINT[]`.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel_for(GridKind::A5, info, input, output, false);
        });
    }
}

/// Scalar function: a5_polygon_to_cells(BLOB | GEOMETRY, BIGINT, VARCHAR) -> UBIGINT[]
///
/// # Safety
/// `info`, `input`, and `output` must be the handles DuckDB passes to this
/// scalar callback, with `input` holding the declared argument types and
/// `output` the declared return type.
pub unsafe extern "C" fn scalar_a5_polygon_to_cells_mode(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    // SAFETY: registered only as `a5_polygon_to_cells(BLOB|GEOMETRY, BIGINT, VARCHAR) -> UBIGINT[]`.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel_for(GridKind::A5, info, input, output, true);
        });
    }
}

/// One overload of a scalar function: parameter types, return type and callback.
type ScalarOverload<'a> = (
    &'a [duckdb_logical_type],
    duckdb_logical_type,
    duckdb_scalar_function_t,
);

/// Register every overload of `name` as one scalar function set. DuckDB before 1.5 rejects a
/// second `duckdb_register_scalar_function` under an existing name, so overloads must be
/// registered together.
///
/// # Safety
/// `con` must be a live DuckDB connection and every logical type in `overloads` must be
/// live for the call.
unsafe fn register_scalar_fn_set(
    con: duckdb_connection,
    name: &str,
    overloads: &[ScalarOverload<'_>],
) -> Result<(), String> {
    // SAFETY: forwarded; `bind_grid` None means identical behaviour.
    unsafe { register_scalar_fn_set_ex(con, name, overloads, None) }
}

/// Register a scalar overload set, optionally attaching the v1.5.6 bind
/// callback used by `{h3,a5}_polygon_to_cells` (ignored on the v1.2.0 build).
///
/// # Safety
/// Same contract as [`register_scalar_fn_set`].
unsafe fn register_scalar_fn_set_ex(
    con: duckdb_connection,
    name: &str,
    overloads: &[ScalarOverload<'_>],
    #[allow(unused_variables)] bind_grid: Option<GridKind>,
) -> Result<(), String> {
    // SAFETY: `con` is a live connection and every logical type in `overloads` is live
    // (caller contract). Names are NUL-terminated CStrings outliving each call, each
    // created function/set is destroyed exactly once after DuckDB has copied it into the
    // set/catalog, and every callback pointer has the `duckdb_scalar_function_t` ABI.
    unsafe {
        let c_name = to_c_string(name);
        let mut set = duckdb_create_scalar_function_set(c_name.as_ptr());
        let mut ok = true;
        for &(param_types, return_type, func) in overloads {
            let mut scalar_fn = duckdb_create_scalar_function();
            duckdb_scalar_function_set_name(scalar_fn, c_name.as_ptr());
            for &param_type in param_types {
                duckdb_scalar_function_add_parameter(scalar_fn, param_type);
            }
            duckdb_scalar_function_set_return_type(scalar_fn, return_type);
            duckdb_scalar_function_set_function(scalar_fn, func);
            #[cfg(feature = "duckdb-api-v1_5_6")]
            if let Some(grid) = bind_grid {
                let bind_cb: duckdb_scalar_function_bind_t = match grid {
                    GridKind::H3 => polygon_cells_bind,
                    GridKind::A5 => a5_polygon_cells_bind,
                    GridKind::S2 => grid_polygon_cells_bind::<{ GridKind::S2 as u8 }>,
                    GridKind::Healpix => grid_polygon_cells_bind::<{ GridKind::Healpix as u8 }>,
                };
                duckdb_scalar_function_set_bind(scalar_fn, bind_cb);
            }
            ok &= duckdb_add_scalar_function_to_set(set, scalar_fn) == DuckDBState::Success;
            duckdb_destroy_scalar_function(&mut scalar_fn);
        }
        ok = ok && duckdb_register_scalar_function_set(con, set) == DuckDBState::Success;
        duckdb_destroy_scalar_function_set(&mut set);
        if ok {
            Ok(())
        } else {
            Err(format!("failed to register scalar function {name}"))
        }
    }
}

/// # Safety
/// `con` must be a live DuckDB connection and both logical types must be live for the call.
#[inline]
unsafe fn register_unary_scalar_fn(
    con: duckdb_connection,
    name: &str,
    param_type: duckdb_logical_type,
    return_type: duckdb_logical_type,
    func: duckdb_scalar_function_t,
) {
    // SAFETY: `con` and both logical types are live (caller contract); `c_name` outlives
    // the calls that read it, and the scalar function is destroyed exactly once after
    // registration has copied it.
    unsafe {
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
}

/// Register scalar functions with DuckDB
///
/// # Safety
/// `con` must be a valid, open DuckDB connection.
pub unsafe fn register_scalar_functions(con: duckdb_connection) -> Result<(), String> {
    // SAFETY: `con` is a live connection (caller contract). Every logical type created here
    // is passed only to registration calls, which copy it, and is destroyed exactly once at
    // the end; each callback is paired with the exact parameter/return types its kernel
    // expects.
    unsafe {
        let type_ubigint = duckdb_create_logical_type(DuckDBType::UBigInt);
        let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
        let type_double = duckdb_create_logical_type(DuckDBType::Double);
        let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
        let type_bool = duckdb_create_logical_type(DuckDBType::Boolean);
        let type_blob = duckdb_create_logical_type(DuckDBType::Blob);
        let type_geom = create_geometry_logical_type();

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

        let mut results = Vec::new();

        // 6-7. h3_is_valid(UBIGINT | VARCHAR) -> BOOLEAN
        results.push(register_scalar_fn_set(
            con,
            "h3_is_valid",
            &[
                (&[type_ubigint], type_bool, scalar_h3_is_valid_u64),
                (&[type_varchar], type_bool, scalar_h3_is_valid_str),
            ],
        ));

        // 8-9. h3_to_wkb(UBIGINT | VARCHAR) -> BLOB
        results.push(register_scalar_fn_set(
            con,
            "h3_to_wkb",
            &[
                (&[type_ubigint], type_blob, scalar_h3_to_wkb_u64),
                (&[type_varchar], type_blob, scalar_h3_to_wkb_str),
            ],
        ));

        // 10-11. h3_cell_to_parent(UBIGINT, BIGINT) -> UBIGINT, (VARCHAR, BIGINT) -> VARCHAR
        results.push(register_scalar_fn_set(
            con,
            "h3_cell_to_parent",
            &[
                (
                    &[type_ubigint, type_bigint],
                    type_ubigint,
                    scalar_h3_cell_to_parent_u64,
                ),
                (
                    &[type_varchar, type_bigint],
                    type_varchar,
                    scalar_h3_cell_to_parent_str,
                ),
            ],
        ));

        // 12-15. h3_to_geometry / h3_cell_to_geometry (standard alias) (UBIGINT | VARCHAR) -> GEOMETRY
        for name in ["h3_to_geometry", "h3_cell_to_geometry"] {
            results.push(register_scalar_fn_set(
                con,
                name,
                &[
                    (&[type_ubigint], type_geom, scalar_h3_to_geometry_u64),
                    (&[type_varchar], type_geom, scalar_h3_to_geometry_str),
                ],
            ));
        }

        // 16-19. h3_polygon_to_cells(BLOB | GEOMETRY, BIGINT[, VARCHAR]) -> UBIGINT[].
        // GEOMETRY overloads only on DuckDB >= 1.5, whose native GEOMETRY vectors hold WKB.
        let type_cell_list = duckdb_create_list_type(type_ubigint);
        let mut geom_inputs = vec![type_blob];
        if is_duckdb_version_at_least(1, 5) {
            geom_inputs.push(type_geom);
        }
        let polygon_params: Vec<[duckdb_logical_type; 3]> = geom_inputs
            .iter()
            .map(|&geom_input| [geom_input, type_bigint, type_varchar])
            .collect();
        let polygon_overloads: Vec<ScalarOverload<'_>> = polygon_params
            .iter()
            .flat_map(|params| {
                [
                    (
                        &params[..2],
                        type_cell_list,
                        scalar_h3_polygon_to_cells as duckdb_scalar_function_t,
                    ),
                    (
                        &params[..],
                        type_cell_list,
                        scalar_h3_polygon_to_cells_mode as duckdb_scalar_function_t,
                    ),
                ]
            })
            .collect();
        results.push(register_scalar_fn_set_ex(
            con,
            "h3_polygon_to_cells",
            &polygon_overloads,
            if cfg!(feature = "duckdb-api-v1_5_6") {
                Some(GridKind::H3)
            } else {
                None
            },
        ));

        // 20-35. A5 scalar functions
        register_unary_scalar_fn(
            con,
            "a5_to_string",
            type_ubigint,
            type_varchar,
            scalar_a5_to_string,
        );
        register_unary_scalar_fn(
            con,
            "string_to_a5",
            type_varchar,
            type_ubigint,
            scalar_string_to_a5,
        );
        register_unary_scalar_fn(
            con,
            "a5_to_lat",
            type_ubigint,
            type_double,
            scalar_a5_to_lat,
        );
        register_unary_scalar_fn(
            con,
            "a5_to_lng",
            type_ubigint,
            type_double,
            scalar_a5_to_lng,
        );
        register_unary_scalar_fn(
            con,
            "a5_get_resolution",
            type_ubigint,
            type_bigint,
            scalar_a5_get_resolution,
        );
        for name in ["a5_is_valid", "a5_is_valid_cell"] {
            results.push(register_scalar_fn_set(
                con,
                name,
                &[
                    (&[type_ubigint], type_bool, scalar_a5_is_valid_u64),
                    (&[type_varchar], type_bool, scalar_a5_is_valid_str),
                ],
            ));
        }
        results.push(register_scalar_fn_set(
            con,
            "a5_to_wkb",
            &[
                (&[type_ubigint], type_blob, scalar_a5_to_wkb_u64),
                (&[type_varchar], type_blob, scalar_a5_to_wkb_str),
            ],
        ));
        results.push(register_scalar_fn_set(
            con,
            "a5_cell_to_parent",
            &[
                (
                    &[type_ubigint, type_bigint],
                    type_ubigint,
                    scalar_a5_cell_to_parent_u64,
                ),
                (
                    &[type_varchar, type_bigint],
                    type_varchar,
                    scalar_a5_cell_to_parent_str,
                ),
            ],
        ));
        for name in ["a5_to_geometry", "a5_cell_to_geometry"] {
            results.push(register_scalar_fn_set(
                con,
                name,
                &[
                    (&[type_ubigint], type_geom, scalar_a5_to_geometry_u64),
                    (&[type_varchar], type_geom, scalar_a5_to_geometry_str),
                ],
            ));
        }
        results.push(register_scalar_fn_set(
            con,
            "a5_lonlat_to_cell",
            &[(
                &[type_double, type_double, type_bigint],
                type_ubigint,
                scalar_a5_lonlat_to_cell,
            )],
        ));
        let a5_polygon_overloads: Vec<ScalarOverload<'_>> = polygon_params
            .iter()
            .flat_map(|params| {
                [
                    (
                        &params[..2],
                        type_cell_list,
                        scalar_a5_polygon_to_cells as duckdb_scalar_function_t,
                    ),
                    (
                        &params[..],
                        type_cell_list,
                        scalar_a5_polygon_to_cells_mode as duckdb_scalar_function_t,
                    ),
                ]
            })
            .collect();
        results.push(register_scalar_fn_set_ex(
            con,
            "a5_polygon_to_cells",
            &a5_polygon_overloads,
            if cfg!(feature = "duckdb-api-v1_5_6") {
                Some(GridKind::A5)
            } else {
                None
            },
        ));
        // S2 and HEALPix scalar functions, the same set as A5's.
        macro_rules! register_grid_scalars {
            ($grid:expr) => {{
                const G: u8 = $grid as u8;
                let grid: GridKind = $grid;
                let name = |suffix: &str| format!("{grid}_{suffix}");
                register_unary_scalar_fn(
                    con,
                    &name("to_string"),
                    type_ubigint,
                    type_varchar,
                    scalar_h3_to_string,
                );
                register_unary_scalar_fn(
                    con,
                    &format!("string_to_{grid}"),
                    type_varchar,
                    type_ubigint,
                    scalar_string_to_h3,
                );
                register_unary_scalar_fn(
                    con,
                    &name("to_lat"),
                    type_ubigint,
                    type_double,
                    scalar_grid_to_lat::<G>,
                );
                register_unary_scalar_fn(
                    con,
                    &name("to_lng"),
                    type_ubigint,
                    type_double,
                    scalar_grid_to_lng::<G>,
                );
                register_unary_scalar_fn(
                    con,
                    &name("get_resolution"),
                    type_ubigint,
                    type_bigint,
                    scalar_grid_get_resolution::<G>,
                );
                for suffix in ["is_valid", "is_valid_cell"] {
                    results.push(register_scalar_fn_set(
                        con,
                        &name(suffix),
                        &[
                            (&[type_ubigint], type_bool, scalar_grid_is_valid_u64::<G>),
                            (&[type_varchar], type_bool, scalar_grid_is_valid_str::<G>),
                        ],
                    ));
                }
                for (suffix, ret) in [
                    ("to_wkb", type_blob),
                    ("to_geometry", type_geom),
                    ("cell_to_geometry", type_geom),
                ] {
                    results.push(register_scalar_fn_set(
                        con,
                        &name(suffix),
                        &[
                            (&[type_ubigint], ret, scalar_grid_to_wkb_u64::<G>),
                            (&[type_varchar], ret, scalar_grid_to_wkb_str::<G>),
                        ],
                    ));
                }
                results.push(register_scalar_fn_set(
                    con,
                    &name("cell_to_parent"),
                    &[
                        (
                            &[type_ubigint, type_bigint],
                            type_ubigint,
                            scalar_grid_cell_to_parent_u64::<G>,
                        ),
                        (
                            &[type_varchar, type_bigint],
                            type_varchar,
                            scalar_grid_cell_to_parent_str::<G>,
                        ),
                    ],
                ));
                results.push(register_scalar_fn_set(
                    con,
                    &name("lonlat_to_cell"),
                    &[(
                        &[type_double, type_double, type_bigint],
                        type_ubigint,
                        scalar_grid_lonlat_to_cell::<G>,
                    )],
                ));
                let overloads: Vec<ScalarOverload<'_>> = polygon_params
                    .iter()
                    .flat_map(|params| {
                        [
                            (
                                &params[..2],
                                type_cell_list,
                                scalar_grid_polygon_to_cells::<G> as duckdb_scalar_function_t,
                            ),
                            (
                                &params[..],
                                type_cell_list,
                                scalar_grid_polygon_to_cells_mode::<G> as duckdb_scalar_function_t,
                            ),
                        ]
                    })
                    .collect();
                results.push(register_scalar_fn_set_ex(
                    con,
                    &name("polygon_to_cells"),
                    &overloads,
                    cfg!(feature = "duckdb-api-v1_5_6").then_some(grid),
                ));
            }};
        }
        register_grid_scalars!(GridKind::S2);
        register_grid_scalars!(GridKind::Healpix);
        let mut type_cell_list_mut = type_cell_list;
        duckdb_destroy_logical_type(&mut type_cell_list_mut);

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

        results.into_iter().collect()
    }
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
        assert_eq!(cell.resolution(), Resolution::Eight);

        let parent_res7 = cell.parent(Resolution::Seven).unwrap();
        assert_eq!(parent_res7.resolution(), Resolution::Seven);

        let parent_res6 = cell.parent(Resolution::Six).unwrap();
        assert_eq!(parent_res6.resolution(), Resolution::Six);

        // Child cannot have parent at higher resolution
        assert!(cell.parent(Resolution::Nine).is_none());
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
        // SAFETY: built above with the variant matching its length; the pointer
        // variant references a live local buffer of at least `length` bytes.
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
        // SAFETY: built above with the variant matching its length; the pointer
        // variant references a live local buffer of at least `length` bytes.
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
        // SAFETY: built above with the variant matching its length; the pointer
        // variant references a live local buffer of at least `length` bytes.
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
        // SAFETY: built above with the variant matching its length; the pointer
        // variant references a live local buffer of at least `length` bytes.
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
        // SAFETY: built above with the variant matching its length; the pointer
        // variant references a live local buffer of at least `length` bytes.
        assert_eq!(unsafe { s15.as_str() }, "8828308281fffff");
    }
}
