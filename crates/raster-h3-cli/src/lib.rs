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
use raster_h3_core::scalar_ops::{parse_bbox_str, parse_resolutions_str};

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
            RasterH3Error::InvalidParameter(format!("Unknown column name in projection: '{}'", name))
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
    config.bbox = opts.bbox;
    config.overlap_rule = OverlapRule::Cutline;

    if !opts.categorical {
        let full_schema = ColumnSchema::continuous(&config.quantiles);
        let projection = resolve_projection_indices(&full_schema, &opts.columns)?;
        let reader = ContinuousBatchReader::open(
            &paths,
            &config,
            projection,
            opts.batch_size,
            opts.limit,
        )?;
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
pub fn export_raster_to_parquet(
    input_path: &str,
    output_path: &Path,
    resolutions: Vec<u8>,
    categorical: bool,
    source_crs: Option<String>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
    geoparquet: bool,
) -> Result<usize> {
    let paths = resolve_raster_sources(input_path)?;
    let mut config = MultiResolutionConfig::new(resolutions);
    config.custom_crs = source_crs.clone();
    config.custom_nodata = nodata;
    config.bbox = bbox;

    let mosaic = Arc::new(MosaicReader::open(
        &paths,
        bbox,
        source_crs.as_deref(),
        OverlapRule::Cutline,
    )?);

    let mut pq_cfg = ParquetExportConfig::default();
    pq_cfg.geoparquet = geoparquet;
    pq_cfg.is_categorical = categorical;

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
pub fn export_raster_to_pmtiles(
    input_path: &str,
    output_path: &Path,
    resolutions: Vec<u8>,
    categorical: bool,
    source_crs: Option<String>,
    nodata: Option<f64>,
    bbox: Option<[f64; 4]>,
) -> Result<usize> {
    let mut config = MultiResolutionConfig::new(resolutions);
    config.custom_crs = source_crs;
    config.custom_nodata = nodata;
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
pub fn parse_stream_args(
    args: &[String],
    default_format: StreamOutputFormat,
) -> Result<(CliStreamOptions, Option<String>)> {
    let mut opts = CliStreamOptions {
        output_format: default_format,
        ..Default::default()
    };
    let mut output_file: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--input" => {
                i += 1;
                if i < args.len() {
                    opts.input_path = args[i].clone();
                }
            }
            "-o" | "--output" => {
                i += 1;
                if i < args.len() && args[i] != "-" {
                    output_file = Some(args[i].clone());
                }
            }
            "-r" | "--resolution" | "--resolutions" => {
                i += 1;
                if i < args.len() {
                    let parsed = parse_resolutions_str(&args[i]);
                    if !parsed.is_empty() {
                        opts.resolutions = parsed;
                    }
                }
            }
            "-c" | "--categorical" => {
                opts.categorical = true;
            }
            "--format" => {
                i += 1;
                if i < args.len() && args[i].eq_ignore_ascii_case("long") {
                    opts.categorical_format = CategoricalFormat::Long;
                }
            }
            "--crs" | "--source-crs" => {
                i += 1;
                if i < args.len() {
                    opts.source_crs = Some(args[i].clone());
                }
            }
            "--nodata" => {
                i += 1;
                if i < args.len() {
                    opts.nodata = args[i].parse().ok();
                }
            }
            "--bbox" => {
                i += 1;
                if i < args.len() {
                    opts.bbox = parse_bbox_str(&args[i]);
                }
            }
            "--columns" => {
                i += 1;
                if i < args.len() {
                    let cols: Vec<String> = args[i]
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !cols.is_empty() {
                        opts.columns = Some(cols);
                    }
                }
            }
            "--limit" => {
                i += 1;
                if i < args.len() {
                    opts.limit = args[i].parse().ok();
                }
            }
            arg if !arg.starts_with('-') && opts.input_path.is_empty() => {
                opts.input_path = arg.to_string();
            }
            _ => {}
        }
        i += 1;
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
}
