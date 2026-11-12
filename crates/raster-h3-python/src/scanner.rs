//! [`PyRasterScanner`]: schema resolution, batch iteration and Arrow / JSON export.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::{
    partitioned_output_schema, ArrowArrayStream, ArrowSchema, CategoricalBatchReader,
    CategoricalFormat, ColumnArray, ColumnSchema, ContinuousBatchReader, PartitionedBatchReader,
    RecordBatch, PARTIAL_STATE_COLUMN,
};
use raster_h3_core::error::{RasterH3Error, Result};
use raster_h3_core::raster::mosaic::resolve_raster_sources;

use crate::config::PyScanConfig;

/// Boxed streaming iterator of [`RecordBatch`]es.
pub type BatchIter = Box<dyn Iterator<Item = Result<RecordBatch>> + Send>;

/// Pull-based batch reader handed to Python, which wraps each batch as its own
/// single-batch Arrow stream so Polars can consume results incrementally.
pub struct PyBatchReader {
    pub(crate) schema: Arc<ColumnSchema>,
    pub(crate) iter: BatchIter,
    pub(crate) transform_failures: Arc<AtomicU64>,
}

/// Scanner backing the Python `RasterH3Scanner` object (`__arrow_c_schema__` and `__arrow_c_stream__`).
#[derive(Debug, Clone)]
pub struct PyRasterScanner {
    pub config: PyScanConfig,
}

impl PyRasterScanner {
    pub fn new(config: PyScanConfig) -> Self {
        Self { config }
    }

    fn base_schema(&self) -> ColumnSchema {
        if !self.config.categorical {
            ColumnSchema::continuous(&self.config.quantiles)
        } else {
            match self.config.format {
                CategoricalFormat::Wide => ColumnSchema::categorical_wide(),
                CategoricalFormat::Long => ColumnSchema::categorical_long(),
            }
        }
    }

    pub(crate) fn resolve_projection(&self) -> Result<Option<Vec<usize>>> {
        let Some(cols) = &self.config.columns else {
            return Ok(None);
        };
        if cols.is_empty() {
            return Ok(None);
        }
        let base = self.base_schema();
        let mut indices = Vec::with_capacity(cols.len());
        // A partitioned scan appends `partial_state` itself, so the caller may list the
        // output columns as they appear.
        let partitioned = self.config.partition.is_some();
        for name in cols
            .iter()
            .filter(|c| !(partitioned && *c == PARTIAL_STATE_COLUMN))
        {
            let idx = base.index_of(name).ok_or_else(|| {
                RasterH3Error::InvalidParameter(format!(
                    "Column '{}' not found in raster_h3 schema",
                    name
                ))
            })?;
            indices.push(idx);
        }
        Ok(Some(indices))
    }

    /// Return the output [`ColumnSchema`] (respecting `columns` projection).
    pub fn schema(&self) -> Result<Arc<ColumnSchema>> {
        let base = self.base_schema();
        if self.config.partition.is_some() {
            self.check_partitionable()?;
            let projection = self.resolve_projection()?;
            return Ok(Arc::new(partitioned_output_schema(
                base,
                projection.as_deref(),
            )?));
        }
        match self.resolve_projection()? {
            Some(indices) => Ok(Arc::new(base.project(&indices)?)),
            None => Ok(Arc::new(base)),
        }
    }

    /// Options a partitioned scan cannot honour.
    fn check_partitionable(&self) -> Result<()> {
        if self.config.categorical && self.config.format == CategoricalFormat::Long {
            return Err(RasterH3Error::InvalidParameter(
                "partitioned scans support the wide categorical format only; \
                 reshape the merged result to long format afterwards"
                    .into(),
            ));
        }
        if self.config.limit.is_some() {
            return Err(RasterH3Error::InvalidParameter(
                "limit cannot be combined with a partition: a per-partition row limit has no \
                 meaning for the merged result"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Export an [`ArrowSchema`] struct for `__arrow_c_schema__`.
    pub fn to_arrow_c_schema(&self) -> Result<ArrowSchema> {
        let schema = self.schema()?;
        Ok(ArrowSchema::from_column_schema(&schema))
    }

    /// Open the streaming [`RecordBatch`] iterator for this scan.
    pub fn batch_iter(&self) -> Result<(Arc<ColumnSchema>, BatchIter)> {
        let paths = resolve_raster_sources(&self.config.path)?;
        let res = if self.config.resolutions.is_empty() {
            vec![8]
        } else {
            self.config.resolutions.clone()
        };
        let mut mr_cfg = MultiResolutionConfig::new(res);
        mr_cfg.custom_crs = self.config.crs.clone();
        mr_cfg.custom_nodata = self.config.nodata;
        mr_cfg.bbox = self.config.bbox;
        mr_cfg.overlap_rule = self.config.overlap_rule;
        mr_cfg.band = self.config.band;
        mr_cfg.sampling = self.config.sampling.clone();
        mr_cfg.spectral_formula = self.config.spectral_formula;
        mr_cfg.remapper = self.config.remapper.clone();
        self.config.apply_cell_options(&mut mr_cfg);
        mr_cfg.strict_transforms = self.config.strict_transforms;
        mr_cfg.transform_failure_counter = self.config.transform_failure_counter.clone();

        let projection = self.resolve_projection()?;
        if let Some(partition) = &self.config.partition {
            self.check_partitionable()?;
            mr_cfg.partition = Some(partition.clone());
            let reader = if self.config.categorical {
                PartitionedBatchReader::open_categorical(
                    &paths,
                    &mr_cfg,
                    projection,
                    self.config.batch_size,
                )?
            } else {
                PartitionedBatchReader::open_continuous(
                    &paths,
                    &mr_cfg,
                    projection,
                    self.config.batch_size,
                )?
            };
            return Ok((reader.schema(), Box::new(reader)));
        }
        if !self.config.categorical {
            let reader = ContinuousBatchReader::open(
                &paths,
                &mr_cfg,
                projection,
                self.config.batch_size,
                self.config.limit,
            )?;
            Ok((reader.schema(), Box::new(reader)))
        } else {
            let reader = CategoricalBatchReader::open(
                &paths,
                &mr_cfg,
                self.config.format,
                projection,
                self.config.batch_size,
                self.config.limit,
            )?;
            Ok((reader.schema(), Box::new(reader)))
        }
    }

    /// Export an [`ArrowArrayStream`] struct for `__arrow_c_stream__`.
    pub fn to_arrow_c_stream(&self) -> Result<ArrowArrayStream> {
        let (schema, iter) = self.batch_iter()?;
        Ok(ArrowArrayStream::from_iterator(schema, iter))
    }

    /// Collect all batches into a columnar JSON object `{"col_name": [v1, v2, ...], ...}`.
    pub fn to_columnar_json(&self) -> Result<String> {
        let mut stream = self.to_arrow_c_stream()?;
        // SAFETY: `stream` was just built by `ArrowArrayStream::from_iterator`, so it is a
        // valid, unreleased stream owned by this frame.
        let (schema, batches) = unsafe { stream.collect_batches()? };
        batches_to_columnar_json(&schema, &batches)
    }
}

fn batches_to_columnar_json(schema: &ColumnSchema, batches: &[RecordBatch]) -> Result<String> {
    let total_rows: usize = batches.iter().map(|b| b.num_rows).sum();
    let mut map = serde_json::Map::with_capacity(schema.fields.len());

    for (col_idx, field) in schema.fields.iter().enumerate() {
        let mut arr = Vec::with_capacity(total_rows);
        for batch in batches {
            let col = &batch.columns[col_idx];
            let validity = col.validity();
            for row in 0..batch.num_rows {
                if let Some(v) = validity {
                    if !v[row] {
                        arr.push(serde_json::Value::Null);
                        continue;
                    }
                }
                let val = match col {
                    ColumnArray::UInt64 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Int64 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::UInt8 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Float64 { values, .. } => {
                        let f = values[row];
                        if f.is_finite() {
                            serde_json::json!(f)
                        } else {
                            serde_json::Value::Null
                        }
                    }
                    ColumnArray::Boolean { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Utf8 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Binary { values, .. } => {
                        let mut hex = String::with_capacity(values[row].len() * 2);
                        for &b in &values[row] {
                            use std::fmt::Write as _;
                            let _ = write!(&mut hex, "{:02x}", b);
                        }
                        serde_json::Value::String(hex)
                    }
                };
                arr.push(val);
            }
        }
        map.insert(field.name.clone(), serde_json::Value::Array(arr));
    }

    serde_json::to_string(&serde_json::Value::Object(map))
        .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))
}
