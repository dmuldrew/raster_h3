//! DuckDB Table Function for Direct GeoTIFF-to-Parquet Generation (Option 5)
//!
//! Exposes:
//!   SELECT * FROM h3_raster_to_parquet(
//!       'input.tif',
//!       'output.parquet',
//!       resolution := 9,
//!       sampling := '5point',
//!       compression := 'snappy',
//!       compact := true
//!   );

use std::ffi::{c_char, c_void, CString};
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parquet::basic::Compression;

use crate::aggregator::multi_horizon::MultiResolutionConfig;
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::{from_duckdb_string, to_c_string};
use crate::parquet::{H3ParquetWriter, ParquetExportConfig};

/// Bind data parsed during SQL query planning
pub struct ParquetBindData {
    pub file_path: String,
    pub output_parquet: String,
    pub resolution: u8,
    pub band: usize,
    pub custom_nodata: Option<f64>,
    pub sampling: SamplingPattern,
    pub is_categorical: bool,
    pub compact: bool,
    pub compression: Compression,
    pub row_group_size: usize,
    pub bbox: Option<[f64; 4]>,
}

/// Global execution state for the single-row generator
pub struct ParquetGlobalData {
    pub executed: AtomicBool,
}

unsafe extern "C" fn delete_parquet_bind_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut ParquetBindData));
    }
}

unsafe extern "C" fn delete_parquet_global_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut ParquetGlobalData));
    }
}

/// Bind callback: parses input arguments and defines output table schema
pub unsafe extern "C" fn parquet_bind(info: duckdb_bind_info) {
    let param_count = duckdb_bind_get_parameter_count(info);
    if param_count < 2 {
        let err_msg = to_c_string("h3_raster_to_parquet requires at least 2 arguments: file_path and output_parquet");
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

    // 1: output_parquet (VARCHAR)
    let out_val = duckdb_bind_get_parameter(info, 1);
    let output_parquet = match from_duckdb_string(duckdb_get_varchar(out_val)) {
        Some(s) => s,
        None => {
            let err_msg = to_c_string("Invalid output_parquet parameter");
            duckdb_bind_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let mut resolution = 8u8;
    let mut band = 1usize;
    let mut custom_nodata = None;
    let mut sampling = SamplingPattern::center();
    let mut is_categorical = false;
    let mut compact = true;
    let mut compression = Compression::SNAPPY;
    let mut row_group_size = 131_072usize;

    // Named parameter: resolution (BIGINT)
    let name_res = to_c_string("resolution");
    let val_res = duckdb_bind_get_named_parameter(info, name_res.as_ptr());
    if !val_res.is_null() {
        let r = duckdb_get_int64(val_res);
        if (0..=15).contains(&r) {
            resolution = r as u8;
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

    // Named parameter: categorical (BOOLEAN)
    let name_cat = to_c_string("categorical");
    let val_cat = duckdb_bind_get_named_parameter(info, name_cat.as_ptr());
    if !val_cat.is_null() {
        is_categorical = duckdb_get_bool(val_cat);
    }

    // Named parameter: compact (BOOLEAN)
    let name_compact = to_c_string("compact");
    let val_compact = duckdb_bind_get_named_parameter(info, name_compact.as_ptr());
    if !val_compact.is_null() {
        compact = duckdb_get_bool(val_compact);
    }

    // Named parameter: compression (VARCHAR)
    let name_comp = to_c_string("compression");
    let val_comp = duckdb_bind_get_named_parameter(info, name_comp.as_ptr());
    if !val_comp.is_null() {
        if let Some(s) = from_duckdb_string(duckdb_get_varchar(val_comp)) {
            match s.to_lowercase().as_str() {
                "snappy" => compression = Compression::SNAPPY,
                "zstd" => compression = Compression::ZSTD(Default::default()),
                "gzip" | "flate" => compression = Compression::GZIP(Default::default()),
                "lz4" => compression = Compression::LZ4,
                "uncompressed" | "none" => compression = Compression::UNCOMPRESSED,
                _ => {}
            }
        }
    }

    // Named parameter: row_group_size (BIGINT)
    let name_rgs = to_c_string("row_group_size");
    let val_rgs = duckdb_bind_get_named_parameter(info, name_rgs.as_ptr());
    if !val_rgs.is_null() {
        let sz = duckdb_get_int64(val_rgs);
        if sz > 0 {
            row_group_size = sz as usize;
        }
    }

    // Named parameters for bounding box: min_lon, min_lat, max_lon, max_lat (DOUBLE)
    let name_min_lon = to_c_string("min_lon");
    let name_min_lat = to_c_string("min_lat");
    let name_max_lon = to_c_string("max_lon");
    let name_max_lat = to_c_string("max_lat");
    let val_min_lon = duckdb_bind_get_named_parameter(info, name_min_lon.as_ptr());
    let val_min_lat = duckdb_bind_get_named_parameter(info, name_min_lat.as_ptr());
    let val_max_lon = duckdb_bind_get_named_parameter(info, name_max_lon.as_ptr());
    let val_max_lat = duckdb_bind_get_named_parameter(info, name_max_lat.as_ptr());

    let bbox = if !val_min_lon.is_null()
        && !val_min_lat.is_null()
        && !val_max_lon.is_null()
        && !val_max_lat.is_null()
    {
        Some([
            duckdb_get_double(val_min_lon),
            duckdb_get_double(val_min_lat),
            duckdb_get_double(val_max_lon),
            duckdb_get_double(val_max_lat),
        ])
    } else {
        None
    };

    // Declare output summary schema:
    // 0: total_hexagons (BIGINT)
    let col_hex = to_c_string("total_hexagons");
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    duckdb_bind_add_result_column(info, col_hex.as_ptr(), type_bigint);

    // 1: parquet_size_bytes (BIGINT)
    let col_size = to_c_string("parquet_size_bytes");
    duckdb_bind_add_result_column(info, col_size.as_ptr(), type_bigint);

    // 2: elapsed_ms (DOUBLE)
    let col_time = to_c_string("elapsed_ms");
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    duckdb_bind_add_result_column(info, col_time.as_ptr(), type_double);

    // 3: hexagons_per_sec (DOUBLE)
    let col_rate = to_c_string("hexagons_per_sec");
    duckdb_bind_add_result_column(info, col_rate.as_ptr(), type_double);

    // 4: output_path (VARCHAR)
    let col_path = to_c_string("output_path");
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_bind_add_result_column(info, col_path.as_ptr(), type_varchar);

    // 5: status (VARCHAR)
    let col_status = to_c_string("status");
    duckdb_bind_add_result_column(info, col_status.as_ptr(), type_varchar);

    // Cleanup types
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);

    // Cardinality is always 1 summary row
    duckdb_bind_set_cardinality(info, 1, true);

    let bind_data = Box::new(ParquetBindData {
        file_path,
        output_parquet,
        resolution,
        band,
        custom_nodata,
        sampling,
        is_categorical,
        compact,
        compression,
        row_group_size,
        bbox,
    });
    duckdb_bind_set_bind_data(info, Box::into_raw(bind_data) as *mut c_void, Some(delete_parquet_bind_data));
}

/// Init callback
pub unsafe extern "C" fn parquet_init(info: duckdb_init_info) {
    let global_data = Box::new(ParquetGlobalData {
        executed: AtomicBool::new(false),
    });
    duckdb_init_set_init_data(info, Box::into_raw(global_data) as *mut c_void, Some(delete_parquet_global_data));
}

/// Scan callback: runs GeoTIFF-to-Parquet conversion and streams the single summary row
pub unsafe extern "C" fn parquet_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    let bind_data = &*(duckdb_function_get_bind_data(info) as *const ParquetBindData);
    let global_data = &*(duckdb_function_get_init_data(info) as *const ParquetGlobalData);

    if global_data.executed.swap(true, Ordering::SeqCst) {
        duckdb_data_chunk_set_size(output, 0);
        return;
    }

    let mut config = MultiResolutionConfig::new(vec![bind_data.resolution]);
    config.band = bind_data.band;
    config.custom_nodata = bind_data.custom_nodata;
    config.sampling = bind_data.sampling.clone();
    config.bbox = bind_data.bbox;

    let parquet_config = ParquetExportConfig {
        compact: bind_data.compact,
        compression: bind_data.compression,
        row_group_size: bind_data.row_group_size,
        is_categorical: bind_data.is_categorical,
    };

    let start = Instant::now();
    let result = H3ParquetWriter::process_raster_source_to_parquet(
        &bind_data.file_path,
        &bind_data.output_parquet,
        config,
        parquet_config,
    );
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;

    let (total_hexagons, size_bytes, rate, status) = match result {
        Ok(count) => {
            let sz = File::open(&bind_data.output_parquet)
                .and_then(|f| f.metadata())
                .map(|m| m.len() as i64)
                .unwrap_or(0);
            let hex_rate = if elapsed.as_secs_f64() > 0.0 {
                count as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            };
            (count as i64, sz, hex_rate, "SUCCESS".to_string())
        }
        Err(e) => (0i64, 0i64, 0.0, format!("ERROR: {}", e)),
    };

    // Populate the 1 output row
    // 0: total_hexagons (BIGINT)
    let v_hex = duckdb_data_chunk_get_vector(output, 0);
    *(duckdb_vector_get_data(v_hex) as *mut i64) = total_hexagons;

    // 1: parquet_size_bytes (BIGINT)
    let v_size = duckdb_data_chunk_get_vector(output, 1);
    *(duckdb_vector_get_data(v_size) as *mut i64) = size_bytes;

    // 2: elapsed_ms (DOUBLE)
    let v_time = duckdb_data_chunk_get_vector(output, 2);
    *(duckdb_vector_get_data(v_time) as *mut f64) = elapsed_ms;

    // 3: hexagons_per_sec (DOUBLE)
    let v_rate = duckdb_data_chunk_get_vector(output, 3);
    *(duckdb_vector_get_data(v_rate) as *mut f64) = rate;

    // 4: output_path (VARCHAR)
    let v_path = duckdb_data_chunk_get_vector(output, 4);
    let path_c = CString::new(bind_data.output_parquet.clone()).unwrap_or_default();
    duckdb_vector_assign_string_element(v_path, 0, path_c.as_ptr() as *const c_char);

    // 5: status (VARCHAR)
    let v_status = duckdb_data_chunk_get_vector(output, 5);
    let status_c = CString::new(status).unwrap_or_default();
    duckdb_vector_assign_string_element(v_status, 0, status_c.as_ptr() as *const c_char);

    duckdb_data_chunk_set_size(output, 1);
}

/// Register `h3_raster_to_parquet` Table Function in DuckDB connection
pub unsafe fn register_parquet_table_function(con: duckdb_connection) -> Result<(), String> {
    let fn_name = to_c_string("h3_raster_to_parquet");
    let tf = duckdb_create_table_function();
    duckdb_table_function_set_name(tf, fn_name.as_ptr());

    // Positional parameters:
    // 0: file_path (VARCHAR)
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_table_function_add_parameter(tf, type_varchar);
    // 1: output_parquet (VARCHAR)
    duckdb_table_function_add_parameter(tf, type_varchar);

    // Named parameter: resolution (BIGINT)
    let name_res = to_c_string("resolution");
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    duckdb_table_function_add_named_parameter(tf, name_res.as_ptr(), type_bigint);

    // Named parameter: sampling (VARCHAR)
    let name_sampling = to_c_string("sampling");
    duckdb_table_function_add_named_parameter(tf, name_sampling.as_ptr(), type_varchar);

    // Named parameter: band (BIGINT)
    let name_band = to_c_string("band");
    duckdb_table_function_add_named_parameter(tf, name_band.as_ptr(), type_bigint);

    // Named parameter: nodata (DOUBLE)
    let name_nodata = to_c_string("nodata");
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    duckdb_table_function_add_named_parameter(tf, name_nodata.as_ptr(), type_double);

    // Named parameter: categorical (BOOLEAN)
    let name_cat = to_c_string("categorical");
    let type_bool = duckdb_create_logical_type(DuckDBType::Boolean);
    duckdb_table_function_add_named_parameter(tf, name_cat.as_ptr(), type_bool);

    // Named parameter: compact (BOOLEAN)
    let name_compact = to_c_string("compact");
    duckdb_table_function_add_named_parameter(tf, name_compact.as_ptr(), type_bool);

    // Named parameter: compression (VARCHAR)
    let name_comp = to_c_string("compression");
    duckdb_table_function_add_named_parameter(tf, name_comp.as_ptr(), type_varchar);

    // Named parameter: row_group_size (BIGINT)
    let name_rgs = to_c_string("row_group_size");
    duckdb_table_function_add_named_parameter(tf, name_rgs.as_ptr(), type_bigint);

    // Named parameters for bounding box: min_lon, min_lat, max_lon, max_lat (DOUBLE)
    let name_min_lon = to_c_string("min_lon");
    let name_min_lat = to_c_string("min_lat");
    let name_max_lon = to_c_string("max_lon");
    let name_max_lat = to_c_string("max_lat");
    duckdb_table_function_add_named_parameter(tf, name_min_lon.as_ptr(), type_double);
    duckdb_table_function_add_named_parameter(tf, name_min_lat.as_ptr(), type_double);
    duckdb_table_function_add_named_parameter(tf, name_max_lon.as_ptr(), type_double);
    duckdb_table_function_add_named_parameter(tf, name_max_lat.as_ptr(), type_double);

    // Set callbacks
    duckdb_table_function_set_bind(tf, parquet_bind);
    duckdb_table_function_set_init(tf, parquet_init);
    duckdb_table_function_set_function(tf, parquet_scan);

    let state = duckdb_register_table_function(con, tf);

    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);
    let mut type_bool_mut = type_bool;
    duckdb_destroy_logical_type(&mut type_bool_mut);

    let mut tf_mut = tf;
    duckdb_destroy_table_function(&mut tf_mut);

    if state != DuckDBState::Success {
        return Err("Failed to register h3_raster_to_parquet table function".to_string());
    }

    Ok(())
}
