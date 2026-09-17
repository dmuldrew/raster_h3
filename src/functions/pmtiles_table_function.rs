//! DuckDB Table Function for Direct GeoTIFF-to-PMTiles v3 Generation
//!
//! Exposes:
//!   SELECT * FROM h3_raster_to_pmtiles('california_dem.tif', 'california_elevation.pmtiles', resolution := 8);

use std::ffi::c_void;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::aggregator::multi_horizon::MultiResolutionConfig;
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::{ffi_bind_guard, ffi_init_guard, ffi_scan_guard, to_c_string};
use crate::functions::bind_utils::lifecycle::set_table_function_init_data;
use crate::functions::bind_utils::{
    add_named_parameter, add_positional_parameter, delete_boxed, BindHelper, ChunkWriter,
};
use crate::pmtiles::tiler::{h3_res_to_zoom, H3PmtilesTiler};

/// Bind data parsed during SQL query planning
pub struct PmtilesBindData {
    pub file_path: String,
    pub output_pmtiles: String,
    pub resolutions: Vec<u8>,
    pub band: usize,
    pub custom_nodata: Option<f64>,
    pub sampling: SamplingPattern,
    pub is_categorical: bool,
    pub properties: Option<String>,
}

/// Global execution state for the single-row generator
pub struct PmtilesGlobalData {
    pub executed: AtomicBool,
}

/// Declare output columns matching the PMTiles summary schema
unsafe fn add_pmtiles_summary_columns(bind: &BindHelper) {
    bind.add_result_column("total_hexagons", DuckDBType::BigInt);
    bind.add_result_column("pmtiles_size_bytes", DuckDBType::BigInt);
    bind.add_result_column("min_zoom", DuckDBType::BigInt);
    bind.add_result_column("max_zoom", DuckDBType::BigInt);
    bind.add_result_column("elapsed_ms", DuckDBType::Double);
    bind.add_result_column("output_path", DuckDBType::Varchar);
    bind.add_result_column("status", DuckDBType::Varchar);
    duckdb_bind_set_cardinality(bind.info, 1, true);
}

/// Bind callback: parses input arguments and defines output table schema
pub unsafe extern "C" fn pmtiles_bind(info: duckdb_bind_info) {
    ffi_bind_guard(info, || {
        let bind = BindHelper::new(info);

        if bind.parameter_count() < 2 {
            bind.set_error(
                "h3_raster_to_pmtiles requires at least 2 arguments: file_path and output_pmtiles",
            );
            return;
        }

        // 0: file_path (VARCHAR)
        let file_path = match bind.get_string_param(0) {
            Some(s) => s,
            None => {
                bind.set_error("Invalid file_path parameter");
                return;
            }
        };

        // 1: output_pmtiles (VARCHAR)
        let output_pmtiles = match bind.get_string_param(1) {
            Some(s) => s,
            None => {
                bind.set_error("Invalid output_pmtiles parameter");
                return;
            }
        };

        let resolutions = bind.parse_resolutions(8, None);
        let band = bind.get_named_int("band").unwrap_or(1).max(1) as usize;
        let custom_nodata = bind.get_named_double("nodata");
        let sampling = bind.parse_sampling();
        let is_categorical = bind.get_named_bool("categorical").unwrap_or(false);
        let properties = bind.get_named_string("properties");

        add_pmtiles_summary_columns(&bind);

        let bind_data = Box::new(PmtilesBindData {
            file_path,
            output_pmtiles,
            resolutions,
            band,
            custom_nodata,
            sampling,
            is_categorical,
            properties,
        });
        duckdb_bind_set_bind_data(
            info,
            Box::into_raw(bind_data) as *mut c_void,
            Some(delete_boxed::<PmtilesBindData>),
        );
    });
}

/// Init callback
pub unsafe extern "C" fn pmtiles_init(info: duckdb_init_info) {
    ffi_init_guard(info, || {
        set_table_function_init_data(
            info,
            PmtilesGlobalData {
                executed: AtomicBool::new(false),
            },
        );
    });
}

/// Scan callback: runs GeoTIFF-to-PMTiles conversion and streams the single summary row
pub unsafe extern "C" fn pmtiles_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    ffi_scan_guard(info, output, || {
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
        config.properties = bind_data.properties.clone();

        let start = Instant::now();
        let result = if bind_data.is_categorical {
            H3PmtilesTiler::process_categorical_geotiff_to_pmtiles(
                &bind_data.file_path,
                &bind_data.output_pmtiles,
                config,
            )
        } else {
            H3PmtilesTiler::process_geotiff_to_pmtiles(
                &bind_data.file_path,
                &bind_data.output_pmtiles,
                config,
            )
        };
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
            if z < min_z {
                min_z = z;
            }
            if z > max_z {
                max_z = z;
            }
        }

        // Populate the 1 output summary row
        let writer = ChunkWriter::new(output);
        writer.set_int64(0, 0, total_hexagons);
        writer.set_int64(1, 0, size_bytes);
        writer.set_int64(2, 0, min_z as i64);
        writer.set_int64(3, 0, max_z as i64);
        writer.set_double(4, 0, elapsed.as_secs_f64() * 1000.0);
        writer.set_string(5, 0, &bind_data.output_pmtiles);
        writer.set_string(6, 0, &status);
        writer.set_size(1);
    });
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

/// Bind callback: parses input arguments for `h3_parquet_to_pmtiles`
pub unsafe extern "C" fn parquet_pmtiles_bind(info: duckdb_bind_info) {
    ffi_bind_guard(info, || {
        let bind = BindHelper::new(info);

        if bind.parameter_count() < 2 {
            bind.set_error(
                "h3_parquet_to_pmtiles requires at least 2 arguments: parquet_path and output_pmtiles",
            );
            return;
        }

        // 0: parquet_path (VARCHAR)
        let parquet_path = match bind.get_string_param(0) {
            Some(s) => s,
            None => {
                bind.set_error("Invalid parquet_path parameter");
                return;
            }
        };

        // 1: output_pmtiles (VARCHAR)
        let output_pmtiles = match bind.get_string_param(1) {
            Some(s) => s,
            None => {
                bind.set_error("Invalid output_pmtiles parameter");
                return;
            }
        };

        let h3_column = bind
            .get_named_string("h3_column")
            .or_else(|| bind.get_named_string("h3_col"));

        add_pmtiles_summary_columns(&bind);

        let bind_data = Box::new(ParquetPmtilesBindData {
            parquet_path,
            output_pmtiles,
            h3_column,
        });
        duckdb_bind_set_bind_data(
            info,
            Box::into_raw(bind_data) as *mut c_void,
            Some(delete_boxed::<ParquetPmtilesBindData>),
        );
    });
}

/// Init callback for `h3_parquet_to_pmtiles`
pub unsafe extern "C" fn parquet_pmtiles_init(info: duckdb_init_info) {
    ffi_init_guard(info, || {
        set_table_function_init_data(
            info,
            ParquetPmtilesGlobalData {
                executed: AtomicBool::new(false),
            },
        );
    });
}

/// Scan callback: runs Parquet-to-PMTiles conversion and streams the single summary row
pub unsafe extern "C" fn parquet_pmtiles_scan(
    info: duckdb_function_info,
    output: duckdb_data_chunk,
) {
    ffi_scan_guard(info, output, || {
        let bind_data = &*(duckdb_function_get_bind_data(info) as *const ParquetPmtilesBindData);
        let global_data =
            &*(duckdb_function_get_init_data(info) as *const ParquetPmtilesGlobalData);

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

        // Populate the 1 output summary row
        let writer = ChunkWriter::new(output);
        writer.set_int64(0, 0, total_hexagons);
        writer.set_int64(1, 0, size_bytes);
        writer.set_int64(2, 0, min_z);
        writer.set_int64(3, 0, max_z);
        writer.set_double(4, 0, elapsed.as_secs_f64() * 1000.0);
        writer.set_string(5, 0, &bind_data.output_pmtiles);
        writer.set_string(6, 0, &status);
        writer.set_size(1);
    });
}

/// Register `h3_raster_to_pmtiles` and `h3_parquet_to_pmtiles` Table Functions with DuckDB
pub unsafe fn register_pmtiles_table_function(con: duckdb_connection) -> Result<(), String> {
    // 1. Register h3_raster_to_pmtiles
    let fn_name = to_c_string("h3_raster_to_pmtiles");
    let tf = duckdb_create_table_function();
    duckdb_table_function_set_name(tf, fn_name.as_ptr());

    // Positional parameters:
    add_positional_parameter(tf, DuckDBType::Varchar);
    add_positional_parameter(tf, DuckDBType::Varchar);

    // Named parameters:
    add_named_parameter(tf, "resolution", DuckDBType::BigInt);
    add_named_parameter(tf, "resolutions", DuckDBType::Varchar);
    add_named_parameter(tf, "min_resolution", DuckDBType::BigInt);
    add_named_parameter(tf, "max_resolution", DuckDBType::BigInt);
    add_named_parameter(tf, "sampling", DuckDBType::Varchar);
    add_named_parameter(tf, "band", DuckDBType::BigInt);
    add_named_parameter(tf, "nodata", DuckDBType::Double);
    add_named_parameter(tf, "categorical", DuckDBType::Boolean);
    add_named_parameter(tf, "properties", DuckDBType::Varchar);

    // Set callbacks
    duckdb_table_function_set_bind(tf, pmtiles_bind);
    duckdb_table_function_set_init(tf, pmtiles_init);
    duckdb_table_function_set_function(tf, pmtiles_scan);

    let state = duckdb_register_table_function(con, tf);

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
    add_positional_parameter(tf_parquet, DuckDBType::Varchar);
    add_positional_parameter(tf_parquet, DuckDBType::Varchar);

    // Named parameters:
    add_named_parameter(tf_parquet, "h3_column", DuckDBType::Varchar);
    add_named_parameter(tf_parquet, "h3_col", DuckDBType::Varchar);
    add_named_parameter(tf_parquet, "properties", DuckDBType::Varchar);

    // Set callbacks
    duckdb_table_function_set_bind(tf_parquet, parquet_pmtiles_bind);
    duckdb_table_function_set_init(tf_parquet, parquet_pmtiles_init);
    duckdb_table_function_set_function(tf_parquet, parquet_pmtiles_scan);

    let state2 = duckdb_register_table_function(con, tf_parquet);

    let mut tf_parquet_mut = tf_parquet;
    duckdb_destroy_table_function(&mut tf_parquet_mut);

    if state2 != DuckDBState::Success {
        return Err("Failed to register h3_parquet_to_pmtiles table function".to_string());
    }

    Ok(())
}
