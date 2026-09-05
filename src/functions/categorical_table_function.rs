use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiResolutionConfig,
};
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
    pub resolutions: Vec<u8>,
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
}

pub struct RasterH3CategoricalLocalData {
    pub thread_id: usize,
    pub hex_buf: [u8; 16],
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

    let mut parsed_resolutions: Option<Vec<u8>> = None;

    // 1. Named parameter: resolutions (VARCHAR, e.g. '7,8' or '7, 8, 9')
    let name_ress = to_c_string("resolutions");
    let named_ress_val = duckdb_bind_get_named_parameter(info, name_ress.as_ptr());
    if !named_ress_val.is_null() {
        let ress_str_ptr = duckdb_get_varchar(named_ress_val);
        if let Some(s) = from_duckdb_string(ress_str_ptr) {
            let mut list: Vec<u8> = s
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|item| !item.is_empty())
                .filter_map(|item| item.parse::<u8>().ok())
                .filter(|&r| r <= 15)
                .collect();
            list.sort_unstable();
            list.dedup();
            if !list.is_empty() {
                parsed_resolutions = Some(list);
            }
        }
    }

    // 2. Named parameters: min_resolution and max_resolution (BIGINT)
    if parsed_resolutions.is_none() {
        let name_min_res = to_c_string("min_resolution");
        let name_max_res = to_c_string("max_resolution");
        let min_res_val = duckdb_bind_get_named_parameter(info, name_min_res.as_ptr());
        let max_res_val = duckdb_bind_get_named_parameter(info, name_max_res.as_ptr());
        if !min_res_val.is_null() && !max_res_val.is_null() {
            let min_r = duckdb_get_int64(min_res_val);
            let max_r = duckdb_get_int64(max_res_val);
            if (0..=15).contains(&min_r) && (0..=15).contains(&max_r) && min_r <= max_r {
                parsed_resolutions = Some(((min_r as u8)..=(max_r as u8)).collect());
            }
        }
    }

    // 3. Named parameter: resolution (BIGINT)
    if parsed_resolutions.is_none() {
        let name_res = to_c_string("resolution");
        let named_res_val = duckdb_bind_get_named_parameter(info, name_res.as_ptr());
        if !named_res_val.is_null() {
            let res_int = duckdb_get_int64(named_res_val);
            if (0..=15).contains(&res_int) {
                parsed_resolutions = Some(vec![res_int as u8]);
            }
        }
    }

    // 4. Positional param 1 (optional): resolution (BIGINT)
    if parsed_resolutions.is_none() && param_count >= 2 {
        let res_val = duckdb_bind_get_parameter(info, 1);
        let res_int = duckdb_get_int64(res_val);
        if (0..=15).contains(&res_int) {
            parsed_resolutions = Some(vec![res_int as u8]);
        }
    }

    let resolutions = parsed_resolutions.unwrap_or_else(|| vec![8]);

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

    let mut bbox = if !named_min_lon_val.is_null()
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

    // Optional spatial filter: h3_cell (BIGINT) or h3_hex (VARCHAR)
    if bbox.is_none() {
        let name_cell = to_c_string("h3_cell");
        let named_cell_val = duckdb_bind_get_named_parameter(info, name_cell.as_ptr());
        if !named_cell_val.is_null() {
            let cell_u64 = duckdb_get_uint64(named_cell_val);
            if let Ok(cell) = h3o::CellIndex::try_from(cell_u64) {
                let ll: h3o::LatLng = cell.into();
                let r = crate::pmtiles::tiler::max_hex_radius_deg(cell.resolution().into());
                bbox = Some([ll.lng() - r, ll.lat() - r, ll.lng() + r, ll.lat() + r]);
            }
        } else {
            let name_h3_hex = to_c_string("h3_hex");
            let named_h3_hex_val = duckdb_bind_get_named_parameter(info, name_h3_hex.as_ptr());
            if !named_h3_hex_val.is_null() {
                let hex_str_ptr = duckdb_get_varchar(named_h3_hex_val);
                if let Some(s) = from_duckdb_string(hex_str_ptr) {
                    if let Ok(cell) = s.trim().parse::<h3o::CellIndex>() {
                        let ll: h3o::LatLng = cell.into();
                        let r = crate::pmtiles::tiler::max_hex_radius_deg(cell.resolution().into());
                        bbox = Some([ll.lng() - r, ll.lat() - r, ll.lng() + r, ll.lat() + r]);
                    }
                }
            }
        }
    }

    // Define result columns based on format
    let type_ubigint = duckdb_create_logical_type(DuckDBType::UBigInt);
    let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
    let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
    let type_double = duckdb_create_logical_type(DuckDBType::Double);
    let type_utinyint = duckdb_create_logical_type(DuckDBType::UTinyInt);

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

            // 8: resolution UTINYINT
            let col_res = to_c_string("resolution");
            duckdb_bind_add_result_column(info, col_res.as_ptr(), type_utinyint);

            // 9: shannon_entropy DOUBLE
            let col_shannon = to_c_string("shannon_entropy");
            duckdb_bind_add_result_column(info, col_shannon.as_ptr(), type_double);

            // 10: entropy DOUBLE (alias for shannon_entropy)
            let col_entropy = to_c_string("entropy");
            duckdb_bind_add_result_column(info, col_entropy.as_ptr(), type_double);

            // 11: distinct_classes BIGINT (alias for unique_classes)
            let col_distinct = to_c_string("distinct_classes");
            duckdb_bind_add_result_column(info, col_distinct.as_ptr(), type_bigint);
        }
        CategoricalOutputFormat::Long => {
            // Option C: Long form (h3_index, h3_hex, category, count, fraction, total_count, resolution, shannon_entropy, entropy, distinct_classes, unique_classes)
            let col_cat = to_c_string("category");
            duckdb_bind_add_result_column(info, col_cat.as_ptr(), type_bigint);

            let col_cnt = to_c_string("count");
            duckdb_bind_add_result_column(info, col_cnt.as_ptr(), type_double);

            let col_frac = to_c_string("fraction");
            duckdb_bind_add_result_column(info, col_frac.as_ptr(), type_double);

            let col_tot = to_c_string("total_count");
            duckdb_bind_add_result_column(info, col_tot.as_ptr(), type_double);

            // 6: resolution UTINYINT
            let col_res = to_c_string("resolution");
            duckdb_bind_add_result_column(info, col_res.as_ptr(), type_utinyint);

            // 7: shannon_entropy DOUBLE
            let col_shannon = to_c_string("shannon_entropy");
            duckdb_bind_add_result_column(info, col_shannon.as_ptr(), type_double);

            // 8: entropy DOUBLE (alias for shannon_entropy)
            let col_entropy = to_c_string("entropy");
            duckdb_bind_add_result_column(info, col_entropy.as_ptr(), type_double);

            // 9: distinct_classes BIGINT
            let col_distinct = to_c_string("distinct_classes");
            duckdb_bind_add_result_column(info, col_distinct.as_ptr(), type_bigint);

            // 10: unique_classes BIGINT (alias for distinct_classes)
            let col_uniq = to_c_string("unique_classes");
            duckdb_bind_add_result_column(info, col_uniq.as_ptr(), type_bigint);
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
    let mut type_utinyint_mut = type_utinyint;
    duckdb_destroy_logical_type(&mut type_utinyint_mut);

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

        let mut total_hex_est = 0u64;
        for &res in &resolutions {
            let hex_area = H3_AREA_M2.get(res as usize).copied().unwrap_or(7.373e5);
            let hex_count = (area_m2 / hex_area).ceil() as u64;
            total_hex_est = total_hex_est.saturating_add(hex_count.min(total_pixels).max(1));
        }
        total_hex_est.max(1)
    } else {
        10_000 * resolutions.len() as u64
    };
    duckdb_bind_set_cardinality(info, estimated_cardinality as idx_t, false);

    let bind_data = Box::new(RasterH3CategoricalBindData {
        file_path,
        resolutions,
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

    let col_count = duckdb_init_get_column_count(info);
    let mut projected_columns = Vec::with_capacity(col_count as usize);
    for i in 0..col_count {
        projected_columns.push(duckdb_init_get_column_index(info, i) as usize);
    }

    let mut config = MultiResolutionConfig::new(bind_data.resolutions.clone());
    config.custom_crs = bind_data.source_crs.clone();
    config.custom_nodata = bind_data.nodata;
    config.bbox = bind_data.bbox;
    config.sampling = bind_data.sampling.clone();
    config.band = bind_data.band as usize;

    let streamer = match MultiCategoricalHorizonStreamer::new(reader, &config) {
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
                    _ => {}
                }
            }

            let need_majority = vec_maj_cls.is_some() || vec_maj_frac.is_some() || vec_maj_cnt.is_some();
            let need_entropy = vec_shannon_entropy.is_some() || vec_entropy.is_some();
            let need_distinct = vec_uniq.is_some() || vec_distinct.is_some();

            let mut fallback_hex_buf = [0u8; 16];
            let hex_buf = if !local_data_ptr.is_null() {
                &mut (*local_data_ptr).hex_buf
            } else {
                &mut fallback_hex_buf
            };

            let batch_len = batch.len();
            for (i, rec) in batch.into_iter().enumerate() {
                let row_idx = i as u64;

                if let Some(p) = vec_h3 {
                    *p.add(i) = rec.h3_index;
                }
                if let Some(v) = vec_hex {
                    let hex_slice = fast_hex_u64(rec.h3_index, hex_buf);
                    duckdb_vector_assign_string_element_len(
                        v,
                        row_idx,
                        hex_slice.as_ptr() as *const c_char,
                        hex_slice.len() as idx_t,
                    );
                }
                if need_majority {
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
                if need_distinct {
                    let distinct = rec.accumulator.unique_classes() as i64;
                    if let Some(p) = vec_uniq {
                        *p.add(i) = distinct;
                    }
                    if let Some(p) = vec_distinct {
                        *p.add(i) = distinct;
                    }
                }
                if let Some(p) = vec_tot {
                    *p.add(i) = rec.accumulator.total_count;
                }
                if let Some(v) = vec_hist {
                    let hist_json = rec.accumulator.histogram_json();
                    duckdb_vector_assign_string_element_len(
                        v,
                        row_idx,
                        hist_json.as_ptr() as *const c_char,
                        hist_json.len() as idx_t,
                    );
                }
                if let Some(p) = vec_res {
                    *p.add(i) = rec.resolution;
                }
                if need_entropy {
                    let ent = rec.accumulator.shannon_entropy();
                    if let Some(p) = vec_shannon_entropy {
                        *p.add(i) = ent;
                    }
                    if let Some(p) = vec_entropy {
                        *p.add(i) = ent;
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
                    _ => {}
                }
            }

            let mut fallback_hex_buf = [0u8; 16];
            let hex_buf = if !local_data_ptr.is_null() {
                &mut (*local_data_ptr).hex_buf
            } else {
                &mut fallback_hex_buf
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

        let name_ress = to_c_string("resolutions");
        duckdb_table_function_add_named_parameter(tf, name_ress.as_ptr(), type_varchar);

        let name_min_res = to_c_string("min_resolution");
        duckdb_table_function_add_named_parameter(tf, name_min_res.as_ptr(), type_bigint);

        let name_max_res = to_c_string("max_resolution");
        duckdb_table_function_add_named_parameter(tf, name_max_res.as_ptr(), type_bigint);

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

        // Spatial H3 cell filter parameters
        let name_cell = to_c_string("h3_cell");
        duckdb_table_function_add_named_parameter(tf, name_cell.as_ptr(), type_bigint);
        let name_hex = to_c_string("h3_hex");
        duckdb_table_function_add_named_parameter(tf, name_hex.as_ptr(), type_varchar);

        duckdb_table_function_set_bind(tf, raster_h3_categorical_bind);
        duckdb_table_function_set_init(tf, raster_h3_categorical_init);
        duckdb_table_function_set_local_init(tf, raster_h3_categorical_init_local);
        duckdb_table_function_set_function(tf, raster_h3_categorical_scan);
        duckdb_table_function_supports_projection_pushdown(tf, true);

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
