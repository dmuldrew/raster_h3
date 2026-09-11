use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::aggregator::multi_horizon::{
    MultiContinuousRecord, MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::to_c_string;
use crate::functions::bind_utils::{
    add_named_parameter, add_positional_parameter, register_common_raster_named_parameters,
    BindHelper,
};
use crate::functions::fast_hex::fast_hex_u64;
use crate::functions::wkb::h3_index_to_wkb;
use crate::raster::geotiff::GeoTiffStreamReader;

/// User-data bound during table function query compilation
pub struct RasterH3BindData {
    pub file_path: String,
    pub resolved_paths: Vec<std::path::PathBuf>,
    pub overlap_rule: crate::raster::mosaic::OverlapRule,
    pub resolutions: Vec<u8>,
    pub source_crs: Option<String>,
    pub nodata: Option<f64>,
    pub chunk_size: u32,
    pub bbox: Option<[f64; 4]>,
    pub sampling: SamplingPattern,
    pub band: u32,
    pub spectral_formula: Option<crate::aggregator::multi_horizon::SpectralFormula>,
    pub min_count: Option<f64>,
    pub min_mean: Option<f64>,
    pub max_mean: Option<f64>,
    pub compact: bool,
    pub quantiles: Vec<QuantileTarget>,
    pub emit_geom: bool,
}

/// Global scan state holding the streaming multi-core horizon aggregator and concurrent batch queue
pub struct RasterH3GlobalData {
    pub streamer: Mutex<MultiScanHorizonStreamer>,
    pub ready_batches: Mutex<VecDeque<Vec<MultiContinuousRecord>>>,
    pub is_finished: AtomicBool,
    pub projected_columns: Vec<usize>,
    pub quantiles: Vec<QuantileTarget>,
    pub emit_geom: bool,
}

/// Thread-local state for parallel DuckDB execution threads
pub struct RasterH3LocalData {
    pub thread_id: usize,
    pub hex_buf: [u8; 16],
    pub wkb_buf: [u8; 128],
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
    let bind = BindHelper::new(info);
    if bind.parameter_count() < 1 {
        bind.set_error("h3_raster_continuous_aggregate requires at least 1 argument: file_path");
        return;
    }

    let file_path = match bind.get_string_param(0) {
        Some(s) => s,
        None => {
            bind.set_error("Invalid file_path parameter");
            return;
        }
    };

    let resolutions = bind.parse_resolutions(8, Some(1));
    let source_crs = bind.parse_source_crs();
    let nodata = bind.get_named_double("nodata");
    let chunk_size = bind.get_named_int("chunk_size").filter(|&cs| cs > 0).unwrap_or(512) as u32;
    let band = bind.get_named_int("band").filter(|&b| b > 0).unwrap_or(1) as u32;
    let sampling = bind.parse_sampling();
    let bbox = bind.parse_bbox();

    // Named parameter: formula (VARCHAR)
    let formula_str = bind.get_named_string("formula");
    let nir_band = bind.get_named_int("nir_band").filter(|&b| b > 0).unwrap_or(4) as usize;
    let red_band = bind.get_named_int("red_band").filter(|&b| b > 0).unwrap_or(3) as usize;
    let green_band = bind.get_named_int("green_band").filter(|&b| b > 0).unwrap_or(2) as usize;
    let blue_band = bind.get_named_int("blue_band").filter(|&b| b > 0).unwrap_or(1) as usize;
    let swir_band = bind.get_named_int("swir_band").filter(|&b| b > 0).unwrap_or(6) as usize;

    let spectral_formula = formula_str.as_deref().and_then(|f| {
        crate::aggregator::multi_horizon::SpectralFormula::parse(
            f, nir_band, red_band, green_band, blue_band, swir_band,
        )
    });

    let min_count = bind.get_named_double("min_count");
    let min_mean = bind.get_named_double("min_mean");
    let max_mean = bind.get_named_double("max_mean");
    let compact = bind.get_named_bool("compact").unwrap_or(false);
    let overlap_rule = bind.parse_overlap_rule();

    let mut quantiles = Vec::new();
    let q_param_val = bind.get_named_string("quantiles").or_else(|| bind.get_named_string("percentiles"));
    if let Some(s) = q_param_val {
        match QuantileTarget::parse_list(&s) {
            Ok(q_targets) => quantiles = q_targets,
            Err(e) => {
                bind.set_error(&format!("Invalid quantiles parameter: {}", e));
                return;
            }
        }
    }

    let emit_geom = bind.get_named_bool("geom").unwrap_or_else(crate::ffi::is_geometry_available);

    let resolved_paths = match crate::raster::mosaic::resolve_raster_sources(&file_path) {
        Ok(paths) => paths,
        Err(e) => {
            bind.set_error(&format!("Failed to resolve raster source(s): {}", e));
            return;
        }
    };

    // Add Output Columns:
    bind.add_result_column("h3_index", DuckDBType::UBigInt);
    bind.add_result_column("h3_hex", DuckDBType::Varchar);
    bind.add_result_column("mean", DuckDBType::Double);
    bind.add_result_column("stddev", DuckDBType::Double);
    bind.add_result_column("count", DuckDBType::Double);
    bind.add_result_column("min", DuckDBType::Double);
    bind.add_result_column("max", DuckDBType::Double);
    bind.add_result_column("sum", DuckDBType::Double);
    bind.add_result_column("resolution", DuckDBType::UTinyInt);
    bind.add_result_column("wkb", DuckDBType::Blob);

    if emit_geom {
        let mut type_geom = crate::ffi::create_geometry_logical_type();
        bind.add_custom_result_column("geom", type_geom);
        duckdb_destroy_logical_type(&mut type_geom);
    }

    for target in &quantiles {
        bind.add_result_column(target.column_name(), DuckDBType::Double);
    }

    // Approximate H3 cell areas in m^2 by resolution (0 to 15) for query planner cardinality estimation
    const H3_AREA_M2: [f64; 16] = [
        4.357e12, 6.097e11, 8.680e10, 1.239e10, 1.770e9, 2.529e8,
        3.613e7, 5.161e6, 7.373e5, 1.053e5, 1.505e4, 2.150e3,
        3.071e2, 4.387e1, 6.268e0, 8.954e-1,
    ];

    let estimated_cardinality = if let Some(first_path) = resolved_paths.first() {
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
            for &res in &resolutions {
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
    };

    duckdb_bind_set_cardinality(info, estimated_cardinality as idx_t, false);

    let bind_data = Box::new(RasterH3BindData {
        file_path,
        resolved_paths,
        overlap_rule,
        resolutions,
        source_crs,
        nodata,
        chunk_size,
        bbox,
        sampling,
        band,
        spectral_formula,
        min_count,
        min_mean,
        max_mean,
        compact,
        quantiles,
        emit_geom,
    });

    duckdb_bind_set_bind_data(
        info,
        Box::into_raw(bind_data) as *mut c_void,
        Some(delete_bind_data),
    );
}

/// Global init callback: opens GeoTIFF stream reader and initializes streaming scanline horizon aggregator
pub unsafe extern "C" fn raster_h3_init(info: duckdb_init_info) {
    let bind_data_ptr = duckdb_init_get_bind_data(info) as *const RasterH3BindData;
    if bind_data_ptr.is_null() {
        let err_msg = to_c_string("Missing bind data in init");
        duckdb_init_set_error(info, err_msg.as_ptr());
        return;
    }
    let bind_data = &*bind_data_ptr;

    let mosaic = match crate::raster::mosaic::MosaicReader::open(
        &bind_data.resolved_paths,
        bind_data.bbox,
        bind_data.source_crs.as_deref(),
        bind_data.overlap_rule,
    ) {
        Ok(m) => std::sync::Arc::new(m),
        Err(e) => {
            let err_msg = CString::new(format!("Failed to open raster mosaic: {}", e))
                .unwrap_or_else(|_| CString::new("Failed to open raster mosaic").unwrap());
            duckdb_init_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let col_count = duckdb_init_get_column_count(info);
    let mut projected_columns = Vec::with_capacity(col_count as usize);
    for i in 0..col_count {
        projected_columns.push(duckdb_init_get_column_index(info, i) as usize);
    }

    let mut config = MultiResolutionConfig::new(bind_data.resolutions.clone());
    config.overlap_rule = bind_data.overlap_rule;
    config.custom_crs = bind_data.source_crs.clone();
    config.custom_nodata = bind_data.nodata;
    config.bbox = bind_data.bbox;
    config.sampling = bind_data.sampling.clone();
    config.band = bind_data.band as usize;
    config.spectral_formula = bind_data.spectral_formula;
    config.min_count = bind_data.min_count;
    config.min_mean = bind_data.min_mean;
    config.max_mean = bind_data.max_mean;
    config.compact = bind_data.compact;
    config.quantiles = bind_data.quantiles.clone();

    let streamer = match MultiScanHorizonStreamer::new_mosaic(mosaic, &config) {
        Ok(s) => s,
        Err(e) => {
            let err_msg = CString::new(format!("Failed to initialize multi-streamer: {}", e))
                .unwrap_or_else(|_| CString::new("Failed to init streamer").unwrap());
            duckdb_init_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let global_data = Box::new(RasterH3GlobalData {
        streamer: Mutex::new(streamer),
        ready_batches: Mutex::new(VecDeque::new()),
        is_finished: AtomicBool::new(false),
        projected_columns,
        quantiles: bind_data.quantiles.clone(),
        emit_geom: bind_data.emit_geom,
    });

    duckdb_init_set_init_data(
        info,
        Box::into_raw(global_data) as *mut c_void,
        Some(delete_global_data),
    );
}

/// Thread-local init callback for multi-threaded parallel DuckDB execution
pub unsafe extern "C" fn raster_h3_init_local(info: duckdb_init_info) {
    let local_data = Box::new(RasterH3LocalData {
        thread_id: 0,
        hex_buf: [0u8; 16],
        wkb_buf: [0u8; 128],
    });
    duckdb_init_set_init_data(
        info,
        Box::into_raw(local_data) as *mut c_void,
        Some(delete_local_data),
    );
}

/// Scan callback: streaming vector emission directly from scanline horizon eviction with projection pushdown
pub unsafe extern "C" fn raster_h3_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    let global_data_ptr = duckdb_function_get_init_data(info) as *const RasterH3GlobalData;
    if global_data_ptr.is_null() {
        duckdb_data_chunk_set_size(output, 0);
        return;
    }
    let global_data = &*global_data_ptr;

    let local_data_ptr = duckdb_function_get_local_init_data(info) as *mut RasterH3LocalData;

    // Retrieve next batch of completed records concurrently across DuckDB worker threads
    let batch_opt = {
        // Fast path 1: check if ready_batches already has pre-evicted records (~10ns lock)
        let mut ready_q = match global_data.ready_batches.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(b) = ready_q.pop_front() {
            Some(b)
        } else if global_data.is_finished.load(Ordering::Acquire) {
            None
        } else {
            drop(ready_q);

            // Path 2: acquire streamer lock to refill ready_batches with multiple chunks
            let mut streamer = match global_data.streamer.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            let mut ready_q = match global_data.ready_batches.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            if let Some(b) = ready_q.pop_front() {
                Some(b)
            } else if global_data.is_finished.load(Ordering::Acquire) {
                None
            } else {
                const BATCH_SIZE: usize = 2048;
                const REFILL_SIZE: usize = BATCH_SIZE * 4;

                let mut current_chunk = Vec::with_capacity(BATCH_SIZE);
                let mut my_batch = None;

                streamer.drain_completed_into(REFILL_SIZE, |_i, rec| {
                    current_chunk.push(rec);
                    if current_chunk.len() == BATCH_SIZE {
                        if my_batch.is_none() {
                            my_batch = Some(std::mem::replace(
                                &mut current_chunk,
                                Vec::with_capacity(BATCH_SIZE),
                            ));
                        } else {
                            ready_q.push_back(std::mem::replace(
                                &mut current_chunk,
                                Vec::with_capacity(BATCH_SIZE),
                            ));
                        }
                    }
                });

                if !current_chunk.is_empty() {
                    if my_batch.is_none() {
                        my_batch = Some(current_chunk);
                    } else {
                        ready_q.push_back(current_chunk);
                    }
                }

                if my_batch.is_none() {
                    global_data.is_finished.store(true, Ordering::Release);
                }

                my_batch
            }
        }
    };

    let batch = match batch_opt {
        Some(b) if !b.is_empty() => b,
        _ => {
            duckdb_data_chunk_set_size(output, 0);
            return;
        }
    };

    let proj_cols = &global_data.projected_columns;

    // Fast path: if 0 columns are projected (e.g. SELECT count(*))
    if proj_cols.is_empty() {
        duckdb_data_chunk_set_size(output, batch.len() as idx_t);
        return;
    }

    // Map output vector pointers only for projected columns
    // Schema column mapping:
    // 0: h3_index UBIGINT
    // 1: h3_hex VARCHAR
    // 2: mean DOUBLE
    // 3: stddev DOUBLE
    // 4: count DOUBLE
    // 5: min DOUBLE
    // 6: max DOUBLE
    // 7: sum DOUBLE
    // 8: resolution UTINYINT
    // 9: wkb BLOB
    // 10: geom GEOMETRY (if emit_geom)
    let mut vec_h3: Option<*mut u64> = None;
    let mut vec_hex: Option<duckdb_vector> = None;
    let mut vec_mean: Option<*mut f64> = None;
    let mut vec_stddev: Option<*mut f64> = None;
    let mut vec_count: Option<*mut f64> = None;
    let mut vec_min: Option<*mut f64> = None;
    let mut vec_max: Option<*mut f64> = None;
    let mut vec_sum: Option<*mut f64> = None;
    let mut vec_res: Option<*mut u8> = None;
    let mut vec_wkb: Option<duckdb_vector> = None;
    let mut vec_geom: Option<duckdb_vector> = None;
    let mut vec_quantiles: Vec<(usize, *mut f64)> = Vec::new();

    let q_start_col = if global_data.emit_geom { 11 } else { 10 };

    for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
        let v = duckdb_data_chunk_get_vector(output, out_idx as idx_t);
        match orig_col {
            0 => vec_h3 = Some(duckdb_vector_get_data(v) as *mut u64),
            1 => vec_hex = Some(v),
            2 => vec_mean = Some(duckdb_vector_get_data(v) as *mut f64),
            3 => vec_stddev = Some(duckdb_vector_get_data(v) as *mut f64),
            4 => vec_count = Some(duckdb_vector_get_data(v) as *mut f64),
            5 => vec_min = Some(duckdb_vector_get_data(v) as *mut f64),
            6 => vec_max = Some(duckdb_vector_get_data(v) as *mut f64),
            7 => vec_sum = Some(duckdb_vector_get_data(v) as *mut f64),
            8 => vec_res = Some(duckdb_vector_get_data(v) as *mut u8),
            9 => vec_wkb = Some(v),
            10 if global_data.emit_geom => vec_geom = Some(v),
            c if c >= q_start_col => {
                let q_idx = c - q_start_col;
                if q_idx < global_data.quantiles.len() {
                    vec_quantiles.push((q_idx, duckdb_vector_get_data(v) as *mut f64));
                }
            }
            _ => {}
        }
    }

    let mut fallback_hex_buf = [0u8; 16];
    let mut fallback_wkb_buf = [0u8; 128];
    let (hex_buf, wkb_buf) = if !local_data_ptr.is_null() {
        (
            &mut (*local_data_ptr).hex_buf,
            &mut (*local_data_ptr).wkb_buf,
        )
    } else {
        (&mut fallback_hex_buf, &mut fallback_wkb_buf)
    };

    let batch_len = batch.len();

    if let Some(p) = vec_h3 {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.h3_index;
        }
    }
    if let Some(p) = vec_mean {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.accumulator.mean();
        }
    }
    if let Some(p) = vec_stddev {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.accumulator.stddev();
        }
    }
    if let Some(p) = vec_count {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.accumulator.count;
        }
    }
    if let Some(p) = vec_min {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.accumulator.min;
        }
    }
    if let Some(p) = vec_max {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.accumulator.max;
        }
    }
    if let Some(p) = vec_sum {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.accumulator.sum;
        }
    }
    if let Some(p) = vec_res {
        for (i, rec) in batch.iter().enumerate() {
            *p.add(i) = rec.resolution;
        }
    }
    for &(q_idx, ptr) in &vec_quantiles {
        match &global_data.quantiles[q_idx] {
            QuantileTarget::Percentile(q, _) => {
                let q_val = *q;
                for (i, rec) in batch.iter().enumerate() {
                    *ptr.add(i) = rec.accumulator.quantile(q_val);
                }
            }
            QuantileTarget::Iqr(_) => {
                for (i, rec) in batch.iter().enumerate() {
                    *ptr.add(i) = rec.accumulator.iqr();
                }
            }
        }
    }
    if let Some(v) = vec_hex {
        for (i, rec) in batch.iter().enumerate() {
            let hex_slice = fast_hex_u64(rec.h3_index, hex_buf);
            duckdb_vector_assign_string_element_len(
                v,
                i as u64,
                hex_slice.as_ptr() as *const c_char,
                hex_slice.len() as idx_t,
            );
        }
    }
    if vec_wkb.is_some() || vec_geom.is_some() {
        for (i, rec) in batch.iter().enumerate() {
            let row_idx = i as u64;
            let wkb_len_opt = h3_index_to_wkb(rec.h3_index, wkb_buf);
            if let Some(v) = vec_wkb {
                if let Some(wkb_len) = wkb_len_opt {
                    duckdb_vector_assign_string_element_len(
                        v,
                        row_idx,
                        wkb_buf.as_ptr() as *const c_char,
                        wkb_len as idx_t,
                    );
                } else {
                    duckdb_vector_assign_string_element_len(v, row_idx, std::ptr::null(), 0);
                }
            }
            if let Some(v) = vec_geom {
                if let Some(wkb_len) = wkb_len_opt {
                    duckdb_vector_assign_string_element_len(
                        v,
                        row_idx,
                        wkb_buf.as_ptr() as *const c_char,
                        wkb_len as idx_t,
                    );
                } else {
                    duckdb_vector_assign_string_element_len(v, row_idx, std::ptr::null(), 0);
                }
            }
        }
    }


    duckdb_data_chunk_set_size(output, batch_len as idx_t);
}

/// Register `h3_raster_continuous_aggregate` and `h3_raster_continuous` table functions
pub unsafe fn register_table_function(con: duckdb_connection) -> std::result::Result<(), String> {
    let names = [
        "h3_raster_continuous_aggregate",
        "h3_raster_continuous",
        "raster_h3",
    ];

    for name in &names {
        let fn_name = to_c_string(name);
        let tf = duckdb_create_table_function();
        duckdb_table_function_set_name(tf, fn_name.as_ptr());

        // Positional Parameters:
        // 0: file_path (VARCHAR)
        add_positional_parameter(tf, DuckDBType::Varchar);

        // Standard Common Raster Named Parameters
        register_common_raster_named_parameters(tf);

        // Continuous-specific named parameters
        add_named_parameter(tf, "chunk_size", DuckDBType::BigInt);
        add_named_parameter(tf, "formula", DuckDBType::Varchar);
        add_named_parameter(tf, "nir_band", DuckDBType::BigInt);
        add_named_parameter(tf, "red_band", DuckDBType::BigInt);
        add_named_parameter(tf, "green_band", DuckDBType::BigInt);
        add_named_parameter(tf, "blue_band", DuckDBType::BigInt);
        add_named_parameter(tf, "swir_band", DuckDBType::BigInt);
        add_named_parameter(tf, "min_count", DuckDBType::Double);
        add_named_parameter(tf, "min_mean", DuckDBType::Double);
        add_named_parameter(tf, "max_mean", DuckDBType::Double);
        add_named_parameter(tf, "geom", DuckDBType::Boolean);
        add_named_parameter(tf, "quantiles", DuckDBType::Varchar);
        add_named_parameter(tf, "percentiles", DuckDBType::Varchar);

        // Set callbacks including parallel init_local and projection pushdown
        duckdb_table_function_set_bind(tf, raster_h3_bind);
        duckdb_table_function_set_init(tf, raster_h3_init);
        duckdb_table_function_set_local_init(tf, raster_h3_init_local);
        duckdb_table_function_set_function(tf, raster_h3_scan);
        duckdb_table_function_supports_projection_pushdown(tf, true);

        let state = duckdb_register_table_function(con, tf);
        let mut tf_mut = tf;
        duckdb_destroy_table_function(&mut tf_mut);

        if state != DuckDBState::Success {
            return Err(format!("Failed to register {} table function", name));
        }
    }

    Ok(())
}
