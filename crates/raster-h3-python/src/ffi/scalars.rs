//! Scalar H3 helpers exposed to Python.

use std::ffi::{c_char, c_int, CString};
use std::ptr;

use raster_h3_core::error::catch_unwind_or;
use raster_h3_core::scalar_ops::{
    h3_cell_to_parent_str, h3_cell_to_parent_u64, h3_get_resolution, h3_is_valid_str,
    h3_is_valid_u64, h3_polygon_to_cells_wkb, h3_to_lat, h3_to_lng, h3_to_string, h3_to_wkb_u64,
    parse_containment_mode, string_to_h3, ContainmentMode, PolygonToCellsError,
};

use crate::ffi::opt_cstr;

/// C-ABI scalar helper: `h3_to_string(cell) -> *mut c_char` (NULL if invalid/0).
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_to_string(cell: u64) -> *mut c_char {
    catch_unwind_or(ptr::null_mut(), || match h3_to_string(cell) {
        Some(s) => CString::new(s).unwrap_or_default().into_raw(),
        None => ptr::null_mut(),
    })
}

/// C-ABI scalar helper: `string_to_h3(hex) -> u64`.
///
/// # Safety
/// `hex` must be a valid NUL-terminated string or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_string_to_h3(hex: *const c_char) -> u64 {
    // SAFETY: `hex` is null or NUL-terminated (caller contract), as `opt_cstr` requires.
    unsafe {
        catch_unwind_or(0, || match opt_cstr(hex) {
            Some(s) => string_to_h3(&s),
            None => 0,
        })
    }
}

/// C-ABI scalar helper: `h3_to_lat(cell) -> f64`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_to_lat(cell: u64) -> f64 {
    catch_unwind_or(f64::NAN, || h3_to_lat(cell))
}

/// C-ABI scalar helper: `h3_to_lng(cell) -> f64`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_to_lng(cell: u64) -> f64 {
    catch_unwind_or(f64::NAN, || h3_to_lng(cell))
}

/// C-ABI scalar helper: `h3_get_resolution(cell) -> i64`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_get_resolution(cell: u64) -> i64 {
    catch_unwind_or(-1, || h3_get_resolution(cell))
}

/// C-ABI scalar helper: `h3_is_valid_u64(cell) -> i32`.
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_is_valid_u64(cell: u64) -> c_int {
    catch_unwind_or(0, || c_int::from(h3_is_valid_u64(cell)))
}

/// C-ABI scalar helper: `h3_is_valid_str(hex) -> i32`.
///
/// # Safety
/// `hex` must be a valid NUL-terminated string or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_is_valid_str(hex: *const c_char) -> c_int {
    // SAFETY: `hex` is null or NUL-terminated (caller contract), as `opt_cstr` requires.
    unsafe {
        catch_unwind_or(0, || match opt_cstr(hex) {
            Some(s) => c_int::from(h3_is_valid_str(&s)),
            None => 0,
        })
    }
}

/// C-ABI scalar helper: `h3_cell_to_parent_u64(cell, parent_res) -> u64` (0 if invalid).
#[no_mangle]
pub extern "C" fn raster_h3_py_h3_cell_to_parent_u64(cell: u64, parent_res: i64) -> u64 {
    catch_unwind_or(0, || h3_cell_to_parent_u64(cell, parent_res).unwrap_or(0))
}

/// C-ABI scalar helper: `h3_cell_to_parent_str(hex, parent_res) -> *mut c_char`.
///
/// # Safety
/// `hex` must be a valid NUL-terminated string or NULL.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_cell_to_parent_str(
    hex: *const c_char,
    parent_res: i64,
) -> *mut c_char {
    // SAFETY: `hex` is null or NUL-terminated (caller contract), as `opt_cstr` requires;
    // the returned string is freed with `raster_h3_py_free_string`.
    unsafe {
        catch_unwind_or(ptr::null_mut(), || {
            let Some(s) = opt_cstr(hex) else {
                return ptr::null_mut();
            };
            match h3_cell_to_parent_str(&s, parent_res) {
                Some(out) => CString::new(out).unwrap_or_default().into_raw(),
                None => ptr::null_mut(),
            }
        })
    }
}

/// C-ABI helper: the WKB buffer capacity that always suffices for
/// [`raster_h3_py_h3_to_wkb`] (`raster_h3_core::encoding::WKB_BUF_LEN`).
#[no_mangle]
pub extern "C" fn raster_h3_py_wkb_buf_len() -> usize {
    raster_h3_core::encoding::WKB_BUF_LEN
}

/// C-ABI helper: write WKB polygon for `cell` into `out_buf` (capacity of
/// `raster_h3_core::encoding::WKB_BUF_LEN` bytes always suffices).
/// Returns number of bytes written, or `0` if `cell` is invalid or `buf_cap` is too small.
///
/// # Safety
/// `out_buf` must point to at least `buf_cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_to_wkb(
    cell: u64,
    out_buf: *mut u8,
    buf_cap: usize,
) -> usize {
    // SAFETY: non-null `out_buf` has `buf_cap` writable bytes (caller contract) and
    // `wkb.len() <= buf_cap` was checked, so the copy stays in bounds; `wkb` is a separate
    // Rust allocation, so the regions cannot overlap.
    unsafe {
        if out_buf.is_null() {
            return 0;
        }
        let Some(wkb) = catch_unwind_or(None, || h3_to_wkb_u64(cell)) else {
            return 0;
        };
        if wkb.len() > buf_cap {
            return 0;
        }
        ptr::copy_nonoverlapping(wkb.as_ptr(), out_buf, wkb.len());
        wkb.len()
    }
}

/// C-ABI scalar helper: `h3_polygon_to_cells(wkb, res, containment)`.
///
/// On success (`0`) writes a heap array of `*out_len` cells to `*out_cells`; release it with
/// [`raster_h3_py_free_cells`]. A NULL `containment` means `contains_centroid`. Other return
/// codes leave the outputs untouched: `1` invalid geometry or resolution (or an internal
/// panic), `2` unknown containment mode, `3` more cells than `H3_POLYGON_TO_CELLS_MAX_CELLS`,
/// `-1` NULL pointers.
///
/// # Safety
/// `wkb` must point to `wkb_len` readable bytes, `containment` must be NULL or a valid
/// NUL-terminated string, and `out_cells` / `out_len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_h3_polygon_to_cells(
    wkb: *const u8,
    wkb_len: usize,
    res: i64,
    containment: *const c_char,
    out_cells: *mut *mut u64,
    out_len: *mut usize,
) -> c_int {
    // SAFETY: `containment` is null or NUL-terminated, `wkb` addresses `wkb_len` readable
    // bytes that stay unchanged during the call, and the out-pointers are non-null
    // (checked) and writable (caller contract). The boxed slice handed out is freed by
    // `raster_h3_py_free_cells` with the same length.
    unsafe {
        if wkb.is_null() || out_cells.is_null() || out_len.is_null() {
            return -1;
        }
        let mode = match opt_cstr(containment) {
            None => ContainmentMode::ContainsCentroid,
            Some(name) => match parse_containment_mode(&name) {
                Some(mode) => mode,
                None => return 2,
            },
        };
        let bytes = std::slice::from_raw_parts(wkb, wkb_len);
        let Some(result) =
            catch_unwind_or(None, || Some(h3_polygon_to_cells_wkb(bytes, res, mode)))
        else {
            return 1;
        };
        match result {
            Ok(cells) => {
                let cells = cells.into_boxed_slice();
                *out_len = cells.len();
                *out_cells = Box::into_raw(cells) as *mut u64;
                0
            }
            Err(PolygonToCellsError::TooManyCells { .. }) => 3,
            Err(_) => 1,
        }
    }
}

/// Free a cell array returned by [`raster_h3_py_h3_polygon_to_cells`].
///
/// # Safety
/// `cells` / `len` must come from one successful `raster_h3_py_h3_polygon_to_cells` call
/// and not have been freed already.
#[no_mangle]
pub unsafe extern "C" fn raster_h3_py_free_cells(cells: *mut u64, len: usize) {
    // SAFETY: non-null `cells`/`len` came from one `Box<[u64]>` leaked by
    // `raster_h3_py_h3_polygon_to_cells` (caller contract), so rebuilding the fat pointer
    // with the same length reproduces that box, which is freed once.
    unsafe {
        if !cells.is_null() {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(cells, len)));
        }
    }
}
