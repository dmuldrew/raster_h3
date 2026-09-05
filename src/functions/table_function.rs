use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::aggregator::multi_horizon::{
    MultiContinuousRecord, MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::duckdb_c::*;
use crate::ffi::{from_duckdb_string, to_c_string};
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
}

/// Global scan state holding the streaming multi-core horizon aggregator and concurrent batch queue
pub struct RasterH3GlobalData {
    pub streamer: Mutex<MultiScanHorizonStreamer>,
    pub ready_batches: Mutex<VecDeque<Vec<MultiContinuousRecord>>>,
    pub is_finished: AtomicBool,
    pub projected_columns: Vec<usize>,
    pub quantiles: Vec<QuantileTarget>,
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
    let param_count = duckdb_bind_get_parameter_count(info);
    if param_count < 1 {
        let err_msg = to_c_string("h3_raster_continuous_aggregate requires at least 1 argument: file_path");
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

    // Named parameter: formula (VARCHAR)
    let name_formula = to_c_string("formula");
    let named_formula_val = duckdb_bind_get_named_parameter(info, name_formula.as_ptr());
    let mut formula_str = None;
    if !named_formula_val.is_null() {
        let f_ptr = duckdb_get_varchar(named_formula_val);
        formula_str = from_duckdb_string(f_ptr);
    }

    // Band parameters for formulas
    let mut nir_band: usize = 4;
    let name_nir = to_c_string("nir_band");
    let named_nir_val = duckdb_bind_get_named_parameter(info, name_nir.as_ptr());
    if !named_nir_val.is_null() {
        let b = duckdb_get_int64(named_nir_val);
        if b > 0 {
            nir_band = b as usize;
        }
    }

    let mut red_band: usize = 3;
    let name_red = to_c_string("red_band");
    let named_red_val = duckdb_bind_get_named_parameter(info, name_red.as_ptr());
    if !named_red_val.is_null() {
        let b = duckdb_get_int64(named_red_val);
        if b > 0 {
            red_band = b as usize;
        }
    }

    let mut green_band: usize = 2;
    let name_green = to_c_string("green_band");
    let named_green_val = duckdb_bind_get_named_parameter(info, name_green.as_ptr());
    if !named_green_val.is_null() {
        let b = duckdb_get_int64(named_green_val);
        if b > 0 {
            green_band = b as usize;
        }
    }

    let mut blue_band: usize = 1;
    let name_blue = to_c_string("blue_band");
    let named_blue_val = duckdb_bind_get_named_parameter(info, name_blue.as_ptr());
    if !named_blue_val.is_null() {
        let b = duckdb_get_int64(named_blue_val);
        if b > 0 {
            blue_band = b as usize;
        }
    }

    let mut swir_band: usize = 6;
    let name_swir = to_c_string("swir_band");
    let named_swir_val = duckdb_bind_get_named_parameter(info, name_swir.as_ptr());
    if !named_swir_val.is_null() {
        let b = duckdb_get_int64(named_swir_val);
        if b > 0 {
            swir_band = b as usize;
        }
    }

    let spectral_formula = formula_str.as_deref().and_then(|f| {
        crate::aggregator::multi_horizon::SpectralFormula::parse(
            f, nir_band, red_band, green_band, blue_band, swir_band,
        )
    });

    // Predicates pushdown: min_count, min_mean, max_mean
    let name_min_count = to_c_string("min_count");
    let named_min_count_val = duckdb_bind_get_named_parameter(info, name_min_count.as_ptr());
    let min_count = if !named_min_count_val.is_null() {
        Some(duckdb_get_double(named_min_count_val))
    } else {
        None
    };

    let name_min_mean = to_c_string("min_mean");
    let named_min_mean_val = duckdb_bind_get_named_parameter(info, name_min_mean.as_ptr());
    let min_mean = if !named_min_mean_val.is_null() {
        Some(duckdb_get_double(named_min_mean_val))
    } else {
        None
    };

    let name_max_mean = to_c_string("max_mean");
    let named_max_mean_val = duckdb_bind_get_named_parameter(info, name_max_mean.as_ptr());
    let max_mean = if !named_max_mean_val.is_null() {
        Some(duckdb_get_double(named_max_mean_val))
    } else {
        None
    };

    // Compaction: compact
    let name_compact = to_c_string("compact");
    let named_compact_val = duckdb_bind_get_named_parameter(info, name_compact.as_ptr());
    let compact = if !named_compact_val.is_null() {
        duckdb_get_bool(named_compact_val)
    } else {
        false
    };

    // Named parameter: overlap_rule (VARCHAR, default: 'cutline')
    let mut overlap_rule = crate::raster::mosaic::OverlapRule::default();
    let name_overlap = to_c_string("overlap_rule");
    let named_overlap_val = duckdb_bind_get_named_parameter(info, name_overlap.as_ptr());
    if !named_overlap_val.is_null() {
        let overlap_ptr = duckdb_get_varchar(named_overlap_val);
        if let Some(s) = from_duckdb_string(overlap_ptr) {
            overlap_rule = crate::raster::mosaic::OverlapRule::parse(&s);
        }
    }

    // Named parameter: quantiles (VARCHAR) or percentiles (VARCHAR)
    let mut quantiles = Vec::new();
    let name_quantiles = to_c_string("quantiles");
    let name_percentiles = to_c_string("percentiles");
    let named_quantiles_val = duckdb_bind_get_named_parameter(info, name_quantiles.as_ptr());
    let named_percentiles_val = duckdb_bind_get_named_parameter(info, name_percentiles.as_ptr());
    let q_param_val = if !named_quantiles_val.is_null() {
        named_quantiles_val
    } else {
        named_percentiles_val
    };
    if !q_param_val.is_null() {
        let q_str_ptr = duckdb_get_varchar(q_param_val);
        if let Some(s) = from_duckdb_string(q_str_ptr) {
            match QuantileTarget::parse_list(&s) {
                Ok(q_targets) => quantiles = q_targets,
                Err(e) => {
                    let err_msg = CString::new(format!("Invalid quantiles parameter: {}", e))
                        .unwrap_or_else(|_| CString::new("Invalid quantiles parameter").unwrap());
                    duckdb_bind_set_error(info, err_msg.as_ptr());
                    return;
                }
            }
        }
    }

    let resolved_paths = match crate::raster::mosaic::resolve_raster_sources(&file_path) {
        Ok(paths) => paths,
        Err(e) => {
            let err_msg = CString::new(format!("Failed to resolve raster source(s): {}", e))
                .unwrap_or_else(|_| CString::new("Failed to resolve raster source(s)").unwrap());
            duckdb_bind_set_error(info, err_msg.as_ptr());
            return;
        }
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

    // 3: stddev DOUBLE (Welford's online sample standard deviation)
    let col_stddev = to_c_string("stddev");
    duckdb_bind_add_result_column(info, col_stddev.as_ptr(), type_double);

    // 4: count DOUBLE (Weighted pixel count)
    let col_cnt = to_c_string("count");
    duckdb_bind_add_result_column(info, col_cnt.as_ptr(), type_double);

    // 5: min DOUBLE
    let col_min = to_c_string("min");
    duckdb_bind_add_result_column(info, col_min.as_ptr(), type_double);

    // 6: max DOUBLE
    let col_max = to_c_string("max");
    duckdb_bind_add_result_column(info, col_max.as_ptr(), type_double);

    // 7: sum DOUBLE
    let col_sum = to_c_string("sum");
    duckdb_bind_add_result_column(info, col_sum.as_ptr(), type_double);
    let mut type_double_mut = type_double;
    duckdb_destroy_logical_type(&mut type_double_mut);

    // 8: resolution UTINYINT
    let col_res = to_c_string("resolution");
    let type_utinyint = duckdb_create_logical_type(DuckDBType::UTinyInt);
    duckdb_bind_add_result_column(info, col_res.as_ptr(), type_utinyint);
    let mut type_utinyint_mut = type_utinyint;
    duckdb_destroy_logical_type(&mut type_utinyint_mut);

    // 9: wkb BLOB (OGC 2D Polygon)
    let col_wkb = to_c_string("wkb");
    let type_blob = duckdb_create_logical_type(DuckDBType::Blob);
    duckdb_bind_add_result_column(info, col_wkb.as_ptr(), type_blob);
    let mut type_blob_mut = type_blob;
    duckdb_destroy_logical_type(&mut type_blob_mut);

    // Quantile columns (10, 11, ...): target.column_name() DOUBLE
    for target in &quantiles {
        let col_q = to_c_string(target.column_name());
        let type_double_q = duckdb_create_logical_type(DuckDBType::Double);
        duckdb_bind_add_result_column(info, col_q.as_ptr(), type_double_q);
        let mut type_double_q_mut = type_double_q;
        duckdb_destroy_logical_type(&mut type_double_q_mut);
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
    let mut vec_quantiles: Vec<(usize, *mut f64)> = Vec::new();

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
            c if c >= 10 => {
                let q_idx = c - 10;
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
        if let Some(p) = vec_mean {
            *p.add(i) = rec.accumulator.mean();
        }
        if let Some(p) = vec_stddev {
            *p.add(i) = rec.accumulator.stddev();
        }
        if let Some(p) = vec_count {
            *p.add(i) = rec.accumulator.count;
        }
        if let Some(p) = vec_min {
            *p.add(i) = rec.accumulator.min;
        }
        if let Some(p) = vec_max {
            *p.add(i) = rec.accumulator.max;
        }
        if let Some(p) = vec_sum {
            *p.add(i) = rec.accumulator.sum;
        }
        if let Some(p) = vec_res {
            *p.add(i) = rec.resolution;
        }
        if let Some(v) = vec_wkb {
            if let Some(wkb_len) = h3_index_to_wkb(rec.h3_index, wkb_buf) {
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
        for &(q_idx, ptr) in &vec_quantiles {
            let val = match &global_data.quantiles[q_idx] {
                QuantileTarget::Percentile(q, _) => rec.accumulator.quantile(*q),
                QuantileTarget::Iqr(_) => rec.accumulator.iqr(),
            };
            *ptr.add(i) = val;
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
        let type_varchar = duckdb_create_logical_type(DuckDBType::Varchar);
        duckdb_table_function_add_parameter(tf, type_varchar);

        // Named Parameters:
        // resolution (BIGINT)
        let name_res = to_c_string("resolution");
        let type_bigint = duckdb_create_logical_type(DuckDBType::BigInt);
        duckdb_table_function_add_named_parameter(tf, name_res.as_ptr(), type_bigint);

        // resolutions (VARCHAR)
        let name_ress = to_c_string("resolutions");
        duckdb_table_function_add_named_parameter(tf, name_ress.as_ptr(), type_varchar);

        // min_resolution (BIGINT)
        let name_min_res = to_c_string("min_resolution");
        duckdb_table_function_add_named_parameter(tf, name_min_res.as_ptr(), type_bigint);

        // max_resolution (BIGINT)
        let name_max_res = to_c_string("max_resolution");
        duckdb_table_function_add_named_parameter(tf, name_max_res.as_ptr(), type_bigint);

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

        // band (BIGINT)
        let name_band = to_c_string("band");
        duckdb_table_function_add_named_parameter(tf, name_band.as_ptr(), type_bigint);

        // sampling (VARCHAR)
        let name_sampling = to_c_string("sampling");
        duckdb_table_function_add_named_parameter(tf, name_sampling.as_ptr(), type_varchar);

        // Bounding box named parameters: min_lon, min_lat, max_lon, max_lat
        let name_min_lon = to_c_string("min_lon");
        let type_double_bbox = duckdb_create_logical_type(DuckDBType::Double);
        duckdb_table_function_add_named_parameter(tf, name_min_lon.as_ptr(), type_double_bbox);

        let name_min_lat = to_c_string("min_lat");
        let name_max_lon = to_c_string("max_lon");
        let name_max_lat = to_c_string("max_lat");
        duckdb_table_function_add_named_parameter(tf, name_min_lat.as_ptr(), type_double_bbox);
        duckdb_table_function_add_named_parameter(tf, name_max_lon.as_ptr(), type_double_bbox);
        duckdb_table_function_add_named_parameter(tf, name_max_lat.as_ptr(), type_double_bbox);

        // Spatial H3 cell filter parameters
        let name_cell = to_c_string("h3_cell");
        duckdb_table_function_add_named_parameter(tf, name_cell.as_ptr(), type_bigint);
        let name_hex = to_c_string("h3_hex");
        duckdb_table_function_add_named_parameter(tf, name_hex.as_ptr(), type_varchar);

        // formula (VARCHAR)
        let name_formula = to_c_string("formula");
        duckdb_table_function_add_named_parameter(tf, name_formula.as_ptr(), type_varchar);

        // Band specification parameters for formulas
        let name_nir = to_c_string("nir_band");
        duckdb_table_function_add_named_parameter(tf, name_nir.as_ptr(), type_bigint);
        let name_red = to_c_string("red_band");
        duckdb_table_function_add_named_parameter(tf, name_red.as_ptr(), type_bigint);
        let name_green = to_c_string("green_band");
        duckdb_table_function_add_named_parameter(tf, name_green.as_ptr(), type_bigint);
        let name_blue = to_c_string("blue_band");
        duckdb_table_function_add_named_parameter(tf, name_blue.as_ptr(), type_bigint);
        let name_swir = to_c_string("swir_band");
        duckdb_table_function_add_named_parameter(tf, name_swir.as_ptr(), type_bigint);

        // Predicate pushdown parameters
        let name_min_count = to_c_string("min_count");
        duckdb_table_function_add_named_parameter(tf, name_min_count.as_ptr(), type_double);
        let name_min_mean = to_c_string("min_mean");
        duckdb_table_function_add_named_parameter(tf, name_min_mean.as_ptr(), type_double);
        let name_max_mean = to_c_string("max_mean");
        duckdb_table_function_add_named_parameter(tf, name_max_mean.as_ptr(), type_double);

        // Compaction parameter
        let type_bool = duckdb_create_logical_type(DuckDBType::Boolean);
        let name_compact = to_c_string("compact");
        duckdb_table_function_add_named_parameter(tf, name_compact.as_ptr(), type_bool);

        // Overlap rule parameter
        let name_overlap = to_c_string("overlap_rule");
        duckdb_table_function_add_named_parameter(tf, name_overlap.as_ptr(), type_varchar);

        // Quantile and percentile parameters (VARCHAR)
        let name_quantiles = to_c_string("quantiles");
        duckdb_table_function_add_named_parameter(tf, name_quantiles.as_ptr(), type_varchar);
        let name_percentiles = to_c_string("percentiles");
        duckdb_table_function_add_named_parameter(tf, name_percentiles.as_ptr(), type_varchar);

        // Set callbacks including parallel init_local and projection pushdown
        duckdb_table_function_set_bind(tf, raster_h3_bind);
        duckdb_table_function_set_init(tf, raster_h3_init);
        duckdb_table_function_set_local_init(tf, raster_h3_init_local);
        duckdb_table_function_set_function(tf, raster_h3_scan);
        duckdb_table_function_supports_projection_pushdown(tf, true);

        let state = duckdb_register_table_function(con, tf);

        // Cleanup logical types
        let mut type_varchar_mut = type_varchar;
        duckdb_destroy_logical_type(&mut type_varchar_mut);
        let mut type_bigint_mut = type_bigint;
        duckdb_destroy_logical_type(&mut type_bigint_mut);
        let mut type_double_mut = type_double;
        duckdb_destroy_logical_type(&mut type_double_mut);
        let mut type_double_bbox_mut = type_double_bbox;
        duckdb_destroy_logical_type(&mut type_double_bbox_mut);
        let mut type_bool_mut = type_bool;
        duckdb_destroy_logical_type(&mut type_bool_mut);

        let mut tf_mut = tf;
        duckdb_destroy_table_function(&mut tf_mut);

        if state != DuckDBState::Success {
            return Err(format!("Failed to register {} table function", name));
        }
    }

    Ok(())
}
