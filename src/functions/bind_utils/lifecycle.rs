use std::ffi::{c_void, CString};
use std::path::PathBuf;
use std::sync::Arc;

use crate::ffi::{
    duckdb_init_get_column_count, duckdb_init_get_column_index, duckdb_init_info,
    duckdb_init_set_error, duckdb_init_set_init_data,
};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::mosaic::{MosaicReader, OverlapRule};

/// Approximate H3 cell areas in m^2 by resolution (0 to 15) for query planner cardinality estimation
pub const H3_AREA_M2: [f64; 16] = [
    4.357e12, 6.097e11, 8.680e10, 1.239e10, 1.770e9, 2.529e8,
    3.613e7, 5.161e6, 7.373e5, 1.053e5, 1.505e4, 2.150e3,
    3.071e2, 4.387e1, 6.268e0, 8.954e-1,
];

/// Estimate query cardinality (number of H3 cells emitted) for a set of raster sources and resolutions
pub fn estimate_raster_cardinality(resolved_paths: &[PathBuf], resolutions: &[u8]) -> u64 {
    if let Some(first_path) = resolved_paths.first() {
        if let Ok(reader) = GeoTiffStreamReader::open(first_path) {
            let w = reader.metadata.width as f64;
            let h = reader.metadata.height as f64;
            let total_pixels = (w * h) as u64 * resolved_paths.len() as u64;

            let (x0, y0) = reader.metadata.geotransform.pixel_to_coord(0.0, 0.0);
            let (x1, y1) = reader.metadata.geotransform.pixel_to_coord(w, h);
            let dx = (x1 - x0).abs();
            let dy = (y1 - y0).abs();

            let area_m2 = if matches!(reader.metadata.epsg, Some(4326)) || reader.metadata.epsg.is_none() {
                dx * 111_320.0 * dy * 110_540.0 * resolved_paths.len() as f64
            } else {
                dx * dy * resolved_paths.len() as f64
            };

            let mut total_hex_est = 0u64;
            for &res in resolutions {
                let hex_area = H3_AREA_M2.get(res as usize).copied().unwrap_or(7.373e5);
                let hex_count = (area_m2 / hex_area).ceil() as u64;
                total_hex_est = total_hex_est.saturating_add(hex_count.min(total_pixels).max(1));
            }
            total_hex_est.max(1)
        } else {
            10_000 * resolutions.len() as u64
        }
    } else {
        10_000 * resolutions.len() as u64
    }
}

/// Generic C-compatible deallocator for Box<T> allocated data pointers
pub unsafe extern "C" fn delete_boxed<T>(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut T));
    }
}

/// Attach boxed global init state to DuckDB table function lifecycle with type-safe destructor
pub unsafe fn set_table_function_init_data<T>(info: duckdb_init_info, data: T) {
    duckdb_init_set_init_data(
        info,
        Box::into_raw(Box::new(data)) as *mut c_void,
        Some(delete_boxed::<T>),
    );
}

/// Open a raster mosaic reader from resolved paths, setting DuckDB init error on failure
pub unsafe fn open_mosaic_or_set_error(
    info: duckdb_init_info,
    paths: &[PathBuf],
    bbox: Option<[f64; 4]>,
    source_crs: Option<&str>,
    overlap_rule: OverlapRule,
) -> Option<Arc<MosaicReader>> {
    match MosaicReader::open(paths, bbox, source_crs, overlap_rule) {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => {
            let err_msg = CString::new(format!("Failed to open raster mosaic: {}", e))
                .unwrap_or_else(|_| CString::new("Failed to open raster mosaic").unwrap());
            duckdb_init_set_error(info, err_msg.as_ptr());
            None
        }
    }
}

/// Unified thread-local state for table function execution, carrying reusable scratch buffers for hex string and WKB encoding
pub struct TableFunctionLocalData {
    pub thread_id: usize,
    pub hex_buf: [u8; 16],
    pub wkb_buf: [u8; 128],
}

impl Default for TableFunctionLocalData {
    fn default() -> Self {
        Self {
            thread_id: 0,
            hex_buf: [0u8; 16],
            wkb_buf: [0u8; 128],
        }
    }
}

impl TableFunctionLocalData {
    /// Resolve scratch buffers from thread-local state or fallback buffers
    #[inline(always)]
    pub unsafe fn get_scratch_buffers<'a>(
        ptr: *mut TableFunctionLocalData,
        fallback_hex: &'a mut [u8; 16],
        fallback_wkb: &'a mut [u8; 128],
    ) -> (&'a mut [u8; 16], &'a mut [u8; 128]) {
        if !ptr.is_null() {
            (&mut (*ptr).hex_buf, &mut (*ptr).wkb_buf)
        } else {
            (fallback_hex, fallback_wkb)
        }
    }
}

/// Standard thread-local initialization callback for DuckDB table functions
pub unsafe extern "C" fn init_table_function_local(info: duckdb_init_info) {
    let local_data = Box::new(TableFunctionLocalData::default());
    duckdb_init_set_init_data(
        info,
        Box::into_raw(local_data) as *mut c_void,
        Some(delete_boxed::<TableFunctionLocalData>),
    );
}

/// Extract projected column indices requested by DuckDB projection pushdown
pub unsafe fn extract_projected_columns(info: duckdb_init_info) -> Vec<usize> {
    let col_count = duckdb_init_get_column_count(info);
    let mut projected_columns = Vec::with_capacity(col_count as usize);
    for i in 0..col_count {
        projected_columns.push(duckdb_init_get_column_index(info, i) as usize);
    }
    projected_columns
}
