use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiResolutionConfig,
};
use crate::aggregator::remap::CategoryRemapper;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoricalOutputFormat {
    Wide,
    Long,
}

pub struct RasterH3CategoricalBindData {
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
    pub format: CategoricalOutputFormat,
    pub min_count: Option<f64>,
    pub min_majority_fraction: Option<f64>,
    pub compact: bool,
    pub emit_geom: bool,
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
    pub ready_batches: Mutex<VecDeque<Vec<MultiCategoricalRecord>>>,
    pub is_finished: AtomicBool,
    pub format: CategoricalOutputFormat,
    pub projected_columns: Vec<usize>,
    pub long_queue: Mutex<VecDeque<LongCategoricalRow>>,
    pub emit_geom: bool,
}

pub struct RasterH3CategoricalLocalData {
    pub thread_id: usize,
    pub hex_buf: [u8; 16],
    pub wkb_buf: [u8; 128],
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
    let bind = BindHelper::new(info);
    if bind.parameter_count() < 1 {
        bind.set_error("h3_raster_categorical_aggregate requires at least 1 argument: file_path");
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

    let mut format = CategoricalOutputFormat::Wide;
    if let Some(s) = bind.get_named_string("format") {
        if s.trim().eq_ignore_ascii_case("long") {
            format = CategoricalOutputFormat::Long;
        }
    }

    let bbox = bind.parse_bbox();
    let min_count = bind.get_named_double("min_count");
    let min_majority_fraction = bind.get_named_double("min_majority_fraction");
    let compact = bind.get_named_bool("compact").unwrap_or(false);
    let overlap_rule = bind.parse_overlap_rule();
    let emit_geom = bind.get_named_bool("geom").unwrap_or_else(crate::ffi::is_geometry_available);

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

    let resolved_paths = match crate::raster::mosaic::resolve_raster_sources(&file_path) {
        Ok(paths) => paths,
        Err(e) => {
            bind.set_error(&format!("Failed to resolve raster source(s): {}", e));
            return;
        }
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

            if emit_geom {
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

            if emit_geom {
                let mut type_geom = crate::ffi::create_geometry_logical_type();
                bind.add_custom_result_column("geom", type_geom);
                duckdb_destroy_logical_type(&mut type_geom);
            }
        }
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

    let bind_data = Box::new(RasterH3CategoricalBindData {
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
        format,
        min_count,
        min_majority_fraction,
        compact,
        emit_geom,
        remapper,
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
    config.min_count = bind_data.min_count;
    config.min_majority_fraction = bind_data.min_majority_fraction;
    config.compact = bind_data.compact;
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

    let global_data = Box::new(RasterH3CategoricalGlobalData {
        streamer: Mutex::new(streamer),
        ready_batches: Mutex::new(VecDeque::new()),
        is_finished: AtomicBool::new(false),
        format: bind_data.format,
        projected_columns,
        long_queue: Mutex::new(VecDeque::new()),
        emit_geom: bind_data.emit_geom,
    });

    duckdb_init_set_init_data(
        info,
        Box::into_raw(global_data) as *mut c_void,
        Some(delete_global_data),
    );
}

/// Thread-local init callback for categorical aggregation
pub unsafe extern "C" fn raster_h3_categorical_init_local(info: duckdb_init_info) {
    let local_data = Box::new(RasterH3CategoricalLocalData {
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
            let batch_opt = {
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

            // Fast path: if 0 columns are projected (e.g. SELECT count(*))
            if proj_cols.is_empty() {
                duckdb_data_chunk_set_size(output, batch.len() as idx_t);
                return;
            }

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
            let mut vec_h3: Option<*mut u64> = None;
            let mut vec_hex: Option<duckdb_vector> = None;
            let mut vec_maj_cls: Option<*mut i64> = None;
            let mut vec_maj_frac: Option<*mut f64> = None;
            let mut vec_maj_cnt: Option<*mut f64> = None;
            let mut vec_uniq: Option<*mut i64> = None;
            let mut vec_tot: Option<*mut f64> = None;
            let mut vec_hist: Option<duckdb_vector> = None;
            let mut vec_res: Option<*mut u8> = None;
            let mut vec_shannon_entropy: Option<*mut f64> = None;
            let mut vec_entropy: Option<*mut f64> = None;
            let mut vec_distinct: Option<*mut i64> = None;
            let mut vec_wkb: Option<duckdb_vector> = None;
            let mut vec_geom: Option<duckdb_vector> = None;

            for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
                let v = duckdb_data_chunk_get_vector(output, out_idx as idx_t);
                match orig_col {
                    0 => vec_h3 = Some(duckdb_vector_get_data(v) as *mut u64),
                    1 => vec_hex = Some(v),
                    2 => vec_maj_cls = Some(duckdb_vector_get_data(v) as *mut i64),
                    3 => vec_maj_frac = Some(duckdb_vector_get_data(v) as *mut f64),
                    4 => vec_maj_cnt = Some(duckdb_vector_get_data(v) as *mut f64),
                    5 => vec_uniq = Some(duckdb_vector_get_data(v) as *mut i64),
                    6 => vec_tot = Some(duckdb_vector_get_data(v) as *mut f64),
                    7 => vec_hist = Some(v),
                    8 => vec_res = Some(duckdb_vector_get_data(v) as *mut u8),
                    9 => vec_shannon_entropy = Some(duckdb_vector_get_data(v) as *mut f64),
                    10 => vec_entropy = Some(duckdb_vector_get_data(v) as *mut f64),
                    11 => vec_distinct = Some(duckdb_vector_get_data(v) as *mut i64),
                    12 => vec_wkb = Some(v),
                    13 if global_data.emit_geom => vec_geom = Some(v),
                    _ => {}
                }
            }

            let need_majority = vec_maj_cls.is_some() || vec_maj_frac.is_some() || vec_maj_cnt.is_some();
            let need_entropy = vec_shannon_entropy.is_some() || vec_entropy.is_some();
            let need_distinct = vec_uniq.is_some() || vec_distinct.is_some();

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
            if let Some(p) = vec_res {
                for (i, rec) in batch.iter().enumerate() {
                    *p.add(i) = rec.resolution;
                }
            }
            if let Some(p) = vec_tot {
                for (i, rec) in batch.iter().enumerate() {
                    *p.add(i) = rec.accumulator.total_count;
                }
            }
            if need_majority {
                for (i, rec) in batch.iter().enumerate() {
                    let (maj_cls, maj_cnt, maj_frac) = rec.accumulator.majority();
                    if let Some(p) = vec_maj_cls {
                        *p.add(i) = maj_cls;
                    }
                    if let Some(p) = vec_maj_frac {
                        *p.add(i) = maj_frac;
                    }
                    if let Some(p) = vec_maj_cnt {
                        *p.add(i) = maj_cnt;
                    }
                }
            }
            if need_distinct {
                for (i, rec) in batch.iter().enumerate() {
                    let distinct = rec.accumulator.unique_classes() as i64;
                    if let Some(p) = vec_uniq {
                        *p.add(i) = distinct;
                    }
                    if let Some(p) = vec_distinct {
                        *p.add(i) = distinct;
                    }
                }
            }
            if need_entropy {
                for (i, rec) in batch.iter().enumerate() {
                    let ent = rec.accumulator.shannon_entropy();
                    if let Some(p) = vec_shannon_entropy {
                        *p.add(i) = ent;
                    }
                    if let Some(p) = vec_entropy {
                        *p.add(i) = ent;
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
            if let Some(v) = vec_hist {
                let mut hist_buf = String::with_capacity(256);
                for (i, rec) in batch.iter().enumerate() {
                    rec.accumulator.histogram_json_into(&mut hist_buf);
                    duckdb_vector_assign_string_element_len(
                        v,
                        i as u64,
                        hist_buf.as_ptr() as *const c_char,
                        hist_buf.len() as idx_t,
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

            if proj_cols.is_empty() {
                duckdb_data_chunk_set_size(output, num_taken as idx_t);
                return;
            }

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
            let mut vec_h3: Option<*mut u64> = None;
            let mut vec_hex: Option<duckdb_vector> = None;
            let mut vec_cat: Option<*mut i64> = None;
            let mut vec_cnt: Option<*mut f64> = None;
            let mut vec_frac: Option<*mut f64> = None;
            let mut vec_tot: Option<*mut f64> = None;
            let mut vec_res: Option<*mut u8> = None;
            let mut vec_shannon_entropy: Option<*mut f64> = None;
            let mut vec_entropy: Option<*mut f64> = None;
            let mut vec_distinct: Option<*mut i64> = None;
            let mut vec_uniq: Option<*mut i64> = None;
            let mut vec_wkb: Option<duckdb_vector> = None;
            let mut vec_geom: Option<duckdb_vector> = None;

            for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
                let v = duckdb_data_chunk_get_vector(output, out_idx as idx_t);
                match orig_col {
                    0 => vec_h3 = Some(duckdb_vector_get_data(v) as *mut u64),
                    1 => vec_hex = Some(v),
                    2 => vec_cat = Some(duckdb_vector_get_data(v) as *mut i64),
                    3 => vec_cnt = Some(duckdb_vector_get_data(v) as *mut f64),
                    4 => vec_frac = Some(duckdb_vector_get_data(v) as *mut f64),
                    5 => vec_tot = Some(duckdb_vector_get_data(v) as *mut f64),
                    6 => vec_res = Some(duckdb_vector_get_data(v) as *mut u8),
                    7 => vec_shannon_entropy = Some(duckdb_vector_get_data(v) as *mut f64),
                    8 => vec_entropy = Some(duckdb_vector_get_data(v) as *mut f64),
                    9 => vec_distinct = Some(duckdb_vector_get_data(v) as *mut i64),
                    10 => vec_uniq = Some(duckdb_vector_get_data(v) as *mut i64),
                    11 => vec_wkb = Some(v),
                    12 if global_data.emit_geom => vec_geom = Some(v),
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

            for (i, row) in rows.into_iter().enumerate() {
                let row_idx = i as u64;

                if let Some(p) = vec_h3 {
                    *p.add(i) = row.cell_u64;
                }
                if let Some(v) = vec_hex {
                    let hex_slice = fast_hex_u64(row.cell_u64, hex_buf);
                    duckdb_vector_assign_string_element_len(
                        v,
                        row_idx,
                        hex_slice.as_ptr() as *const c_char,
                        hex_slice.len() as idx_t,
                    );
                }
                if let Some(p) = vec_cat {
                    *p.add(i) = row.category;
                }
                if let Some(p) = vec_cnt {
                    *p.add(i) = row.count;
                }
                if let Some(p) = vec_frac {
                    *p.add(i) = row.fraction;
                }
                if let Some(p) = vec_tot {
                    *p.add(i) = row.total_count;
                }
                if let Some(p) = vec_res {
                    *p.add(i) = row.resolution;
                }
                if let Some(p) = vec_shannon_entropy {
                    *p.add(i) = row.entropy;
                }
                if let Some(p) = vec_entropy {
                    *p.add(i) = row.entropy;
                }
                if let Some(p) = vec_distinct {
                    *p.add(i) = row.distinct_classes;
                }
                if let Some(p) = vec_uniq {
                    *p.add(i) = row.distinct_classes;
                }
                if vec_wkb.is_some() || vec_geom.is_some() {
                    let wkb_len_opt = h3_index_to_wkb(row.cell_u64, wkb_buf);
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
