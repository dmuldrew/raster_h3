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

use std::ffi::c_void;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parquet::basic::Compression;

use crate::aggregator::multi_horizon::MultiResolutionConfig;
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::to_c_string;
use crate::functions::bind_utils::{
    add_named_parameter, add_positional_parameter, delete_boxed, BindHelper, ChunkWriter,
};
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
    pub geoparquet: bool,
}

/// Global execution state for the single-row generator
pub struct ParquetGlobalData {
    pub executed: AtomicBool,
}

/// Bind callback: parses input arguments and defines output table schema
pub unsafe extern "C" fn parquet_bind(info: duckdb_bind_info) {
    let bind = BindHelper::new(info);

    if bind.parameter_count() < 2 {
        bind.set_error("h3_raster_to_parquet requires at least 2 arguments: file_path and output_parquet");
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

    // 1: output_parquet (VARCHAR)
    let output_parquet = match bind.get_string_param(1) {
        Some(s) => s,
        None => {
            bind.set_error("Invalid output_parquet parameter");
            return;
        }
    };

    let mut resolution = 8u8;
    if let Some(r) = bind.get_named_int("resolution") {
        if (0..=15).contains(&r) {
            resolution = r as u8;
        }
    }

    let sampling = bind.parse_sampling();
    let band = bind.get_named_int("band").unwrap_or(1).max(1) as usize;
    let custom_nodata = bind.get_named_double("nodata");
    let is_categorical = bind.get_named_bool("categorical").unwrap_or(false);
    let compact = bind.get_named_bool("compact").unwrap_or(true);
    let geoparquet = bind.get_named_bool("geoparquet").or_else(|| bind.get_named_bool("geom")).unwrap_or(false);

    let mut compression = Compression::SNAPPY;
    if let Some(s) = bind.get_named_string("compression") {
        match s.to_lowercase().as_str() {
            "snappy" => compression = Compression::SNAPPY,
            "zstd" => compression = Compression::ZSTD(Default::default()),
            "gzip" | "flate" => compression = Compression::GZIP(Default::default()),
            "lz4" => compression = Compression::LZ4,
            "uncompressed" | "none" => compression = Compression::UNCOMPRESSED,
            _ => {}
        }
    }

    let row_group_size = bind.get_named_int("row_group_size").unwrap_or(131_072).max(1) as usize;
    let bbox = bind.parse_bbox();

    // Declare output summary schema:
    bind.add_result_column("total_hexagons", DuckDBType::BigInt);
    bind.add_result_column("parquet_size_bytes", DuckDBType::BigInt);
    bind.add_result_column("elapsed_ms", DuckDBType::Double);
    bind.add_result_column("hexagons_per_sec", DuckDBType::Double);
    bind.add_result_column("output_path", DuckDBType::Varchar);
    bind.add_result_column("status", DuckDBType::Varchar);

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
        geoparquet,
    });
    duckdb_bind_set_bind_data(info, Box::into_raw(bind_data) as *mut c_void, Some(delete_boxed::<ParquetBindData>));
}

/// Init callback
pub unsafe extern "C" fn parquet_init(info: duckdb_init_info) {
    let global_data = Box::new(ParquetGlobalData {
        executed: AtomicBool::new(false),
    });
    duckdb_init_set_init_data(info, Box::into_raw(global_data) as *mut c_void, Some(delete_boxed::<ParquetGlobalData>));
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
        geoparquet: bind_data.geoparquet,
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

    // Populate the 1 output summary row
    let writer = ChunkWriter::new(output);
    writer.set_int64(0, 0, total_hexagons);
    writer.set_int64(1, 0, size_bytes);
    writer.set_double(2, 0, elapsed_ms);
    writer.set_double(3, 0, rate);
    writer.set_string(4, 0, &bind_data.output_parquet);
    writer.set_string(5, 0, &status);
    writer.set_size(1);
}

/// Register `h3_raster_to_parquet` Table Function in DuckDB connection
pub unsafe fn register_parquet_table_function(con: duckdb_connection) -> Result<(), String> {
    let fn_name = to_c_string("h3_raster_to_parquet");
    let tf = duckdb_create_table_function();
    duckdb_table_function_set_name(tf, fn_name.as_ptr());

    // Positional parameters:
    add_positional_parameter(tf, DuckDBType::Varchar);
    add_positional_parameter(tf, DuckDBType::Varchar);

    // Named parameters:
    add_named_parameter(tf, "resolution", DuckDBType::BigInt);
    add_named_parameter(tf, "sampling", DuckDBType::Varchar);
    add_named_parameter(tf, "band", DuckDBType::BigInt);
    add_named_parameter(tf, "nodata", DuckDBType::Double);
    add_named_parameter(tf, "categorical", DuckDBType::Boolean);
    add_named_parameter(tf, "compact", DuckDBType::Boolean);
    add_named_parameter(tf, "compression", DuckDBType::Varchar);
    add_named_parameter(tf, "row_group_size", DuckDBType::BigInt);
    add_named_parameter(tf, "min_lon", DuckDBType::Double);
    add_named_parameter(tf, "min_lat", DuckDBType::Double);
    add_named_parameter(tf, "max_lon", DuckDBType::Double);
    add_named_parameter(tf, "max_lat", DuckDBType::Double);
    add_named_parameter(tf, "bbox", DuckDBType::Varchar);
    add_named_parameter(tf, "h3_cell", DuckDBType::BigInt);
    add_named_parameter(tf, "h3_hex", DuckDBType::Varchar);
    add_named_parameter(tf, "geoparquet", DuckDBType::Boolean);
    add_named_parameter(tf, "geom", DuckDBType::Boolean);

    // Set callbacks
    duckdb_table_function_set_bind(tf, parquet_bind);
    duckdb_table_function_set_init(tf, parquet_init);
    duckdb_table_function_set_function(tf, parquet_scan);

    let state = duckdb_register_table_function(con, tf);

    let mut tf_mut = tf;
    duckdb_destroy_table_function(&mut tf_mut);

    if state != DuckDBState::Success {
        return Err("Failed to register h3_raster_to_parquet table function".to_string());
    }

    Ok(())
}
