use std::ffi::{c_void, CString};
use std::sync::Mutex;

use crate::aggregator::multi_horizon::{
    MultiContinuousRecord, MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::to_c_string;
use crate::functions::bind_utils::{
    add_named_parameter, add_positional_parameter, delete_boxed, estimate_raster_cardinality,
    extract_projected_columns, init_table_function_local, open_mosaic_or_set_error,
    register_common_raster_named_parameters, set_table_function_init_data, BindHelper, ChunkWriter,
    ConcurrentRecordQueue, TableFunctionLocalData,
};

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
    pub record_queue: ConcurrentRecordQueue<MultiContinuousRecord>,
    pub projected_columns: Vec<usize>,
    pub quantiles: Vec<QuantileTarget>,
    pub emit_geom: bool,
}

/// Thread-local state for parallel DuckDB execution threads (scratch buffers for hex string and WKB encoding)
pub type RasterH3LocalData = TableFunctionLocalData;

/// Bind callback: parses input arguments, defines output columns, and returns bind data
pub unsafe extern "C" fn raster_h3_bind(info: duckdb_bind_info) {
    let bind = BindHelper::new(info);
    let common = match bind.parse_common_raster_params("h3_raster_continuous_aggregate") {
        Some(p) => p,
        None => return,
    };

    // Continuous-specific named parameter: formula (VARCHAR)
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

    if common.emit_geom {
        let mut type_geom = crate::ffi::create_geometry_logical_type();
        bind.add_custom_result_column("geom", type_geom);
        duckdb_destroy_logical_type(&mut type_geom);
    }

    for target in &quantiles {
        bind.add_result_column(target.column_name(), DuckDBType::Double);
    }

    let estimated_cardinality = estimate_raster_cardinality(&common.resolved_paths, &common.resolutions);
    duckdb_bind_set_cardinality(info, estimated_cardinality as idx_t, false);

    let bind_data = Box::new(RasterH3BindData {
        file_path: common.file_path,
        resolved_paths: common.resolved_paths,
        overlap_rule: common.overlap_rule,
        resolutions: common.resolutions,
        source_crs: common.source_crs,
        nodata: common.nodata,
        chunk_size: common.chunk_size,
        bbox: common.bbox,
        sampling: common.sampling,
        band: common.band,
        spectral_formula,
        min_count,
        min_mean,
        max_mean,
        compact: common.compact,
        quantiles,
        emit_geom: common.emit_geom,
    });

    duckdb_bind_set_bind_data(
        info,
        Box::into_raw(bind_data) as *mut c_void,
        Some(delete_boxed::<RasterH3BindData>),
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

    let mosaic = match open_mosaic_or_set_error(
        info,
        &bind_data.resolved_paths,
        bind_data.bbox,
        bind_data.source_crs.as_deref(),
        bind_data.overlap_rule,
    ) {
        Some(m) => m,
        None => return,
    };

    let projected_columns = extract_projected_columns(info);

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

    let global_data = RasterH3GlobalData {
        streamer: Mutex::new(streamer),
        record_queue: ConcurrentRecordQueue::new(),
        projected_columns,
        quantiles: bind_data.quantiles.clone(),
        emit_geom: bind_data.emit_geom,
    };

    set_table_function_init_data(info, global_data);
}

/// Thread-local init callback for multi-threaded parallel DuckDB execution
pub unsafe extern "C" fn raster_h3_init_local(info: duckdb_init_info) {
    init_table_function_local(info);
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
    let batch_opt = global_data
        .record_queue
        .pop_or_refill(&global_data.streamer, |s, max_rows, f| {
            s.drain_completed_into(max_rows, f)
        });

    let batch = match batch_opt {
        Some(b) if !b.is_empty() => b,
        _ => {
            duckdb_data_chunk_set_size(output, 0);
            return;
        }
    };

    let proj_cols = &global_data.projected_columns;
    let writer = ChunkWriter::new(output);

    // Fast path: if 0 columns are projected (e.g. SELECT count(*))
    if proj_cols.is_empty() {
        writer.set_size(batch.len());
        return;
    }

    let mut fallback_hex_buf = [0u8; 16];
    let mut fallback_wkb_buf = [0u8; 128];
    let (hex_buf, wkb_buf) = TableFunctionLocalData::get_scratch_buffers(
        local_data_ptr,
        &mut fallback_hex_buf,
        &mut fallback_wkb_buf,
    );

    let batch_len = batch.len();
    let q_start_col = if global_data.emit_geom { 11 } else { 10 };
    let mut out_idx_wkb: Option<usize> = None;
    let mut out_idx_geom: Option<usize> = None;

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
    // 10/11+: quantiles DOUBLE
    for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
        match orig_col {
            0 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.h3_index)),
            1 => writer.write_hex_column(out_idx, batch.iter().map(|r| &r.h3_index), hex_buf),
            2 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.accumulator.mean())),
            3 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.accumulator.stddev())),
            4 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.accumulator.count)),
            5 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.accumulator.min)),
            6 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.accumulator.max)),
            7 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.accumulator.sum)),
            8 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.resolution)),
            9 => {
                out_idx_wkb = Some(out_idx);
            }
            10 if global_data.emit_geom => {
                out_idx_geom = Some(out_idx);
            }
            c if c >= q_start_col => {
                let q_idx = c - q_start_col;
                if q_idx < global_data.quantiles.len() {
                    match &global_data.quantiles[q_idx] {
                        QuantileTarget::Percentile(q, _) => {
                            let q_val = *q;
                            writer.fill_column(
                                out_idx,
                                batch_len,
                                batch.iter().map(|r| r.accumulator.quantile(q_val)),
                            );
                        }
                        QuantileTarget::Iqr(_) => {
                            writer.fill_column(
                                out_idx,
                                batch_len,
                                batch.iter().map(|r| r.accumulator.iqr()),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    writer.write_wkb_and_geom_columns(
        out_idx_wkb,
        out_idx_geom,
        batch.iter().map(|r| &r.h3_index),
        wkb_buf,
    );

    writer.set_size(batch_len);
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
