//! Parsing of C-ABI arguments and the `options` JSON object into a [`PyScanConfig`].

use std::ffi::{c_char, c_int, CStr};
use std::ptr;

use raster_h3_core::aggregator::multi_horizon::{QuantileTarget, ScanPartition, SpectralFormula};
use raster_h3_core::aggregator::{CategoryRemapper, SamplingPattern};
use raster_h3_core::columnar::CategoricalFormat;
use raster_h3_core::error::{RasterH3Error, Result};
use raster_h3_core::raster::mosaic::OverlapRule;
use raster_h3_core::scalar_ops::{parse_bbox_str, try_parse_resolutions_str};

use crate::config::PyScanConfig;
use crate::ffi::opt_cstr;

/// H3 resolutions from a C string; NULL means the default `[8]`. An empty string is
/// rejected like any other list without a resolution.
///
/// # Safety
/// `ptr` must be null or a valid NUL-terminated string.
pub(crate) unsafe fn parse_c_resolutions(ptr: *const c_char) -> Result<Vec<u8>> {
    if ptr.is_null() {
        return Ok(vec![8]);
    }
    // SAFETY: `ptr` is non-null and NUL-terminated (caller contract).
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
    try_parse_resolutions_str(&s).ok_or_else(|| {
        RasterH3Error::InvalidParameter(format!(
            "Invalid resolutions '{s}': expected H3 resolutions between 0 and 15, e.g. 7,8"
        ))
    })
}

/// Categorical output format from a C string; NULL or empty means `wide`.
///
/// # Safety
/// `ptr` must be null or a valid NUL-terminated string.
pub(crate) unsafe fn parse_c_format(ptr: *const c_char) -> Result<CategoricalFormat> {
    // SAFETY: `ptr` is null or NUL-terminated (caller contract), as `opt_cstr` requires.
    match unsafe { opt_cstr(ptr) } {
        None => Ok(CategoricalFormat::Wide),
        Some(s) if s.eq_ignore_ascii_case("wide") => Ok(CategoricalFormat::Wide),
        Some(s) if s.eq_ignore_ascii_case("long") => Ok(CategoricalFormat::Long),
        Some(s) => Err(RasterH3Error::InvalidParameter(format!(
            "Invalid format '{s}': expected 'wide' or 'long'"
        ))),
    }
}

/// Like [`parse_bbox_str`], but also rejects non-numeric entries rather than skipping them.
fn parse_bbox_strict(s: &str) -> Option<[f64; 4]> {
    let all_numeric = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .all(|item| item.parse::<f64>().is_ok());
    if all_numeric {
        parse_bbox_str(s)
    } else {
        None
    }
}

/// # Safety
/// Every C string argument must be null or a valid NUL-terminated string.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn build_py_scan_config(
    path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    format: *const c_char,
    crs: *const c_char,
    has_nodata: c_int,
    nodata: f64,
    bbox: *const c_char,
    columns: *const c_char,
    limit: i64,
) -> Result<PyScanConfig> {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract), as
    // `opt_cstr` requires.
    unsafe {
        let path_str = opt_cstr(path)
            .ok_or_else(|| RasterH3Error::InvalidParameter("Missing input raster path".into()))?;
        let res_vec = parse_c_resolutions(resolutions)?;
        let fmt = parse_c_format(format)?;
        let crs_opt = opt_cstr(crs);
        let nodata_opt = if has_nodata != 0 { Some(nodata) } else { None };
        let bbox_opt = opt_cstr(bbox)
            .map(|s| {
                parse_bbox_strict(&s).ok_or_else(|| {
                    RasterH3Error::InvalidParameter(format!(
                        "Invalid bbox '{s}': expected four numbers min_lon,min_lat,max_lon,max_lat"
                    ))
                })
            })
            .transpose()?;
        let cols_opt = opt_cstr(columns).map(|s| {
            s.split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
        });
        let limit_opt = if limit >= 0 {
            Some(limit as usize)
        } else {
            None
        };

        Ok(PyScanConfig {
            path: path_str,
            resolutions: res_vec,
            categorical: categorical != 0,
            format: fmt,
            crs: crs_opt,
            nodata: nodata_opt,
            bbox: bbox_opt,
            columns: cols_opt,
            batch_size: 2048,
            limit: limit_opt,
            ..Default::default()
        })
    }
}

/// Scan options beyond the positional C-ABI arguments, passed to the `_v2` entry points as
/// a JSON object. Every field is optional; unknown fields are rejected so a typo is an
/// error rather than a silently ignored option.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PyScanOptions {
    band: Option<i64>,
    sampling: Option<String>,
    pub(crate) overlap_rule: Option<String>,
    min_majority_fraction: Option<f64>,
    spectral_formula: Option<String>,
    nir_band: Option<i64>,
    red_band: Option<i64>,
    green_band: Option<i64>,
    blue_band: Option<i64>,
    swir_band: Option<i64>,
    quantiles: Option<String>,
    remap: Option<String>,
    min_count: Option<f64>,
    min_mean: Option<f64>,
    max_mean: Option<f64>,
}

/// Names accepted for `overlap_rule`, as in DuckDB's `overlap_rule :=`.
const OVERLAP_RULE_NAMES: &str = "cutline, first, average";
/// Names accepted for `spectral_formula`.
const SPECTRAL_FORMULA_NAMES: &str = "ndvi, ndwi, nbr, evi";

/// Core's [`OverlapRule::parse`] falls back to `Cutline` for unknown names; reject them
/// instead.
pub(crate) fn parse_overlap_rule_strict(name: &str) -> Result<OverlapRule> {
    match name.trim().to_ascii_lowercase().as_str() {
        "cutline" | "voronoi" | "nearest" | "first" | "priority" | "painter" | "average"
        | "mean" | "all" | "multi_temporal" => Ok(OverlapRule::parse(name)),
        _ => Err(RasterH3Error::InvalidParameter(format!(
            "Invalid overlap_rule '{name}': expected one of {OVERLAP_RULE_NAMES}"
        ))),
    }
}

/// Quantile targets in DuckDB's `quantiles :=` syntax (repeated targets are dropped).
pub(crate) fn parse_quantiles(spec: &str) -> Result<Vec<QuantileTarget>> {
    QuantileTarget::parse_list(spec)
}

/// A 1-indexed band number option, or `default` when unset.
fn band_option(name: &str, value: Option<i64>, default: usize) -> Result<usize> {
    match value {
        None => Ok(default),
        Some(b) if b >= 1 => usize::try_from(b)
            .map_err(|_| RasterH3Error::InvalidParameter(format!("{name} {b} is too large"))),
        Some(b) => Err(RasterH3Error::InvalidParameter(format!(
            "{name} must be at least 1 (bands are 1-indexed), got {b}"
        ))),
    }
}

/// Parse the `options` JSON of the `_v2` entry points (NULL or empty means none) and apply
/// it to `cfg`, whose `categorical` flag must already be set.
fn apply_scan_options(cfg: &mut PyScanConfig, options: Option<&str>) -> Result<()> {
    let Some(json) = options else {
        return Ok(());
    };
    let opts: PyScanOptions = serde_json::from_str(json)
        .map_err(|e| RasterH3Error::InvalidParameter(format!("Invalid scan options: {e}")))?;

    cfg.band = band_option("band", opts.band, 1)?;
    if let Some(name) = &opts.sampling {
        cfg.sampling = SamplingPattern::parse(name)?;
    }
    if let Some(name) = &opts.overlap_rule {
        cfg.overlap_rule = parse_overlap_rule_strict(name)?;
    }
    if let Some(frac) = opts.min_majority_fraction {
        if !cfg.categorical {
            return Err(RasterH3Error::InvalidParameter(
                "min_majority_fraction applies to categorical scans only".into(),
            ));
        }
        if !(0.0..=1.0).contains(&frac) {
            return Err(RasterH3Error::InvalidParameter(format!(
                "min_majority_fraction must be between 0 and 1, got {frac}"
            )));
        }
        cfg.min_majority_fraction = Some(frac);
    }
    if let Some(spec) = &opts.quantiles {
        if cfg.categorical {
            return Err(RasterH3Error::InvalidParameter(
                "quantiles apply to continuous scans only".into(),
            ));
        }
        cfg.quantiles = parse_quantiles(spec)?;
    }
    if let Some(spec) = &opts.remap {
        if !cfg.categorical {
            return Err(RasterH3Error::InvalidParameter(
                "remap applies to categorical scans only".into(),
            ));
        }
        cfg.remapper = Some(CategoryRemapper::parse(spec)?.into_arc());
    }
    cfg.min_count = opts.min_count;
    for (name, value) in [("min_mean", opts.min_mean), ("max_mean", opts.max_mean)] {
        if value.is_some() && cfg.categorical {
            return Err(RasterH3Error::InvalidParameter(format!(
                "{name} applies to continuous scans only"
            )));
        }
    }
    cfg.min_mean = opts.min_mean;
    cfg.max_mean = opts.max_mean;

    let band_args = [
        opts.nir_band,
        opts.red_band,
        opts.green_band,
        opts.blue_band,
        opts.swir_band,
    ];
    match &opts.spectral_formula {
        None => {
            if band_args.iter().any(Option::is_some) {
                return Err(RasterH3Error::InvalidParameter(
                    "nir_band, red_band, green_band, blue_band and swir_band need a \
                     spectral_formula"
                        .into(),
                ));
            }
        }
        Some(name) => {
            if cfg.categorical {
                return Err(RasterH3Error::InvalidParameter(
                    "spectral_formula applies to continuous scans only".into(),
                ));
            }
            // Defaults match DuckDB's `nir_band := 4, red_band := 3, ...`.
            let formula = SpectralFormula::parse(
                name,
                band_option("nir_band", opts.nir_band, 4)?,
                band_option("red_band", opts.red_band, 3)?,
                band_option("green_band", opts.green_band, 2)?,
                band_option("blue_band", opts.blue_band, 1)?,
                band_option("swir_band", opts.swir_band, 6)?,
            )
            .ok_or_else(|| {
                RasterH3Error::InvalidParameter(format!(
                    "Invalid spectral_formula '{name}': expected one of {SPECTRAL_FORMULA_NAMES}"
                ))
            })?;
            cfg.spectral_formula = Some(formula);
        }
    }
    Ok(())
}

/// [`build_py_scan_config`] plus the `_v2` `options` JSON.
///
/// # Safety
/// Every C string argument must be null or a valid NUL-terminated string.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn build_py_scan_config_v2(
    path: *const c_char,
    resolutions: *const c_char,
    categorical: c_int,
    format: *const c_char,
    crs: *const c_char,
    has_nodata: c_int,
    nodata: f64,
    bbox: *const c_char,
    columns: *const c_char,
    limit: i64,
    options: *const c_char,
) -> Result<PyScanConfig> {
    // SAFETY: every C string argument is null or NUL-terminated (caller contract).
    unsafe {
        let mut cfg = build_py_scan_config(
            path,
            resolutions,
            categorical,
            format,
            crs,
            has_nodata,
            nodata,
            bbox,
            columns,
            limit,
        )?;
        apply_scan_options(&mut cfg, opt_cstr(options).as_deref())?;
        Ok(cfg)
    }
}

/// A stand-in partition for code paths that only need "this scan is partitioned" (schema
/// and projection resolution), not a particular partition.
pub(crate) fn placeholder_partition() -> ScanPartition {
    ScanPartition {
        index: 0,
        count: 1,
        mosaic_fingerprint: 0,
        blocks: Vec::new(),
    }
}

/// Parse the partition JSON argument of the `_partitioned` entry points.
///
/// # Safety
/// `partition` must be null or a valid NUL-terminated string.
pub(crate) unsafe fn parse_c_partition(partition: *const c_char) -> Result<ScanPartition> {
    // SAFETY: null or NUL-terminated (caller contract), as `opt_cstr` requires.
    let json = unsafe { opt_cstr(partition) }
        .ok_or_else(|| RasterH3Error::InvalidParameter("Missing scan partition".into()))?;
    ScanPartition::from_json(&json)
}

/// The optional partition argument of the `_v2` entry points: NULL or empty scans the
/// whole raster.
///
/// # Safety
/// `partition` must be null or a valid NUL-terminated string.
pub(crate) unsafe fn parse_c_opt_partition(
    partition: *const c_char,
) -> Result<Option<ScanPartition>> {
    // SAFETY: null or NUL-terminated (caller contract), as `opt_cstr` requires.
    unsafe { opt_cstr(partition) }
        .map(|json| ScanPartition::from_json(&json))
        .transpose()
}
