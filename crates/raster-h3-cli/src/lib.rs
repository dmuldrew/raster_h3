//! # `raster-h3-cli`
//!
//! Command-line pipeline utilities for [`raster_h3_core`], supporting:
//! - Binary Columnar IPC (`RH3IPC01`) streaming to `stdout` or file
//! - Newline-delimited JSON (`JSONEachRow` / NDJSON) streaming to `stdout` for ClickHouse
//!   executable table functions and shell pipelines
//! - Native Parquet / GeoParquet and PMTiles v3 exports

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::columnar::ipc::{write_ipc_stream, write_jsonl_stream};
use raster_h3_core::columnar::{
    CategoricalBatchReader, CategoricalFormat, ColumnSchema, ContinuousBatchReader,
};
use raster_h3_core::error::{RasterH3Error, Result};
use raster_h3_core::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3_core::pmtiles::H3PmtilesTiler;
use raster_h3_core::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};
use raster_h3_core::scalar_ops::{parse_bbox_str, try_parse_resolutions_str};

/// Output wire format for streaming CLI queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutputFormat {
    /// Binary columnar IPC stream (`RH3IPC01`).
    Ipc,
    /// Newline-delimited JSON (`JSONEachRow`), compatible with ClickHouse executable table functions.
    Jsonl,
}

/// Configuration for streaming a raster aggregation to a writer (`stdout` or file).
#[derive(Debug, Clone)]
pub struct CliStreamOptions {
    pub input_path: String,
    pub resolutions: Vec<u8>,
    pub categorical: bool,
    pub categorical_format: CategoricalFormat,
    pub source_crs: Option<String>,
    pub nodata: Option<f64>,
    /// Calibrate stored values with GDAL band scale/offset (`--raw-values` disables).
    pub apply_scale_offset: bool,
    pub bbox: Option<[f64; 4]>,
    pub columns: Option<Vec<String>>,
    pub batch_size: usize,
    pub limit: Option<usize>,
    pub output_format: StreamOutputFormat,
}

impl Default for CliStreamOptions {
    fn default() -> Self {
        Self {
            input_path: String::new(),
            resolutions: vec![8],
            categorical: false,
            categorical_format: CategoricalFormat::Wide,
            source_crs: None,
            nodata: None,
            apply_scale_offset: true,
            bbox: None,
            columns: None,
            batch_size: 2048,
            limit: None,
            output_format: StreamOutputFormat::Ipc,
        }
    }
}

fn resolve_projection_indices(
    schema: &ColumnSchema,
    columns: &Option<Vec<String>>,
) -> Result<Option<Vec<usize>>> {
    let Some(cols) = columns else {
        return Ok(None);
    };
    if cols.is_empty() {
        return Ok(None);
    }
    let mut indices = Vec::with_capacity(cols.len());
    for name in cols {
        let idx = schema.index_of(name).ok_or_else(|| {
            RasterH3Error::InvalidParameter(format!(
                "Unknown column name in projection: '{}'",
                name
            ))
        })?;
        indices.push(idx);
    }
    Ok(Some(indices))
}

/// Stream a GeoTIFF/COG aggregation directly to `writer` in binary IPC or JSONL format.
/// Returns the total number of rows emitted.
pub fn stream_raster_to_writer<W: Write>(opts: &CliStreamOptions, writer: &mut W) -> Result<usize> {
    let paths = resolve_raster_sources(&opts.input_path)?;
    let mut config = MultiResolutionConfig::new(opts.resolutions.clone());
    config.custom_crs = opts.source_crs.clone();
    config.custom_nodata = opts.nodata;
    config.apply_scale_offset = opts.apply_scale_offset;
    config.bbox = opts.bbox;
    config.overlap_rule = OverlapRule::Cutline;

    if !opts.categorical {
        let full_schema = ColumnSchema::continuous(&config.quantiles);
        let projection = resolve_projection_indices(&full_schema, &opts.columns)?;
        let reader =
            ContinuousBatchReader::open(&paths, &config, projection, opts.batch_size, opts.limit)?;
        let schema = reader.schema();
        match opts.output_format {
            StreamOutputFormat::Ipc => write_ipc_stream(writer, &schema, reader),
            StreamOutputFormat::Jsonl => write_jsonl_stream(writer, reader),
        }
    } else {
        let full_schema = match opts.categorical_format {
            CategoricalFormat::Wide => ColumnSchema::categorical_wide(),
            CategoricalFormat::Long => ColumnSchema::categorical_long(),
        };
        let projection = resolve_projection_indices(&full_schema, &opts.columns)?;
        let reader = CategoricalBatchReader::open(
            &paths,
            &config,
            opts.categorical_format,
            projection,
            opts.batch_size,
            opts.limit,
        )?;
        let schema = reader.schema();
        match opts.output_format {
            StreamOutputFormat::Ipc => write_ipc_stream(writer, &schema, reader),
            StreamOutputFormat::Jsonl => write_jsonl_stream(writer, reader),
        }
    }
}

/// Export a raster directly to a Parquet / GeoParquet file.
#[allow(clippy::too_many_arguments)]
pub fn export_raster_to_parquet(
    input_path: &str,
    output_path: &Path,
    resolutions: Vec<u8>,
    categorical: bool,
    source_crs: Option<String>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    geoparquet: bool,
    apply_scale_offset: bool,
) -> Result<usize> {
    let paths = resolve_raster_sources(input_path)?;
    let mut config = MultiResolutionConfig::new(resolutions);
    config.custom_crs = source_crs.clone();
    config.custom_nodata = nodata;
    config.apply_scale_offset = apply_scale_offset;
    config.bbox = bbox;

    let mosaic = Arc::new(MosaicReader::open(
        &paths,
        bbox,
        source_crs.as_deref(),
        OverlapRule::Cutline,
    )?);

    let pq_cfg = ParquetExportConfig {
        geoparquet,
        is_categorical: categorical,
        ..Default::default()
    };

    if !categorical {
        let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?;
        H3ParquetWriter::write_continuous_streamer_to_parquet(streamer, output_path, pq_cfg)
            .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
    } else {
        let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)?;
        H3ParquetWriter::write_categorical_streamer_to_parquet(streamer, output_path, pq_cfg)
            .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
    }
}

/// Export a raster directly to a PMTiles v3 archive.
#[allow(clippy::too_many_arguments)]
pub fn export_raster_to_pmtiles(
    input_path: &str,
    output_path: &Path,
    resolutions: Vec<u8>,
    categorical: bool,
    source_crs: Option<String>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    apply_scale_offset: bool,
) -> Result<usize> {
    let mut config = MultiResolutionConfig::new(resolutions);
    config.custom_crs = source_crs;
    config.custom_nodata = nodata;
    config.apply_scale_offset = apply_scale_offset;
    config.bbox = bbox;

    if !categorical {
        H3PmtilesTiler::process_raster_source_to_pmtiles(input_path, output_path, config)
            .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
    } else {
        H3PmtilesTiler::process_categorical_source_to_pmtiles(input_path, output_path, config)
            .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
    }
}

/// Parse CLI arguments into [`CliStreamOptions`] and an optional output file path (`None` = stdout).
///
/// Unknown flags, missing flag values, malformed numbers/lists and extra positional
/// arguments are rejected rather than silently falling back to defaults.
pub fn parse_stream_args(
    args: &[String],
    default_format: StreamOutputFormat,
) -> Result<(CliStreamOptions, Option<String>)> {
    let mut opts = CliStreamOptions {
        output_format: default_format,
        ..Default::default()
    };
    let mut output_file: Option<String> = None;

    let invalid = |flag: &str, value: &str, expected: &str| {
        RasterH3Error::InvalidParameter(format!(
            "Invalid value '{value}' for {flag}: expected {expected}"
        ))
    };

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let flag = arg.as_str();
        let mut value = || {
            iter.next()
                .map(String::as_str)
                .ok_or_else(|| RasterH3Error::InvalidParameter(format!("Missing value for {flag}")))
        };
        match flag {
            "-i" | "--input" => opts.input_path = value()?.to_string(),
            "-o" | "--output" => {
                let v = value()?;
                output_file = (v != "-").then(|| v.to_string());
            }
            "-r" | "--resolution" | "--resolutions" => {
                let v = value()?;
                opts.resolutions = try_parse_resolutions_str(v)
                    .ok_or_else(|| invalid(flag, v, "H3 resolutions between 0 and 15, e.g. 7,8"))?;
            }
            "-c" | "--categorical" => opts.categorical = true,
            "--format" => {
                let v = value()?;
                opts.categorical_format = if v.eq_ignore_ascii_case("long") {
                    CategoricalFormat::Long
                } else if v.eq_ignore_ascii_case("wide") {
                    CategoricalFormat::Wide
                } else {
                    return Err(invalid(flag, v, "'wide' or 'long'"));
                };
            }
            "--crs" | "--source-crs" => opts.source_crs = Some(value()?.to_string()),
            "--nodata" => {
                let v = value()?;
                opts.nodata = Some(v.parse().map_err(|_| invalid(flag, v, "a number"))?);
            }
            "--raw-values" => opts.apply_scale_offset = false,
            "--bbox" => {
                let v = value()?;
                opts.bbox = Some(
                    parse_bbox_str(v)
                        .ok_or_else(|| invalid(flag, v, "min_lon,min_lat,max_lon,max_lat"))?,
                );
            }
            "--columns" => {
                let v = value()?;
                let cols: Vec<String> = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if cols.is_empty() {
                    return Err(invalid(flag, v, "a comma-separated column list"));
                }
                opts.columns = Some(cols);
            }
            "--limit" => {
                let v = value()?;
                opts.limit = Some(
                    v.parse()
                        .map_err(|_| invalid(flag, v, "a non-negative integer"))?,
                );
            }
            _ if flag.starts_with('-') && flag != "-" => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Unknown option '{flag}'"
                )));
            }
            _ if opts.input_path.is_empty() => opts.input_path = flag.to_string(),
            _ => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Unexpected argument '{flag}' (input path already set to '{}')",
                    opts.input_path
                )));
            }
        }
    }

    if opts.input_path.is_empty() {
        return Err(RasterH3Error::InvalidParameter(
            "Missing required input raster path".into(),
        ));
    }

    Ok((opts, output_file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use raster_h3_core::columnar::ipc::read_ipc_stream;
    use std::fs::File;
    use tempfile::NamedTempFile;
    use tiff::encoder::colortype::Gray32Float;
    use tiff::encoder::TiffEncoder;

    fn write_test_tif(width: u32, height: u32, val: f32) -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let file = File::create(tmp.path()).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut img = encoder.new_image::<Gray32Float>(width, height).unwrap();
        // PixelScale tag (33550) and ModelTiepoint tag (33922) for WGS84 around (-122.4, 37.75)
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
    fn test_cli_ipc_and_jsonl_streaming_with_projection() {
        let tif = write_test_tif(16, 16, 42.0);
        let args = vec![
            tif.path().to_string_lossy().to_string(),
            "--resolution".to_string(),
            "8".to_string(),
            "--crs".to_string(),
            "EPSG:4326".to_string(),
            "--columns".to_string(),
            "h3_index,h3_hex,mean,count".to_string(),
        ];
        let (opts, out_file) = parse_stream_args(&args, StreamOutputFormat::Ipc).unwrap();
        assert!(out_file.is_none());

        let mut ipc_buf = Vec::new();
        let rows = stream_raster_to_writer(&opts, &mut ipc_buf).unwrap();
        assert!(rows > 0);

        let mut cursor = std::io::Cursor::new(ipc_buf);
        let (schema, batches) = read_ipc_stream(&mut cursor).unwrap();
        assert_eq!(schema.fields.len(), 4);
        assert_eq!(schema.fields[0].name, "h3_index");
        assert_eq!(schema.fields[1].name, "h3_hex");
        assert_eq!(schema.fields[2].name, "mean");
        assert_eq!(schema.fields[3].name, "count");
        let total_decoded: usize = batches.iter().map(|b| b.num_rows).sum();
        assert_eq!(total_decoded, rows);

        // Also test JSONL mode (for ClickHouse executable table functions)
        let mut jsonl_opts = opts;
        jsonl_opts.output_format = StreamOutputFormat::Jsonl;
        jsonl_opts.limit = Some(2);
        let mut jsonl_buf = Vec::new();
        let jrows = stream_raster_to_writer(&jsonl_opts, &mut jsonl_buf).unwrap();
        assert!(jrows <= 2 && jrows > 0);
        let text = String::from_utf8(jsonl_buf).unwrap();
        assert_eq!(text.lines().count(), jrows);
        assert!(text.contains("\"mean\":42.0"));
    }

    #[test]
    fn test_cli_rejects_invalid_arguments() {
        let parse = |extra: &[&str]| {
            let mut args = vec!["in.tif".to_string()];
            args.extend(extra.iter().map(|s| s.to_string()));
            parse_stream_args(&args, StreamOutputFormat::Ipc)
        };

        let (opts, out) =
            parse(&["-r", "7,8", "--bbox", "1,2,3,4", "--limit", "5", "-o", "x"]).unwrap();
        assert_eq!(opts.resolutions, vec![7, 8]);
        assert_eq!(opts.bbox, Some([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(opts.limit, Some(5));
        assert_eq!(out.as_deref(), Some("x"));
        assert!(opts.apply_scale_offset);
        assert!(!parse(&["--raw-values"]).unwrap().0.apply_scale_offset);

        for bad in [
            &["--bbox", "10,20,30"][..],
            &["-r", "16"],
            &["-r", "8,abc"],
            &["--nodata", "abc"],
            &["--limit", "-1"],
            &["--format", "tall"],
            &["--limt", "10"],
            &["--bbox"],
            &["second.tif"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(parse_stream_args(&[], StreamOutputFormat::Ipc).is_err());
    }
}
