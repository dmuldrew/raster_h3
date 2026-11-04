//! PMTiles v3 output for DataFusion queries: `COPY (query) TO 'cells.pmtiles'`.
//!
//! [`PmtilesFormatFactory`] registers the `pmtiles` file type (see
//! [`register_pmtiles_format`], called by [`crate::register_all`]), so DataFusion and
//! SedonaDB SQL can write the rows of any query as one hexagon feature per row:
//!
//! ```sql
//! COPY (
//!     SELECT h3_index, mean, count FROM h3_raster_continuous_aggregate('dem.tif', 8)
//!     WHERE count >= 4
//! ) TO 'dem.pmtiles' OPTIONS (min_zoom 5, max_zoom 11);
//! ```
//!
//! - The H3 column is `h3_column` if given, else the first of `h3_index`, `h3_hex`, `h3`,
//!   `cell` or `hex`, else the first column. It may be `UInt64`, `Int64` (the same 64 bits,
//!   as PostgreSQL and Spark store it) or a hex string. Rows where it is NULL are skipped;
//!   any other value that is not a valid H3 cell fails the write.
//! - Every other column becomes a feature property under its own name. Integers, floats,
//!   booleans and strings are written natively; binary columns (such as the `wkb`
//!   geometry) are left out, since the feature already is the cell's polygon; other types
//!   (dates, decimals, ...) are written as their string form. NULL values are omitted.
//! - Options: `h3_column`, `min_zoom` / `max_zoom` (keep only cells whose PMTiles zoom,
//!   derived from their H3 resolution, is in range) and `memory_budget_mb` (default 256).
//!
//! Rows go through [`ExternalFeatureExport`], which buffers up to the memory budget and
//! external-sorts beyond it, so any number of rows in any order is written with bounded
//! memory. The archive is built on a blocking thread, then uploaded through the session's
//! object store (local paths, or `s3://` and others registered with the runtime).
//! `COPY` returns the number of features written.
//!
//! [`write_pmtiles`] does the same for a [`DataFrame`], and [`write_record_batches`] for
//! any iterator of record batches (it backs the Python bindings).

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use arrow::compute::{can_cast_types, cast};
use arrow::datatypes::{
    DataType, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, Schema,
    SchemaRef, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
};
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::common::{config_err, not_impl_err, plan_err, GetExt};
use datafusion::dataframe::DataFrame;
use datafusion::datasource::file_format::file_compression_type::FileCompressionType;
use datafusion::datasource::file_format::{format_as_file_type, FileFormat, FileFormatFactory};
use datafusion::datasource::physical_plan::{
    ArrowSource, FileScanConfig, FileSinkConfig, FileSource,
};
use datafusion::datasource::sink::{DataSink, DataSinkExec};
use datafusion::datasource::table_schema::TableSchema;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::logical_expr::LogicalPlanBuilder;
use datafusion::object_store::path::Path as ObjectPath;
use datafusion::object_store::{ObjectMeta, ObjectStore, ObjectStoreExt, WriteMultipart};
use datafusion::physical_expr::LexRequirement;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan};
use datafusion::prelude::SessionContext;
use futures::StreamExt;
use raster_h3_core::pmtiles::features::H3Feature;
use raster_h3_core::pmtiles::mvt::MvtValue;
use raster_h3_core::pmtiles::pyramid::h3_res_to_zoom;
use raster_h3_core::pmtiles::{
    ExternalFeatureExport, PmtilesExportSummary, DEFAULT_EXPORT_MEMORY_BUDGET,
};
use raster_h3_core::scalar_ops::{h3_get_resolution, h3_is_valid_u64, string_to_h3};

/// The file extension, and the name `STORED AS` takes (case-insensitive).
pub const PMTILES_EXTENSION: &str = "pmtiles";

/// Column names tried, in order, when no `h3_column` is given.
const H3_COLUMN_NAMES: [&str; 5] = ["h3_index", "h3_hex", "h3", "cell", "hex"];

/// Largest accepted `memory_budget_mb` (1 TiB), so the byte count cannot overflow.
const MAX_MEMORY_BUDGET_MB: usize = 1 << 20;

/// Highest accepted zoom option. H3 resolution 15 maps to zoom 20.
const MAX_ZOOM_OPTION: u8 = 30;

/// Upload chunk size; above S3's 5 MiB minimum part size.
const UPLOAD_CHUNK_BYTES: usize = 16 << 20;

/// Options of a PMTiles write. `COPY ... OPTIONS (...)` sets them by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmtilesWriteOptions {
    /// The H3 column; `None` picks the first of `h3_index`, `h3_hex`, `h3`, `cell`, `hex`,
    /// or the first column.
    pub h3_column: Option<String>,
    /// Drop cells whose PMTiles zoom (from their H3 resolution) is below this.
    pub min_zoom: Option<u8>,
    /// Drop cells whose PMTiles zoom is above this.
    pub max_zoom: Option<u8>,
    /// Bytes of rows buffered in memory before sorted runs spill to temporary files.
    pub memory_budget_bytes: usize,
}

impl Default for PmtilesWriteOptions {
    fn default() -> Self {
        Self {
            h3_column: None,
            min_zoom: None,
            max_zoom: None,
            memory_budget_bytes: DEFAULT_EXPORT_MEMORY_BUDGET,
        }
    }
}

impl PmtilesWriteOptions {
    /// Parse `COPY` options: `format.h3_column`, `format.min_zoom`, `format.max_zoom` and
    /// `format.memory_budget_mb` (DataFusion adds the `format.` prefix to un-namespaced
    /// keys). Other namespaces (such as `execution.`) belong to DataFusion and are ignored;
    /// unknown `format.` keys are refused.
    pub fn from_options(options: &HashMap<String, String>) -> Result<Self> {
        let mut out = Self::default();
        for (key, value) in options {
            let Some(name) = key.strip_prefix("format.") else {
                continue;
            };
            let value = value.trim();
            let number = |max: usize| -> Result<usize> {
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|&v| v <= max)
                    .ok_or_else(|| {
                        DataFusionError::Configuration(format!(
                            "PMTiles option '{name}' must be an integer between 0 and {max}, got '{value}'"
                        ))
                    })
            };
            match name {
                "h3_column" | "h3_col" => out.h3_column = Some(value.to_string()),
                "min_zoom" => out.min_zoom = Some(number(MAX_ZOOM_OPTION.into())? as u8),
                "max_zoom" => out.max_zoom = Some(number(MAX_ZOOM_OPTION.into())? as u8),
                "memory_budget_mb" => {
                    let mb = number(MAX_MEMORY_BUDGET_MB)?;
                    if mb == 0 {
                        return config_err!("PMTiles option 'memory_budget_mb' must be at least 1");
                    }
                    out.memory_budget_bytes = mb << 20;
                }
                _ => {
                    return config_err!(
                        "Unknown PMTiles option '{name}'; expected h3_column, min_zoom, max_zoom or memory_budget_mb"
                    )
                }
            }
        }
        out.validate()?;
        Ok(out)
    }

    fn validate(&self) -> Result<()> {
        if let (Some(min), Some(max)) = (self.min_zoom, self.max_zoom) {
            if min > max {
                return config_err!(
                    "PMTiles min_zoom ({min}) must not be greater than max_zoom ({max})"
                );
            }
        }
        Ok(())
    }

    fn keeps_zoom(&self, zoom: u8) -> bool {
        self.min_zoom.is_none_or(|z| zoom >= z) && self.max_zoom.is_none_or(|z| zoom <= z)
    }
}

/// How the H3 column is read.
#[derive(Debug, Clone, Copy)]
enum H3Kind {
    UInt64,
    Int64,
    /// Hex strings (any string type; dictionaries are cast to `Utf8` per batch).
    Hex,
}

/// How a property column becomes an MVT value.
#[derive(Debug, Clone, Copy)]
enum PropKind {
    Int,
    UInt,
    Float,
    Double,
    Bool,
    /// Native string types, read without casting.
    String,
    /// Other types, cast to `Utf8` per batch.
    CastToString,
}

impl PropKind {
    fn of(data_type: &DataType) -> Option<Self> {
        Some(match data_type {
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => Self::Int,
            DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => Self::UInt,
            DataType::Float32 => Self::Float,
            DataType::Float16 | DataType::Float64 => Self::Double,
            DataType::Boolean => Self::Bool,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Self::String,
            // The cell polygon is the feature's geometry; WKB and other bytes are left out.
            DataType::Binary
            | DataType::LargeBinary
            | DataType::BinaryView
            | DataType::FixedSizeBinary(_)
            | DataType::Null => return None,
            other if can_cast_types(other, &DataType::Utf8) => Self::CastToString,
            _ => return None,
        })
    }

    /// TileJSON field type for the archive's `vector_layers` metadata.
    fn field_type(self) -> &'static str {
        match self {
            Self::Bool => "Boolean",
            Self::String | Self::CastToString => "String",
            _ => "Number",
        }
    }
}

/// Converts record batches of one schema into H3 features. Built once per write, so
/// schema problems are reported when the query is planned.
#[derive(Debug, Clone)]
pub struct FeatureEncoder {
    h3_index: usize,
    h3_kind: H3Kind,
    h3_name: String,
    properties: Vec<(usize, Cow<'static, str>, PropKind)>,
    options: PmtilesWriteOptions,
}

impl FeatureEncoder {
    /// Resolve the H3 column and the property columns of `schema`.
    pub fn try_new(schema: &Schema, options: PmtilesWriteOptions) -> Result<Self> {
        options.validate()?;
        let fields = schema.fields();
        if fields.is_empty() {
            return plan_err!("PMTiles output needs at least an H3 column");
        }
        let h3_index = match &options.h3_column {
            Some(name) => fields
                .iter()
                .position(|f| f.name() == name)
                .or_else(|| {
                    fields
                        .iter()
                        .position(|f| f.name().eq_ignore_ascii_case(name))
                })
                .ok_or_else(|| {
                    let names: Vec<&str> = fields.iter().map(|f| f.name().as_str()).collect();
                    DataFusionError::Plan(format!(
                        "PMTiles h3_column '{name}' not found; columns: {}",
                        names.join(", ")
                    ))
                })?,
            None => H3_COLUMN_NAMES
                .iter()
                .find_map(|candidate| {
                    fields
                        .iter()
                        .position(|f| f.name().eq_ignore_ascii_case(candidate))
                })
                .unwrap_or(0),
        };
        let h3_field = &fields[h3_index];
        let h3_kind = match h3_field.data_type() {
            DataType::UInt64 => H3Kind::UInt64,
            DataType::Int64 => H3Kind::Int64,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => H3Kind::Hex,
            DataType::Dictionary(_, value)
                if matches!(
                    value.as_ref(),
                    DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
                ) =>
            {
                H3Kind::Hex
            }
            other => {
                return plan_err!(
                "PMTiles H3 column '{}' has type {other}; expected UInt64, Int64 or a hex string \
                     (name another column with the h3_column option)",
                h3_field.name()
            )
            }
        };
        let properties = fields
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != h3_index)
            .filter_map(|(i, f)| {
                PropKind::of(f.data_type()).map(|kind| (i, Cow::Owned(f.name().clone()), kind))
            })
            .collect();
        Ok(Self {
            h3_index,
            h3_kind,
            h3_name: h3_field.name().clone(),
            properties,
            options,
        })
    }

    /// The property names and TileJSON types, for the archive metadata.
    pub fn layer_fields(&self) -> Vec<(String, &'static str)> {
        self.properties
            .iter()
            .map(|(_, name, kind)| (name.to_string(), kind.field_type()))
            .collect()
    }

    /// The write options this encoder applies.
    pub fn options(&self) -> &PmtilesWriteOptions {
        &self.options
    }

    /// Push the rows of `batch` into `exporter`; returns the number of features added
    /// (rows with a NULL H3 value or outside the zoom range are skipped).
    pub fn push_batch(
        &self,
        batch: &RecordBatch,
        exporter: &mut ExternalFeatureExport,
    ) -> Result<u64> {
        self.encode_batch(batch, |feature| {
            exporter
                .push(feature)
                .map_err(|e| DataFusionError::External(Box::new(e)))
        })
    }

    /// Convert the rows of `batch` to features, handing each to `sink`; returns the number
    /// of features produced (rows with a NULL H3 value or outside the zoom range are
    /// skipped).
    pub fn encode_batch(
        &self,
        batch: &RecordBatch,
        mut sink: impl FnMut(H3Feature) -> Result<()>,
    ) -> Result<u64> {
        let h3_col = batch.column(self.h3_index);
        let h3_strings = match self.h3_kind {
            H3Kind::Hex
                if !matches!(
                    h3_col.data_type(),
                    DataType::Utf8View | DataType::Utf8 | DataType::LargeUtf8
                ) =>
            {
                Some(cast(h3_col, &DataType::Utf8)?)
            }
            _ => None,
        };
        let h3_col = h3_strings.as_ref().unwrap_or(h3_col);
        let props: Vec<(Cow<'static, str>, PropKind, ArrayRef)> = self
            .properties
            .iter()
            .map(|(i, name, kind)| {
                let col = batch.column(*i);
                let col = match kind {
                    PropKind::CastToString => cast(col, &DataType::Utf8)?,
                    _ => Arc::clone(col),
                };
                Ok((name.clone(), *kind, col))
            })
            .collect::<Result<_>>()?;

        let mut added = 0;
        for row in 0..batch.num_rows() {
            let Some(cell) = self.cell_at(h3_col, row)? else {
                continue;
            };
            let zoom = h3_res_to_zoom(h3_get_resolution(cell) as u8);
            if !self.options.keeps_zoom(zoom) {
                continue;
            }
            let properties = props
                .iter()
                .filter_map(|(name, kind, col)| {
                    prop_value(col.as_ref(), *kind, row).map(|v| (name.clone(), v))
                })
                .collect();
            sink(H3Feature {
                h3_index: cell,
                properties,
            })?;
            added += 1;
        }
        Ok(added)
    }

    /// The valid H3 cell in `row`, `None` for NULL, an error for anything else.
    fn cell_at(&self, col: &ArrayRef, row: usize) -> Result<Option<u64>> {
        if col.is_null(row) {
            return Ok(None);
        }
        let (cell, shown) = match self.h3_kind {
            H3Kind::UInt64 => {
                let v = col.as_primitive::<UInt64Type>().value(row);
                (v, v.to_string())
            }
            H3Kind::Int64 => {
                let v = col.as_primitive::<Int64Type>().value(row);
                (v as u64, v.to_string())
            }
            H3Kind::Hex => {
                let s = string_at(col.as_ref(), row).unwrap_or_default();
                (string_to_h3(s.trim()), format!("'{s}'"))
            }
        };
        if h3_is_valid_u64(cell) {
            Ok(Some(cell))
        } else {
            Err(DataFusionError::Execution(format!(
                "PMTiles H3 column '{}': {shown} is not a valid H3 cell",
                self.h3_name
            )))
        }
    }
}

/// A string-typed array's value at `row`.
fn string_at(col: &dyn Array, row: usize) -> Option<&str> {
    match col.data_type() {
        DataType::Utf8 => Some(col.as_string::<i32>().value(row)),
        DataType::LargeUtf8 => Some(col.as_string::<i64>().value(row)),
        DataType::Utf8View => Some(col.as_string_view().value(row)),
        _ => None,
    }
}

/// The MVT value of `col[row]`, `None` when it is NULL.
fn prop_value(col: &dyn Array, kind: PropKind, row: usize) -> Option<MvtValue> {
    if col.is_null(row) {
        return None;
    }
    Some(match kind {
        PropKind::Int => MvtValue::Int(match col.data_type() {
            DataType::Int8 => col.as_primitive::<Int8Type>().value(row).into(),
            DataType::Int16 => col.as_primitive::<Int16Type>().value(row).into(),
            DataType::Int32 => col.as_primitive::<Int32Type>().value(row).into(),
            _ => col.as_primitive::<Int64Type>().value(row),
        }),
        PropKind::UInt => MvtValue::UInt(match col.data_type() {
            DataType::UInt8 => col.as_primitive::<UInt8Type>().value(row).into(),
            DataType::UInt16 => col.as_primitive::<UInt16Type>().value(row).into(),
            DataType::UInt32 => col.as_primitive::<UInt32Type>().value(row).into(),
            _ => col.as_primitive::<UInt64Type>().value(row),
        }),
        PropKind::Float => MvtValue::Float(col.as_primitive::<Float32Type>().value(row)),
        PropKind::Double => match col.data_type() {
            DataType::Float16 => MvtValue::Double(
                col.as_primitive::<arrow::datatypes::Float16Type>()
                    .value(row)
                    .to_f64(),
            ),
            _ => MvtValue::Double(col.as_primitive::<Float64Type>().value(row)),
        },
        PropKind::Bool => MvtValue::Bool(col.as_boolean().value(row)),
        PropKind::String | PropKind::CastToString => {
            MvtValue::String(string_at(col, row)?.to_string())
        }
    })
}

/// Write record batches to a local PMTiles file (atomically, via a temporary file next to
/// it). Returns the export summary; `valid_features` counts the features written.
pub fn write_record_batches<I>(
    schema: &Schema,
    batches: I,
    path: &Path,
    options: PmtilesWriteOptions,
) -> Result<PmtilesExportSummary>
where
    I: IntoIterator<Item = Result<RecordBatch>>,
{
    let encoder = FeatureEncoder::try_new(schema, options)?;
    let mut exporter = new_exporter(&encoder);
    for batch in batches {
        encoder.push_batch(&batch?, &mut exporter)?;
    }
    exporter
        .finish(path)
        .map_err(|e| DataFusionError::External(Box::new(e)))
}

fn new_exporter(encoder: &FeatureEncoder) -> ExternalFeatureExport {
    ExternalFeatureExport::new(encoder.options.memory_budget_bytes, None)
        .with_layer_fields(encoder.layer_fields())
}

/// Write a [`DataFrame`] to a PMTiles file through the same sink as `COPY ... TO`
/// (`path` may be any URL the session's object stores accept). Returns the number of
/// features written.
pub async fn write_pmtiles(df: DataFrame, path: &str, options: PmtilesWriteOptions) -> Result<u64> {
    let mut map = HashMap::new();
    if let Some(col) = &options.h3_column {
        map.insert("format.h3_column".to_string(), col.clone());
    }
    if let Some(z) = options.min_zoom {
        map.insert("format.min_zoom".to_string(), z.to_string());
    }
    if let Some(z) = options.max_zoom {
        map.insert("format.max_zoom".to_string(), z.to_string());
    }
    let mb = options.memory_budget_bytes.div_ceil(1 << 20).max(1);
    map.insert("format.memory_budget_mb".to_string(), mb.to_string());

    let (state, plan) = df.into_parts();
    let file_type = format_as_file_type(Arc::new(PmtilesFormatFactory));
    let plan =
        LogicalPlanBuilder::copy_to(plan, path.to_string(), file_type, map, Vec::new())?.build()?;
    let batches = DataFrame::new(state, plan).collect().await?;
    let count = batches
        .first()
        .filter(|b| b.num_rows() > 0)
        .and_then(|b| {
            b.column(0)
                .as_primitive_opt::<UInt64Type>()
                .map(|c| c.value(0))
        })
        .unwrap_or(0);
    Ok(count)
}

/// Register the `pmtiles` file type with `ctx`, so `COPY ... TO 'x.pmtiles'` (or
/// `STORED AS PMTILES`) writes PMTiles. An existing `pmtiles` registration is replaced.
pub fn register_pmtiles_format(ctx: &SessionContext) -> Result<()> {
    ctx.state_ref()
        .write()
        .register_file_format(Arc::new(PmtilesFormatFactory), true)
}

/// Creates [`PmtilesFormat`]s from `COPY` options.
#[derive(Debug, Default, Clone, Copy)]
pub struct PmtilesFormatFactory;

impl GetExt for PmtilesFormatFactory {
    fn get_ext(&self) -> String {
        PMTILES_EXTENSION.to_string()
    }
}

impl FileFormatFactory for PmtilesFormatFactory {
    fn create(
        &self,
        _state: &dyn Session,
        format_options: &HashMap<String, String>,
    ) -> Result<Arc<dyn FileFormat>> {
        Ok(Arc::new(PmtilesFormat {
            options: PmtilesWriteOptions::from_options(format_options)?,
        }))
    }

    fn default(&self) -> Arc<dyn FileFormat> {
        Arc::new(PmtilesFormat::default())
    }
}

/// The write-only PMTiles file format.
#[derive(Debug, Default, Clone)]
pub struct PmtilesFormat {
    options: PmtilesWriteOptions,
}

#[async_trait]
impl FileFormat for PmtilesFormat {
    fn get_ext(&self) -> String {
        PMTILES_EXTENSION.to_string()
    }

    fn get_ext_with_compression(&self, compression: &FileCompressionType) -> Result<String> {
        if compression.is_compressed() {
            return not_impl_err!(
                "PMTiles archives compress their own tiles; file compression is not supported"
            );
        }
        Ok(self.get_ext())
    }

    fn compression_type(&self) -> Option<FileCompressionType> {
        None
    }

    async fn infer_schema(
        &self,
        _state: &dyn Session,
        _store: &Arc<dyn ObjectStore>,
        _objects: &[ObjectMeta],
    ) -> Result<SchemaRef> {
        not_impl_err!("Reading PMTiles is not supported; PMTiles is an output format only")
    }

    async fn infer_stats(
        &self,
        _state: &dyn Session,
        _store: &Arc<dyn ObjectStore>,
        _table_schema: SchemaRef,
        _object: &ObjectMeta,
    ) -> Result<datafusion::common::Statistics> {
        not_impl_err!("Reading PMTiles is not supported; PMTiles is an output format only")
    }

    async fn create_physical_plan(
        &self,
        _state: &dyn Session,
        _conf: FileScanConfig,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        not_impl_err!("Reading PMTiles is not supported; PMTiles is an output format only")
    }

    async fn create_writer_physical_plan(
        &self,
        input: Arc<dyn ExecutionPlan>,
        _state: &dyn Session,
        conf: FileSinkConfig,
        order_requirements: Option<LexRequirement>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if !conf.table_partition_cols.is_empty() {
            return not_impl_err!(
                "PMTiles output is a single archive; PARTITIONED BY is not supported"
            );
        }
        let [url] = conf.table_paths.as_slice() else {
            return plan_err!("PMTiles output needs exactly one output path");
        };
        if !conf.file_output_mode.single_file_output(url) {
            return plan_err!(
                "PMTiles output is a single file; give a file path such as 'cells.pmtiles', not a directory ('{}')",
                conf.original_url
            );
        }
        let encoder = FeatureEncoder::try_new(conf.output_schema(), self.options.clone())?;
        let sink = Arc::new(PmtilesSink {
            config: conf,
            encoder: Arc::new(encoder),
        });
        Ok(Arc::new(DataSinkExec::new(input, sink, order_requirements)))
    }

    fn file_source(&self, table_schema: TableSchema) -> Arc<dyn FileSource> {
        // Never used to read: `infer_schema` and `create_physical_plan` refuse first.
        Arc::new(ArrowSource::new_file_source(table_schema))
    }
}

/// The [`DataSink`] behind `COPY ... TO '*.pmtiles'`.
pub struct PmtilesSink {
    config: FileSinkConfig,
    encoder: Arc<FeatureEncoder>,
}

impl fmt::Debug for PmtilesSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PmtilesSink")
            .field("url", &self.config.original_url)
            .finish()
    }
}

impl DisplayAs for PmtilesSink {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(
                    f,
                    "PmtilesSink(file={}, h3_column={})",
                    self.config.original_url, self.encoder.h3_name
                )
            }
            DisplayFormatType::TreeRender => {
                writeln!(f, "format: pmtiles")?;
                write!(f, "file={}", self.config.original_url)
            }
        }
    }
}

#[async_trait]
impl DataSink for PmtilesSink {
    fn schema(&self) -> &SchemaRef {
        self.config.output_schema()
    }

    async fn write_all(
        &self,
        mut data: SendableRecordBatchStream,
        context: &Arc<TaskContext>,
    ) -> Result<u64> {
        let store = context
            .runtime_env()
            .object_store(&self.config.object_store_url)?;
        let path = self.config.table_paths[0].prefix().clone();

        // Encoding, sorting and building the archive are CPU and disk bound: run them on a
        // blocking thread fed through a small channel, so the stream keeps flowing.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<RecordBatch>(2);
        let encoder = Arc::clone(&self.encoder);
        let worker = tokio::task::spawn_blocking(move || -> Result<(u64, File)> {
            let mut exporter = new_exporter(&encoder);
            let mut written = 0;
            while let Some(batch) = rx.blocking_recv() {
                written += encoder.push_batch(&batch, &mut exporter)?;
            }
            let mut file = tempfile::tempfile()?;
            {
                let mut out = BufWriter::new(&mut file);
                exporter
                    .finish_into(&mut out)
                    .map_err(|e| DataFusionError::External(Box::new(e)))?;
                out.flush()?;
            }
            file.seek(SeekFrom::Start(0))?;
            Ok((written, file))
        });

        while let Some(batch) = data.next().await {
            let batch = batch?;
            if tx.send(batch).await.is_err() {
                break; // the worker stopped early; its error is reported below
            }
        }
        drop(tx);
        let (written, file) = worker
            .await
            .map_err(|e| DataFusionError::ExecutionJoin(Box::new(e)))??;
        upload(file, store, &path).await?;
        Ok(written)
    }
}

/// Copy the finished archive to `path` in `store` as one multipart upload.
async fn upload(mut file: File, store: Arc<dyn ObjectStore>, path: &ObjectPath) -> Result<()> {
    let mut upload =
        WriteMultipart::new_with_chunk_size(store.put_multipart(path).await?, UPLOAD_CHUNK_BYTES);
    let mut buf = vec![0u8; UPLOAD_CHUNK_BYTES];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = upload.abort().await;
                return Err(e.into());
            }
        };
        if let Err(e) = upload.wait_for_capacity(4).await {
            let _ = upload.abort().await;
            return Err(e.into());
        }
        upload.write(&buf[..n]);
    }
    upload.finish().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{
        BinaryArray, BooleanArray, Date32Array, DictionaryArray, Float32Array, Float64Array,
        Int32Array, Int64Array, StringArray, StringViewArray, UInt64Array, UInt8Array,
    };
    use arrow::datatypes::{Field, Int32Type as DictKey};

    /// Two res-8 cells and one res-5 cell near San Francisco.
    const RES8_A: u64 = 0x8828308281fffff;
    const RES8_B: u64 = 0x8828308283fffff;
    const RES5: u64 = 0x85283473fffffff;

    fn encode(batch: &RecordBatch, options: PmtilesWriteOptions) -> Result<Vec<H3Feature>> {
        let encoder = FeatureEncoder::try_new(&batch.schema(), options)?;
        let mut out = Vec::new();
        let n = encoder.encode_batch(batch, |f| {
            out.push(f);
            Ok(())
        })?;
        assert_eq!(n as usize, out.len());
        Ok(out)
    }

    fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
        RecordBatch::try_from_iter(columns).unwrap()
    }

    fn props(f: &H3Feature) -> Vec<(&str, &MvtValue)> {
        f.properties.iter().map(|(k, v)| (k.as_ref(), v)).collect()
    }

    #[test]
    fn properties_keep_names_and_native_types() {
        let b = batch(vec![
            (
                "mean",
                Arc::new(Float64Array::from(vec![Some(1.5), None])) as ArrayRef,
            ),
            (
                "h3_index",
                Arc::new(UInt64Array::from(vec![RES8_A, RES8_B])),
            ),
            ("n", Arc::new(Int32Array::from(vec![-3, 4]))),
            ("class", Arc::new(UInt8Array::from(vec![7, 8]))),
            ("ratio", Arc::new(Float32Array::from(vec![0.25, 0.5]))),
            ("ok", Arc::new(BooleanArray::from(vec![true, false]))),
            ("name", Arc::new(StringViewArray::from(vec!["a", "b"]))),
            ("day", Arc::new(Date32Array::from(vec![0, 1]))),
            (
                "wkb",
                Arc::new(BinaryArray::from(vec![&b"\x01"[..], b"\x02"])),
            ),
        ]);
        let features = encode(&b, PmtilesWriteOptions::default()).unwrap();
        assert_eq!(features.len(), 2);
        assert_eq!(features[0].h3_index, RES8_A);
        assert_eq!(
            props(&features[0]),
            vec![
                ("mean", &MvtValue::Double(1.5)),
                ("n", &MvtValue::Int(-3)),
                ("class", &MvtValue::UInt(7)),
                ("ratio", &MvtValue::Float(0.25)),
                ("ok", &MvtValue::Bool(true)),
                ("name", &MvtValue::String("a".into())),
                ("day", &MvtValue::String("1970-01-01".into())),
            ]
        );
        // A NULL value is omitted; the binary column never appears.
        assert_eq!(props(&features[1])[0], ("n", &MvtValue::Int(4)));

        let encoder = FeatureEncoder::try_new(&b.schema(), PmtilesWriteOptions::default()).unwrap();
        assert_eq!(
            encoder.layer_fields(),
            vec![
                ("mean".to_string(), "Number"),
                ("n".to_string(), "Number"),
                ("class".to_string(), "Number"),
                ("ratio".to_string(), "Number"),
                ("ok".to_string(), "Boolean"),
                ("name".to_string(), "String"),
                ("day".to_string(), "String"),
            ]
        );
    }

    #[test]
    fn h3_column_types_and_selection() {
        let hex = |s: &[Option<&str>]| Arc::new(StringArray::from(s.to_vec())) as ArrayRef;
        // Signed 64-bit (Spark / PostgreSQL), found by name in another position.
        let b = batch(vec![
            ("v", Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef),
            (
                "CELL",
                Arc::new(Int64Array::from(vec![RES8_A as i64, RES5 as i64])),
            ),
        ]);
        let cells: Vec<u64> = encode(&b, PmtilesWriteOptions::default())
            .unwrap()
            .iter()
            .map(|f| f.h3_index)
            .collect();
        assert_eq!(cells, vec![RES8_A, RES5]);

        // Hex strings; NULL rows are skipped.
        let b = batch(vec![(
            "h3_hex",
            hex(&[Some("8828308281fffff"), None, Some(" 8828308283FFFFF ")]),
        )]);
        let cells: Vec<u64> = encode(&b, PmtilesWriteOptions::default())
            .unwrap()
            .iter()
            .map(|f| f.h3_index)
            .collect();
        assert_eq!(cells, vec![RES8_A, RES8_B]);

        // Dictionary-encoded hex strings, named explicitly; the first column is a property.
        let dict: DictionaryArray<DictKey> = vec!["8828308281fffff", "8828308283fffff"]
            .into_iter()
            .collect();
        let b = batch(vec![
            (
                "h3_index",
                Arc::new(UInt64Array::from(vec![RES5, RES5])) as ArrayRef,
            ),
            ("id", Arc::new(dict)),
        ]);
        let options = PmtilesWriteOptions {
            h3_column: Some("id".into()),
            ..Default::default()
        };
        let features = encode(&b, options).unwrap();
        assert_eq!(features[1].h3_index, RES8_B);
        assert_eq!(
            props(&features[1]),
            vec![("h3_index", &MvtValue::UInt(RES5))]
        );

        // Without a known name, the first column.
        let b = batch(vec![(
            "x",
            Arc::new(UInt64Array::from(vec![RES5])) as ArrayRef,
        )]);
        assert_eq!(
            encode(&b, PmtilesWriteOptions::default()).unwrap()[0].h3_index,
            RES5
        );
    }

    #[test]
    fn zoom_options_filter_cells() {
        let b = batch(vec![(
            "h3_index",
            Arc::new(UInt64Array::from(vec![RES8_A, RES5, RES8_B])) as ArrayRef,
        )]);
        let (z5, z8) = (h3_res_to_zoom(5), h3_res_to_zoom(8));
        assert!(z5 < z8);
        let count = |min_zoom, max_zoom| {
            encode(
                &b,
                PmtilesWriteOptions {
                    min_zoom,
                    max_zoom,
                    ..Default::default()
                },
            )
            .unwrap()
            .len()
        };
        assert_eq!(count(None, None), 3);
        assert_eq!(count(Some(z8), None), 2);
        assert_eq!(count(None, Some(z5)), 1);
        assert_eq!(count(Some(z5 + 1), Some(z8 - 1)), 0);
    }

    #[test]
    fn invalid_cells_and_schemas_are_errors() {
        let b = batch(vec![(
            "h3_index",
            Arc::new(UInt64Array::from(vec![RES5, 42])) as ArrayRef,
        )]);
        let err = encode(&b, PmtilesWriteOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("42 is not a valid H3 cell"), "{err}");

        let b = batch(vec![(
            "h3_hex",
            Arc::new(StringArray::from(vec!["zzz"])) as ArrayRef,
        )]);
        let err = encode(&b, PmtilesWriteOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("'zzz' is not a valid H3 cell"), "{err}");

        let b = batch(vec![(
            "mean",
            Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
        )]);
        let err = encode(&b, PmtilesWriteOptions::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("has type Float64; expected UInt64, Int64 or a hex string"),
            "{err}"
        );

        let schema = Schema::new(vec![Field::new("h3_index", DataType::UInt64, true)]);
        let options = PmtilesWriteOptions {
            h3_column: Some("nope".into()),
            ..Default::default()
        };
        let err = FeatureEncoder::try_new(&schema, options)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("h3_column 'nope' not found; columns: h3_index"),
            "{err}"
        );
    }

    #[test]
    fn copy_options() {
        let parse = |pairs: &[(&str, &str)]| {
            PmtilesWriteOptions::from_options(
                &pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        };
        let opts = parse(&[
            ("format.h3_column", "cell"),
            ("format.min_zoom", "4"),
            ("format.max_zoom", " 9 "),
            ("format.memory_budget_mb", "64"),
            ("execution.keep_partition_by_columns", "true"),
        ])
        .unwrap();
        assert_eq!(
            opts,
            PmtilesWriteOptions {
                h3_column: Some("cell".into()),
                min_zoom: Some(4),
                max_zoom: Some(9),
                memory_budget_bytes: 64 << 20,
            }
        );
        for (bad, needle) in [
            (
                ("format.min_zoom", "x"),
                "must be an integer between 0 and 30",
            ),
            (
                ("format.max_zoom", "31"),
                "must be an integer between 0 and 30",
            ),
            (("format.memory_budget_mb", "0"), "at least 1"),
            (
                ("format.compression", "gzip"),
                "Unknown PMTiles option 'compression'",
            ),
        ] {
            let err = parse(&[bad]).unwrap_err().to_string();
            assert!(err.contains(needle), "{bad:?}: {err}");
        }
        let err = parse(&[("format.min_zoom", "9"), ("format.max_zoom", "4")])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("min_zoom (9) must not be greater than max_zoom (4)"),
            "{err}"
        );
    }
}
