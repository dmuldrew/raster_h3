use std::ffi::CString;
use std::path::PathBuf;

use crate::aggregator::sampling::SamplingPattern;
use crate::ffi::{
    duckdb_bind_add_result_column, duckdb_bind_get_named_parameter, duckdb_bind_get_parameter,
    duckdb_bind_get_parameter_count, duckdb_bind_info, duckdb_bind_set_error,
    duckdb_create_logical_type, duckdb_destroy_logical_type, duckdb_destroy_value, duckdb_get_bool,
    duckdb_get_double, duckdb_get_int64, duckdb_get_uint64, duckdb_get_varchar,
    duckdb_logical_type, duckdb_value, from_duckdb_string, to_c_string, DuckDBType,
};
use crate::grid::GridKind;
use crate::raster::mosaic::OverlapRule;

use super::parsing::{parse_bbox_str, parse_resolutions_str_for};

/// Conservative WGS84 bounding box `[min_lon, min_lat, max_lon, max_lat]` for an H3 cell.
/// Accounts for high-latitude longitudinal widening, polar containment, and antimeridian
/// crossing (`min_lon > max_lon`), so `is_point_in_bbox` and `rect_intersects_bbox` never
/// clip valid samples belonging to `cell`.
pub fn cell_wgs84_bbox(cell: h3o::CellIndex) -> [f64; 4] {
    cell_wgs84_bbox_for(GridKind::H3, u64::from(cell)).unwrap_or([-180.0, -90.0, 180.0, 90.0])
}

/// Conservative WGS84 bounding box `[min_lon, min_lat, max_lon, max_lat]` for a cell on `grid`.
pub fn cell_wgs84_bbox_for(grid: GridKind, cell_u64: u64) -> Option<[f64; 4]> {
    let res = grid.resolution(cell_u64)?;
    let (is_north_pole, is_south_pole) = match grid {
        GridKind::H3 => {
            let cell = h3o::CellIndex::try_from(cell_u64).ok()?;
            let r = cell.resolution();
            (
                cell == h3o::LatLng::new(90.0, 0.0).expect("valid").to_cell(r),
                cell == h3o::LatLng::new(-90.0, 0.0).expect("valid").to_cell(r),
            )
        }
        GridKind::A5 => {
            let north =
                crate::a5::lonlat_to_cell(crate::a5::LonLat::new(0.0, 90.0), 0).unwrap_or(0);
            let south =
                crate::a5::lonlat_to_cell(crate::a5::LonLat::new(0.0, -90.0), 0).unwrap_or(0);
            (cell_u64 == north, cell_u64 == south)
        }
    };
    let mut verts = [(0.0, 0.0); 10];
    let n = grid.boundary_lng_lat(cell_u64, &mut verts);
    if n == 0 {
        return Some([-180.0, -90.0, 180.0, 90.0]);
    }

    let south_lat = crate::aggregator::horizon_streamer::compute_cell_south_lat_for(grid, cell_u64);
    // By symmetry, great-circle edges in the northern hemisphere can bulge northward by at most
    // (|delta_lat| + |delta_lon|) / 2 relative to their endpoints.
    let mut north_lat = if is_north_pole { 90.0 } else { -90.0_f64 };
    if !is_north_pole {
        for i in 0..n {
            let (p_lng, p_lat) = verts[i];
            let (q_lng, q_lat) = verts[(i + 1) % n];
            let mut edge_north = p_lat.max(q_lat).next_up();
            if p_lat >= 0.0 || q_lat >= 0.0 {
                let direct = (p_lng - q_lng).abs();
                let delta_lon = direct.min(360.0 - direct);
                let delta_lat = (p_lat - q_lat).abs();
                edge_north = (edge_north + 0.5 * (delta_lat + delta_lon)).next_up();
            }
            north_lat = north_lat.max(edge_north);
        }
        north_lat = north_lat.min(90.0);
    }

    if is_north_pole || is_south_pole || north_lat >= 89.5 || south_lat <= -89.5 {
        return Some([-180.0, south_lat, 180.0, north_lat]);
    }

    // Unwrap boundary vertex longitudes around vertex 0 so antimeridian-crossing
    // cells form a tight continuous interval instead of [-180, 180].
    let mut lngs = [0.0f64; 10];
    lngs[0] = verts[0].0;
    let mut min_u = lngs[0];
    let mut max_u = lngs[0];
    for i in 1..n {
        let mut lng = verts[i].0;
        while lng - lngs[i - 1] > 180.0 {
            lng -= 360.0;
        }
        while lng - lngs[i - 1] < -180.0 {
            lng += 360.0;
        }
        lngs[i] = lng;
        min_u = min_u.min(lng);
        max_u = max_u.max(lng);
    }

    // Pad longitude by a small geodesic safety margin scaled by sec(lat).
    let max_abs_lat = south_lat.abs().max(north_lat.abs()).min(89.0);
    let r = crate::pmtiles::tiler::max_cell_radius_deg(grid, res);
    let pad_lon = (r * 0.25 / max_abs_lat.to_radians().cos()).max(1e-5);
    let lo = min_u - pad_lon;
    let hi = max_u + pad_lon;
    if hi - lo >= 360.0 {
        return Some([-180.0, south_lat, 180.0, north_lat]);
    }

    let wrap = crate::crs::transformer::wrap_lon;
    let (min_lon, max_lon) = if lo >= -180.0 && hi <= 180.0 {
        (lo, hi)
    } else {
        (wrap(lo), wrap(hi))
    };

    Some([min_lon, south_lat, max_lon, north_lat])
}

/// RAII guard ensuring DuckDB allocated value handle is destroyed
///
/// The handle is private so that only this module, which takes it straight
/// from a DuckDB getter, can construct one; `Drop` relies on that.
pub struct OwnedValue(duckdb_value);

impl Drop for OwnedValue {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a non-null handle was returned by `duckdb_bind_get_*parameter`,
            // which transfers ownership to us, and this guard is its only owner,
            // so it is destroyed exactly once.
            unsafe {
                duckdb_destroy_value(&mut self.0);
            }
        }
    }
}

/// Ergonomic, safe wrapper around DuckDB's `duckdb_bind_info`
///
/// Invariant: `info` is the live bind handle of the bind callback that
/// created the helper (see [`BindHelper::new`]); every safe method relies on it.
pub struct BindHelper {
    info: duckdb_bind_info,
}

impl BindHelper {
    /// # Safety
    /// `info` must be the `duckdb_bind_info` DuckDB passed to the current bind
    /// callback, and the helper must not be used after that callback returns.
    #[inline(always)]
    pub unsafe fn new(info: duckdb_bind_info) -> Self {
        Self { info }
    }

    /// The wrapped bind handle.
    #[inline(always)]
    pub fn info(&self) -> duckdb_bind_info {
        self.info
    }

    /// Number of positional parameters passed to the function
    #[inline(always)]
    pub fn parameter_count(&self) -> usize {
        // SAFETY: `self.info` is the live bind handle (struct invariant).
        unsafe { duckdb_bind_get_parameter_count(self.info) as usize }
    }

    /// Get a positional parameter value by index wrapped in an RAII `OwnedValue`
    #[inline(always)]
    pub fn get_owned_parameter(&self, index: usize) -> Option<OwnedValue> {
        if index >= self.parameter_count() {
            return None;
        }
        // SAFETY: `self.info` is the live bind handle (struct invariant) and `index` is below the
        // parameter count; ownership of the returned value passes to `OwnedValue`.
        let val = unsafe { duckdb_bind_get_parameter(self.info, index as u64) };
        if val.is_null() {
            None
        } else {
            Some(OwnedValue(val))
        }
    }

    /// Get a positional parameter value by index. The caller owns the returned
    /// handle and must release it with `duckdb_destroy_value`.
    #[inline(always)]
    pub fn get_parameter(&self, index: usize) -> duckdb_value {
        // SAFETY: `self.info` is the live bind handle (struct invariant); DuckDB
        // returns null for an out-of-range index rather than reading past the end.
        unsafe { duckdb_bind_get_parameter(self.info, index as u64) }
    }

    /// Extract a positional VARCHAR parameter as a Rust `String`
    pub fn get_string_param(&self, index: usize) -> Option<String> {
        let val = self.get_owned_parameter(index)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; `duckdb_get_varchar` returns a fresh DuckDB-allocated string
        // (or null) that `from_duckdb_string` copies and frees.
        unsafe { from_duckdb_string(duckdb_get_varchar(val.0)) }
    }

    /// Extract a positional BIGINT parameter as `i64`
    pub fn get_int_param(&self, index: usize) -> Option<i64> {
        let val = self.get_owned_parameter(index)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; the getter only reads it.
        unsafe { Some(duckdb_get_int64(val.0)) }
    }

    /// Extract a positional DOUBLE parameter as `f64`
    pub fn get_double_param(&self, index: usize) -> Option<f64> {
        let val = self.get_owned_parameter(index)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; the getter only reads it.
        unsafe { Some(duckdb_get_double(val.0)) }
    }

    /// Get a named parameter by name wrapped in an RAII `OwnedValue`
    #[inline(always)]
    pub fn get_owned_named_parameter(&self, name: &str) -> Option<OwnedValue> {
        let c_name = to_c_string(name);
        // SAFETY: `self.info` is the live bind handle (struct invariant) and `c_name` is NUL-terminated
        // for the call; ownership of the returned value passes to `OwnedValue`.
        let val = unsafe { duckdb_bind_get_named_parameter(self.info, c_name.as_ptr()) };
        if val.is_null() {
            None
        } else {
            Some(OwnedValue(val))
        }
    }

    /// Get a named parameter by name. The caller owns the returned handle and
    /// must release it with `duckdb_destroy_value`.
    pub fn get_named_parameter(&self, name: &str) -> duckdb_value {
        let c_name = to_c_string(name);
        // SAFETY: `self.info` is the live bind handle (struct invariant) and
        // `c_name` is NUL-terminated for the call.
        unsafe { duckdb_bind_get_named_parameter(self.info, c_name.as_ptr()) }
    }

    /// Extract a named VARCHAR parameter as `String`
    pub fn get_named_string(&self, name: &str) -> Option<String> {
        let val = self.get_owned_named_parameter(name)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; `duckdb_get_varchar` returns a fresh DuckDB-allocated string
        // (or null) that `from_duckdb_string` copies and frees.
        unsafe { from_duckdb_string(duckdb_get_varchar(val.0)) }
    }

    /// Extract a named BIGINT parameter as `i64`
    pub fn get_named_int(&self, name: &str) -> Option<i64> {
        let val = self.get_owned_named_parameter(name)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; the getter only reads it.
        unsafe { Some(duckdb_get_int64(val.0)) }
    }

    /// Extract a named UBIGINT parameter as `u64`
    pub fn get_named_uint(&self, name: &str) -> Option<u64> {
        let val = self.get_owned_named_parameter(name)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; the getter only reads it.
        unsafe { Some(duckdb_get_uint64(val.0)) }
    }

    /// Extract a named DOUBLE parameter as `f64`
    pub fn get_named_double(&self, name: &str) -> Option<f64> {
        let val = self.get_owned_named_parameter(name)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; the getter only reads it.
        unsafe { Some(duckdb_get_double(val.0)) }
    }

    /// Extract a named BOOLEAN parameter as `bool`
    pub fn get_named_bool(&self, name: &str) -> Option<bool> {
        let val = self.get_owned_named_parameter(name)?;
        // SAFETY: `val` owns a live `duckdb_value` until it drops at the end of this function; the getter only reads it.
        unsafe { Some(duckdb_get_bool(val.0)) }
    }

    /// Set an error message on the bind context and halt execution
    pub fn set_error(&self, msg: &str) {
        let c_err = CString::new(msg)
            .unwrap_or_else(|_| CString::new("Error in table function bind").unwrap());
        // SAFETY: `self.info` is the live bind handle (struct invariant); DuckDB copies the
        // NUL-terminated message before `c_err` drops.
        unsafe {
            duckdb_bind_set_error(self.info, c_err.as_ptr());
        }
    }

    /// Add a result column to the output schema with automated logical type lifecycle management
    pub fn add_result_column(&self, name: &str, duckdb_type: DuckDBType) {
        // SAFETY: `self.info` is the live bind handle (struct invariant). DuckDB copies the
        // name and logical type, so the type we created is destroyed once here.
        unsafe {
            let col_name = to_c_string(name);
            let mut logical_type = duckdb_create_logical_type(duckdb_type);
            duckdb_bind_add_result_column(self.info, col_name.as_ptr(), logical_type);
            duckdb_destroy_logical_type(&mut logical_type);
        }
    }

    /// Add a custom logical type column (e.g. GEOMETRY)
    ///
    /// # Safety
    /// `logical_type` must be a live DuckDB logical type for the duration of this call.
    pub unsafe fn add_custom_result_column(&self, name: &str, logical_type: duckdb_logical_type) {
        // SAFETY: `self.info` is the live bind handle (struct invariant), `logical_type` is
        // live for the call (caller contract), and DuckDB copies both arguments.
        unsafe {
            let col_name = to_c_string(name);
            duckdb_bind_add_result_column(self.info, col_name.as_ptr(), logical_type);
        }
    }

    // =========================================================================
    // High-Level Standard Raster Parameter Parsers
    // =========================================================================

    /// Parse `grid := 'h3' | 'a5'`, defaulting to `default_grid`.
    pub fn parse_grid(&self, default_grid: GridKind) -> Option<GridKind> {
        match self.get_named_string("grid") {
            Some(s) => match GridKind::parse(&s) {
                Some(g) => Some(g),
                None => {
                    self.set_error(&format!(
                        "Invalid grid parameter '{}': expected 'h3' or 'a5'",
                        s
                    ));
                    None
                }
            },
            None => Some(default_grid),
        }
    }

    /// Parse target H3 resolutions across all supported conventions:
    /// 1. `resolutions := '7,8'` (comma/whitespace separated list)
    /// 2. `min_resolution := 6, max_resolution := 8` (inclusive integer range)
    /// 3. `resolution := 8` (named integer)
    /// 4. Positional fallback parameter at `positional_idx`
    pub fn parse_resolutions(&self, default_res: u8, positional_idx: Option<usize>) -> Vec<u8> {
        self.parse_resolutions_for(GridKind::H3, default_res, positional_idx)
    }

    /// Parse target resolutions for `grid` across all supported conventions.
    pub fn parse_resolutions_for(
        &self,
        grid: GridKind,
        default_res: u8,
        positional_idx: Option<usize>,
    ) -> Vec<u8> {
        let max_res = i64::from(grid.max_resolution());
        // 1. Named parameter: resolutions (VARCHAR, e.g. '7,8' or '7, 8, 9')
        if let Some(s) = self.get_named_string("resolutions") {
            let list = parse_resolutions_str_for(grid, &s);
            if !list.is_empty() {
                return list;
            }
        }

        // 2. Named parameters: min_resolution and max_resolution (BIGINT)
        let min_res_val = self.get_named_int("min_resolution");
        let max_res_val = self.get_named_int("max_resolution");
        if let (Some(min_r), Some(max_r)) = (min_res_val, max_res_val) {
            if (0..=max_res).contains(&min_r) && (0..=max_res).contains(&max_r) && min_r <= max_r {
                return ((min_r as u8)..=(max_r as u8)).collect();
            }
        }

        // 3. Named parameter: resolution (BIGINT)
        if let Some(res_int) = self.get_named_int("resolution") {
            if (0..=max_res).contains(&res_int) {
                return vec![res_int as u8];
            }
        }

        // 4. Positional fallback parameter
        if let Some(pos_idx) = positional_idx {
            if let Some(res_int) = self.get_int_param(pos_idx) {
                if (0..=max_res).contains(&res_int) {
                    return vec![res_int as u8];
                }
            }
        }

        vec![default_res]
    }

    /// Parse spatial bounding box from coordinates or H3/A5 cell:
    /// 1. `min_lon, min_lat, max_lon, max_lat` (4 explicit coordinates)
    /// 2. `bbox := 'min_lon,min_lat,max_lon,max_lat'` (list/string)
    /// 3. `h3_cell` / `a5_cell` (BIGINT/UBIGINT)
    /// 4. `h3_hex` / `a5_hex` (VARCHAR)
    pub fn parse_bbox(&self) -> Option<[f64; 4]> {
        // 1. Explicit min_lon, min_lat, max_lon, max_lat
        let min_lon = self.get_named_double("min_lon");
        let min_lat = self.get_named_double("min_lat");
        let max_lon = self.get_named_double("max_lon");
        let max_lat = self.get_named_double("max_lat");

        if let (Some(min_x), Some(min_y), Some(max_x), Some(max_y)) =
            (min_lon, min_lat, max_lon, max_lat)
        {
            return Some([min_x, min_y, max_x, max_y]);
        }

        // 2. Comma-separated bbox string
        if let Some(s) = self.get_named_string("bbox") {
            if let Some(bbox) = parse_bbox_str(&s) {
                return Some(bbox);
            }
        }

        // 3. H3 cell index (u64 / i64)
        if let Some(cell_u64) = self
            .get_named_uint("h3_cell")
            .or_else(|| self.get_named_int("h3_cell").map(|v| v as u64))
        {
            if let Ok(cell) = h3o::CellIndex::try_from(cell_u64) {
                return Some(cell_wgs84_bbox(cell));
            }
        }

        // 4. H3 hex string
        if let Some(hex_str) = self.get_named_string("h3_hex") {
            if let Ok(cell) = hex_str.trim().parse::<h3o::CellIndex>() {
                return Some(cell_wgs84_bbox(cell));
            }
        }

        // 5. A5 cell index (u64 / i64)
        if let Some(cell_u64) = self
            .get_named_uint("a5_cell")
            .or_else(|| self.get_named_int("a5_cell").map(|v| v as u64))
        {
            if let Some(bbox) = cell_wgs84_bbox_for(GridKind::A5, cell_u64) {
                return Some(bbox);
            }
        }

        // 6. A5 hex string
        if let Some(hex_str) = self.get_named_string("a5_hex") {
            if let Some(cell_u64) = crate::encoding::parse_hex_u64(hex_str.trim()) {
                if let Some(bbox) = cell_wgs84_bbox_for(GridKind::A5, cell_u64) {
                    return Some(bbox);
                }
            }
        }

        None
    }

    /// Parse the sampling preset; report unknown names as binding errors.
    pub fn parse_sampling(&self) -> Option<SamplingPattern> {
        match self.get_named_string("sampling") {
            Some(s) => match SamplingPattern::parse(&s) {
                Ok(pattern) => Some(pattern),
                Err(error) => {
                    self.set_error(&error.to_string());
                    None
                }
            },
            None => Some(SamplingPattern::default()),
        }
    }

    /// Parse mosaic overlap rule (`cutline`, `replace`, `highest_resolution`)
    pub fn parse_overlap_rule(&self) -> OverlapRule {
        if let Some(s) = self.get_named_string("overlap_rule") {
            OverlapRule::parse(&s)
        } else {
            OverlapRule::default()
        }
    }

    /// Parse source coordinate reference system override (`source_crs` or `crs`)
    pub fn parse_source_crs(&self) -> Option<String> {
        self.get_named_string("source_crs")
            .or_else(|| self.get_named_string("crs"))
    }

    /// `scale_offset` (default true): convert stored values to physical
    /// values with each band's GDAL `SCALE`/`OFFSET` metadata.
    pub fn parse_scale_offset(&self) -> bool {
        self.get_named_bool("scale_offset").unwrap_or(true)
    }

    /// Parse execution limits: `workers`/`threads` (aggregation),
    /// `decode_workers`, `fetch_workers`, `decode_budget_mb` and
    /// `compaction_budget_mb`, plus `strict_transforms`.
    /// Non-positive values are errors.
    pub fn parse_execution(&self) -> Option<ExecutionParams> {
        let positive = |names: &[&str]| -> Result<Option<usize>, ()> {
            for name in names {
                if let Some(v) = self.get_named_int(name) {
                    if v <= 0 {
                        self.set_error(&format!("{} must be a positive integer", name));
                        return Err(());
                    }
                    return Ok(Some(v as usize));
                }
            }
            Ok(None)
        };
        let mut execution = ExecutionParams {
            aggregation_workers: positive(&["workers", "threads"]).ok()?,
            decode_workers: positive(&["decode_workers"]).ok()?,
            fetch_workers: positive(&["fetch_workers"]).ok()?,
            decode_budget_mb: positive(&["decode_budget_mb"]).ok()?,
            compaction_budget_mb: positive(&["compaction_budget_mb"]).ok()?,
            strict_transforms: self.get_named_bool("strict_transforms").unwrap_or(false),
        };
        // Fill unset values from `SET raster_h3_*` session options (v1.5.6
        // build) or `RASTER_H3_*` environment fallbacks.
        // SAFETY: `self.info` is the live bind handle (struct invariant).
        unsafe { crate::functions::config::apply_duckdb_config_options(self.info, &mut execution) };
        Some(execution)
    }

    /// Parse common raster parameters shared across continuous and categorical aggregations
    pub fn parse_common_raster_params(&self, func_name: &str) -> Option<CommonRasterParams> {
        self.parse_common_raster_params_for(func_name, GridKind::H3)
    }

    /// Parse common raster parameters shared across continuous and categorical aggregations
    /// using `default_grid` when `grid` is not explicitly specified.
    pub fn parse_common_raster_params_for(
        &self,
        func_name: &str,
        default_grid: GridKind,
    ) -> Option<CommonRasterParams> {
        if self.parameter_count() < 1 {
            self.set_error(&format!(
                "{} requires at least 1 argument: file_path",
                func_name
            ));
            return None;
        }

        let file_path = match self.get_string_param(0) {
            Some(p) => p,
            None => {
                self.set_error("Invalid file_path parameter");
                return None;
            }
        };

        let grid = self.parse_grid(default_grid)?;
        let resolutions = self.parse_resolutions_for(grid, 8, Some(1));
        let source_crs = self.parse_source_crs();
        let nodata = self.get_named_double("nodata");
        let scale_offset = self.parse_scale_offset();
        let chunk_size = self
            .get_named_int("chunk_size")
            .filter(|&cs| cs > 0)
            .unwrap_or(512) as u32;
        let band = self.get_named_int("band").filter(|&b| b > 0).unwrap_or(1) as u32;
        let sampling = self.parse_sampling()?;
        let bbox = self.parse_bbox();
        let compact = self.get_named_bool("compact").unwrap_or(false);
        let overlap_rule = self.parse_overlap_rule();
        let emit_geom = self
            .get_named_bool("geom")
            .unwrap_or_else(crate::ffi::is_geometry_available);
        let execution = self.parse_execution()?;

        let resolved_paths = match crate::raster::mosaic::resolve_raster_sources(&file_path) {
            Ok(paths) => paths,
            Err(e) => {
                self.set_error(&format!("Failed to resolve raster source(s): {}", e));
                return None;
            }
        };

        Some(CommonRasterParams {
            file_path,
            resolved_paths,
            grid,
            resolutions,
            source_crs,
            nodata,
            scale_offset,
            chunk_size,
            band,
            sampling,
            bbox,
            compact,
            overlap_rule,
            emit_geom,
            execution,
            // SAFETY: `self.info` is the live bind handle (struct invariant).
            remote_source_factory: unsafe {
                crate::functions::config::raster_source_factory_for_bind(self.info)
            },
        })
    }
}

// =============================================================================
// Common Raster Binding Parameters Struct
// =============================================================================

/// Standard parameters common to all raster aggregation table functions
#[derive(Debug, Clone)]
pub struct CommonRasterParams {
    pub file_path: String,
    pub resolved_paths: Vec<PathBuf>,
    pub grid: GridKind,
    pub resolutions: Vec<u8>,
    pub source_crs: Option<String>,
    pub nodata: Option<f64>,
    /// `scale_offset`: calibrate stored values with GDAL band scale/offset.
    pub scale_offset: bool,
    pub chunk_size: u32,
    pub band: u32,
    pub sampling: SamplingPattern,
    pub bbox: Option<[f64; 4]>,
    pub compact: bool,
    pub overlap_rule: OverlapRule,
    pub emit_geom: bool,
    pub execution: ExecutionParams,
    /// Routes raster byte-range reads through the DuckDB file system when
    /// `raster_h3_use_duckdb_fs` is set (v1.5.6 build only).
    pub remote_source_factory: Option<raster_h3_core::raster::http_range::RemoteSourceFactory>,
}

/// SQL execution limits and options. Each `None` keeps the streamer default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecutionParams {
    /// `workers` / `threads`: dedicated aggregation pool size.
    pub aggregation_workers: Option<usize>,
    /// `decode_workers`: chunk decoding threads.
    pub decode_workers: Option<usize>,
    /// `fetch_workers`: concurrent remote range requests.
    pub fetch_workers: Option<usize>,
    /// `decode_budget_mb`: decoded chunk memory budget in MiB.
    pub decode_budget_mb: Option<usize>,
    /// `compaction_budget_mb`: opt-in limit on pending compaction groups in MiB.
    pub compaction_budget_mb: Option<usize>,
    /// `strict_transforms`: fail the query when a valid sample point cannot
    /// be transformed to WGS84 instead of silently skipping it.
    pub strict_transforms: bool,
}

impl ExecutionParams {
    /// Apply the requested limits to a streamer configuration.
    pub fn apply(&self, config: &mut crate::aggregator::multi_horizon::MultiResolutionConfig) {
        if self.aggregation_workers.is_some() {
            config.aggregation_workers = self.aggregation_workers;
        }
        if let Some(n) = self.decode_workers {
            config.decode_workers = n;
        }
        if let Some(n) = self.fetch_workers {
            config.fetch_workers = n;
        }
        if let Some(mb) = self.decode_budget_mb {
            config.decode_budget_bytes = mb.saturating_mul(1024 * 1024);
        }
        if let Some(mb) = self.compaction_budget_mb {
            config.compaction_budget_bytes = Some(mb.saturating_mul(1024 * 1024));
        }
        config.strict_transforms = self.strict_transforms;
    }
}

pub type CommonRasterBindParams = CommonRasterParams;

impl CommonRasterParams {
    /// Extract all common raster parameters from bind context
    pub fn extract(bind: &BindHelper, func_name: &str, default_compact: bool) -> Option<Self> {
        let mut params = bind.parse_common_raster_params(func_name)?;
        if bind.get_named_bool("compact").is_none() {
            params.compact = default_compact;
        }
        Some(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_owned_value_null_safety() {
        let val = OwnedValue(std::ptr::null_mut());
        // Dropping null value must not panic or segfault
        drop(val);
    }

    #[test]
    fn test_execution_params_override_streamer_defaults() {
        use crate::aggregator::multi_horizon::MultiResolutionConfig;
        let mut config = MultiResolutionConfig::new(vec![8]);
        let defaults = (
            config.aggregation_workers,
            config.decode_workers,
            config.fetch_workers,
        );
        ExecutionParams::default().apply(&mut config);
        assert_eq!(
            (
                config.aggregation_workers,
                config.decode_workers,
                config.fetch_workers
            ),
            defaults
        );
        ExecutionParams {
            aggregation_workers: Some(3),
            decode_workers: Some(2),
            fetch_workers: Some(5),
            decode_budget_mb: Some(32),
            compaction_budget_mb: Some(8),
            strict_transforms: true,
        }
        .apply(&mut config);
        assert_eq!(config.aggregation_workers, Some(3));
        assert_eq!(config.decode_workers, 2);
        assert_eq!(config.fetch_workers, 5);
        assert_eq!(config.decode_budget_bytes, 32 * 1024 * 1024);
        assert_eq!(config.compaction_budget_bytes, Some(8 * 1024 * 1024));
        assert!(config.strict_transforms);
    }
}
