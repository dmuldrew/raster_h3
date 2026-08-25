use std::ffi::{c_char, c_void, CString};
use std::sync::Mutex;

use crate::aggregator::horizon_streamer::{AggregationConfig, ScanHorizonStreamer};
use crate::ffi::duckdb_c::*;
use crate::ffi::{from_duckdb_string, to_c_string};
use crate::functions::fast_hex::fast_hex_u64;
use crate::raster::geotiff::GeoTiffStreamReader;

/// User-data bound during table function query compilation
pub struct RasterH3BindData {
    pub file_path: String,
    pub resolution: u8,
    pub source_crs: Option<String>,
    pub nodata: Option<f64>,
    pub chunk_size: u32,
    pub bbox: Option<[f64; 4]>,
}

/// Global scan state wrapping ScanHorizonStreamer in a thread-safe mutex
pub struct RasterH3GlobalData {
    pub streamer: Mutex<ScanHorizonStreamer>,
}

/// Thread-local state for parallel DuckDB execution threads
pub struct RasterH3LocalData {
    pub thread_id: usize,
}

unsafe extern "C" fn delete_bind_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3BindData));
    }
}

unsafe extern "C" fn delete_global_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3GlobalData));
    }
}

unsafe extern "C" fn delete_local_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3LocalData));
    }
}

/// Bind callback: parses input arguments, defines output columns, and returns bind data
pub unsafe extern "C" fn raster_h3_bind(info: duckdb_bind_info) {
    let param_count = duckdb_bind_get_parameter_count(info);
    if param_count < 1 {
        let err_msg = to_c_string("h3_raster_aggregate requires at least 1 argument: file_path");
        duckdb_bind_set_error(info, err_msg.as_ptr());
        return;
    }

    // Param 0: file_path (VARCHAR)
    let path_val = duckdb_bind_get_parameter(info, 0);
    let path_str_ptr = duckdb_get_varchar(path_val);
    let file_path = match from_duckdb_string(path_str_ptr) {
        Some(s) => s,
        None => {
            let err_msg = to_c_string("Invalid file_path parameter");
            duckdb_bind_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    // Param 1 (optional positional): resolution (BIGINT)
    let mut resolution: u8 = 8;
    if param_count >= 2 {
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

    // Named parameters for bounding box filtering
    let name_min_lon = to_c_string("min_lon");
    let named_min_lon_val = duckdb_bind_get_named_parameter(info, name_min_lon.as_ptr());

    let name_min_lat = to_c_string("min_lat");
    let named_min_lat_val = duckdb_bind_get_named_parameter(info, name_min_lat.as_ptr());

    let name_max_lon = to_c_string("max_lon");
    let named_max_lon_val = duckdb_bind_get_named_parameter(info, name_max_lon.as_ptr());

    let name_max_lat = to_c_string("max_lat");
    let named_max_lat_val = duckdb_bind_get_named_parameter(info, name_max_lat.as_ptr());

    let bbox = if !named_min_lon_val.is_null()
        && !named_min_lat_val.is_null()
        && !named_max_lon_val.is_null()
        && !named_max_lat_val.is_null()
    {
        Some([
            duckdb_get_double(named_min_lon_val),
            duckdb_get_double(named_min_lat_val),
            duckdb_get_double(named_max_lon_val),
            duckdb_get_double(named_max_lat_val),
        ])
    } else {
        None
    };

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

    // Approximate H3 cell areas in m^2 by resolution (0 to 15) for query planner cardinality estimation
    const H3_AREA_M2: [f64; 16] = [
        4.357e12, 6.097e11, 8.680e10, 1.239e10, 1.770e9, 2.529e8,
        3.613e7, 5.161e6, 7.373e5, 1.053e5, 1.505e4, 2.150e3,
        3.071e2, 4.387e1, 6.268e0, 8.954e-1,
    ];

    let estimated_cardinality = if let Ok(reader) = GeoTiffStreamReader::open(&file_path) {
        let w = reader.metadata.width as f64;
        let h = reader.metadata.height as f64;
        let total_pixels = (w * h) as u64;

        let (x0, y0) = reader.metadata.geotransform.pixel_to_coord(0.0, 0.0);
        let (x1, y1) = reader.metadata.geotransform.pixel_to_coord(w, h);
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();

        let area_m2 = if matches!(reader.metadata.epsg, Some(4326)) || reader.metadata.epsg.is_none() {
            dx * 111_320.0 * dy * 110_540.0
        } else {
            dx * dy
        };

        let hex_area = H3_AREA_M2.get(resolution as usize).copied().unwrap_or(7.373e5);
        let hex_count = (area_m2 / hex_area).ceil() as u64;
        hex_count.min(total_pixels).max(1)
    } else {
        10_000
    };

    duckdb_bind_set_cardinality(info, estimated_cardinality as idx_t, false);

    let bind_data = Box::new(RasterH3BindData {
        file_path,
        resolution,
        source_crs,
        nodata,
        chunk_size,
        bbox,
    });

    duckdb_bind_set_bind_data(
        info,
        Box::into_raw(bind_data) as *mut c_void,
        Some(delete_bind_data),
    );
}

/// Global init callback: opens GeoTIFF stream reader and initializes shared ScanHorizonStreamer
pub unsafe extern "C" fn raster_h3_init(info: duckdb_init_info) {
    let bind_data_ptr = duckdb_init_get_bind_data(info) as *const RasterH3BindData;
    if bind_data_ptr.is_null() {
        let err_msg = to_c_string("Missing bind data in init");
        duckdb_init_set_error(info, err_msg.as_ptr());
        return;
    }
    let bind_data = &*bind_data_ptr;

    let reader = match GeoTiffStreamReader::open(&bind_data.file_path) {
        Ok(r) => r,
        Err(e) => {
            let err_msg = CString::new(format!("Failed to open GeoTIFF: {}", e))
                .unwrap_or_else(|_| CString::new("Failed to open GeoTIFF").unwrap());
            duckdb_init_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let config = AggregationConfig {
        resolution: bind_data.resolution,
        custom_crs: bind_data.source_crs.clone(),
        custom_nodata: bind_data.nodata,
        bbox: bind_data.bbox,
    };

    let streamer = match ScanHorizonStreamer::new(reader, &config) {
        Ok(s) => s,
        Err(e) => {
            let err_msg = CString::new(format!("Failed to initialize streaming aggregator: {}", e))
                .unwrap_or_else(|_| CString::new("Failed to init streamer").unwrap());
            duckdb_init_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let global_data = Box::new(RasterH3GlobalData {
        streamer: Mutex::new(streamer),
    });

    duckdb_init_set_init_data(
        info,
        Box::into_raw(global_data) as *mut c_void,
        Some(delete_global_data),
    );
}

/// Thread-local init callback for multi-threaded parallel DuckDB execution
pub unsafe extern "C" fn raster_h3_init_local(info: duckdb_init_info) {
    let local_data = Box::new(RasterH3LocalData { thread_id: 0 });
    duckdb_init_set_init_data(
        info,
        Box::into_raw(local_data) as *mut c_void,
        Some(delete_local_data),
    );
}

/// Scan callback: streams up to 2048 records per invocation directly from ScanHorizonStreamer
pub unsafe extern "C" fn raster_h3_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    let global_data_ptr = duckdb_function_get_init_data(info) as *const RasterH3GlobalData;
    if global_data_ptr.is_null() {
        duckdb_data_chunk_set_size(output, 0);
        return;
    }
    let global_data = &*global_data_ptr;

    let batch = {
        let mut streamer = match global_data.streamer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        streamer.fetch_next_batch(2048)
    };

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

    let mut hex_buf = [0u8; 16];

    for (i, (cell_u64, acc)) in batch.iter().enumerate() {
        let row_idx = i as u64;

        *p_h3.add(i) = *cell_u64;

        // Zero-allocation hexadecimal string formatting
        let hex_slice = fast_hex_u64(*cell_u64, &mut hex_buf);
        duckdb_vector_assign_string_element_len(
            v_hex,
            row_idx,
            hex_slice.as_ptr() as *const c_char,
            hex_slice.len() as idx_t,
        );

        *p_mean.add(i) = acc.mean();
        *p_cnt.add(i) = acc.count;
        *p_min.add(i) = acc.min;
        *p_max.add(i) = acc.max;
        *p_sum.add(i) = acc.sum;
    }

    duckdb_data_chunk_set_size(output, batch_size as idx_t);
}

/// Register `h3_raster_aggregate` table function in DuckDB connection
pub unsafe fn register_table_function(con: duckdb_connection) -> std::result::Result<(), String> {
    let fn_name = to_c_string("h3_raster_aggregate");
    let tf = duckdb_create_table_function();
    duckdb_table_function_set_name(tf, fn_name.as_ptr());

    // Positional Parameters:
    // 0: file_path (VARCHAR)
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

    // Bounding box named parameters: min_lon, min_lat, max_lon, max_lat
    let name_min_lon = to_c_string("min_lon");
    duckdb_table_function_add_named_parameter(tf, name_min_lon.as_ptr(), type_double);

    let name_min_lat = to_c_string("min_lat");
    let name_max_lon = to_c_string("max_lon");
    let name_max_lat = to_c_string("max_lat");
    duckdb_table_function_add_named_parameter(tf, name_min_lat.as_ptr(), type_double);
    duckdb_table_function_add_named_parameter(tf, name_max_lon.as_ptr(), type_double);
    duckdb_table_function_add_named_parameter(tf, name_max_lat.as_ptr(), type_double);

    // Set callbacks including parallel init_local
    duckdb_table_function_set_bind(tf, raster_h3_bind);
    duckdb_table_function_set_init(tf, raster_h3_init);
    duckdb_table_function_set_init_local(tf, raster_h3_init_local);
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
