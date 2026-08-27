use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::sync::Mutex;

use crate::aggregator::categorical::CategoricalHorizonStreamer;
use crate::aggregator::horizon_streamer::AggregationConfig;
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::{from_duckdb_string, to_c_string};
use crate::functions::fast_hex::fast_hex_u64;
use crate::raster::geotiff::GeoTiffStreamReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoricalOutputFormat {
    Wide,
    Long,
}

pub struct RasterH3CategoricalBindData {
    pub file_path: String,
    pub resolution: u8,
    pub source_crs: Option<String>,
    pub nodata: Option<f64>,
    pub chunk_size: u32,
    pub bbox: Option<[f64; 4]>,
    pub sampling: SamplingPattern,
    pub band: u32,
    pub format: CategoricalOutputFormat,
}

pub struct LongCategoricalRow {
    pub cell_u64: u64,
    pub category: i64,
    pub count: f64,
    pub fraction: f64,
    pub total_count: f64,
}

pub struct RasterH3CategoricalGlobalData {
    pub streamer: Mutex<CategoricalHorizonStreamer>,
    pub format: CategoricalOutputFormat,
    pub long_queue: Mutex<VecDeque<LongCategoricalRow>>,
}

pub struct RasterH3CategoricalLocalData {
    pub thread_id: usize,
}

unsafe extern "C" fn delete_bind_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3CategoricalBindData));
    }
}

unsafe extern "C" fn delete_global_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3CategoricalGlobalData));
    }
}

unsafe extern "C" fn delete_local_data(data: *mut c_void) {
    if !data.is_null() {
        drop(Box::from_raw(data as *mut RasterH3CategoricalLocalData));
    }
}

/// Bind callback for categorical aggregation table function
pub unsafe extern "C" fn raster_h3_categorical_bind(info: duckdb_bind_info) {
    let param_count = duckdb_bind_get_parameter_count(info);
    if param_count < 1 {
        let err_msg = to_c_string("h3_raster_categorical_aggregate requires at least 1 argument: file_path");
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

    // Named parameter: band (1-indexed, default 1)
    let mut band: u32 = 1;
    let name_band = to_c_string("band");
    let named_band_val = duckdb_bind_get_named_parameter(info, name_band.as_ptr());
    if !named_band_val.is_null() {
        let b = duckdb_get_int64(named_band_val);
        if b > 0 {
            band = b as u32;
        }
    }

    // Named parameter: sampling
    let mut sampling = SamplingPattern::default();
    let name_sampling = to_c_string("sampling");
    let named_sampling_val = duckdb_bind_get_named_parameter(info, name_sampling.as_ptr());
    if !named_sampling_val.is_null() {
        let sampling_ptr = duckdb_get_varchar(named_sampling_val);
        if let Some(s) = from_duckdb_string(sampling_ptr) {
            sampling = SamplingPattern::parse(&s);
        }
    }

    // Named parameter: format ('wide' or 'long', default 'wide')
    let mut format = CategoricalOutputFormat::Wide;
    let name_fmt = to_c_string("format");
    let named_fmt_val = duckdb_bind_get_named_parameter(info, name_fmt.as_ptr());
    if !named_fmt_val.is_null() {
        let fmt_ptr = duckdb_get_varchar(named_fmt_val);
        if let Some(s) = from_duckdb_string(fmt_ptr) {
            if s.trim().eq_ignore_ascii_case("long") {
                format = CategoricalOutputFormat::Long;
            }
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

    // Define result columns based on format
    let type_ubigint = duckdb_create_logical_type(DuckDBType::UBigInt);
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    let type_double = duckdb_create_logical_type(DuckDBType::Double);

    let col_h3 = to_c_string("h3_index");
    duckdb_bind_add_result_column(info, col_h3.as_ptr(), type_ubigint);

    let col_hex = to_c_string("h3_hex");
    duckdb_bind_add_result_column(info, col_hex.as_ptr(), type_varchar);

    match format {
        CategoricalOutputFormat::Wide => {
            // Option A: Majority class, count, fraction
            let col_maj_class = to_c_string("majority_class");
            duckdb_bind_add_result_column(info, col_maj_class.as_ptr(), type_bigint);

            let col_maj_fraction = to_c_string("majority_fraction");
            duckdb_bind_add_result_column(info, col_maj_fraction.as_ptr(), type_double);

            let col_maj_cnt = to_c_string("majority_count");
            duckdb_bind_add_result_column(info, col_maj_cnt.as_ptr(), type_double);

            let col_uniq = to_c_string("unique_classes");
            duckdb_bind_add_result_column(info, col_uniq.as_ptr(), type_bigint);

            let col_tot = to_c_string("total_count");
            duckdb_bind_add_result_column(info, col_tot.as_ptr(), type_double);

            // Option B: JSON Histogram
            let col_hist = to_c_string("histogram");
            duckdb_bind_add_result_column(info, col_hist.as_ptr(), type_varchar);
        }
        CategoricalOutputFormat::Long => {
            // Option C: Long form (h3_index, h3_hex, category, count, fraction, total_count)
            let col_cat = to_c_string("category");
            duckdb_bind_add_result_column(info, col_cat.as_ptr(), type_bigint);

            let col_cnt = to_c_string("count");
            duckdb_bind_add_result_column(info, col_cnt.as_ptr(), type_double);

            let col_frac = to_c_string("fraction");
            duckdb_bind_add_result_column(info, col_frac.as_ptr(), type_double);

            let col_tot = to_c_string("total_count");
            duckdb_bind_add_result_column(info, col_tot.as_ptr(), type_double);
        }
    }

    let mut type_ubigint_mut = type_ubigint;
    duckdb_destroy_logical_type(&mut type_ubigint_mut);
    let mut type_varchar_mut = type_varchar;
    duckdb_destroy_logical_type(&mut type_varchar_mut);
    let mut type_bigint_mut = type_bigint;
    duckdb_destroy_logical_type(&mut type_bigint_mut);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);

    // Cardinality estimation
    let estimated_cardinality = if let Ok(reader) = GeoTiffStreamReader::open(&file_path) {
        let w = reader.metadata.width as u64;
        let h = reader.metadata.height as u64;
        (w * h / 10).max(1)
    } else {
        10_000
    };
    duckdb_bind_set_cardinality(info, estimated_cardinality as idx_t, false);

    let bind_data = Box::new(RasterH3CategoricalBindData {
        file_path,
        resolution,
        source_crs,
        nodata,
        chunk_size,
        bbox,
        sampling,
        band,
        format,
    });

    duckdb_bind_set_bind_data(
        info,
        Box::into_raw(bind_data) as *mut c_void,
        Some(delete_bind_data),
    );
}

/// Global init callback for categorical aggregation
pub unsafe extern "C" fn raster_h3_categorical_init(info: duckdb_init_info) {
    let bind_data_ptr = duckdb_init_get_bind_data(info) as *const RasterH3CategoricalBindData;
    if bind_data_ptr.is_null() {
        let err_msg = to_c_string("Missing bind data in categorical init");
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
        sampling: bind_data.sampling.clone(),
    };

    let streamer = match CategoricalHorizonStreamer::new(reader, &config) {
        Ok(s) => s,
        Err(e) => {
            let err_msg = CString::new(format!("Failed to initialize categorical streamer: {}", e))
                .unwrap_or_else(|_| CString::new("Failed to init categorical streamer").unwrap());
            duckdb_init_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let global_data = Box::new(RasterH3CategoricalGlobalData {
        streamer: Mutex::new(streamer),
        format: bind_data.format,
        long_queue: Mutex::new(VecDeque::new()),
    });

    duckdb_init_set_init_data(
        info,
        Box::into_raw(global_data) as *mut c_void,
        Some(delete_global_data),
    );
}

/// Thread-local init callback for categorical aggregation
pub unsafe extern "C" fn raster_h3_categorical_init_local(info: duckdb_init_info) {
    let local_data = Box::new(RasterH3CategoricalLocalData { thread_id: 0 });
    duckdb_init_set_init_data(
        info,
        Box::into_raw(local_data) as *mut c_void,
        Some(delete_local_data),
    );
}

/// Scan callback for categorical aggregation
pub unsafe extern "C" fn raster_h3_categorical_scan(
    info: duckdb_function_info,
    output: duckdb_data_chunk,
) {
    let global_data_ptr = duckdb_function_get_init_data(info) as *const RasterH3CategoricalGlobalData;
    if global_data_ptr.is_null() {
        duckdb_data_chunk_set_size(output, 0);
        return;
    }
    let global_data = &*global_data_ptr;

    match global_data.format {
        CategoricalOutputFormat::Wide => {
            // Options A & B: Wide format (1 row per hex)
            let batch = {
                let mut streamer = match global_data.streamer.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                streamer.fetch_next_batch(2048)
            };

            let batch_size = batch.len();
            if batch_size == 0 {
                duckdb_data_chunk_set_size(output, 0);
                return;
            }

            let v_h3 = duckdb_data_chunk_get_vector(output, 0);
            let v_hex = duckdb_data_chunk_get_vector(output, 1);
            let v_maj_cls = duckdb_data_chunk_get_vector(output, 2);
            let v_maj_frac = duckdb_data_chunk_get_vector(output, 3);
            let v_maj_cnt = duckdb_data_chunk_get_vector(output, 4);
            let v_uniq = duckdb_data_chunk_get_vector(output, 5);
            let v_tot = duckdb_data_chunk_get_vector(output, 6);
            let v_hist = duckdb_data_chunk_get_vector(output, 7);

            let p_h3 = duckdb_vector_get_data(v_h3) as *mut u64;
            let p_maj_cls = duckdb_vector_get_data(v_maj_cls) as *mut i64;
            let p_maj_frac = duckdb_vector_get_data(v_maj_frac) as *mut f64;
            let p_maj_cnt = duckdb_vector_get_data(v_maj_cnt) as *mut f64;
            let p_uniq = duckdb_vector_get_data(v_uniq) as *mut i64;
            let p_tot = duckdb_vector_get_data(v_tot) as *mut f64;

            let mut hex_buf = [0u8; 16];

            for (i, (cell_u64, acc)) in batch.iter().enumerate() {
                let row_idx = i as u64;

                *p_h3.add(i) = *cell_u64;

                let hex_slice = fast_hex_u64(*cell_u64, &mut hex_buf);
                duckdb_vector_assign_string_element_len(
                    v_hex,
                    row_idx,
                    hex_slice.as_ptr() as *const c_char,
                    hex_slice.len() as idx_t,
                );

                let (maj_cls, maj_cnt, maj_frac) = acc.majority();
                *p_maj_cls.add(i) = maj_cls;
                *p_maj_frac.add(i) = maj_frac;
                *p_maj_cnt.add(i) = maj_cnt;
                *p_uniq.add(i) = acc.unique_classes() as i64;
                *p_tot.add(i) = acc.total_count;

                let hist_json = acc.histogram_json();
                duckdb_vector_assign_string_element_len(
                    v_hist,
                    row_idx,
                    hist_json.as_ptr() as *const c_char,
                    hist_json.len() as idx_t,
                );
            }

            duckdb_data_chunk_set_size(output, batch_size as idx_t);
        }
        CategoricalOutputFormat::Long => {
            // Option C: Long format (1 row per (hex, category))
            let mut long_queue = match global_data.long_queue.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            while long_queue.len() < 2048 {
                let batch = {
                    let mut streamer = match global_data.streamer.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    streamer.fetch_next_batch(256)
                };

                if batch.is_empty() {
                    break;
                }

                for (cell_u64, acc) in batch {
                    let mut sorted_cats: Vec<_> = acc.counts.keys().copied().collect();
                    sorted_cats.sort_unstable();

                    for cat in sorted_cats {
                        let cnt = acc.counts.get(&cat).copied().unwrap_or(0.0);
                        let fraction = if acc.total_count > 0.0 {
                            cnt / acc.total_count
                        } else {
                            0.0
                        };
                        long_queue.push_back(LongCategoricalRow {
                            cell_u64,
                            category: cat,
                            count: cnt,
                            fraction,
                            total_count: acc.total_count,
                        });
                    }
                }
            }

            let num_taken = 2048.min(long_queue.len());
            if num_taken == 0 {
                duckdb_data_chunk_set_size(output, 0);
                return;
            }

            let v_h3 = duckdb_data_chunk_get_vector(output, 0);
            let v_hex = duckdb_data_chunk_get_vector(output, 1);
            let v_cat = duckdb_data_chunk_get_vector(output, 2);
            let v_cnt = duckdb_data_chunk_get_vector(output, 3);
            let v_frac = duckdb_data_chunk_get_vector(output, 4);
            let v_tot = duckdb_data_chunk_get_vector(output, 5);

            let p_h3 = duckdb_vector_get_data(v_h3) as *mut u64;
            let p_cat = duckdb_vector_get_data(v_cat) as *mut i64;
            let p_cnt = duckdb_vector_get_data(v_cnt) as *mut f64;
            let p_frac = duckdb_vector_get_data(v_frac) as *mut f64;
            let p_tot = duckdb_vector_get_data(v_tot) as *mut f64;

            let mut hex_buf = [0u8; 16];

            for i in 0..num_taken {
                let row = long_queue.pop_front().unwrap();
                let row_idx = i as u64;

                *p_h3.add(i) = row.cell_u64;

                let hex_slice = fast_hex_u64(row.cell_u64, &mut hex_buf);
                duckdb_vector_assign_string_element_len(
                    v_hex,
                    row_idx,
                    hex_slice.as_ptr() as *const c_char,
                    hex_slice.len() as idx_t,
                );

                *p_cat.add(i) = row.category;
                *p_cnt.add(i) = row.count;
                *p_frac.add(i) = row.fraction;
                *p_tot.add(i) = row.total_count;
            }

            duckdb_data_chunk_set_size(output, num_taken as idx_t);
        }
    }
}

/// Register `h3_raster_categorical_aggregate` and `h3_raster_categorical` table functions
pub unsafe fn register_categorical_table_function(
    con: duckdb_connection,
) -> std::result::Result<(), String> {
    for name in &["h3_raster_categorical_aggregate", "h3_raster_categorical"] {
        let fn_name = to_c_string(name);
        let tf = duckdb_create_table_function();
        duckdb_table_function_set_name(tf, fn_name.as_ptr());

        // Positional parameter: file_path (VARCHAR)
        let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
        duckdb_table_function_add_parameter(tf, type_varchar);

        // Named parameters
        let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
        let type_double = duckdb_create_logical_type(DuckDBType::Double);

        let name_res = to_c_string("resolution");
        duckdb_table_function_add_named_parameter(tf, name_res.as_ptr(), type_bigint);

        let name_crs = to_c_string("source_crs");
        duckdb_table_function_add_named_parameter(tf, name_crs.as_ptr(), type_varchar);

        let name_nodata = to_c_string("nodata");
        duckdb_table_function_add_named_parameter(tf, name_nodata.as_ptr(), type_double);

        let name_chunk = to_c_string("chunk_size");
        duckdb_table_function_add_named_parameter(tf, name_chunk.as_ptr(), type_bigint);

        let name_band = to_c_string("band");
        duckdb_table_function_add_named_parameter(tf, name_band.as_ptr(), type_bigint);

        let name_sampling = to_c_string("sampling");
        duckdb_table_function_add_named_parameter(tf, name_sampling.as_ptr(), type_varchar);

        let name_fmt = to_c_string("format");
        duckdb_table_function_add_named_parameter(tf, name_fmt.as_ptr(), type_varchar);

        let name_min_lon = to_c_string("min_lon");
        let name_min_lat = to_c_string("min_lat");
        let name_max_lon = to_c_string("max_lon");
        let name_max_lat = to_c_string("max_lat");
        duckdb_table_function_add_named_parameter(tf, name_min_lon.as_ptr(), type_double);
        duckdb_table_function_add_named_parameter(tf, name_min_lat.as_ptr(), type_double);
        duckdb_table_function_add_named_parameter(tf, name_max_lon.as_ptr(), type_double);
        duckdb_table_function_add_named_parameter(tf, name_max_lat.as_ptr(), type_double);

        duckdb_table_function_set_bind(tf, raster_h3_categorical_bind);
        duckdb_table_function_set_init(tf, raster_h3_categorical_init);
        duckdb_table_function_set_local_init(tf, raster_h3_categorical_init_local);
        duckdb_table_function_set_function(tf, raster_h3_categorical_scan);
        duckdb_table_function_supports_projection_pushdown(tf, false);

        let state = duckdb_register_table_function(con, tf);

        let mut type_varchar_mut = type_varchar;
        duckdb_destroy_logical_type(&mut type_varchar_mut);
        let mut type_bigint_mut = type_bigint;
        duckdb_destroy_logical_type(&mut type_bigint_mut);
        let mut type_double_mut = type_double;
        duckdb_destroy_logical_type(&mut type_double_mut);

        let mut tf_mut = tf;
        duckdb_destroy_table_function(&mut tf_mut);

        if state != DuckDBState::Success {
            return Err(format!("Failed to register {} table function", name));
        }
    }

    Ok(())
}
