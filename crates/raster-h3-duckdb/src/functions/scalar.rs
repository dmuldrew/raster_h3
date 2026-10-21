use h3o::{CellIndex, Resolution};
use std::ffi::c_char;

#[cfg(test)]
use crate::encoding::WKB_BUF_LEN;
use crate::encoding::{fast_hex_u64, h3_index_to_wkb, parse_hex_u64, WkbBuf};
use crate::ffi::*;
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

/// Shared body of `h3_polygon_to_cells(geom, res[, containment])` -> `UBIGINT[]`.
///
/// Rows with a NULL argument, invalid WKB or a resolution outside `0..=15` yield NULL. An
/// unknown containment mode or a polygon over the cell limit fails the query, since retrying
/// row by row would not help.
///
/// # Safety
/// `info`, `input` and `output` must be live handles from a DuckDB scalar callback; `input`
/// must hold (BLOB|GEOMETRY, BIGINT[, VARCHAR when `has_mode`]) columns and `output` must
/// be a `UBIGINT[]` list vector.
unsafe fn polygon_to_cells_kernel(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
    has_mode: bool,
) {
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
                            "h3_polygon_to_cells: unknown containment mode '{name}' (expected one of \
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
            let cells = match scalar_ops::h3_polygon_to_cells_wkb(wkb, *p_res.add(i as usize), mode)
            {
                Ok(cells) => cells,
                Err(err @ scalar_ops::PolygonToCellsError::TooManyCells { .. }) => {
                    let msg = to_c_string(&format!("h3_polygon_to_cells: {err}"));
                    duckdb_scalar_function_set_error(info, msg.as_ptr());
                    return;
                }
                Err(_) => {
                    duckdb_vector_set_row_invalid(output, i);
                    continue;
                }
            };
            let new_total = total + cells.len() as u64;
            if duckdb_list_vector_reserve(output, new_total) != DuckDBState::Success {
                duckdb_scalar_function_set_error(
                    info,
                    c"h3_polygon_to_cells: failed to reserve list storage".as_ptr(),
                );
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
    // BIGINT) -> UBIGINT[]`, so the layout matches `polygon_to_cells_kernel` with `has_mode
    // = false`, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel(info, input, output, false);
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
    // BIGINT, VARCHAR) -> UBIGINT[]`, so the layout matches `polygon_to_cells_kernel` with
    // `has_mode = true`, which is what the kernel requires.
    unsafe {
        ffi_scalar_guard(info, || {
            polygon_to_cells_kernel(info, input, output, true);
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
        results.push(register_scalar_fn_set(
            con,
            "h3_polygon_to_cells",
            &polygon_overloads,
        ));
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
