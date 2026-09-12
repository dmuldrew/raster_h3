//! Shared DuckDB Table Function Parameter & Binding Utilities
//!
//! Provides a safe, ergonomic abstraction over DuckDB C FFI bind and function registration APIs.
//! Centralizes parameter extraction, type conversion, bounding box resolution, and column definitions
//! across continuous, categorical, parquet, and pmtiles table functions.

use std::ffi::CString;

use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::{
    duckdb_bind_add_result_column, duckdb_bind_get_named_parameter, duckdb_bind_get_parameter,
    duckdb_bind_get_parameter_count, duckdb_bind_info, duckdb_bind_set_error,
    duckdb_create_logical_type, duckdb_data_chunk, duckdb_data_chunk_get_vector,
    duckdb_data_chunk_set_size, duckdb_destroy_logical_type, duckdb_get_bool, duckdb_get_double,
    duckdb_get_int64, duckdb_get_uint64, duckdb_get_varchar, duckdb_logical_type,
    duckdb_table_function, duckdb_table_function_add_named_parameter,
    duckdb_table_function_add_parameter, duckdb_value, duckdb_vector,
    duckdb_vector_assign_string_element, duckdb_vector_assign_string_element_len,
    duckdb_vector_get_data, from_duckdb_string, idx_t, to_c_string, DuckDBType,
};
use crate::raster::mosaic::OverlapRule;

/// Ergonomic, safe wrapper around DuckDB's `duckdb_bind_info`
pub struct BindHelper {
    pub info: duckdb_bind_info,
}

impl BindHelper {
    #[inline(always)]
    pub fn new(info: duckdb_bind_info) -> Self {
        Self { info }
    }

    /// Number of positional parameters passed to the function
    #[inline(always)]
    pub fn parameter_count(&self) -> usize {
        unsafe { duckdb_bind_get_parameter_count(self.info) as usize }
    }

    /// Get a positional parameter value by index
    #[inline(always)]
    pub fn get_parameter(&self, index: usize) -> duckdb_value {
        unsafe { duckdb_bind_get_parameter(self.info, index as u64) }
    }

    /// Extract a positional VARCHAR parameter as a Rust `String`
    pub fn get_string_param(&self, index: usize) -> Option<String> {
        if index >= self.parameter_count() {
            return None;
        }
        let val = self.get_parameter(index);
        if val.is_null() {
            return None;
        }
        unsafe { from_duckdb_string(duckdb_get_varchar(val)) }
    }

    /// Extract a positional BIGINT parameter as `i64`
    pub fn get_int_param(&self, index: usize) -> Option<i64> {
        if index >= self.parameter_count() {
            return None;
        }
        let val = self.get_parameter(index);
        if val.is_null() {
            return None;
        }
        unsafe { Some(duckdb_get_int64(val)) }
    }

    /// Extract a positional DOUBLE parameter as `f64`
    pub fn get_double_param(&self, index: usize) -> Option<f64> {
        if index >= self.parameter_count() {
            return None;
        }
        let val = self.get_parameter(index);
        if val.is_null() {
            return None;
        }
        unsafe { Some(duckdb_get_double(val)) }
    }

    /// Get a named parameter by name
    pub fn get_named_parameter(&self, name: &str) -> duckdb_value {
        let c_name = to_c_string(name);
        unsafe { duckdb_bind_get_named_parameter(self.info, c_name.as_ptr()) }
    }

    /// Extract a named VARCHAR parameter as `String`
    pub fn get_named_string(&self, name: &str) -> Option<String> {
        let val = self.get_named_parameter(name);
        if val.is_null() {
            return None;
        }
        unsafe { from_duckdb_string(duckdb_get_varchar(val)) }
    }

    /// Extract a named BIGINT parameter as `i64`
    pub fn get_named_int(&self, name: &str) -> Option<i64> {
        let val = self.get_named_parameter(name);
        if val.is_null() {
            return None;
        }
        unsafe { Some(duckdb_get_int64(val)) }
    }

    /// Extract a named UBIGINT parameter as `u64`
    pub fn get_named_uint(&self, name: &str) -> Option<u64> {
        let val = self.get_named_parameter(name);
        if val.is_null() {
            return None;
        }
        unsafe { Some(duckdb_get_uint64(val)) }
    }

    /// Extract a named DOUBLE parameter as `f64`
    pub fn get_named_double(&self, name: &str) -> Option<f64> {
        let val = self.get_named_parameter(name);
        if val.is_null() {
            return None;
        }
        unsafe { Some(duckdb_get_double(val)) }
    }

    /// Extract a named BOOLEAN parameter as `bool`
    pub fn get_named_bool(&self, name: &str) -> Option<bool> {
        let val = self.get_named_parameter(name);
        if val.is_null() {
            return None;
        }
        unsafe { Some(duckdb_get_bool(val)) }
    }

    /// Set an error message on the bind context and halt execution
    pub fn set_error(&self, msg: &str) {
        let c_err = CString::new(msg).unwrap_or_else(|_| CString::new("Error in table function bind").unwrap());
        unsafe {
            duckdb_bind_set_error(self.info, c_err.as_ptr());
        }
    }

    /// Add a result column to the output schema with automated logical type lifecycle management
    pub fn add_result_column(&self, name: &str, duckdb_type: DuckDBType) {
        unsafe {
            let col_name = to_c_string(name);
            let mut logical_type = duckdb_create_logical_type(duckdb_type);
            duckdb_bind_add_result_column(self.info, col_name.as_ptr(), logical_type);
            duckdb_destroy_logical_type(&mut logical_type);
        }
    }

    /// Add a custom logical type column (e.g. GEOMETRY)
    pub fn add_custom_result_column(&self, name: &str, logical_type: duckdb_logical_type) {
        unsafe {
            let col_name = to_c_string(name);
            duckdb_bind_add_result_column(self.info, col_name.as_ptr(), logical_type);
        }
    }

    // =========================================================================
    // High-Level Standard Raster Parameter Parsers
    // =========================================================================

    /// Parse target H3 resolutions across all supported conventions:
    /// 1. `resolutions := '7,8'` (comma/whitespace separated list)
    /// 2. `min_resolution := 6, max_resolution := 8` (inclusive integer range)
    /// 3. `resolution := 8` (named integer)
    /// 4. Positional fallback parameter at `positional_idx`
    pub fn parse_resolutions(&self, default_res: u8, positional_idx: Option<usize>) -> Vec<u8> {
        // 1. Named parameter: resolutions (VARCHAR, e.g. '7,8' or '7, 8, 9')
        if let Some(s) = self.get_named_string("resolutions") {
            let list = parse_resolutions_str(&s);
            if !list.is_empty() {
                return list;
            }
        }

        // 2. Named parameters: min_resolution and max_resolution (BIGINT)
        let min_res_val = self.get_named_int("min_resolution");
        let max_res_val = self.get_named_int("max_resolution");
        if let (Some(min_r), Some(max_r)) = (min_res_val, max_res_val) {
            if (0..=15).contains(&min_r) && (0..=15).contains(&max_r) && min_r <= max_r {
                return ((min_r as u8)..=(max_r as u8)).collect();
            }
        }

        // 3. Named parameter: resolution (BIGINT)
        if let Some(res_int) = self.get_named_int("resolution") {
            if (0..=15).contains(&res_int) {
                return vec![res_int as u8];
            }
        }

        // 4. Positional fallback parameter
        if let Some(pos_idx) = positional_idx {
            if let Some(res_int) = self.get_int_param(pos_idx) {
                if (0..=15).contains(&res_int) {
                    return vec![res_int as u8];
                }
            }
        }

        vec![default_res]
    }

    /// Parse spatial bounding box from coordinates or H3 cell:
    /// 1. `min_lon, min_lat, max_lon, max_lat` (4 explicit coordinates)
    /// 2. `bbox := 'min_lon,min_lat,max_lon,max_lat'` (list/string)
    /// 3. `h3_cell := 613725953826226175` (BIGINT/UBIGINT)
    /// 4. `h3_hex := '8846480db3fffff'` (VARCHAR)
    pub fn parse_bbox(&self) -> Option<[f64; 4]> {
        // 1. Explicit min_lon, min_lat, max_lon, max_lat
        let min_lon = self.get_named_double("min_lon");
        let min_lat = self.get_named_double("min_lat");
        let max_lon = self.get_named_double("max_lon");
        let max_lat = self.get_named_double("max_lat");

        if let (Some(min_x), Some(min_y), Some(max_x), Some(max_y)) = (min_lon, min_lat, max_lon, max_lat) {
            return Some([min_x, min_y, max_x, max_y]);
        }

        // 2. Comma-separated bbox string
        if let Some(s) = self.get_named_string("bbox") {
            if let Some(bbox) = parse_bbox_str(&s) {
                return Some(bbox);
            }
        }

        // 3. H3 cell index (u64 / i64)
        if let Some(cell_u64) = self.get_named_uint("h3_cell").or_else(|| self.get_named_int("h3_cell").map(|v| v as u64)) {
            if let Ok(cell) = h3o::CellIndex::try_from(cell_u64) {
                let ll: h3o::LatLng = cell.into();
                let r = crate::pmtiles::tiler::max_hex_radius_deg(cell.resolution().into());
                return Some([ll.lng() - r, ll.lat() - r, ll.lng() + r, ll.lat() + r]);
            }
        }

        // 4. H3 hex string
        if let Some(hex_str) = self.get_named_string("h3_hex") {
            if let Ok(cell) = hex_str.trim().parse::<h3o::CellIndex>() {
                let ll: h3o::LatLng = cell.into();
                let r = crate::pmtiles::tiler::max_hex_radius_deg(cell.resolution().into());
                return Some([ll.lng() - r, ll.lat() - r, ll.lng() + r, ll.lat() + r]);
            }
        }

        None
    }

    /// Parse sampling pattern (`center`, `5point`, `9point`, `cross`, `9point_bilinear`)
    pub fn parse_sampling(&self) -> SamplingPattern {
        if let Some(s) = self.get_named_string("sampling") {
            SamplingPattern::parse(&s)
        } else {
            SamplingPattern::default()
        }
    }

    /// Parse mosaic overlap rule (`cutline`, `replace`, `highest_resolution`)
    pub fn parse_overlap_rule(&self) -> OverlapRule {
        if let Some(s) = self.get_named_string("overlap_rule") {
            OverlapRule::parse(&s)
        } else {
            OverlapRule::default()
        }
    }

    /// Parse source coordinate reference system override (`source_crs` or `crs`)
    pub fn parse_source_crs(&self) -> Option<String> {
        self.get_named_string("source_crs")
            .or_else(|| self.get_named_string("crs"))
    }

    /// Parse thread/worker limit (`workers` or `threads`)
    pub fn parse_workers(&self) -> Option<usize> {
        self.get_named_int("workers")
            .or_else(|| self.get_named_int("threads"))
            .and_then(|w| if w > 0 { Some(w as usize) } else { None })
    }
}

// =============================================================================
// Common Raster Binding Parameters Struct
// =============================================================================

/// Standard parameters common to all raster aggregation table functions
pub struct CommonRasterBindParams {
    pub file_path: String,
    pub resolutions: Vec<u8>,
    pub band: usize,
    pub custom_nodata: Option<f64>,
    pub source_crs: Option<String>,
    pub sampling: SamplingPattern,
    pub bbox: Option<[f64; 4]>,
    pub overlap_rule: OverlapRule,
    pub compact: bool,
}

impl CommonRasterBindParams {
    /// Extract all common raster parameters from bind context
    pub fn extract(bind: &BindHelper, func_name: &str, default_compact: bool) -> Option<Self> {
        let file_path = match bind.get_string_param(0) {
            Some(p) => p,
            None => {
                bind.set_error(&format!("{} requires at least 1 argument: file_path", func_name));
                return None;
            }
        };

        let resolutions = bind.parse_resolutions(8, Some(1));
        let band = bind.get_named_int("band").unwrap_or(1).max(1) as usize;
        let custom_nodata = bind.get_named_double("nodata");
        let source_crs = bind.parse_source_crs();
        let sampling = bind.parse_sampling();
        let bbox = bind.parse_bbox();
        let overlap_rule = bind.parse_overlap_rule();
        let compact = bind.get_named_bool("compact").unwrap_or(default_compact);

        Some(Self {
            file_path,
            resolutions,
            band,
            custom_nodata,
            source_crs,
            sampling,
            bbox,
            overlap_rule,
            compact,
        })
    }
}

// =============================================================================
// Function Registration Helpers
// =============================================================================

/// Add a positional parameter with automated logical type lifecycle management
pub unsafe fn add_positional_parameter(
    func: duckdb_table_function,
    duckdb_type: DuckDBType,
) {
    let mut logical_type = duckdb_create_logical_type(duckdb_type);
    duckdb_table_function_add_parameter(func, logical_type);
    duckdb_destroy_logical_type(&mut logical_type);
}

/// Add a named parameter with automated logical type lifecycle management
pub unsafe fn add_named_parameter(
    func: duckdb_table_function,
    name: &str,
    duckdb_type: DuckDBType,
) {
    let param_name = to_c_string(name);
    let mut logical_type = duckdb_create_logical_type(duckdb_type);
    duckdb_table_function_add_named_parameter(func, param_name.as_ptr(), logical_type);
    duckdb_destroy_logical_type(&mut logical_type);
}

/// Register standard named parameters shared across all raster aggregation functions
pub unsafe fn register_common_raster_named_parameters(func: duckdb_table_function) {
    add_named_parameter(func, "resolution", DuckDBType::BigInt);
    add_named_parameter(func, "resolutions", DuckDBType::Varchar);
    add_named_parameter(func, "min_resolution", DuckDBType::BigInt);
    add_named_parameter(func, "max_resolution", DuckDBType::BigInt);
    add_named_parameter(func, "sampling", DuckDBType::Varchar);
    add_named_parameter(func, "band", DuckDBType::BigInt);
    add_named_parameter(func, "nodata", DuckDBType::Double);
    add_named_parameter(func, "source_crs", DuckDBType::Varchar);
    add_named_parameter(func, "crs", DuckDBType::Varchar);
    add_named_parameter(func, "min_lon", DuckDBType::Double);
    add_named_parameter(func, "min_lat", DuckDBType::Double);
    add_named_parameter(func, "max_lon", DuckDBType::Double);
    add_named_parameter(func, "max_lat", DuckDBType::Double);
    add_named_parameter(func, "bbox", DuckDBType::Varchar);
    add_named_parameter(func, "h3_cell", DuckDBType::BigInt);
    add_named_parameter(func, "h3_hex", DuckDBType::Varchar);
    add_named_parameter(func, "compact", DuckDBType::Boolean);
    add_named_parameter(func, "overlap_rule", DuckDBType::Varchar);
    add_named_parameter(func, "workers", DuckDBType::BigInt);
    add_named_parameter(func, "threads", DuckDBType::BigInt);
}

// =============================================================================
// Output Data Chunk Writer
// =============================================================================

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

    /// Set the total number of valid rows in this data chunk
    #[inline(always)]
    pub unsafe fn set_size(&self, size: usize) {
        duckdb_data_chunk_set_size(self.chunk, size as idx_t);
    }
}

// =============================================================================
// Pure Parsing Helpers & Unit Tests
// =============================================================================

/// Parse a list of H3 resolutions from a string (comma/whitespace separated, sorted, deduplicated, <= 15)
pub fn parse_resolutions_str(s: &str) -> Vec<u8> {
    let mut list: Vec<u8> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<u8>().ok())
        .filter(|&r| r <= 15)
        .collect();
    list.sort_unstable();
    list.dedup();
    list
}

/// Parse bounding box coordinates from a comma/whitespace separated string `[min_lon, min_lat, max_lon, max_lat]`
pub fn parse_bbox_str(s: &str) -> Option<[f64; 4]> {
    let coords: Vec<f64> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<f64>().ok())
        .collect();
    if coords.len() == 4 {
        Some([coords[0], coords[1], coords[2], coords[3]])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_resolutions_str() {
        assert_eq!(parse_resolutions_str("7,8"), vec![7, 8]);
        assert_eq!(parse_resolutions_str("8, 7, 8"), vec![7, 8]);
        assert_eq!(parse_resolutions_str("  6  7  8  "), vec![6, 7, 8]);
        assert_eq!(parse_resolutions_str("0,15,16,99"), vec![0, 15]);
        assert_eq!(parse_resolutions_str("invalid,foo"), Vec::<u8>::new());
        assert_eq!(parse_resolutions_str(""), Vec::<u8>::new());
    }

    #[test]
    fn test_parse_bbox_str() {
        assert_eq!(
            parse_bbox_str("-122.5,37.5,-122.0,38.0"),
            Some([-122.5, 37.5, -122.0, 38.0])
        );
        assert_eq!(
            parse_bbox_str("  -122.5   37.5   -122.0   38.0  "),
            Some([-122.5, 37.5, -122.0, 38.0])
        );
        assert_eq!(parse_bbox_str("-122.5,37.5,-122.0"), None);
        assert_eq!(parse_bbox_str("not,a,bbox,coords"), None);
        assert_eq!(parse_bbox_str(""), None);
    }
}

