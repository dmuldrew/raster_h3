//! # `raster-h3-pg`
//!
//! PostgreSQL extension interface for [`raster_h3_core`], providing:
//! - Set-Returning Function (SRF) iterators [`PgContinuousSrf`] (`h3_raster_continuous_aggregate`)
//!   and [`PgCategoricalSrf`] (`h3_raster_categorical_aggregate`)
//! - PostgreSQL V1 Function Manager (`fmgr`) calling convention structs (`Pg_finfo_record`,
//!   `PgFnInfoV1`, `Datum`, `NullableDatum`, `FunctionCallInfoBaseData`)
//! - C-ABI entry points for PostgreSQL scalar functions and SRFs.
//!
//! The loadable module itself (`PG_MODULE_MAGIC`, `text`/`bytea` functions and the
//! `h3_raster_*_aggregate` SRFs) lives in `fmgr` and is built with the `pg-extension`
//! feature, e.g. `RASTER_H3_PG_MAJOR=16 cargo build -p raster-h3-pg --features pg-extension`.

#![allow(non_camel_case_types)]

#[cfg(feature = "pg-extension")]
pub mod fmgr;

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_short, c_void, CStr, CString};
use std::ptr;
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::encoding::{fast_hex_u64, h3_index_to_wkb, WkbBuf, WKB_BUF_LEN};
use raster_h3_core::error::{safe_panic_payload_to_string, RasterH3Error, Result};
use raster_h3_core::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};
use raster_h3_core::scalar_ops::{
    h3_cell_to_parent_u64, h3_get_resolution, h3_is_valid_u64, h3_to_lat, h3_to_lng,
    parse_bbox_str, parse_resolutions_str,
};

// =============================================================================
// PostgreSQL V1 Function Manager (`fmgr`) ABI Types
// =============================================================================

/// PostgreSQL `Datum` (pointer-sized unsigned integer).
pub type Datum = usize;

/// PostgreSQL `Oid` (unsigned 32-bit object identifier).
pub type Oid = u32;

/// PostgreSQL V1 function info descriptor (`Pg_finfo_record`).
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Pg_finfo_record {
    pub api_version: c_int,
}

pub const PG_FINFO_V1: Pg_finfo_record = Pg_finfo_record { api_version: 1 };

/// PostgreSQL 12+ `NullableDatum` argument slot.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct NullableDatum {
    pub value: Datum,
    pub isnull: bool,
}

/// PostgreSQL `FunctionCallInfoBaseData` header (PostgreSQL 12+ layout with flexible args array).
#[repr(C)]
pub struct FunctionCallInfoBaseData {
    pub flinfo: *mut c_void,
    pub context: *mut c_void,
    pub resultinfo: *mut c_void,
    pub fncollation: Oid,
    pub isnull: bool,
    pub nargs: c_short,
    pub args: [NullableDatum; 8],
}

impl FunctionCallInfoBaseData {
    /// Construct a call info struct with `args` for in-process testing or invocation.
    pub fn with_args(args: &[NullableDatum]) -> Self {
        let mut arr = [NullableDatum::default(); 8];
        let n = args.len().min(8);
        arr[..n].copy_from_slice(&args[..n]);
        Self {
            flinfo: ptr::null_mut(),
            context: ptr::null_mut(),
            resultinfo: ptr::null_mut(),
            fncollation: 0,
            isnull: false,
            nargs: n as c_short,
            args: arr,
        }
    }

    #[inline]
    pub unsafe fn arg_i64(&self, idx: usize) -> Option<i64> {
        if idx >= self.nargs as usize || self.args[idx].isnull {
            None
        } else {
            Some(self.args[idx].value as i64)
        }
    }
}

pub type pg_func_call_info = *mut FunctionCallInfoBaseData;

/// Encode a `f64` into a 64-bit PostgreSQL `Datum` (`Float8GetDatum`).
#[inline]
pub fn float8_to_datum(v: f64) -> Datum {
    v.to_bits() as Datum
}

/// Decode a `f64` from a 64-bit PostgreSQL `Datum` (`DatumGetFloat8`).
#[inline]
pub fn datum_to_float8(d: Datum) -> f64 {
    f64::from_bits(d as u64)
}

/// Encode an `i64` into a PostgreSQL `Datum` (`Int64GetDatum`).
#[inline]
pub fn int64_to_datum(v: i64) -> Datum {
    v as usize
}

/// Decode an `i64` from a PostgreSQL `Datum` (`DatumGetInt64`).
#[inline]
pub fn datum_to_int64(d: Datum) -> i64 {
    d as i64
}

// =============================================================================
// PostgreSQL V1 Scalar Function Entry Points
// =============================================================================

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_to_lat() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// PostgreSQL V1 scalar function: `h3_to_lat(int8) -> float8`
///
/// # Safety
/// `fcinfo` must point to a valid `FunctionCallInfoBaseData`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_to_lat(fcinfo: pg_func_call_info) -> Datum {
    if fcinfo.is_null() {
        return 0;
    }
    let info = &mut *fcinfo;
    let Some(cell_i64) = info.arg_i64(0) else {
        info.isnull = true;
        return 0;
    };
    let lat = h3_to_lat(cell_i64 as u64);
    if lat.is_nan() {
        info.isnull = true;
        0
    } else {
        float8_to_datum(lat)
    }
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_to_lng() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// PostgreSQL V1 scalar function: `h3_to_lng(int8) -> float8`
///
/// # Safety
/// `fcinfo` must point to a valid `FunctionCallInfoBaseData`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_to_lng(fcinfo: pg_func_call_info) -> Datum {
    if fcinfo.is_null() {
        return 0;
    }
    let info = &mut *fcinfo;
    let Some(cell_i64) = info.arg_i64(0) else {
        info.isnull = true;
        return 0;
    };
    let lng = h3_to_lng(cell_i64 as u64);
    if lng.is_nan() {
        info.isnull = true;
        0
    } else {
        float8_to_datum(lng)
    }
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_get_resolution() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// PostgreSQL V1 scalar function: `h3_get_resolution(int8) -> int8`
///
/// # Safety
/// `fcinfo` must point to a valid `FunctionCallInfoBaseData`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_get_resolution(fcinfo: pg_func_call_info) -> Datum {
    if fcinfo.is_null() {
        return 0;
    }
    let info = &mut *fcinfo;
    let Some(cell_i64) = info.arg_i64(0) else {
        info.isnull = true;
        return 0;
    };
    let res = h3_get_resolution(cell_i64 as u64);
    if res < 0 {
        info.isnull = true;
        0
    } else {
        int64_to_datum(res)
    }
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_is_valid() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// PostgreSQL V1 scalar function: `h3_is_valid(int8) -> bool`
///
/// # Safety
/// `fcinfo` must point to a valid `FunctionCallInfoBaseData`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_is_valid(fcinfo: pg_func_call_info) -> Datum {
    if fcinfo.is_null() {
        return 0;
    }
    let info = &mut *fcinfo;
    let Some(cell_i64) = info.arg_i64(0) else {
        info.isnull = true;
        return 0;
    };
    usize::from(h3_is_valid_u64(cell_i64 as u64))
}

#[no_mangle]
pub extern "C" fn pg_finfo_pg_h3_cell_to_parent() -> *const Pg_finfo_record {
    &PG_FINFO_V1
}

/// PostgreSQL V1 scalar function: `h3_cell_to_parent(int8, int8) -> int8`
///
/// # Safety
/// `fcinfo` must point to a valid `FunctionCallInfoBaseData`.
#[no_mangle]
pub unsafe extern "C" fn pg_h3_cell_to_parent(fcinfo: pg_func_call_info) -> Datum {
    if fcinfo.is_null() {
        return 0;
    }
    let info = &mut *fcinfo;
    let (Some(cell_i64), Some(parent_res)) = (info.arg_i64(0), info.arg_i64(1)) else {
        info.isnull = true;
        return 0;
    };
    match h3_cell_to_parent_u64(cell_i64 as u64, parent_res) {
        Some(parent) => int64_to_datum(parent as i64),
        None => {
            info.isnull = true;
            0
        }
    }
}

// =============================================================================
// Set-Returning Functions (SRF) for PostgreSQL (`h3_raster_*_aggregate`)
// =============================================================================

/// A single output row of `h3_raster_continuous_aggregate` in PostgreSQL types.
#[derive(Debug, Clone, PartialEq)]
pub struct PgContinuousRow {
    /// `bigint` (signed 64-bit representation of H3 index, matching `h3-pg` convention)
    pub h3_index: i64,
    /// `text`
    pub h3_hex: String,
    /// `double precision`
    pub mean: f64,
    /// `double precision` (nullable when sample count < 2)
    pub stddev: Option<f64>,
    /// `double precision`
    pub count: f64,
    /// `double precision`
    pub min: f64,
    /// `double precision`
    pub max: f64,
    /// `double precision`
    pub sum: f64,
    /// `smallint`
    pub resolution: i16,
    /// `bytea`
    pub wkb: Vec<u8>,
}

/// A single output row of `h3_raster_categorical_aggregate` in PostgreSQL types.
#[derive(Debug, Clone, PartialEq)]
pub struct PgCategoricalRow {
    pub h3_index: i64,
    pub h3_hex: String,
    pub majority_class: i64,
    pub majority_fraction: f64,
    pub majority_count: f64,
    pub unique_classes: i64,
    pub total_count: f64,
    pub histogram: String,
    pub resolution: i16,
    pub shannon_entropy: f64,
    pub wkb: Vec<u8>,
}

/// Stateful Set-Returning Function (SRF) iterator for `h3_raster_continuous_aggregate`.
pub struct PgContinuousSrf {
    streamer: MultiScanHorizonStreamer,
    buffer: VecDeque<MultiContinuousRecord>,
    batch_size: usize,
    hex_buf: [u8; 16],
    wkb_buf: WkbBuf,
    finished: bool,
}

impl PgContinuousSrf {
    pub fn open(
        raster_path: &str,
        resolutions: Vec<u8>,
        source_crs: Option<String>,
        nodata: Option<f64>,
        bbox: Option<[f64; 4]>,
    ) -> Result<Self> {
        let paths = resolve_raster_sources(raster_path)?;
        let res = if resolutions.is_empty() {
            vec![8]
        } else {
            resolutions
        };
        let mut config = MultiResolutionConfig::new(res);
        config.custom_crs = source_crs.clone();
        config.custom_nodata = nodata;
        config.bbox = bbox;
        config.overlap_rule = OverlapRule::Cutline;

        let mosaic = Arc::new(MosaicReader::open(
            &paths,
            bbox,
            source_crs.as_deref(),
            OverlapRule::Cutline,
        )?);
        let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?;
        Ok(Self {
            streamer,
            buffer: VecDeque::with_capacity(2048),
            batch_size: 2048,
            hex_buf: [0u8; 16],
            wkb_buf: [0u8; WKB_BUF_LEN],
            finished: false,
        })
    }

    pub fn next_row(&mut self) -> Result<Option<PgContinuousRow>> {
        if self.buffer.is_empty() && !self.finished {
            let mut batch = Vec::with_capacity(self.batch_size);
            let count = self
                .streamer
                .drain_completed_into(self.batch_size, |_, rec| {
                    batch.push(rec);
                })?;
            if count == 0 {
                self.finished = true;
                return Ok(None);
            }
            self.buffer.extend(batch);
        }

        let Some(rec) = self.buffer.pop_front() else {
            self.finished = true;
            return Ok(None);
        };

        let hex_bytes = fast_hex_u64(rec.h3_index, &mut self.hex_buf);
        let h3_hex = std::str::from_utf8(hex_bytes)
            .unwrap_or_default()
            .to_owned();
        let wkb = match h3_index_to_wkb(rec.h3_index, &mut self.wkb_buf) {
            Some(len) => self.wkb_buf[..len].to_vec(),
            None => Vec::new(),
        };
        let sd = rec.accumulator.stddev();

        Ok(Some(PgContinuousRow {
            h3_index: rec.h3_index as i64,
            h3_hex,
            mean: rec.accumulator.mean(),
            stddev: if sd.is_finite() { Some(sd) } else { None },
            count: rec.accumulator.count,
            min: rec.accumulator.min,
            max: rec.accumulator.max,
            sum: rec.accumulator.sum,
            resolution: rec.resolution as i16,
            wkb,
        }))
    }
}

impl Iterator for PgContinuousSrf {
    type Item = Result<PgContinuousRow>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_row() {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Stateful Set-Returning Function (SRF) iterator for `h3_raster_categorical_aggregate`.
pub struct PgCategoricalSrf {
    streamer: MultiCategoricalHorizonStreamer,
    buffer: VecDeque<MultiCategoricalRecord>,
    batch_size: usize,
    hex_buf: [u8; 16],
    wkb_buf: WkbBuf,
    finished: bool,
}

impl PgCategoricalSrf {
    pub fn open(
        raster_path: &str,
        resolutions: Vec<u8>,
        source_crs: Option<String>,
        nodata: Option<f64>,
        bbox: Option<[f64; 4]>,
    ) -> Result<Self> {
        let paths = resolve_raster_sources(raster_path)?;
        let res = if resolutions.is_empty() {
            vec![8]
        } else {
            resolutions
        };
        let mut config = MultiResolutionConfig::new(res);
        config.custom_crs = source_crs.clone();
        config.custom_nodata = nodata;
        config.bbox = bbox;
        config.overlap_rule = OverlapRule::Cutline;

        let mosaic = Arc::new(MosaicReader::open(
            &paths,
            bbox,
            source_crs.as_deref(),
            OverlapRule::Cutline,
        )?);
        let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)?;
        Ok(Self {
            streamer,
            buffer: VecDeque::with_capacity(2048),
            batch_size: 2048,
            hex_buf: [0u8; 16],
            wkb_buf: [0u8; WKB_BUF_LEN],
            finished: false,
        })
    }

    pub fn next_row(&mut self) -> Result<Option<PgCategoricalRow>> {
        if self.buffer.is_empty() && !self.finished {
            let mut batch = Vec::with_capacity(self.batch_size);
            let count = self
                .streamer
                .drain_completed_into(self.batch_size, |_, rec| {
                    batch.push(rec);
                })?;
            if count == 0 {
                self.finished = true;
                return Ok(None);
            }
            self.buffer.extend(batch);
        }

        let Some(rec) = self.buffer.pop_front() else {
            self.finished = true;
            return Ok(None);
        };

        let hex_bytes = fast_hex_u64(rec.h3_index, &mut self.hex_buf);
        let h3_hex = std::str::from_utf8(hex_bytes)
            .unwrap_or_default()
            .to_owned();
        let (maj_class, maj_count, maj_frac) = rec.accumulator.majority();
        let wkb = match h3_index_to_wkb(rec.h3_index, &mut self.wkb_buf) {
            Some(len) => self.wkb_buf[..len].to_vec(),
            None => Vec::new(),
        };

        Ok(Some(PgCategoricalRow {
            h3_index: rec.h3_index as i64,
            h3_hex,
            majority_class: maj_class,
            majority_fraction: maj_frac,
            majority_count: maj_count,
            unique_classes: rec.accumulator.unique_classes() as i64,
            total_count: rec.accumulator.total_count,
            histogram: rec.accumulator.histogram_json(),
            resolution: rec.resolution as i16,
            shannon_entropy: rec.accumulator.shannon_entropy(),
            wkb,
        }))
    }
}

impl Iterator for PgCategoricalSrf {
    type Item = Result<PgCategoricalRow>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_row() {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

// =============================================================================
// C-ABI SRF Lifecycle Entry Points (`pg_srf_continuous_*`)
// =============================================================================

/// C-compatible row struct filled by `pg_srf_continuous_next`.
#[repr(C)]
pub struct PgCContinuousRow {
    pub h3_index: i64,
    pub h3_hex: [u8; 16],
    pub mean: f64,
    pub stddev: f64,
    pub has_stddev: c_int,
    pub count: f64,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub resolution: i16,
    pub wkb_len: usize,
    /// Sized to hold any H3 cell polygon, including 10-vertex odd-resolution pentagons.
    pub wkb: WkbBuf,
}

/// Copy a full WKB geometry into the C row. Rows are built from a [`WkbBuf`], so they always fit.
fn fill_c_wkb(out: &mut PgCContinuousRow, wkb: &[u8]) {
    out.wkb = [0u8; WKB_BUF_LEN];
    out.wkb[..wkb.len()].copy_from_slice(wkb);
    out.wkb_len = wkb.len();
}

unsafe fn opt_cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Open a continuous SRF state handle (`*mut PgContinuousSrf`).
///
/// # Safety
/// String pointer arguments must be valid NUL-terminated C strings or NULL.
#[no_mangle]
pub unsafe extern "C" fn pg_srf_continuous_open(
    raster_path: *const c_char,
    resolutions: *const c_char,
    source_crs: *const c_char,
    has_nodata: c_int,
    nodata: f64,
    bbox: *const c_char,
    out_err: *mut *mut c_char,
) -> *mut PgContinuousSrf {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<PgContinuousSrf> {
            let path_str = opt_cstr(raster_path)
                .ok_or_else(|| RasterH3Error::InvalidParameter("Missing raster_path".into()))?;
            let res_vec = opt_cstr(resolutions)
                .map(|s| parse_resolutions_str(&s))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| vec![8]);
            let crs_opt = opt_cstr(source_crs);
            let nodata_opt = if has_nodata != 0 { Some(nodata) } else { None };
            let bbox_opt = opt_cstr(bbox).and_then(|s| parse_bbox_str(&s));
            PgContinuousSrf::open(&path_str, res_vec, crs_opt, nodata_opt, bbox_opt)
        },
    ));

    match res {
        Ok(Ok(srf)) => Box::into_raw(Box::new(srf)),
        Ok(Err(e)) => {
            if !out_err.is_null() {
                *out_err = CString::new(e.to_string()).unwrap_or_default().into_raw();
            }
            ptr::null_mut()
        }
        Err(payload) => {
            let msg = safe_panic_payload_to_string(payload);
            if !out_err.is_null() {
                *out_err = CString::new(format!("Panic in pg_srf_continuous_open: {msg}"))
                    .unwrap_or_default()
                    .into_raw();
            }
            ptr::null_mut()
        }
    }
}

/// Fetch the next row from a `*mut PgContinuousSrf` into `out_row`.
/// Returns `1` if a row was written, `0` on EOF, or `-1` on error.
///
/// # Safety
/// `srf` and `out_row` must be valid non-null pointers.
#[no_mangle]
pub unsafe extern "C" fn pg_srf_continuous_next(
    srf: *mut PgContinuousSrf,
    out_row: *mut PgCContinuousRow,
) -> c_int {
    if srf.is_null() || out_row.is_null() {
        return -1;
    }
    match (*srf).next_row() {
        Ok(Some(row)) => {
            let out = &mut *out_row;
            out.h3_index = row.h3_index;
            out.h3_hex = [0u8; 16];
            let hb = row.h3_hex.as_bytes();
            let hlen = hb.len().min(15);
            out.h3_hex[..hlen].copy_from_slice(&hb[..hlen]);
            out.mean = row.mean;
            if let Some(sd) = row.stddev {
                out.stddev = sd;
                out.has_stddev = 1;
            } else {
                out.stddev = 0.0;
                out.has_stddev = 0;
            }
            out.count = row.count;
            out.min = row.min;
            out.max = row.max;
            out.sum = row.sum;
            out.resolution = row.resolution;
            fill_c_wkb(out, &row.wkb);
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
}

/// Close and deallocate a `*mut PgContinuousSrf`.
///
/// # Safety
/// `srf` must be null or returned by `pg_srf_continuous_open`.
#[no_mangle]
pub unsafe extern "C" fn pg_srf_continuous_close(srf: *mut PgContinuousSrf) {
    if !srf.is_null() {
        drop(Box::from_raw(srf));
    }
}

// Re-export core scalar helpers for Rust callers of `raster_h3_pg`
pub use raster_h3_core::scalar_ops::{
    h3_cell_to_parent_str as pg_scalar_h3_cell_to_parent_str,
    h3_cell_to_parent_u64 as pg_scalar_h3_cell_to_parent_u64,
    h3_get_resolution as pg_scalar_h3_get_resolution, h3_is_valid_str as pg_scalar_h3_is_valid_str,
    h3_is_valid_u64 as pg_scalar_h3_is_valid_u64, h3_to_lat as pg_scalar_h3_to_lat,
    h3_to_lng as pg_scalar_h3_to_lng, h3_to_string as pg_scalar_h3_to_string,
    h3_to_wkb_u64 as pg_scalar_h3_to_wkb_u64, string_to_h3 as pg_scalar_string_to_h3,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::NamedTempFile;
    use tiff::encoder::colortype::Gray32Float;
    use tiff::encoder::TiffEncoder;

    fn write_test_tif(width: u32, height: u32, val: f32) -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let file = File::create(tmp.path()).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut img = encoder.new_image::<Gray32Float>(width, height).unwrap();
        img.encoder()
            .write_tag(
                tiff::tags::Tag::Unknown(33550),
                &[0.001f64, 0.001f64, 0.0f64][..],
            )
            .unwrap();
        img.encoder()
            .write_tag(
                tiff::tags::Tag::Unknown(33922),
                &[0.0f64, 0.0f64, 0.0f64, -122.45f64, 37.80f64, 0.0f64][..],
            )
            .unwrap();
        let data = vec![val; (width * height) as usize];
        img.write_data(&data).unwrap();
        tmp
    }

    #[test]
    fn test_pg_v1_scalar_functions_and_null_handling() {
        let valid_cell = 0x8828308281fffffu64 as i64;
        unsafe {
            // 1. pg_h3_to_lat / pg_h3_to_lng
            let mut fc = FunctionCallInfoBaseData::with_args(&[NullableDatum {
                value: int64_to_datum(valid_cell),
                isnull: false,
            }]);
            let lat_d = pg_h3_to_lat(&mut fc);
            assert!(!fc.isnull);
            assert!((datum_to_float8(lat_d) - 37.77).abs() < 0.2);

            let lng_d = pg_h3_to_lng(&mut fc);
            assert!(!fc.isnull);
            assert!((datum_to_float8(lng_d) - (-122.42)).abs() < 0.2);

            // 2. pg_h3_get_resolution
            let res_d = pg_h3_get_resolution(&mut fc);
            assert_eq!(datum_to_int64(res_d), 8);

            // 3. pg_h3_is_valid
            assert_eq!(pg_h3_is_valid(&mut fc), 1);

            // 4. pg_h3_cell_to_parent
            let mut fc_parent = FunctionCallInfoBaseData::with_args(&[
                NullableDatum {
                    value: int64_to_datum(valid_cell),
                    isnull: false,
                },
                NullableDatum {
                    value: int64_to_datum(5),
                    isnull: false,
                },
            ]);
            let parent_d = pg_h3_cell_to_parent(&mut fc_parent);
            assert!(!fc_parent.isnull);
            assert_eq!(
                pg_scalar_h3_get_resolution(datum_to_int64(parent_d) as u64),
                5
            );

            // 5. Null propagation on invalid / NULL argument
            let mut fc_null = FunctionCallInfoBaseData::with_args(&[NullableDatum {
                value: 0,
                isnull: true,
            }]);
            let _ = pg_h3_to_lat(&mut fc_null);
            assert!(fc_null.isnull);
        }
    }

    #[test]
    fn test_pg_continuous_and_categorical_srf_streaming() {
        let tif = write_test_tif(16, 16, 7.0);
        let path_str = tif.path().to_string_lossy().to_string();

        // Rust SRF iterator
        let srf = PgContinuousSrf::open(
            &path_str,
            vec![8],
            Some("EPSG:4326".to_string()),
            None,
            None,
        )
        .unwrap();
        let rows: Vec<PgContinuousRow> = srf.collect::<Result<Vec<_>>>().unwrap();
        assert!(!rows.is_empty());
        for r in &rows {
            assert_eq!(r.resolution, 8);
            assert!((r.mean - 7.0).abs() < 1e-6);
            assert_eq!(r.h3_hex.len(), 15);
            assert!(r.wkb.len() >= 109);
        }

        // Categorical SRF iterator
        let cat_srf = PgCategoricalSrf::open(
            &path_str,
            vec![8],
            Some("EPSG:4326".to_string()),
            None,
            None,
        )
        .unwrap();
        let cat_rows: Vec<PgCategoricalRow> = cat_srf.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(cat_rows.len(), rows.len());
        assert_eq!(cat_rows[0].majority_class, 7);

        // C-ABI SRF lifecycle
        unsafe {
            let path_c = CString::new(path_str).unwrap();
            let res_c = CString::new("8").unwrap();
            let crs_c = CString::new("EPSG:4326").unwrap();
            let mut err: *mut c_char = ptr::null_mut();
            let handle = pg_srf_continuous_open(
                path_c.as_ptr(),
                res_c.as_ptr(),
                crs_c.as_ptr(),
                0,
                0.0,
                ptr::null(),
                &mut err,
            );
            assert!(err.is_null());
            assert!(!handle.is_null());

            let mut c_row: PgCContinuousRow = std::mem::zeroed();
            let mut count = 0;
            while pg_srf_continuous_next(handle, &mut c_row) == 1 {
                assert_eq!(c_row.resolution, 8);
                assert!((c_row.mean - 7.0).abs() < 1e-6);
                assert!(c_row.wkb_len >= 109);
                count += 1;
            }
            assert_eq!(count, rows.len());
            pg_srf_continuous_close(handle);
        }
    }

    #[test]
    fn test_c_row_holds_full_pentagon_wkb() {
        // Odd-resolution pentagons have 10 vertices: the largest H3 polygon (189 bytes).
        let pentagon = h3o::Resolution::One.pentagons().next().unwrap();
        let mut buf = [0u8; WKB_BUF_LEN];
        let len = h3_index_to_wkb(u64::from(pentagon), &mut buf).unwrap();
        assert!(len > 128, "pentagon WKB is {len} bytes");

        let mut out: PgCContinuousRow = unsafe { std::mem::zeroed() };
        fill_c_wkb(&mut out, &buf[..len]);
        assert_eq!(out.wkb_len, len);
        assert_eq!(&out.wkb[..out.wkb_len], &buf[..len]);
    }
}
