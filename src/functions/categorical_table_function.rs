use std::collections::VecDeque;
use std::ffi::{c_void, CString};
use std::sync::{Arc, Mutex};

use crate::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiResolutionConfig,
};
use crate::aggregator::remap::CategoryRemapper;
use crate::ffi::duckdb_c::*;
use crate::ffi::to_c_string;
use crate::functions::bind_utils::{
    add_named_parameter, add_positional_parameter, delete_boxed, estimate_raster_cardinality,
    extract_projected_columns, init_table_function_local, open_mosaic_or_set_error,
    register_common_raster_named_parameters, set_table_function_init_data, BindHelper, ChunkWriter,
    ConcurrentRecordQueue, TableFunctionLocalData,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoricalOutputFormat {
    Wide,
    Long,
}

pub struct RasterH3CategoricalBindData {
    pub common: crate::functions::bind_utils::CommonRasterParams,
    pub format: CategoricalOutputFormat,
    pub min_count: Option<f64>,
    pub min_majority_fraction: Option<f64>,
    pub remapper: Option<Arc<CategoryRemapper>>,
}

pub struct LongCategoricalRow {
    pub cell_u64: u64,
    pub resolution: u8,
    pub category: i64,
    pub count: f64,
    pub fraction: f64,
    pub total_count: f64,
    pub entropy: f64,
    pub distinct_classes: i64,
}

pub struct RasterH3CategoricalGlobalData {
    pub streamer: Mutex<MultiCategoricalHorizonStreamer>,
    pub record_queue: ConcurrentRecordQueue<MultiCategoricalRecord>,
    pub format: CategoricalOutputFormat,
    pub projected_columns: Vec<usize>,
    pub long_queue: Mutex<VecDeque<LongCategoricalRow>>,
    pub emit_geom: bool,
}

pub type RasterH3CategoricalLocalData = TableFunctionLocalData;

/// Bind callback for categorical aggregation table function
pub unsafe extern "C" fn raster_h3_categorical_bind(info: duckdb_bind_info) {
    let bind = BindHelper::new(info);
    let common = match bind.parse_common_raster_params("h3_raster_categorical_aggregate") {
        Some(p) => p,
        None => return,
    };

    let mut format = CategoricalOutputFormat::Wide;
    if let Some(s) = bind.get_named_string("format") {
        if s.trim().eq_ignore_ascii_case("long") {
            format = CategoricalOutputFormat::Long;
        }
    }

    let min_count = bind.get_named_double("min_count");
    let min_majority_fraction = bind.get_named_double("min_majority_fraction");

    let remapper = if let Some(s) = bind.get_named_string("remap") {
        match CategoryRemapper::parse(&s) {
            Ok(rem) => Some(rem.into_arc()),
            Err(e) => {
                bind.set_error(&format!("Failed to parse remap parameter: {}", e));
                return;
            }
        }
    } else {
        None
    };

    // Define result columns based on format
    bind.add_result_column("h3_index", DuckDBType::UBigInt);
    bind.add_result_column("h3_hex", DuckDBType::Varchar);

    match format {
        CategoricalOutputFormat::Wide => {
            bind.add_result_column("majority_class", DuckDBType::BigInt);
            bind.add_result_column("majority_fraction", DuckDBType::Double);
            bind.add_result_column("majority_count", DuckDBType::Double);
            bind.add_result_column("unique_classes", DuckDBType::BigInt);
            bind.add_result_column("total_count", DuckDBType::Double);
            bind.add_result_column("histogram", DuckDBType::Varchar);
            bind.add_result_column("resolution", DuckDBType::UTinyInt);
            bind.add_result_column("shannon_entropy", DuckDBType::Double);
            bind.add_result_column("entropy", DuckDBType::Double);
            bind.add_result_column("distinct_classes", DuckDBType::BigInt);
            bind.add_result_column("wkb", DuckDBType::Blob);

            if common.emit_geom {
                let mut type_geom = crate::ffi::create_geometry_logical_type();
                bind.add_custom_result_column("geom", type_geom);
                duckdb_destroy_logical_type(&mut type_geom);
            }
        }
        CategoricalOutputFormat::Long => {
            bind.add_result_column("category", DuckDBType::BigInt);
            bind.add_result_column("count", DuckDBType::Double);
            bind.add_result_column("fraction", DuckDBType::Double);
            bind.add_result_column("total_count", DuckDBType::Double);
            bind.add_result_column("resolution", DuckDBType::UTinyInt);
            bind.add_result_column("shannon_entropy", DuckDBType::Double);
            bind.add_result_column("entropy", DuckDBType::Double);
            bind.add_result_column("distinct_classes", DuckDBType::BigInt);
            bind.add_result_column("unique_classes", DuckDBType::BigInt);
            bind.add_result_column("wkb", DuckDBType::Blob);

            if common.emit_geom {
                let mut type_geom = crate::ffi::create_geometry_logical_type();
                bind.add_custom_result_column("geom", type_geom);
                duckdb_destroy_logical_type(&mut type_geom);
            }
        }
    }

    let estimated_cardinality = estimate_raster_cardinality(&common.resolved_paths, &common.resolutions);
    duckdb_bind_set_cardinality(info, estimated_cardinality as idx_t, false);

    let bind_data = Box::new(RasterH3CategoricalBindData {
        common,
        format,
        min_count,
        min_majority_fraction,
        remapper,
    });

    duckdb_bind_set_bind_data(
        info,
        Box::into_raw(bind_data) as *mut c_void,
        Some(delete_boxed::<RasterH3CategoricalBindData>),
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

    let mosaic = match open_mosaic_or_set_error(
        info,
        &bind_data.common.resolved_paths,
        bind_data.common.bbox,
        bind_data.common.source_crs.as_deref(),
        bind_data.common.overlap_rule,
    ) {
        Some(m) => m,
        None => return,
    };

    let projected_columns = extract_projected_columns(info);

    let mut config = MultiResolutionConfig::new(bind_data.common.resolutions.clone());
    config.overlap_rule = bind_data.common.overlap_rule;
    config.custom_crs = bind_data.common.source_crs.clone();
    config.custom_nodata = bind_data.common.nodata;
    config.bbox = bind_data.common.bbox;
    config.sampling = bind_data.common.sampling.clone();
    config.band = bind_data.common.band as usize;
    config.min_count = bind_data.min_count;
    config.min_majority_fraction = bind_data.min_majority_fraction;
    config.compact = bind_data.common.compact;
    config.remapper = bind_data.remapper.clone();

    let streamer = match MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config) {
        Ok(s) => s,
        Err(e) => {
            let err_msg = CString::new(format!("Failed to initialize categorical streamer: {}", e))
                .unwrap_or_else(|_| CString::new("Failed to init categorical streamer").unwrap());
            duckdb_init_set_error(info, err_msg.as_ptr());
            return;
        }
    };

    let global_data = RasterH3CategoricalGlobalData {
        streamer: Mutex::new(streamer),
        record_queue: ConcurrentRecordQueue::new(),
        format: bind_data.format,
        projected_columns,
        long_queue: Mutex::new(VecDeque::new()),
        emit_geom: bind_data.common.emit_geom,
    };

    set_table_function_init_data(info, global_data);
}

/// Thread-local init callback for categorical aggregation
pub unsafe extern "C" fn raster_h3_categorical_init_local(info: duckdb_init_info) {
    init_table_function_local(info);
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
    let proj_cols = &global_data.projected_columns;
    let local_data_ptr = duckdb_function_get_local_init_data(info) as *mut RasterH3CategoricalLocalData;

    match global_data.format {
        CategoricalOutputFormat::Wide => {
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

            let mut out_maj_cls: Option<usize> = None;
            let mut out_maj_frac: Option<usize> = None;
            let mut out_maj_cnt: Option<usize> = None;
            let mut out_uniq: Option<usize> = None;
            let mut out_distinct: Option<usize> = None;
            let mut out_shannon: Option<usize> = None;
            let mut out_entropy: Option<usize> = None;
            let mut out_idx_wkb: Option<usize> = None;
            let mut out_idx_geom: Option<usize> = None;

            // Schema column mapping (Wide):
            // 0: h3_index UBIGINT
            // 1: h3_hex VARCHAR
            // 2: majority_class BIGINT
            // 3: majority_fraction DOUBLE
            // 4: majority_count DOUBLE
            // 5: unique_classes BIGINT
            // 6: total_count DOUBLE
            // 7: histogram VARCHAR
            // 8: resolution UTINYINT
            // 9: shannon_entropy DOUBLE
            // 10: entropy DOUBLE
            // 11: distinct_classes BIGINT
            // 12: wkb BLOB
            // 13: geom GEOMETRY (if emit_geom)
            for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
                match orig_col {
                    0 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.h3_index)),
                    1 => writer.write_hex_column(out_idx, batch.iter().map(|r| &r.h3_index), hex_buf),
                    2 => out_maj_cls = Some(out_idx),
                    3 => out_maj_frac = Some(out_idx),
                    4 => out_maj_cnt = Some(out_idx),
                    5 => out_uniq = Some(out_idx),
                    6 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.accumulator.total_count)),
                    7 => {
                        let mut hist_buf = String::with_capacity(256);
                        for (i, rec) in batch.iter().enumerate() {
                            rec.accumulator.histogram_json_into(&mut hist_buf);
                            writer.set_string_bytes(out_idx, i, hist_buf.as_bytes());
                        }
                    }
                    8 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.resolution)),
                    9 => out_shannon = Some(out_idx),
                    10 => out_entropy = Some(out_idx),
                    11 => out_distinct = Some(out_idx),
                    12 => out_idx_wkb = Some(out_idx),
                    13 if global_data.emit_geom => out_idx_geom = Some(out_idx),
                    _ => {}
                }
            }

            if out_maj_cls.is_some() || out_maj_frac.is_some() || out_maj_cnt.is_some() {
                let mut slice_cls = out_maj_cls.map(|col| writer.get_data_slice_mut::<i64>(col, batch_len));
                let mut slice_frac = out_maj_frac.map(|col| writer.get_data_slice_mut::<f64>(col, batch_len));
                let mut slice_cnt = out_maj_cnt.map(|col| writer.get_data_slice_mut::<f64>(col, batch_len));

                for (i, rec) in batch.iter().enumerate() {
                    let (maj_cls, maj_cnt, maj_frac) = rec.accumulator.majority();
                    if let Some(ref mut s) = slice_cls {
                        s[i] = maj_cls;
                    }
                    if let Some(ref mut s) = slice_frac {
                        s[i] = maj_frac;
                    }
                    if let Some(ref mut s) = slice_cnt {
                        s[i] = maj_cnt;
                    }
                }
            }

            if out_uniq.is_some() || out_distinct.is_some() {
                let mut slice_uniq = out_uniq.map(|col| writer.get_data_slice_mut::<i64>(col, batch_len));
                let mut slice_dist = out_distinct.map(|col| writer.get_data_slice_mut::<i64>(col, batch_len));

                for (i, rec) in batch.iter().enumerate() {
                    let distinct = rec.accumulator.unique_classes() as i64;
                    if let Some(ref mut s) = slice_uniq {
                        s[i] = distinct;
                    }
                    if let Some(ref mut s) = slice_dist {
                        s[i] = distinct;
                    }
                }
            }

            if out_shannon.is_some() || out_entropy.is_some() {
                let mut slice_shannon = out_shannon.map(|col| writer.get_data_slice_mut::<f64>(col, batch_len));
                let mut slice_ent = out_entropy.map(|col| writer.get_data_slice_mut::<f64>(col, batch_len));

                for (i, rec) in batch.iter().enumerate() {
                    let ent = rec.accumulator.shannon_entropy();
                    if let Some(ref mut s) = slice_shannon {
                        s[i] = ent;
                    }
                    if let Some(ref mut s) = slice_ent {
                        s[i] = ent;
                    }
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
        CategoricalOutputFormat::Long => {
            let rows = {
                let mut long_queue = match global_data.long_queue.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };

                while long_queue.len() < 2048 * 4 {
                    let mut streamer = match global_data.streamer.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };

                    let mut added_any = false;
                    streamer.drain_completed_into(512, |_i, rec| {
                        added_any = true;
                        let mut entries: Vec<(i64, f64)> = Vec::with_capacity(rec.accumulator.unique_classes());
                        rec.accumulator.for_each_class(|cat, cnt| entries.push((cat, cnt)));
                        entries.sort_unstable_by_key(|&(cat, _)| cat);

                        let entropy = rec.accumulator.shannon_entropy();
                        let distinct_classes = rec.accumulator.unique_classes() as i64;

                        for (cat, cnt) in entries {
                            let fraction = if rec.accumulator.total_count > 0.0 {
                                cnt / rec.accumulator.total_count
                            } else {
                                0.0
                            };
                            long_queue.push_back(LongCategoricalRow {
                                cell_u64: rec.h3_index,
                                resolution: rec.resolution,
                                category: cat,
                                count: cnt,
                                fraction,
                                total_count: rec.accumulator.total_count,
                                entropy,
                                distinct_classes,
                            });
                        }
                    });

                    if !added_any {
                        break;
                    }
                }

                let num_taken = 2048.min(long_queue.len());
                let mut chunk_rows = Vec::with_capacity(num_taken);
                for _ in 0..num_taken {
                    if let Some(r) = long_queue.pop_front() {
                        chunk_rows.push(r);
                    }
                }
                chunk_rows
            };

            let num_taken = rows.len();
            if num_taken == 0 {
                duckdb_data_chunk_set_size(output, 0);
                return;
            }

            let writer = ChunkWriter::new(output);

            if proj_cols.is_empty() {
                writer.set_size(num_taken);
                return;
            }

            let mut fallback_hex_buf = [0u8; 16];
            let mut fallback_wkb_buf = [0u8; 128];
            let (hex_buf, wkb_buf) = TableFunctionLocalData::get_scratch_buffers(
                local_data_ptr,
                &mut fallback_hex_buf,
                &mut fallback_wkb_buf,
            );

            let mut out_idx_wkb: Option<usize> = None;
            let mut out_idx_geom: Option<usize> = None;

            // Schema column mapping (Long):
            // 0: h3_index UBIGINT
            // 1: h3_hex VARCHAR
            // 2: category BIGINT
            // 3: count DOUBLE
            // 4: fraction DOUBLE
            // 5: total_count DOUBLE
            // 6: resolution UTINYINT
            // 7: shannon_entropy DOUBLE
            // 8: entropy DOUBLE
            // 9: distinct_classes BIGINT
            // 10: unique_classes BIGINT
            // 11: wkb BLOB
            // 12: geom GEOMETRY (if emit_geom)
            for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
                match orig_col {
                    0 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.cell_u64)),
                    1 => writer.write_hex_column(out_idx, rows.iter().map(|r| &r.cell_u64), hex_buf),
                    2 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.category)),
                    3 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.count)),
                    4 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.fraction)),
                    5 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.total_count)),
                    6 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.resolution)),
                    7 | 8 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.entropy)),
                    9 | 10 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.distinct_classes)),
                    11 => out_idx_wkb = Some(out_idx),
                    12 if global_data.emit_geom => out_idx_geom = Some(out_idx),
                    _ => {}
                }
            }

            writer.write_wkb_and_geom_columns(
                out_idx_wkb,
                out_idx_geom,
                rows.iter().map(|r| &r.cell_u64),
                wkb_buf,
            );

            writer.set_size(num_taken);
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
        add_positional_parameter(tf, DuckDBType::Varchar);

        // Standard Common Raster Named Parameters
        register_common_raster_named_parameters(tf);

        // Categorical-specific named parameters
        add_named_parameter(tf, "chunk_size", DuckDBType::BigInt);
        add_named_parameter(tf, "format", DuckDBType::Varchar);
        add_named_parameter(tf, "min_count", DuckDBType::Double);
        add_named_parameter(tf, "min_majority_fraction", DuckDBType::Double);
        add_named_parameter(tf, "geom", DuckDBType::Boolean);
        add_named_parameter(tf, "remap", DuckDBType::Varchar);

        duckdb_table_function_set_bind(tf, raster_h3_categorical_bind);
        duckdb_table_function_set_init(tf, raster_h3_categorical_init);
        duckdb_table_function_set_local_init(tf, raster_h3_categorical_init_local);
        duckdb_table_function_set_function(tf, raster_h3_categorical_scan);
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
