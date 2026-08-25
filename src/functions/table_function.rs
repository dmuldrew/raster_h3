use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::aggregator::{aggregate_raster_stream, AggregationConfig, H3Accumulator};
use crate::ffi::*;
use crate::raster::GeoTiffStreamReader;

/// Bind state passed between bind -> init
pub struct RasterH3BindData {
    pub file_path: String,
    pub resolution: u8,
    pub chunk_size: u32,
    pub source_crs: Option<String>,
    pub nodata: Option<f64>,
}

/// Execution state across scan batches using Southernmost Scan-Line Horizon Eviction
pub struct RasterH3InitData {
    pub streamer: std::sync::Mutex<crate::aggregator::ScanHorizonStreamer>,
}

unsafe extern "C" fn delete_bind_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3BindData));
    }
}

unsafe extern "C" fn delete_init_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3InitData));
    }
}

/// Bind callback: parses arguments and registers result columns
pub unsafe extern "C" fn raster_h3_bind(info: duckdb_bind_info) {
    let param_count = duckdb_bind_get_parameter_count(info);
    if param_count == 0 {
        let err = to_c_string("h3_raster_aggregate requires at least 1 parameter: file_path");
        duckdb_bind_set_error(info, err.as_ptr());
        return;
    }

    // Param 0: file_path (VARCHAR)
    let file_val = duckdb_bind_get_parameter(info, 0);
    let file_ptr = duckdb_get_varchar(file_val);
    let file_path = match from_duckdb_string(file_ptr) {
        Some(s) => s,
        None => {
            let err = to_c_string("Invalid file_path parameter");
            duckdb_bind_set_error(info, err.as_ptr());
            return;
        }
    };

    // Param 1: resolution (optional positional)
    let mut resolution: u8 = 8;
    if param_count > 1 {
        let res_val = duckdb_bind_get_parameter(info, 1);
        let res_int = duckdb_get_int64(res_val);
        if (0..=15).contains(&res_int) {
            resolution = res_int as u8;
        }
    }

    // Named parameter: resolution
    let name_res = to_c_string("resolution");
    let named_res_val = duckdb_bind_get_named_parameter(info, name_res.as_ptr());
    if !named_res_val.is_null() {
        let res_int = duckdb_get_int64(named_res_val);
        if (0..=15).contains(&res_int) {
            resolution = res_int as u8;
        }
    }

    // Named parameter: source_crs
    let name_crs = to_c_string("source_crs");
    let named_crs_val = duckdb_bind_get_named_parameter(info, name_crs.as_ptr());
    let mut source_crs = None;
    if !named_crs_val.is_null() {
        let crs_ptr = duckdb_get_varchar(named_crs_val);
        source_crs = from_duckdb_string(crs_ptr);
    }

    // Named parameter: nodata
    let name_nodata = to_c_string("nodata");
    let named_nodata_val = duckdb_bind_get_named_parameter(info, name_nodata.as_ptr());
    let mut nodata = None;
    if !named_nodata_val.is_null() {
        nodata = Some(duckdb_get_double(named_nodata_val));
    }

    // Named parameter: chunk_size
    let mut chunk_size: u32 = 512;
    let name_chunk = to_c_string("chunk_size");
    let named_chunk_val = duckdb_bind_get_named_parameter(info, name_chunk.as_ptr());
    if !named_chunk_val.is_null() {
        let cs = duckdb_get_int64(named_chunk_val);
        if cs > 0 {
            chunk_size = cs as u32;
        }
    }

    // Add Output Columns:
    // 0: h3_index UBIGINT
    let col_h3 = to_c_string("h3_index");
    let type_ubigint = duckdb_create_logical_type(DuckDBType::UBigInt);
    duckdb_bind_add_result_column(info, col_h3.as_ptr(), type_ubigint);
    let mut type_ubigint_mut = type_ubigint;
    duckdb_destroy_logical_type(&mut type_ubigint_mut);

    // 1: h3_hex VARCHAR
    let col_hex = to_c_string("h3_hex");
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_bind_add_result_column(info, col_hex.as_ptr(), type_varchar);
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);

    // 2: mean DOUBLE
    let col_mean = to_c_string("mean");
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    duckdb_bind_add_result_column(info, col_mean.as_ptr(), type_double);

    // 3: count UBIGINT
    let col_cnt = to_c_string("count");
    let type_ubigint2 = duckdb_create_logical_type(DuckDBType::UBigInt);
    duckdb_bind_add_result_column(info, col_cnt.as_ptr(), type_ubigint2);
    let mut type_ubigint2_mut = type_ubigint2;
    duckdb_destroy_logical_type(&mut type_ubigint2_mut);

    // 4: min DOUBLE
    let col_min = to_c_string("min");
    duckdb_bind_add_result_column(info, col_min.as_ptr(), type_double);

    // 5: max DOUBLE
    let col_max = to_c_string("max");
    duckdb_bind_add_result_column(info, col_max.as_ptr(), type_double);

    // 6: sum DOUBLE
    let col_sum = to_c_string("sum");
    duckdb_bind_add_result_column(info, col_sum.as_ptr(), type_double);

    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);

    let bind_data = Box::new(RasterH3BindData {
        file_path,
        resolution,
        chunk_size,
        source_crs,
        nodata,
    });

    duckdb_bind_set_bind_data(
        info,
        Box::into_raw(bind_data) as *mut c_void,
        Some(delete_bind_data),
    );
}

/// Init callback: instantiates ScanHorizonStreamer for on-demand streaming scan
pub unsafe extern "C" fn raster_h3_init(info: duckdb_init_info) {
    let bind_data_ptr = duckdb_init_get_bind_data(info) as *const RasterH3BindData;
    if bind_data_ptr.is_null() {
        let err = to_c_string("Missing bind data in raster_h3_init");
        duckdb_init_set_error(info, err.as_ptr());
        return;
    }
    let bind_data = &*bind_data_ptr;

    // Open GeoTIFF Header and metadata stream
    let reader = match GeoTiffStreamReader::open(&bind_data.file_path) {
        Ok(r) => r,
        Err(e) => {
            let err = to_c_string(&format!("Failed to open raster '{}': {}", bind_data.file_path, e));
            duckdb_init_set_error(info, err.as_ptr());
            return;
        }
    };

    let config = AggregationConfig {
        resolution: bind_data.resolution,
        custom_crs: bind_data.source_crs.clone(),
        custom_nodata: bind_data.nodata,
    };

    let streamer = match crate::aggregator::ScanHorizonStreamer::new(reader, &config) {
        Ok(s) => s,
        Err(e) => {
            let err = to_c_string(&format!("Failed to initialize stream: {}", e));
            duckdb_init_set_error(info, err.as_ptr());
            return;
        }
    };

    let init_data = Box::new(RasterH3InitData {
        streamer: std::sync::Mutex::new(streamer),
    });

    duckdb_init_set_init_data(
        info,
        Box::into_raw(init_data) as *mut c_void,
        Some(delete_init_data),
    );
}

/// Scan callback: streams up to 2048 records per invocation directly from ScanHorizonStreamer
pub unsafe extern "C" fn raster_h3_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    let init_data_ptr = duckdb_function_get_init_data(info) as *const RasterH3InitData;
    if init_data_ptr.is_null() {
        duckdb_data_chunk_set_size(output, 0);
        return;
    }
    let init_data = &*init_data_ptr;

    let mut streamer = match init_data.streamer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    let batch = streamer.fetch_next_batch(2048);
    let batch_size = batch.len();

    if batch_size == 0 {
        // EOF: All finished hexagons emitted
        duckdb_data_chunk_set_size(output, 0);
        return;
    }

    // Get output vector pointers
    let v_h3 = duckdb_data_chunk_get_vector(output, 0);
    let v_hex = duckdb_data_chunk_get_vector(output, 1);
    let v_mean = duckdb_data_chunk_get_vector(output, 2);
    let v_cnt = duckdb_data_chunk_get_vector(output, 3);
    let v_min = duckdb_data_chunk_get_vector(output, 4);
    let v_max = duckdb_data_chunk_get_vector(output, 5);
    let v_sum = duckdb_data_chunk_get_vector(output, 6);

    let p_h3 = duckdb_vector_get_data(v_h3) as *mut u64;
    let p_mean = duckdb_vector_get_data(v_mean) as *mut f64;
    let p_cnt = duckdb_vector_get_data(v_cnt) as *mut u64;
    let p_min = duckdb_vector_get_data(v_min) as *mut f64;
    let p_max = duckdb_vector_get_data(v_max) as *mut f64;
    let p_sum = duckdb_vector_get_data(v_sum) as *mut f64;

    for (i, (cell_u64, acc)) in batch.iter().enumerate() {
        let row_idx = i as u64;

        *p_h3.add(i) = *cell_u64;

        // Write hexadecimal string
        let hex_str = format!("{:x}", cell_u64);
        duckdb_vector_assign_string_element_len(
            v_hex,
            row_idx,
            hex_str.as_ptr() as *const c_char,
            hex_str.len() as idx_t,
        );

        *p_mean.add(i) = acc.mean();
        *p_cnt.add(i) = acc.count;
        *p_min.add(i) = acc.min;
        *p_max.add(i) = acc.max;
        *p_sum.add(i) = acc.sum;
    }

    duckdb_data_chunk_set_size(output, batch_size as u64);
}

/// Register `h3_raster_aggregate` table function with DuckDB
pub unsafe fn register_table_function(con: duckdb_connection) -> Result<(), String> {
    let tf = duckdb_create_table_function();
    let name = to_c_string("h3_raster_aggregate");
    duckdb_table_function_set_name(tf, name.as_ptr());

    // Parameter 0: file_path (VARCHAR)
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    duckdb_table_function_add_parameter(tf, type_varchar);

    // Named Parameters:
    // resolution (BIGINT)
    let name_res = to_c_string("resolution");
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    duckdb_table_function_add_named_parameter(tf, name_res.as_ptr(), type_bigint);

    // source_crs (VARCHAR)
    let name_crs = to_c_string("source_crs");
    duckdb_table_function_add_named_parameter(tf, name_crs.as_ptr(), type_varchar);

    // nodata (DOUBLE)
    let name_nodata = to_c_string("nodata");
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    duckdb_table_function_add_named_parameter(tf, name_nodata.as_ptr(), type_double);

    // chunk_size (BIGINT)
    let name_chunk = to_c_string("chunk_size");
    duckdb_table_function_add_named_parameter(tf, name_chunk.as_ptr(), type_bigint);

    // Set callbacks
    duckdb_table_function_set_bind(tf, raster_h3_bind);
    duckdb_table_function_set_init(tf, raster_h3_init);
    duckdb_table_function_set_function(tf, raster_h3_scan);

    let state = duckdb_register_table_function(con, tf);

    // Cleanup logical types
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);

    let mut tf_mut = tf;
    duckdb_destroy_table_function(&mut tf_mut);

    if state != DuckDBState::Success {
        return Err("Failed to register h3_raster_aggregate table function".to_string());
    }

    Ok(())
}
