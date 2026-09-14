use std::ffi::CString;

use crate::ffi::{
    duckdb_data_chunk, duckdb_data_chunk_get_vector, duckdb_data_chunk_set_size, duckdb_vector,
    duckdb_vector_assign_string_element, duckdb_vector_assign_string_element_len,
    duckdb_vector_get_data, idx_t,
};
use crate::functions::fast_hex::fast_hex_u64;
use crate::functions::wkb::h3_index_to_wkb;

/// Safe, ergonomic wrapper around DuckDB's output `duckdb_data_chunk`
pub struct ChunkWriter {
    pub chunk: duckdb_data_chunk,
}

impl ChunkWriter {
    #[inline(always)]
    pub fn new(chunk: duckdb_data_chunk) -> Self {
        Self { chunk }
    }

    /// Retrieve the underlying vector for a specific column index
    #[inline(always)]
    pub unsafe fn get_vector(&self, col_idx: usize) -> duckdb_vector {
        duckdb_data_chunk_get_vector(self.chunk, col_idx as idx_t)
    }

    /// Obtain a typed mutable slice over a vector's raw data buffer.
    ///
    /// This allows idiomatic, bounds-check-free, and auto-vectorizable writes:
    /// ```rust,ignore
    /// let slice: &mut [f64] = writer.get_data_slice_mut(out_idx, batch_len);
    /// for (dest, rec) in slice.iter_mut().zip(batch.iter()) {
    ///     *dest = rec.accumulator.mean();
    /// }
    /// ```
    #[inline(always)]
    pub unsafe fn get_data_slice_mut<T>(&self, col_idx: usize, len: usize) -> &mut [T] {
        let v = self.get_vector(col_idx);
        let ptr = duckdb_vector_get_data(v) as *mut T;
        std::slice::from_raw_parts_mut(ptr, len)
    }

    /// Set an i64 integer sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_int64(&self, col_idx: usize, row_idx: usize, val: i64) {
        let v = self.get_vector(col_idx);
        *(duckdb_vector_get_data(v) as *mut i64).add(row_idx) = val;
    }

    /// Set a u64 unsigned integer sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_uint64(&self, col_idx: usize, row_idx: usize, val: u64) {
        let v = self.get_vector(col_idx);
        *(duckdb_vector_get_data(v) as *mut u64).add(row_idx) = val;
    }

    /// Set a u8 unsigned integer sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_uint8(&self, col_idx: usize, row_idx: usize, val: u8) {
        let v = self.get_vector(col_idx);
        *(duckdb_vector_get_data(v) as *mut u8).add(row_idx) = val;
    }

    /// Set an f64 double sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_double(&self, col_idx: usize, row_idx: usize, val: f64) {
        let v = self.get_vector(col_idx);
        *(duckdb_vector_get_data(v) as *mut f64).add(row_idx) = val;
    }

    /// Assign a null-terminated UTF-8 string at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_string(&self, col_idx: usize, row_idx: usize, s: &str) {
        let v = self.get_vector(col_idx);
        let c_s = CString::new(s).unwrap_or_default();
        duckdb_vector_assign_string_element(v, row_idx as idx_t, c_s.as_ptr() as *const std::ffi::c_char);
    }

    /// Assign a string slice or binary blob bytes of known length at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_string_bytes(&self, col_idx: usize, row_idx: usize, bytes: &[u8]) {
        let v = self.get_vector(col_idx);
        duckdb_vector_assign_string_element_len(
            v,
            row_idx as idx_t,
            bytes.as_ptr() as *const std::ffi::c_char,
            bytes.len() as idx_t,
        );
    }

    /// Set a NULL value at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_null(&self, col_idx: usize, row_idx: usize) {
        let v = self.get_vector(col_idx);
        duckdb_vector_assign_string_element_len(v, row_idx as idx_t, std::ptr::null(), 0);
    }

    /// Fill a primitive copyable column slice from an iterator
    #[inline(always)]
    pub unsafe fn fill_column<T: Copy, I: IntoIterator<Item = T>>(
        &self,
        col_idx: usize,
        count: usize,
        values: I,
    ) {
        let slice: &mut [T] = self.get_data_slice_mut(col_idx, count);
        for (dest, val) in slice.iter_mut().zip(values) {
            *dest = val;
        }
    }

    /// Write formatted 16-character hexadecimal H3 cell index strings using scratch buffer
    #[inline(always)]
    pub unsafe fn write_hex_column<'a, I>(&self, out_idx: usize, cells: I, hex_buf: &mut [u8; 16])
    where
        I: IntoIterator<Item = &'a u64>,
    {
        for (i, &cell_u64) in cells.into_iter().enumerate() {
            let hex_slice = fast_hex_u64(cell_u64, hex_buf);
            self.set_string_bytes(out_idx, i, hex_slice);
        }
    }

    /// Write WKB and Native GEOMETRY columns from H3 cell indices with NULL handling
    #[inline(always)]
    pub unsafe fn write_wkb_and_geom_columns<'a, I>(
        &self,
        out_idx_wkb: Option<usize>,
        out_idx_geom: Option<usize>,
        cells: I,
        wkb_buf: &mut [u8; 128],
    ) where
        I: IntoIterator<Item = &'a u64>,
    {
        if out_idx_wkb.is_none() && out_idx_geom.is_none() {
            return;
        }
        for (i, &cell_u64) in cells.into_iter().enumerate() {
            if let Some(wkb_len) = h3_index_to_wkb(cell_u64, wkb_buf) {
                let bytes = &wkb_buf[..wkb_len];
                if let Some(out_wkb) = out_idx_wkb {
                    self.set_string_bytes(out_wkb, i, bytes);
                }
                if let Some(out_geom) = out_idx_geom {
                    self.set_string_bytes(out_geom, i, bytes);
                }
            } else {
                if let Some(out_wkb) = out_idx_wkb {
                    self.set_null(out_wkb, i);
                }
                if let Some(out_geom) = out_idx_geom {
                    self.set_null(out_geom, i);
                }
            }
        }
    }

    /// Set the total number of valid rows in this data chunk
    #[inline(always)]
    pub unsafe fn set_size(&self, size: usize) {
        duckdb_data_chunk_set_size(self.chunk, size as idx_t);
    }
}
