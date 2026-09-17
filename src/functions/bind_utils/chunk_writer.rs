use std::ffi::CString;

#[cfg(test)]
use crate::encoding::WKB_BUF_LEN;
use crate::encoding::{fast_hex_u64, h3_index_to_wkb, WkbBuf};
use crate::ffi::{
    duckdb_data_chunk, duckdb_data_chunk_get_column_count, duckdb_data_chunk_get_vector,
    duckdb_data_chunk_set_size, duckdb_vector, duckdb_vector_assign_string_element,
    duckdb_vector_assign_string_element_len, duckdb_vector_get_data, duckdb_vector_set_row_invalid,
    get_vector_size, idx_t,
};

/// Ergonomic wrapper around DuckDB's output `duckdb_data_chunk` with row and column bounds checking.
///
/// # Safety Notes
/// While `ChunkWriter` validates vector row indices against `vector_size` and column indices against
/// chunk column capacity, write methods remain `unsafe`: the caller must guarantee that the target
/// column matches the expected physical type `T` and that vectors are not concurrently accessed.
pub struct ChunkWriter {
    pub chunk: duckdb_data_chunk,
    pub vector_size: usize,
    pub column_count: usize,
}

impl ChunkWriter {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    #[inline(always)]
    pub fn new(chunk: duckdb_data_chunk) -> Self {
        let column_count = if chunk.is_null() {
            0
        } else {
            unsafe { duckdb_data_chunk_get_column_count(chunk) as usize }
        };
        Self {
            chunk,
            vector_size: get_vector_size(),
            column_count,
        }
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    #[inline(always)]
    pub fn with_vector_size(chunk: duckdb_data_chunk, vector_size: usize) -> Self {
        let column_count = if chunk.is_null() {
            0
        } else {
            unsafe { duckdb_data_chunk_get_column_count(chunk) as usize }
        };
        Self {
            chunk,
            vector_size: vector_size.max(1),
            column_count,
        }
    }

    /// Retrieve the underlying vector for a specific column index, bounds-checked against column count
    #[inline(always)]
    pub unsafe fn get_vector(&self, col_idx: usize) -> duckdb_vector {
        if self.chunk.is_null() || col_idx >= self.column_count {
            return std::ptr::null_mut();
        }
        duckdb_data_chunk_get_vector(self.chunk, col_idx as idx_t)
    }

    /// Obtain a typed mutable slice over a vector's raw data buffer.
    ///
    /// # Safety
    /// The chunk must be live, the column must have physical type `T`, and its
    /// buffer must hold `len` elements. No other references may alias the buffer
    /// for the lifetime of the returned slice.
    #[inline(always)]
    pub unsafe fn get_data_slice_mut<T>(&mut self, col_idx: usize, len: usize) -> &mut [T] {
        if len == 0 {
            return &mut [];
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return &mut [];
        }
        let ptr = duckdb_vector_get_data(v) as *mut T;
        if ptr.is_null() {
            return &mut [];
        }
        let safe_len = len.min(self.vector_size);
        std::slice::from_raw_parts_mut(ptr, safe_len)
    }

    /// Set an i64 integer sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_int64(&self, col_idx: usize, row_idx: usize, val: i64) {
        if row_idx >= self.vector_size {
            return;
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return;
        }
        let ptr = duckdb_vector_get_data(v) as *mut i64;
        if ptr.is_null() {
            return;
        }
        *ptr.add(row_idx) = val;
    }

    /// Set a u64 unsigned integer sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_uint64(&self, col_idx: usize, row_idx: usize, val: u64) {
        if row_idx >= self.vector_size {
            return;
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return;
        }
        let ptr = duckdb_vector_get_data(v) as *mut u64;
        if ptr.is_null() {
            return;
        }
        *ptr.add(row_idx) = val;
    }

    /// Set a u8 unsigned integer sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_uint8(&self, col_idx: usize, row_idx: usize, val: u8) {
        if row_idx >= self.vector_size {
            return;
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return;
        }
        let ptr = duckdb_vector_get_data(v) as *mut u8;
        if ptr.is_null() {
            return;
        }
        *ptr.add(row_idx) = val;
    }

    /// Set an f64 double sample at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_double(&self, col_idx: usize, row_idx: usize, val: f64) {
        if row_idx >= self.vector_size {
            return;
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return;
        }
        let ptr = duckdb_vector_get_data(v) as *mut f64;
        if ptr.is_null() {
            return;
        }
        *ptr.add(row_idx) = val;
    }

    /// Assign a null-terminated UTF-8 string at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_string(&self, col_idx: usize, row_idx: usize, s: &str) {
        if row_idx >= self.vector_size {
            return;
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return;
        }
        let c_s = CString::new(s).unwrap_or_default();
        duckdb_vector_assign_string_element(
            v,
            row_idx as idx_t,
            c_s.as_ptr() as *const std::ffi::c_char,
        );
    }

    /// Assign a string slice or binary blob bytes of known length at (col_idx, row_idx)
    #[inline(always)]
    pub unsafe fn set_string_bytes(&self, col_idx: usize, row_idx: usize, bytes: &[u8]) {
        if row_idx >= self.vector_size {
            return;
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return;
        }
        duckdb_vector_assign_string_element_len(
            v,
            row_idx as idx_t,
            bytes.as_ptr() as *const std::ffi::c_char,
            bytes.len() as idx_t,
        );
    }

    /// Set a NULL value at (col_idx, row_idx) by updating the DuckDB validity mask
    #[inline(always)]
    pub unsafe fn set_null(&self, col_idx: usize, row_idx: usize) {
        if row_idx >= self.vector_size {
            return;
        }
        let v = self.get_vector(col_idx);
        if v.is_null() {
            return;
        }
        duckdb_vector_set_row_invalid(v, row_idx as idx_t);
    }

    /// Fill a primitive copyable column slice from an iterator
    #[inline(always)]
    pub unsafe fn fill_column<T: Copy, I: IntoIterator<Item = T>>(
        &mut self,
        col_idx: usize,
        count: usize,
        values: I,
    ) {
        let safe_count = count.min(self.vector_size);
        let slice: &mut [T] = self.get_data_slice_mut(col_idx, safe_count);
        for (dest, val) in slice.iter_mut().zip(values.into_iter().take(safe_count)) {
            *dest = val;
        }
    }

    /// Write formatted 16-character hexadecimal H3 cell index strings using scratch buffer
    #[inline(always)]
    pub unsafe fn write_hex_column<'a, I>(&self, out_idx: usize, cells: I, hex_buf: &mut [u8; 16])
    where
        I: IntoIterator<Item = &'a u64>,
    {
        for (i, &cell_u64) in cells.into_iter().take(self.vector_size).enumerate() {
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
        wkb_buf: &mut WkbBuf,
    ) where
        I: IntoIterator<Item = &'a u64>,
    {
        if out_idx_wkb.is_none() && out_idx_geom.is_none() {
            return;
        }
        for (i, &cell_u64) in cells.into_iter().take(self.vector_size).enumerate() {
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
        let safe_size = size.min(self.vector_size);
        duckdb_data_chunk_set_size(self.chunk, safe_size as idx_t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_writer_null_pointer_safety() {
        let mut writer = ChunkWriter::new(std::ptr::null_mut());
        assert_eq!(writer.vector_size, get_vector_size());

        unsafe {
            // Null chunk/vector returns empty slice without panicking
            let slice: &mut [f64] = writer.get_data_slice_mut(0, 10);
            assert!(slice.is_empty());

            // Null writes do not crash
            writer.set_int64(0, 0, 42);
            writer.set_uint64(0, 0, 42);
            writer.set_uint8(0, 0, 42);
            writer.set_double(0, 0, 42.0);
            writer.set_string(0, 0, "test");
            writer.set_string_bytes(0, 0, b"test");
            writer.set_null(0, 0);
            writer.fill_column(0, 5, [1.0, 2.0, 3.0]);
            let mut hex_buf = [0u8; 16];
            writer.write_hex_column(0, &[0x8828308281fffffu64], &mut hex_buf);
            let mut wkb_buf: WkbBuf = [0u8; WKB_BUF_LEN];
            writer.write_wkb_and_geom_columns(
                Some(0),
                Some(1),
                &[0x8828308281fffffu64],
                &mut wkb_buf,
            );
        }
    }

    #[test]
    fn test_chunk_writer_capacity_bounds() {
        let writer = ChunkWriter::with_vector_size(std::ptr::null_mut(), 100);
        assert_eq!(writer.vector_size, 100);

        unsafe {
            // Out of bounds row index is safely rejected
            writer.set_int64(0, 200, 42);
            writer.set_null(0, 200);
        }
    }

    #[test]
    fn test_chunk_writer_column_bounds() {
        let writer = ChunkWriter::new(std::ptr::null_mut());
        assert_eq!(writer.column_count, 0);

        unsafe {
            // Out of bounds column index returns null vector safely
            let v = writer.get_vector(5);
            assert!(v.is_null());

            // Out of bounds writes are safely ignored
            writer.set_int64(10, 0, 42);
            writer.set_uint64(10, 0, 42);
            writer.set_double(10, 0, 42.0);
            writer.set_string(10, 0, "test");
            writer.set_null(10, 0);
        }
    }
}
