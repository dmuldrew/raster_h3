//! DuckDB Table Function for Direct GeoTIFF-to-PMTiles v3 Generation
//!
//! Exposes:
//!   SELECT * FROM h3_raster_to_pmtiles('california_dem.tif', 'california_elevation.pmtiles', resolution := 8);

use std::ffi::{c_char, c_void, CString};
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::aggregator::multi_horizon::MultiResolutionConfig;
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::{from_duckdb_string, to_c_string};
use crate::pmtiles::tiler::{h3_res_to_zoom, H3PmtilesTiler};

/// Bind data parsed during SQL query planning
pub struct PmtilesBindData {
    pub file_path: String,
    pub output_pmtiles: String,
    pub resolutions: Vec<u8>,
    pub band: usize,
    pub custom_nodata: Option<f64>,
    pub sampling: SamplingPattern,
}

/// Global execution state for the single-row generator
pub struct PmtilesGlobalData {
    pub executed: AtomicBool,
}

unsafe extern "C" fn delete_bind_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut PmtilesBindData));
    }
}

unsafe extern "C" fn delete_global_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut PmtilesGlobalData));
    }
}

/// Bind callback: parses input arguments and defines output table schema
pub unsafe extern "C" fn pmtiles_bind(info: duckdb_bind_info) {
    let param_count = duckdb_bind_get_parameter_count(info);
    if param_count < 2 {
        let err_msg = to_c_string("h3_raster_to_pmtiles requires at least 2 arguments: file_path and output_pmtiles");
        duckdb_bind_set_error(info, err_msg.as_ptr());
        return;
    }

    // 0: file_path (VARCHAR)
    let path_val = duckdb_bind_get_parameter(info, 0);
    let file_path = match from_duckdb_string(duckdb_get_varchar(path_val)) {
        Some(s) => s,
        None => {
            let err_msg = to_c_string("Invalid file_path parameter");
            duckdb_bind_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    // 1: output_pmtiles (VARCHAR)
    let out_val = duckdb_bind_get_parameter(info, 1);
    let output_pmtiles = match from_duckdb_string(duckdb_get_varchar(out_val)) {
        Some(s) => s,
        None => {
            let err_msg = to_c_string("Invalid output_pmtiles parameter");
            duckdb_bind_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let mut resolutions = vec![8u8];
    let mut band = 1usize;
    let mut custom_nodata = None;
    let mut sampling = SamplingPattern::center();

    // Named parameter: resolution (BIGINT)
    let name_res = to_c_string("resolution");
    let val_res = duckdb_bind_get_named_parameter(info, name_res.as_ptr());
    if !val_res.is_null() {
        let r = duckdb_get_int64(val_res);
        if (0..=15).contains(&r) {
            resolutions = vec![r as u8];
        }
    }

    // Named parameter: min_resolution & max_resolution
    let name_min_res = to_c_string("min_resolution");
    let val_min_res = duckdb_bind_get_named_parameter(info, name_min_res.as_ptr());
    let name_max_res = to_c_string("max_resolution");
    let val_max_res = duckdb_bind_get_named_parameter(info, name_max_res.as_ptr());
    if !val_min_res.is_null() && !val_max_res.is_null() {
        let min_r = duckdb_get_int64(val_min_res) as u8;
        let max_r = duckdb_get_int64(val_max_res) as u8;
        if min_r <= max_r && max_r <= 15 {
            resolutions = (min_r..=max_r).collect();
        }
    }

    // Named parameter: sampling (VARCHAR)
    let name_sampling = to_c_string("sampling");
    let val_sampling = duckdb_bind_get_named_parameter(info, name_sampling.as_ptr());
    if !val_sampling.is_null() {
        if let Some(s) = from_duckdb_string(duckdb_get_varchar(val_sampling)) {
            sampling = SamplingPattern::parse(&s);
        }
    }

    // Named parameter: band (BIGINT)
    let name_band = to_c_string("band");
    let val_band = duckdb_bind_get_named_parameter(info, name_band.as_ptr());
    if !val_band.is_null() {
        let b = duckdb_get_int64(val_band);
        if b > 0 {
            band = b as usize;
        }
    }

    // Named parameter: nodata (DOUBLE)
    let name_nodata = to_c_string("nodata");
    let val_nodata = duckdb_bind_get_named_parameter(info, name_nodata.as_ptr());
    if !val_nodata.is_null() {
        custom_nodata = Some(duckdb_get_double(val_nodata));
    }

    // Declare output columns:
    // 0: total_hexagons (BIGINT)
    let col_hex = to_c_string("total_hexagons");
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    duckdb_bind_add_result_column(info, col_hex.as_ptr(), type_bigint);

    // 1: pmtiles_size_bytes (BIGINT)
    let col_size = to_c_string("pmtiles_size_bytes");
    duckdb_bind_add_result_column(info, col_size.as_ptr(), type_bigint);

    // 2: min_zoom (BIGINT)
    let col_min_z = to_c_string("min_zoom");
    duckdb_bind_add_result_column(info, col_min_z.as_ptr(), type_bigint);

    // 3: max_zoom (BIGINT)
    let col_max_z = to_c_string("max_zoom");
    duckdb_bind_add_result_column(info, col_max_z.as_ptr(), type_bigint);

    // 4: elapsed_ms (DOUBLE)
    let col_time = to_c_string("elapsed_ms");
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    duckdb_bind_add_result_column(info, col_time.as_ptr(), type_double);

    // 5: output_path (VARCHAR)
    let col_path = to_c_string("output_path");
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_bind_add_result_column(info, col_path.as_ptr(), type_varchar);

    // 6: status (VARCHAR)
    let col_status = to_c_string("status");
    duckdb_bind_add_result_column(info, col_status.as_ptr(), type_varchar);

    // Cleanup types
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);

    // Set cardinality: 1 summary row
    duckdb_bind_set_cardinality(info, 1, true);

    let bind_data = Box::new(PmtilesBindData {
        file_path,
        output_pmtiles,
        resolutions,
        band,
        custom_nodata,
        sampling,
    });
    duckdb_bind_set_bind_data(info, Box::into_raw(bind_data) as *mut c_void, Some(delete_bind_data));
}

/// Init callback
pub unsafe extern "C" fn pmtiles_init(info: duckdb_init_info) {
    let global_data = Box::new(PmtilesGlobalData {
        executed: AtomicBool::new(false),
    });
    duckdb_init_set_init_data(info, Box::into_raw(global_data) as *mut c_void, Some(delete_global_data));
}

/// Scan callback: runs GeoTIFF-to-PMTiles conversion and streams the single summary row
pub unsafe extern "C" fn pmtiles_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    let bind_data = &*(duckdb_function_get_bind_data(info) as *const PmtilesBindData);
    let global_data = &*(duckdb_function_get_init_data(info) as *const PmtilesGlobalData);

    if global_data.executed.swap(true, Ordering::SeqCst) {
        duckdb_data_chunk_set_size(output, 0);
        return;
    }

    let mut config = MultiResolutionConfig::new(bind_data.resolutions.clone());
    config.band = bind_data.band;
    config.custom_nodata = bind_data.custom_nodata;
    config.sampling = bind_data.sampling.clone();

    let start = Instant::now();
    let result = H3PmtilesTiler::process_geotiff_to_pmtiles(
        &bind_data.file_path,
        &bind_data.output_pmtiles,
        config,
    );
    let elapsed = start.elapsed();

    let (total_hexagons, size_bytes, status) = match result {
        Ok(count) => {
            let sz = File::open(&bind_data.output_pmtiles)
                .and_then(|f| f.metadata())
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            (count as i64, sz, "SUCCESS".to_string())
        }
        Err(e) => (0i64, 0i64, format!("ERROR: {}", e)),
    };

    let mut min_z = 255u8;
    let mut max_z = 0u8;
    for &r in &bind_data.resolutions {
        let z = h3_res_to_zoom(r);
        if z < min_z { min_z = z; }
        if z > max_z { max_z = z; }
    }

    // Populate the 1 output row
    // 0: total_hexagons (BIGINT)
    let v_hex = duckdb_data_chunk_get_vector(output, 0);
    *(duckdb_vector_get_data(v_hex) as *mut i64) = total_hexagons;

    // 1: pmtiles_size_bytes (BIGINT)
    let v_size = duckdb_data_chunk_get_vector(output, 1);
    *(duckdb_vector_get_data(v_size) as *mut i64) = size_bytes;

    // 2: min_zoom (BIGINT)
    let v_min_z = duckdb_data_chunk_get_vector(output, 2);
    *(duckdb_vector_get_data(v_min_z) as *mut i64) = min_z as i64;

    // 3: max_zoom (BIGINT)
    let v_max_z = duckdb_data_chunk_get_vector(output, 3);
    *(duckdb_vector_get_data(v_max_z) as *mut i64) = max_z as i64;

    // 4: elapsed_ms (DOUBLE)
    let v_time = duckdb_data_chunk_get_vector(output, 4);
    *(duckdb_vector_get_data(v_time) as *mut f64) = elapsed.as_secs_f64() * 1000.0;

    // 5: output_path (VARCHAR)
    let v_path = duckdb_data_chunk_get_vector(output, 5);
    let path_c = CString::new(bind_data.output_pmtiles.clone()).unwrap_or_default();
    duckdb_vector_assign_string_element(v_path, 0, path_c.as_ptr() as *const c_char);

    // 6: status (VARCHAR)
    let v_status = duckdb_data_chunk_get_vector(output, 6);
    let status_c = CString::new(status).unwrap_or_default();
    duckdb_vector_assign_string_element(v_status, 0, status_c.as_ptr() as *const c_char);

    duckdb_data_chunk_set_size(output, 1);
}

/// Bind data parsed during SQL query planning for Parquet to PMTiles
pub struct ParquetPmtilesBindData {
    pub parquet_path: String,
    pub output_pmtiles: String,
    pub h3_column: Option<String>,
}

/// Global execution state for the single-row generator
pub struct ParquetPmtilesGlobalData {
    pub executed: AtomicBool,
}

unsafe extern "C" fn delete_parquet_bind_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut ParquetPmtilesBindData));
    }
}

unsafe extern "C" fn delete_parquet_global_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut ParquetPmtilesGlobalData));
    }
}

/// Bind callback: parses input arguments for `h3_parquet_to_pmtiles`
pub unsafe extern "C" fn parquet_pmtiles_bind(info: duckdb_bind_info) {
    let param_count = duckdb_bind_get_parameter_count(info);
    if param_count < 2 {
        let err_msg = to_c_string("h3_parquet_to_pmtiles requires at least 2 arguments: parquet_path and output_pmtiles");
        duckdb_bind_set_error(info, err_msg.as_ptr());
        return;
    }

    // 0: parquet_path (VARCHAR)
    let path_val = duckdb_bind_get_parameter(info, 0);
    let parquet_path = match from_duckdb_string(duckdb_get_varchar(path_val)) {
        Some(s) => s,
        None => {
            let err_msg = to_c_string("Invalid parquet_path parameter");
            duckdb_bind_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    // 1: output_pmtiles (VARCHAR)
    let out_val = duckdb_bind_get_parameter(info, 1);
    let output_pmtiles = match from_duckdb_string(duckdb_get_varchar(out_val)) {
        Some(s) => s,
        None => {
            let err_msg = to_c_string("Invalid output_pmtiles parameter");
            duckdb_bind_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let mut h3_column = None;

    // Named parameter: h3_column / h3_col (VARCHAR)
    let name_col = to_c_string("h3_column");
    let val_col = duckdb_bind_get_named_parameter(info, name_col.as_ptr());
    if !val_col.is_null() {
        if let Some(c) = from_duckdb_string(duckdb_get_varchar(val_col)) {
            h3_column = Some(c);
        }
    } else {
        let name_col2 = to_c_string("h3_col");
        let val_col2 = duckdb_bind_get_named_parameter(info, name_col2.as_ptr());
        if !val_col2.is_null() {
            if let Some(c) = from_duckdb_string(duckdb_get_varchar(val_col2)) {
                h3_column = Some(c);
            }
        }
    }

    // Declare output columns (matching h3_raster_to_pmtiles schema):
    // 0: total_hexagons (BIGINT)
    let col_hex = to_c_string("total_hexagons");
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    duckdb_bind_add_result_column(info, col_hex.as_ptr(), type_bigint);

    // 1: pmtiles_size_bytes (BIGINT)
    let col_size = to_c_string("pmtiles_size_bytes");
    duckdb_bind_add_result_column(info, col_size.as_ptr(), type_bigint);

    // 2: min_zoom (BIGINT)
    let col_min_z = to_c_string("min_zoom");
    duckdb_bind_add_result_column(info, col_min_z.as_ptr(), type_bigint);

    // 3: max_zoom (BIGINT)
    let col_max_z = to_c_string("max_zoom");
    duckdb_bind_add_result_column(info, col_max_z.as_ptr(), type_bigint);

    // 4: elapsed_ms (DOUBLE)
    let col_time = to_c_string("elapsed_ms");
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    duckdb_bind_add_result_column(info, col_time.as_ptr(), type_double);

    // 5: output_path (VARCHAR)
    let col_path = to_c_string("output_path");
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_bind_add_result_column(info, col_path.as_ptr(), type_varchar);

    // 6: status (VARCHAR)
    let col_status = to_c_string("status");
    duckdb_bind_add_result_column(info, col_status.as_ptr(), type_varchar);

    // Cleanup types
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);

    // Set cardinality: 1 summary row
    duckdb_bind_set_cardinality(info, 1, true);

    let bind_data = Box::new(ParquetPmtilesBindData {
        parquet_path,
        output_pmtiles,
        h3_column,
    });
    duckdb_bind_set_bind_data(info, Box::into_raw(bind_data) as *mut c_void, Some(delete_parquet_bind_data));
}

/// Init callback for `h3_parquet_to_pmtiles`
pub unsafe extern "C" fn parquet_pmtiles_init(info: duckdb_init_info) {
    let global_data = Box::new(ParquetPmtilesGlobalData {
        executed: AtomicBool::new(false),
    });
    duckdb_init_set_init_data(info, Box::into_raw(global_data) as *mut c_void, Some(delete_parquet_global_data));
}

/// Scan callback: runs Parquet-to-PMTiles conversion and streams the single summary row
pub unsafe extern "C" fn parquet_pmtiles_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    let bind_data = &*(duckdb_function_get_bind_data(info) as *const ParquetPmtilesBindData);
    let global_data = &*(duckdb_function_get_init_data(info) as *const ParquetPmtilesGlobalData);

    if global_data.executed.swap(true, Ordering::SeqCst) {
        duckdb_data_chunk_set_size(output, 0);
        return;
    }

    let start = Instant::now();
    let result = H3PmtilesTiler::process_parquet_to_pmtiles(
        &bind_data.parquet_path,
        &bind_data.output_pmtiles,
        bind_data.h3_column.as_deref(),
    );
    let elapsed = start.elapsed();

    let (total_hexagons, min_z, max_z, size_bytes, status) = match result {
        Ok(summary) => {
            let sz = File::open(&bind_data.output_pmtiles)
                .and_then(|f| f.metadata())
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            (
                summary.valid_features as i64,
                summary.min_zoom as i64,
                summary.max_zoom as i64,
                sz,
                "SUCCESS".to_string(),
            )
        }
        Err(e) => (0i64, 0i64, 0i64, 0i64, format!("ERROR: {}", e)),
    };

    // Populate the 1 output row
    // 0: total_hexagons (BIGINT)
    let v_hex = duckdb_data_chunk_get_vector(output, 0);
    *(duckdb_vector_get_data(v_hex) as *mut i64) = total_hexagons;

    // 1: pmtiles_size_bytes (BIGINT)
    let v_size = duckdb_data_chunk_get_vector(output, 1);
    *(duckdb_vector_get_data(v_size) as *mut i64) = size_bytes;

    // 2: min_zoom (BIGINT)
    let v_min_z = duckdb_data_chunk_get_vector(output, 2);
    *(duckdb_vector_get_data(v_min_z) as *mut i64) = min_z;

    // 3: max_zoom (BIGINT)
    let v_max_z = duckdb_data_chunk_get_vector(output, 3);
    *(duckdb_vector_get_data(v_max_z) as *mut i64) = max_z;

    // 4: elapsed_ms (DOUBLE)
    let v_time = duckdb_data_chunk_get_vector(output, 4);
    *(duckdb_vector_get_data(v_time) as *mut f64) = elapsed.as_secs_f64() * 1000.0;

    // 5: output_path (VARCHAR)
    let v_path = duckdb_data_chunk_get_vector(output, 5);
    let path_c = CString::new(bind_data.output_pmtiles.clone()).unwrap_or_default();
    duckdb_vector_assign_string_element(v_path, 0, path_c.as_ptr() as *const c_char);

    // 6: status (VARCHAR)
    let v_status = duckdb_data_chunk_get_vector(output, 6);
    let status_c = CString::new(status).unwrap_or_default();
    duckdb_vector_assign_string_element(v_status, 0, status_c.as_ptr() as *const c_char);

    duckdb_data_chunk_set_size(output, 1);
}

/// Register `h3_raster_to_pmtiles` and `h3_parquet_to_pmtiles` Table Functions with DuckDB
pub unsafe fn register_pmtiles_table_function(con: duckdb_connection) -> Result<(), String> {
    // 1. Register h3_raster_to_pmtiles
    let fn_name = to_c_string("h3_raster_to_pmtiles");
    let tf = duckdb_create_table_function();
    duckdb_table_function_set_name(tf, fn_name.as_ptr());

    // Positional parameters:
    // 0: file_path (VARCHAR)
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_table_function_add_parameter(tf, type_varchar);
    // 1: output_pmtiles (VARCHAR)
    duckdb_table_function_add_parameter(tf, type_varchar);

    // Named parameters:
    // resolution (BIGINT)
    let name_res = to_c_string("resolution");
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    duckdb_table_function_add_named_parameter(tf, name_res.as_ptr(), type_bigint);

    // min_resolution (BIGINT)
    let name_min_res = to_c_string("min_resolution");
    duckdb_table_function_add_named_parameter(tf, name_min_res.as_ptr(), type_bigint);

    // max_resolution (BIGINT)
    let name_max_res = to_c_string("max_resolution");
    duckdb_table_function_add_named_parameter(tf, name_max_res.as_ptr(), type_bigint);

    // sampling (VARCHAR)
    let name_sampling = to_c_string("sampling");
    duckdb_table_function_add_named_parameter(tf, name_sampling.as_ptr(), type_varchar);

    // band (BIGINT)
    let name_band = to_c_string("band");
    duckdb_table_function_add_named_parameter(tf, name_band.as_ptr(), type_bigint);

    // nodata (DOUBLE)
    let name_nodata = to_c_string("nodata");
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    duckdb_table_function_add_named_parameter(tf, name_nodata.as_ptr(), type_double);

    // Set callbacks
    duckdb_table_function_set_bind(tf, pmtiles_bind);
    duckdb_table_function_set_init(tf, pmtiles_init);
    duckdb_table_function_set_function(tf, pmtiles_scan);

    let state = duckdb_register_table_function(con, tf);

    // Cleanup types
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);

    let mut tf_mut = tf;
    duckdb_destroy_table_function(&mut tf_mut);

    if state != DuckDBState::Success {
        return Err("Failed to register h3_raster_to_pmtiles table function".to_string());
    }

    // 2. Register h3_parquet_to_pmtiles
    let fn_parquet_name = to_c_string("h3_parquet_to_pmtiles");
    let tf_parquet = duckdb_create_table_function();
    duckdb_table_function_set_name(tf_parquet, fn_parquet_name.as_ptr());

    // Positional parameters:
    // 0: parquet_path (VARCHAR)
    let type_varchar2 = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_table_function_add_parameter(tf_parquet, type_varchar2);
    // 1: output_pmtiles (VARCHAR)
    duckdb_table_function_add_parameter(tf_parquet, type_varchar2);

    // Named parameter: h3_column (VARCHAR)
    let name_col = to_c_string("h3_column");
    duckdb_table_function_add_named_parameter(tf_parquet, name_col.as_ptr(), type_varchar2);

    // Named parameter: h3_col (VARCHAR alias)
    let name_col2 = to_c_string("h3_col");
    duckdb_table_function_add_named_parameter(tf_parquet, name_col2.as_ptr(), type_varchar2);

    // Set callbacks
    duckdb_table_function_set_bind(tf_parquet, parquet_pmtiles_bind);
    duckdb_table_function_set_init(tf_parquet, parquet_pmtiles_init);
    duckdb_table_function_set_function(tf_parquet, parquet_pmtiles_scan);

    let state2 = duckdb_register_table_function(con, tf_parquet);

    let mut type_varchar2_mut = type_varchar2;
    duckdb_destroy_logical_type(&mut type_varchar2_mut);
    let mut tf_parquet_mut = tf_parquet;
    duckdb_destroy_table_function(&mut tf_parquet_mut);

    if state2 != DuckDBState::Success {
        return Err("Failed to register h3_parquet_to_pmtiles table function".to_string());
    }

    Ok(())
}

