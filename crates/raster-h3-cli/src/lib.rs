//! # `raster-h3-cli`
//!
//! Command-line pipeline utilities for [`raster_h3_core`], supporting:
//! - Binary Columnar IPC (`RH3IPC01`) streaming to `stdout` or file
//! - Newline-delimited JSON (`JSONEachRow` / NDJSON) streaming to `stdout` for ClickHouse
//!   executable table functions and shell pipelines
//! - Native Parquet / GeoParquet and PMTiles v3 exports
//! - Partitioned scans for shell fan-outs: [`plan_raster_partitions`], a partition per
//!   [`CliStreamOptions::partition`] scan, and [`merge_partition_files`]

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    plan_partitions, MultiCategoricalHorizonStreamer, MultiResolutionConfig,
    MultiScanHorizonStreamer, ScanPartition,
};
use raster_h3_core::columnar::ipc::{read_ipc_stream, write_ipc_stream, write_jsonl_stream};
use raster_h3_core::columnar::partitioned::{
    merge_partial_batches, PartitionedBatchReader, PARTIAL_STATE_COLUMN,
};
use raster_h3_core::columnar::{
    CategoricalBatchReader, CategoricalFormat, ColumnArray, ColumnSchema, ContinuousBatchReader,
    RecordBatch,
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
    /// Scan only this partition (from [`plan_raster_partitions`]). The output gains a
    /// nullable `partial_state` column; combine the partitions with [`merge_partition_files`].
    pub partition: Option<ScanPartition>,
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
            partition: None,
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

/// Report valid samples the coordinate transform could not place, on stderr
/// so streamed output stays clean.
fn warn_transform_failures(failures: u64) {
    if failures > 0 {
        eprintln!(
            "warning: {failures} valid sample(s) were dropped because their coordinate \
             transform failed or fell outside the valid latitude/longitude range; check the source CRS"
        );
    }
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

    if let Some(partition) = &opts.partition {
        config.partition = Some(partition.clone());
        return stream_partition(opts, &paths, &config, writer);
    }

    if !opts.categorical {
        let full_schema = ColumnSchema::continuous(&config.quantiles);
        let projection = resolve_projection_indices(&full_schema, &opts.columns)?;
        let mut reader =
            ContinuousBatchReader::open(&paths, &config, projection, opts.batch_size, opts.limit)?;
        let schema = reader.schema();
        let rows = match opts.output_format {
            StreamOutputFormat::Ipc => write_ipc_stream(writer, &schema, &mut reader),
            StreamOutputFormat::Jsonl => write_jsonl_stream(writer, &mut reader),
        }?;
        warn_transform_failures(reader.transform_failures());
        Ok(rows)
    } else {
        let full_schema = match opts.categorical_format {
            CategoricalFormat::Wide => ColumnSchema::categorical_wide(),
            CategoricalFormat::Long => ColumnSchema::categorical_long(),
        };
        let projection = resolve_projection_indices(&full_schema, &opts.columns)?;
        let mut reader = CategoricalBatchReader::open(
            &paths,
            &config,
            opts.categorical_format,
            projection,
            opts.batch_size,
            opts.limit,
        )?;
        let schema = reader.schema();
        let rows = match opts.output_format {
            StreamOutputFormat::Ipc => write_ipc_stream(writer, &schema, &mut reader),
            StreamOutputFormat::Jsonl => write_jsonl_stream(writer, &mut reader),
        }?;
        warn_transform_failures(reader.transform_failures());
        Ok(rows)
    }
}

/// Scan one partition (`config.partition`) to `writer`: the projected columns plus
/// `partial_state`. Only IPC output is mergeable, so other formats are refused.
fn stream_partition<W: Write>(
    opts: &CliStreamOptions,
    paths: &[PathBuf],
    config: &MultiResolutionConfig,
    writer: &mut W,
) -> Result<usize> {
    let refuse = |why: &str| {
        Err(RasterH3Error::InvalidParameter(format!(
            "--partition {why}"
        )))
    };
    if opts.output_format != StreamOutputFormat::Ipc {
        return refuse("needs `ipc` output: `merge` reads the partitions' IPC files");
    }
    if opts.limit.is_some() {
        return refuse("cannot be combined with --limit (a partition holds parts of cells)");
    }
    if opts.categorical && opts.categorical_format == CategoricalFormat::Long {
        return refuse("needs the wide categorical format");
    }
    // `partial_state` may be listed in --columns; it is always appended.
    let columns = opts.columns.as_ref().map(|cols| {
        cols.iter()
            .filter(|c| !c.eq_ignore_ascii_case(PARTIAL_STATE_COLUMN))
            .cloned()
            .collect::<Vec<_>>()
    });
    let mut reader = if opts.categorical {
        let projection = resolve_projection_indices(&ColumnSchema::categorical_wide(), &columns)?;
        PartitionedBatchReader::open_categorical(paths, config, projection, opts.batch_size)?
    } else {
        let full = ColumnSchema::continuous(&config.quantiles);
        let projection = resolve_projection_indices(&full, &columns)?;
        PartitionedBatchReader::open_continuous(paths, config, projection, opts.batch_size)?
    };
    let schema = reader.schema();
    let rows = write_ipc_stream(writer, &schema, &mut reader)?;
    warn_transform_failures(reader.transform_failures());
    Ok(rows)
}

/// Split a raster or mosaic scan into partitions of about `target_chunks` TIFF tiles or
/// strips each, reading only metadata. `source_crs` and `bbox` must be the ones the
/// partitions will be scanned with (a scan with another layout refuses the partition).
pub fn plan_raster_partitions(
    input_path: &str,
    target_chunks: usize,
    source_crs: Option<&str>,
    bbox: Option<[f64; 4]>,
) -> Result<Vec<ScanPartition>> {
    if target_chunks == 0 {
        return Err(RasterH3Error::InvalidParameter(
            "--target-chunks must be at least 1".into(),
        ));
    }
    let paths = resolve_raster_sources(input_path)?;
    // Open the mosaic exactly as the scans do, so the fingerprints match.
    let mosaic = MosaicReader::open(&paths, bbox, source_crs, OverlapRule::Cutline)?;
    plan_partitions(&mosaic, target_chunks)
}

/// Write a plan as JSON Lines, one partition spec per line (line `k` is partition `k`).
pub fn write_plan<W: Write>(plan: &[ScanPartition], writer: &mut W) -> Result<()> {
    for partition in plan {
        writeln!(writer, "{}", partition.to_json())?;
    }
    writer.flush()?;
    Ok(())
}

/// Read a partition spec: a JSON object, or `@FILE` holding one spec or a plan written by
/// [`write_plan`], from which `index` picks a line (required when it has several).
pub fn read_partition_spec(spec: &str, index: Option<usize>) -> Result<ScanPartition> {
    let Some(path) = spec.strip_prefix('@') else {
        if index.is_some() {
            return Err(RasterH3Error::InvalidParameter(
                "--partition-index needs --partition @PLAN_FILE".into(),
            ));
        }
        return ScanPartition::from_json(spec);
    };
    let text = std::fs::read_to_string(path).map_err(|e| {
        RasterH3Error::InvalidParameter(format!("cannot read partition file '{path}': {e}"))
    })?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let line = match (index, lines.as_slice()) {
        (None, [only]) => *only,
        (None, _) => {
            return Err(RasterH3Error::InvalidParameter(format!(
                "'{path}' holds {} partition specs: pick one with --partition-index",
                lines.len()
            )))
        }
        (Some(i), _) => *lines.get(i).ok_or_else(|| {
            RasterH3Error::InvalidParameter(format!(
                "--partition-index {i} is out of range: '{path}' holds {} partition specs",
                lines.len()
            ))
        })?,
    };
    ScanPartition::from_json(line)
}

/// Options for [`merge_partition_files`].
#[derive(Debug, Clone)]
pub struct CliMergeOptions {
    /// IPC files written by partitioned `ipc` scans of the same raster with the same options.
    pub inputs: Vec<PathBuf>,
    /// Categorical partitions (also inferred from categorical-only columns).
    pub categorical: bool,
    pub output_format: StreamOutputFormat,
    pub batch_size: usize,
}

/// Combine the partition files of a scan into its final rows: every file's final rows are
/// passed through (without `partial_state`), then the partial rows of all files are merged
/// per cell. The result equals an unpartitioned scan's rows, in another order. Only the
/// partial rows are held in memory. Returns the number of rows written.
pub fn merge_partition_files<W: Write>(opts: &CliMergeOptions, writer: &mut W) -> Result<usize> {
    let mut files: VecDeque<PathBuf> = opts.inputs.iter().cloned().collect();
    let first_path = files.pop_front().ok_or_else(|| {
        RasterH3Error::InvalidParameter("merge needs at least one partition file".into())
    })?;
    let (input_schema, first) = read_partition_file(&first_path)?;
    let state_idx = input_schema.index_of(PARTIAL_STATE_COLUMN).ok_or_else(|| {
        RasterH3Error::InvalidParameter(format!(
            "'{}' has no {PARTIAL_STATE_COLUMN} column: is it the output of a partitioned scan (ipc --partition)?",
            first_path.display()
        ))
    })?;
    let keep: Vec<usize> = (0..input_schema.fields.len())
        .filter(|&i| i != state_idx)
        .collect();
    let names: Vec<&str> = keep
        .iter()
        .map(|&i| input_schema.fields[i].name.as_str())
        .collect();

    let continuous = ColumnSchema::continuous(&[]);
    let wide = ColumnSchema::categorical_wide();
    let categorical_only = names.iter().any(|n| continuous.index_of(n).is_none());
    let categorical = opts.categorical || categorical_only;
    let base = if categorical { &wide } else { &continuous };
    let projection = names
        .iter()
        .map(|n| {
            base.index_of(n).ok_or_else(|| {
                RasterH3Error::InvalidParameter(format!(
                    "column '{n}' is not a {} scan column",
                    if categorical {
                        "categorical"
                    } else {
                        "continuous"
                    }
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let output_schema = Arc::new(input_schema.project(&keep)?);

    let mut merger = PartitionMerge {
        files,
        input_schema: Arc::clone(&input_schema),
        state_idx,
        keep,
        queue: first.into(),
        partials: Vec::new(),
        merged: VecDeque::new(),
        merge: Some(MergeSpec {
            categorical,
            projection,
            batch_size: opts.batch_size,
        }),
    };
    let rows = match opts.output_format {
        StreamOutputFormat::Ipc => write_ipc_stream(writer, &output_schema, &mut merger),
        StreamOutputFormat::Jsonl => write_jsonl_stream(writer, &mut merger),
    }?;
    Ok(rows)
}

fn read_partition_file(path: &Path) -> Result<(Arc<ColumnSchema>, Vec<RecordBatch>)> {
    let file = File::open(path).map_err(|e| {
        RasterH3Error::InvalidParameter(format!("cannot open '{}': {e}", path.display()))
    })?;
    read_ipc_stream(&mut BufReader::new(file)).map_err(|e| {
        RasterH3Error::InvalidParameter(format!(
            "'{}' is not a raster-h3-cli IPC stream: {e}",
            path.display()
        ))
    })
}

struct MergeSpec {
    categorical: bool,
    projection: Vec<usize>,
    batch_size: usize,
}

/// Yields every file's final rows, reading the files one at a time and keeping their
/// partial rows, then the merged partial rows.
struct PartitionMerge {
    files: VecDeque<PathBuf>,
    input_schema: Arc<ColumnSchema>,
    state_idx: usize,
    keep: Vec<usize>,
    /// Batches of the current file, not yet split.
    queue: VecDeque<RecordBatch>,
    partials: Vec<RecordBatch>,
    /// Merged output batches.
    merged: VecDeque<RecordBatch>,
    /// Taken when the merged rows are queued.
    merge: Option<MergeSpec>,
}

impl PartitionMerge {
    /// Queue the next output batch; `Ok(None)` when done.
    fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
        loop {
            if let Some(batch) = self.merged.pop_front() {
                return Ok(Some(batch));
            }
            if let Some(batch) = self.queue.pop_front() {
                if let Some(finals) = self.split(batch)? {
                    return Ok(Some(finals));
                }
                continue;
            }
            if let Some(path) = self.files.pop_front() {
                let (schema, batches) = read_partition_file(&path)?;
                if *schema != *self.input_schema {
                    return Err(RasterH3Error::InvalidParameter(format!(
                        "'{}' has other columns than the first partition file; merge the partitions of one scan",
                        path.display()
                    )));
                }
                self.queue = batches.into();
                continue;
            }
            let Some(spec) = self.merge.take() else {
                return Ok(None);
            };
            // The CLI's scans have no value filters or quantiles, so the default merge
            // settings match them; the resolutions come from the partial rows.
            let config = MultiResolutionConfig::new(vec![8]);
            let (_, merged) = merge_partial_batches(
                &std::mem::take(&mut self.partials),
                spec.categorical,
                &config,
                Some(&spec.projection),
                spec.batch_size,
            )?;
            self.merged = merged.into();
        }
    }

    /// Keep the partial rows of an input batch; return its final rows without
    /// `partial_state` (`None` when it has none). Rows are split into contiguous runs.
    fn split(&mut self, batch: RecordBatch) -> Result<Option<RecordBatch>> {
        let is_partial: Vec<bool> = match &batch.columns[self.state_idx] {
            ColumnArray::Binary { validity, .. } => match validity {
                Some(v) => v.clone(),
                None => vec![true; batch.num_rows],
            },
            _ => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "{PARTIAL_STATE_COLUMN} must be a Binary column"
                )))
            }
        };
        let mut finals: Vec<RecordBatch> = Vec::new();
        let mut start = 0;
        while start < batch.num_rows {
            let partial = is_partial[start];
            let len = is_partial[start..]
                .iter()
                .take_while(|&&p| p == partial)
                .count();
            let run = batch.slice(start, len);
            if partial {
                self.partials.push(run);
            } else {
                finals.push(run.project(&self.keep)?);
            }
            start += len;
        }
        Ok(match finals.len() {
            0 => None,
            1 => finals.pop(),
            _ => Some(concat_batches(finals)?),
        })
    }
}

impl Iterator for PartitionMerge {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_batch().transpose()
    }
}

/// Concatenate batches of one schema.
fn concat_batches(batches: Vec<RecordBatch>) -> Result<RecordBatch> {
    let schema = Arc::clone(&batches[0].schema);
    let columns = (0..schema.fields.len())
        .map(|c| {
            let mut parts = batches.iter().map(|b| b.columns[c].clone());
            let mut acc = parts.next().expect("at least one batch");
            for part in parts {
                append_column(&mut acc, part);
            }
            acc
        })
        .collect();
    RecordBatch::try_new(schema, columns)
}

fn append_column(acc: &mut ColumnArray, part: ColumnArray) {
    let (acc_len, part_len) = (acc.len(), part.len());
    fn join(a: &mut Option<Vec<bool>>, b: Option<Vec<bool>>, a_len: usize, b_len: usize) {
        if a.is_none() && b.is_none() {
            return;
        }
        let mut v = a.take().unwrap_or_else(|| vec![true; a_len]);
        v.extend(b.unwrap_or_else(|| vec![true; b_len]));
        *a = Some(v);
    }
    macro_rules! extend {
        ($($variant:ident),*) => {
            match (acc, part) {
                $((
                    ColumnArray::$variant { values, validity },
                    ColumnArray::$variant { values: more, validity: more_validity },
                ) => {
                    values.extend(more);
                    join(validity, more_validity, acc_len, part_len);
                })*
                _ => unreachable!("batches of one schema have equal column types"),
            }
        };
    }
    extend!(UInt64, Int64, UInt8, Float64, Boolean, Utf8, Binary);
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
    let failures = Arc::new(AtomicU64::new(0));
    config.transform_failure_counter = Some(Arc::clone(&failures));

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

    let rows = if !categorical {
        let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?;
        H3ParquetWriter::write_continuous_streamer_to_parquet(streamer, output_path, pq_cfg)
    } else {
        let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)?;
        H3ParquetWriter::write_categorical_streamer_to_parquet(streamer, output_path, pq_cfg)
    }
    .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
    warn_transform_failures(failures.load(Ordering::Relaxed));
    Ok(rows)
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
    let failures = Arc::new(AtomicU64::new(0));
    config.transform_failure_counter = Some(Arc::clone(&failures));

    let tiles = if !categorical {
        H3PmtilesTiler::process_raster_source_to_pmtiles(input_path, output_path, config)
    } else {
        H3PmtilesTiler::process_categorical_source_to_pmtiles(input_path, output_path, config)
    }
    .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
    warn_transform_failures(failures.load(Ordering::Relaxed));
    Ok(tiles)
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
    let mut partition_spec: Option<String> = None;
    let mut partition_index: Option<usize> = None;

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
            "--partition" => partition_spec = Some(value()?.to_string()),
            "--partition-index" => {
                let v = value()?;
                partition_index = Some(
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
    match (partition_spec, partition_index) {
        (Some(spec), index) => opts.partition = Some(read_partition_spec(&spec, index)?),
        (None, Some(_)) => {
            return Err(RasterH3Error::InvalidParameter(
                "--partition-index needs --partition @PLAN_FILE".into(),
            ))
        }
        (None, None) => {}
    }

    Ok((opts, output_file))
}

/// Arguments of the `plan` subcommand.
#[derive(Debug, Clone, PartialEq)]
pub struct CliPlanArgs {
    pub input_path: String,
    pub target_chunks: usize,
    pub source_crs: Option<String>,
    pub bbox: Option<[f64; 4]>,
    /// `None` = stdout.
    pub output: Option<String>,
}

/// Parse `plan <input> [--target-chunks N] [--crs CRS] [--bbox ...] [-o FILE]`.
pub fn parse_plan_args(args: &[String]) -> Result<CliPlanArgs> {
    let mut out = CliPlanArgs {
        input_path: String::new(),
        target_chunks: 256,
        source_crs: None,
        bbox: None,
        output: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let flag = arg.as_str();
        let mut value = || {
            iter.next()
                .map(String::as_str)
                .ok_or_else(|| RasterH3Error::InvalidParameter(format!("Missing value for {flag}")))
        };
        let invalid = |v: &str, expected: &str| {
            RasterH3Error::InvalidParameter(format!(
                "Invalid value '{v}' for {flag}: expected {expected}"
            ))
        };
        match flag {
            "-i" | "--input" => out.input_path = value()?.to_string(),
            "-o" | "--output" => {
                let v = value()?;
                out.output = (v != "-").then(|| v.to_string());
            }
            "--target-chunks" => {
                let v = value()?;
                out.target_chunks = v
                    .parse()
                    .ok()
                    .filter(|&n: &usize| n > 0)
                    .ok_or_else(|| invalid(v, "a positive integer"))?;
            }
            "--crs" | "--source-crs" => out.source_crs = Some(value()?.to_string()),
            "--bbox" => {
                let v = value()?;
                out.bbox = Some(
                    parse_bbox_str(v)
                        .ok_or_else(|| invalid(v, "min_lon,min_lat,max_lon,max_lat"))?,
                );
            }
            _ if flag.starts_with('-') && flag != "-" => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Unknown option '{flag}' for plan"
                )));
            }
            _ if out.input_path.is_empty() => out.input_path = flag.to_string(),
            _ => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Unexpected argument '{flag}' (input path already set to '{}')",
                    out.input_path
                )));
            }
        }
    }
    if out.input_path.is_empty() {
        return Err(RasterH3Error::InvalidParameter(
            "Missing required input raster path".into(),
        ));
    }
    Ok(out)
}

/// Parse `merge <part>... [-c] [--jsonl] [-o FILE]` into options and an output path
/// (`None` = stdout).
pub fn parse_merge_args(args: &[String]) -> Result<(CliMergeOptions, Option<String>)> {
    let mut opts = CliMergeOptions {
        inputs: Vec::new(),
        categorical: false,
        output_format: StreamOutputFormat::Ipc,
        batch_size: 2048,
    };
    let mut output = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-o" | "--output" => {
                let v = iter.next().ok_or_else(|| {
                    RasterH3Error::InvalidParameter(format!("Missing value for {arg}"))
                })?;
                output = (v != "-").then(|| v.clone());
            }
            "-c" | "--categorical" => opts.categorical = true,
            "--jsonl" => opts.output_format = StreamOutputFormat::Jsonl,
            flag if flag.starts_with('-') => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Unknown option '{flag}' for merge"
                )));
            }
            path => opts.inputs.push(PathBuf::from(path)),
        }
    }
    if opts.inputs.is_empty() {
        return Err(RasterH3Error::InvalidParameter(
            "merge needs at least one partition file".into(),
        ));
    }
    Ok((opts, output))
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
