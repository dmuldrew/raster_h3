//! Columnar `RecordBatch` representation, streaming batch readers,
//! Apache Arrow C Data / Stream Interface (`ArrowSchema`, `ArrowArray`, `ArrowArrayStream`),
//! and streaming IPC serialization for `raster-h3-core`.

pub mod arrow_c;
pub mod ipc;

use std::path::PathBuf;
use std::sync::Arc;

use crate::aggregator::multi_horizon::{
    CategoricalLongCursor, CategoricalLongRow, MultiCategoricalHorizonStreamer,
    MultiCategoricalRecord, MultiContinuousRecord, MultiResolutionConfig, MultiScanHorizonStreamer,
    QuantilePlan, QuantileTarget,
};
use crate::encoding::{fast_hex_u64, h3_index_to_wkb, WkbBuf, WKB_BUF_LEN};
use crate::error::{RasterH3Error, Result};
use crate::raster::mosaic::MosaicReader;

pub use arrow_c::{ArrowArray, ArrowArrayStream, ArrowSchema};

/// Columnar data types corresponding to Apache Arrow physical types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowDataType {
    UInt64,
    Int64,
    UInt8,
    Float64,
    Boolean,
    Utf8,
    Binary,
}

impl ArrowDataType {
    /// Official Apache Arrow C Data Interface format string.
    #[inline]
    pub const fn arrow_format_str(self) -> &'static str {
        match self {
            ArrowDataType::UInt64 => "L",
            ArrowDataType::Int64 => "l",
            ArrowDataType::UInt8 => "C",
            ArrowDataType::Float64 => "g",
            ArrowDataType::Boolean => "b",
            ArrowDataType::Utf8 => "u",
            ArrowDataType::Binary => "z",
        }
    }
}

/// Metadata for a single column in a [`ColumnSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnField {
    pub name: String,
    pub data_type: ArrowDataType,
    pub nullable: bool,
}

impl ColumnField {
    pub fn new(name: impl Into<String>, data_type: ArrowDataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }
}

/// Schema describing the columns of a [`RecordBatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    pub fields: Vec<ColumnField>,
}

impl ColumnSchema {
    pub fn new(fields: Vec<ColumnField>) -> Self {
        Self { fields }
    }

    /// Standard schema for `h3_raster_continuous_aggregate`.
    pub fn continuous(quantiles: &[QuantileTarget]) -> Self {
        let mut fields = vec![
            ColumnField::new("h3_index", ArrowDataType::UInt64, false),
            ColumnField::new("h3_hex", ArrowDataType::Utf8, false),
            ColumnField::new("mean", ArrowDataType::Float64, false),
            ColumnField::new("stddev", ArrowDataType::Float64, true),
            ColumnField::new("count", ArrowDataType::Float64, false),
            ColumnField::new("min", ArrowDataType::Float64, false),
            ColumnField::new("max", ArrowDataType::Float64, false),
            ColumnField::new("sum", ArrowDataType::Float64, false),
            ColumnField::new("resolution", ArrowDataType::UInt8, false),
            ColumnField::new("wkb", ArrowDataType::Binary, false),
        ];
        for q in quantiles {
            fields.push(ColumnField::new(
                q.column_name(),
                ArrowDataType::Float64,
                true,
            ));
        }
        Self { fields }
    }

    /// Standard wide-format schema for `h3_raster_categorical_aggregate`.
    pub fn categorical_wide() -> Self {
        Self {
            fields: vec![
                ColumnField::new("h3_index", ArrowDataType::UInt64, false),
                ColumnField::new("h3_hex", ArrowDataType::Utf8, false),
                ColumnField::new("majority_class", ArrowDataType::Int64, false),
                ColumnField::new("majority_fraction", ArrowDataType::Float64, false),
                ColumnField::new("majority_count", ArrowDataType::Float64, false),
                ColumnField::new("unique_classes", ArrowDataType::Int64, false),
                ColumnField::new("total_count", ArrowDataType::Float64, false),
                ColumnField::new("histogram", ArrowDataType::Utf8, false),
                ColumnField::new("resolution", ArrowDataType::UInt8, false),
                ColumnField::new("shannon_entropy", ArrowDataType::Float64, false),
                ColumnField::new("entropy", ArrowDataType::Float64, false),
                ColumnField::new("distinct_classes", ArrowDataType::Int64, false),
                ColumnField::new("wkb", ArrowDataType::Binary, false),
            ],
        }
    }

    /// Standard long-format schema for `h3_raster_categorical_aggregate`.
    pub fn categorical_long() -> Self {
        Self {
            fields: vec![
                ColumnField::new("h3_index", ArrowDataType::UInt64, false),
                ColumnField::new("h3_hex", ArrowDataType::Utf8, false),
                ColumnField::new("category", ArrowDataType::Int64, false),
                ColumnField::new("count", ArrowDataType::Float64, false),
                ColumnField::new("fraction", ArrowDataType::Float64, false),
                ColumnField::new("total_count", ArrowDataType::Float64, false),
                ColumnField::new("resolution", ArrowDataType::UInt8, false),
                ColumnField::new("shannon_entropy", ArrowDataType::Float64, false),
                ColumnField::new("entropy", ArrowDataType::Float64, false),
                ColumnField::new("distinct_classes", ArrowDataType::Int64, false),
                ColumnField::new("unique_classes", ArrowDataType::Int64, false),
                ColumnField::new("wkb", ArrowDataType::Binary, false),
            ],
        }
    }

    /// Project a subset of columns by index.
    pub fn project(&self, indices: &[usize]) -> Result<Self> {
        let mut projected = Vec::with_capacity(indices.len());
        for &idx in indices {
            let field = self.fields.get(idx).ok_or_else(|| {
                RasterH3Error::InvalidParameter(format!(
                    "Projected column index {} out of bounds (schema has {} fields)",
                    idx,
                    self.fields.len()
                ))
            })?;
            projected.push(field.clone());
        }
        Ok(Self { fields: projected })
    }

    /// Lookup column index by name (case-insensitive).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
    }
}

/// Typed columnar array storage with optional validity bitmap (`true` = valid, `false` = null).
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnArray {
    UInt64 {
        values: Vec<u64>,
        validity: Option<Vec<bool>>,
    },
    Int64 {
        values: Vec<i64>,
        validity: Option<Vec<bool>>,
    },
    UInt8 {
        values: Vec<u8>,
        validity: Option<Vec<bool>>,
    },
    Float64 {
        values: Vec<f64>,
        validity: Option<Vec<bool>>,
    },
    Boolean {
        values: Vec<bool>,
        validity: Option<Vec<bool>>,
    },
    Utf8 {
        values: Vec<String>,
        validity: Option<Vec<bool>>,
    },
    Binary {
        values: Vec<Vec<u8>>,
        validity: Option<Vec<bool>>,
    },
}

impl ColumnArray {
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            ColumnArray::UInt64 { values, .. } => values.len(),
            ColumnArray::Int64 { values, .. } => values.len(),
            ColumnArray::UInt8 { values, .. } => values.len(),
            ColumnArray::Float64 { values, .. } => values.len(),
            ColumnArray::Boolean { values, .. } => values.len(),
            ColumnArray::Utf8 { values, .. } => values.len(),
            ColumnArray::Binary { values, .. } => values.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn data_type(&self) -> ArrowDataType {
        match self {
            ColumnArray::UInt64 { .. } => ArrowDataType::UInt64,
            ColumnArray::Int64 { .. } => ArrowDataType::Int64,
            ColumnArray::UInt8 { .. } => ArrowDataType::UInt8,
            ColumnArray::Float64 { .. } => ArrowDataType::Float64,
            ColumnArray::Boolean { .. } => ArrowDataType::Boolean,
            ColumnArray::Utf8 { .. } => ArrowDataType::Utf8,
            ColumnArray::Binary { .. } => ArrowDataType::Binary,
        }
    }

    #[inline]
    pub fn validity(&self) -> Option<&[bool]> {
        match self {
            ColumnArray::UInt64 { validity, .. }
            | ColumnArray::Int64 { validity, .. }
            | ColumnArray::UInt8 { validity, .. }
            | ColumnArray::Float64 { validity, .. }
            | ColumnArray::Boolean { validity, .. }
            | ColumnArray::Utf8 { validity, .. }
            | ColumnArray::Binary { validity, .. } => validity.as_deref(),
        }
    }

    #[inline]
    pub fn null_count(&self) -> usize {
        match self.validity() {
            Some(v) => v.iter().filter(|&&b| !b).count(),
            None => 0,
        }
    }

    /// Slice the array to `[offset..offset + length]`.
    pub fn slice(&self, offset: usize, length: usize) -> Self {
        let end = (offset + length).min(self.len());
        let start = offset.min(end);
        let slice_val = |v: &Option<Vec<bool>>| v.as_ref().map(|b| b[start..end].to_vec());
        match self {
            ColumnArray::UInt64 { values, validity } => ColumnArray::UInt64 {
                values: values[start..end].to_vec(),
                validity: slice_val(validity),
            },
            ColumnArray::Int64 { values, validity } => ColumnArray::Int64 {
                values: values[start..end].to_vec(),
                validity: slice_val(validity),
            },
            ColumnArray::UInt8 { values, validity } => ColumnArray::UInt8 {
                values: values[start..end].to_vec(),
                validity: slice_val(validity),
            },
            ColumnArray::Float64 { values, validity } => ColumnArray::Float64 {
                values: values[start..end].to_vec(),
                validity: slice_val(validity),
            },
            ColumnArray::Boolean { values, validity } => ColumnArray::Boolean {
                values: values[start..end].to_vec(),
                validity: slice_val(validity),
            },
            ColumnArray::Utf8 { values, validity } => ColumnArray::Utf8 {
                values: values[start..end].to_vec(),
                validity: slice_val(validity),
            },
            ColumnArray::Binary { values, validity } => ColumnArray::Binary {
                values: values[start..end].to_vec(),
                validity: slice_val(validity),
            },
        }
    }
}

/// A batch of columnar arrays conforming to a [`ColumnSchema`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordBatch {
    pub schema: Arc<ColumnSchema>,
    pub columns: Vec<ColumnArray>,
    pub num_rows: usize,
}

impl RecordBatch {
    pub fn try_new(schema: Arc<ColumnSchema>, columns: Vec<ColumnArray>) -> Result<Self> {
        if schema.fields.len() != columns.len() {
            return Err(RasterH3Error::InvalidParameter(format!(
                "Schema field count ({}) does not match column count ({})",
                schema.fields.len(),
                columns.len()
            )));
        }
        let num_rows = columns.first().map(|c| c.len()).unwrap_or(0);
        for (i, col) in columns.iter().enumerate() {
            if col.len() != num_rows {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Column {} length ({}) does not match batch row count ({})",
                    i,
                    col.len(),
                    num_rows
                )));
            }
            if col.data_type() != schema.fields[i].data_type {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Column '{}' type {:?} does not match schema type {:?}",
                    schema.fields[i].name,
                    col.data_type(),
                    schema.fields[i].data_type
                )));
            }
        }
        Ok(Self {
            schema,
            columns,
            num_rows,
        })
    }

    /// Project a subset of columns by index.
    pub fn project(&self, indices: &[usize]) -> Result<Self> {
        let proj_schema = Arc::new(self.schema.project(indices)?);
        let mut proj_cols = Vec::with_capacity(indices.len());
        for &idx in indices {
            proj_cols.push(self.columns[idx].clone());
        }
        Ok(Self {
            schema: proj_schema,
            columns: proj_cols,
            num_rows: self.num_rows,
        })
    }

    /// Return a row-sliced `RecordBatch`.
    pub fn slice(&self, offset: usize, length: usize) -> Self {
        let end = (offset + length).min(self.num_rows);
        let start = offset.min(end);
        let num_rows = end - start;
        let columns = self
            .columns
            .iter()
            .map(|c| c.slice(start, num_rows))
            .collect();
        Self {
            schema: Arc::clone(&self.schema),
            columns,
            num_rows,
        }
    }

    /// Lookup column by name (case-insensitive).
    pub fn column_by_name(&self, name: &str) -> Option<&ColumnArray> {
        let idx = self.schema.index_of(name)?;
        self.columns.get(idx)
    }

    /// Convert a slice of [`MultiContinuousRecord`] into a columnar [`RecordBatch`].
    /// Supports optional column projection indices (`projection`) so expensive encodings
    /// (`h3_hex`, `wkb`, `quantiles`) are only computed if requested.
    pub fn from_continuous_records(
        records: &[MultiContinuousRecord],
        quantiles: &[QuantileTarget],
        projection: Option<&[usize]>,
    ) -> Result<Self> {
        let full_schema = ColumnSchema::continuous(quantiles);
        let active_indices: Vec<usize> = match projection {
            Some(indices) => indices.to_vec(),
            None => (0..full_schema.fields.len()).collect(),
        };
        let schema = Arc::new(full_schema.project(&active_indices)?);
        let n = records.len();

        // Evaluate every projected quantile column with one sketch sweep per
        // record, rather than one sweep per (record, column).
        let mut q_targets: Vec<usize> = active_indices
            .iter()
            .filter_map(|&c| c.checked_sub(10))
            .collect();
        q_targets.sort_unstable();
        q_targets.dedup();
        let q_values: Vec<Vec<f64>> = if q_targets.is_empty() {
            Vec::new()
        } else {
            let plan = QuantilePlan::new(q_targets.iter().map(|&i| &quantiles[i]));
            let mut columns: Vec<Vec<f64>> =
                (0..plan.len()).map(|_| Vec::with_capacity(n)).collect();
            let mut row = Vec::with_capacity(plan.len());
            for r in records {
                plan.evaluate_into(&r.accumulator, &mut row);
                for (column, &v) in columns.iter_mut().zip(&row) {
                    column.push(v);
                }
            }
            columns
        };

        let mut hex_buf = [0u8; 16];
        let mut wkb_buf: WkbBuf = [0u8; WKB_BUF_LEN];

        let mut columns = Vec::with_capacity(active_indices.len());
        for &col_idx in &active_indices {
            let col = match col_idx {
                0 => ColumnArray::UInt64 {
                    values: records.iter().map(|r| r.h3_index).collect(),
                    validity: None,
                },
                1 => {
                    let mut values = Vec::with_capacity(n);
                    for r in records {
                        let bytes = fast_hex_u64(r.h3_index, &mut hex_buf);
                        let s = std::str::from_utf8(bytes).unwrap_or_default().to_owned();
                        values.push(s);
                    }
                    ColumnArray::Utf8 {
                        values,
                        validity: None,
                    }
                }
                2 => ColumnArray::Float64 {
                    values: records.iter().map(|r| r.accumulator.mean()).collect(),
                    validity: None,
                },
                3 => {
                    let mut values = Vec::with_capacity(n);
                    let mut validity = Vec::with_capacity(n);
                    let mut has_null = false;
                    for r in records {
                        let sd = r.accumulator.stddev();
                        if sd.is_finite() {
                            values.push(sd);
                            validity.push(true);
                        } else {
                            values.push(0.0);
                            validity.push(false);
                            has_null = true;
                        }
                    }
                    ColumnArray::Float64 {
                        values,
                        validity: if has_null { Some(validity) } else { None },
                    }
                }
                4 => ColumnArray::Float64 {
                    values: records.iter().map(|r| r.accumulator.count).collect(),
                    validity: None,
                },
                5 => ColumnArray::Float64 {
                    values: records.iter().map(|r| r.accumulator.min).collect(),
                    validity: None,
                },
                6 => ColumnArray::Float64 {
                    values: records.iter().map(|r| r.accumulator.max).collect(),
                    validity: None,
                },
                7 => ColumnArray::Float64 {
                    values: records.iter().map(|r| r.accumulator.sum).collect(),
                    validity: None,
                },
                8 => ColumnArray::UInt8 {
                    values: records.iter().map(|r| r.resolution).collect(),
                    validity: None,
                },
                9 => {
                    let mut values = Vec::with_capacity(n);
                    for r in records {
                        if let Some(len) = h3_index_to_wkb(r.h3_index, &mut wkb_buf) {
                            values.push(wkb_buf[..len].to_vec());
                        } else {
                            values.push(Vec::new());
                        }
                    }
                    ColumnArray::Binary {
                        values,
                        validity: None,
                    }
                }
                q_col if q_col >= 10 => {
                    // `project` above rejected out-of-range columns, so every
                    // projected quantile column is in `q_targets`.
                    let slot = q_targets.binary_search(&(q_col - 10)).map_err(|_| {
                        RasterH3Error::InvalidParameter(format!(
                            "Quantile column {q_col} is not configured"
                        ))
                    })?;
                    let mut values = Vec::with_capacity(n);
                    let mut validity = Vec::with_capacity(n);
                    let mut has_null = false;
                    for &val in &q_values[slot] {
                        if val.is_finite() {
                            values.push(val);
                            validity.push(true);
                        } else {
                            values.push(0.0);
                            validity.push(false);
                            has_null = true;
                        }
                    }
                    ColumnArray::Float64 {
                        values,
                        validity: if has_null { Some(validity) } else { None },
                    }
                }
                _ => unreachable!(),
            };
            columns.push(col);
        }

        Ok(Self {
            schema,
            columns,
            num_rows: n,
        })
    }

    /// Convert a slice of [`MultiCategoricalRecord`] into a Wide-format [`RecordBatch`].
    pub fn from_categorical_wide_records(
        records: &[MultiCategoricalRecord],
        projection: Option<&[usize]>,
    ) -> Result<Self> {
        let full_schema = ColumnSchema::categorical_wide();
        let active_indices: Vec<usize> = match projection {
            Some(indices) => indices.to_vec(),
            None => (0..full_schema.fields.len()).collect(),
        };
        let schema = Arc::new(full_schema.project(&active_indices)?);
        let n = records.len();

        let mut hex_buf = [0u8; 16];
        let mut wkb_buf: WkbBuf = [0u8; WKB_BUF_LEN];

        // Precompute majority stats if any majority column is projected
        let needs_majority = active_indices.iter().any(|&i| (2..=4).contains(&i));
        let majorities: Vec<(i64, f64, f64)> = if needs_majority {
            records.iter().map(|r| r.accumulator.majority()).collect()
        } else {
            Vec::new()
        };

        let needs_entropy = active_indices.iter().any(|&i| i == 9 || i == 10);
        let entropies: Vec<f64> = if needs_entropy {
            records
                .iter()
                .map(|r| r.accumulator.shannon_entropy())
                .collect()
        } else {
            Vec::new()
        };

        let mut columns = Vec::with_capacity(active_indices.len());
        for &col_idx in &active_indices {
            let col = match col_idx {
                0 => ColumnArray::UInt64 {
                    values: records.iter().map(|r| r.h3_index).collect(),
                    validity: None,
                },
                1 => {
                    let mut values = Vec::with_capacity(n);
                    for r in records {
                        let bytes = fast_hex_u64(r.h3_index, &mut hex_buf);
                        values.push(std::str::from_utf8(bytes).unwrap_or_default().to_owned());
                    }
                    ColumnArray::Utf8 {
                        values,
                        validity: None,
                    }
                }
                2 => ColumnArray::Int64 {
                    values: majorities.iter().map(|m| m.0).collect(),
                    validity: None,
                },
                3 => ColumnArray::Float64 {
                    values: majorities.iter().map(|m| m.2).collect(),
                    validity: None,
                },
                4 => ColumnArray::Float64 {
                    values: majorities.iter().map(|m| m.1).collect(),
                    validity: None,
                },
                5 | 11 => ColumnArray::Int64 {
                    values: records
                        .iter()
                        .map(|r| r.accumulator.unique_classes() as i64)
                        .collect(),
                    validity: None,
                },
                6 => ColumnArray::Float64 {
                    values: records.iter().map(|r| r.accumulator.total_count).collect(),
                    validity: None,
                },
                7 => ColumnArray::Utf8 {
                    values: records
                        .iter()
                        .map(|r| r.accumulator.histogram_json())
                        .collect(),
                    validity: None,
                },
                8 => ColumnArray::UInt8 {
                    values: records.iter().map(|r| r.resolution).collect(),
                    validity: None,
                },
                9 | 10 => ColumnArray::Float64 {
                    values: entropies.clone(),
                    validity: None,
                },
                12 => {
                    let mut values = Vec::with_capacity(n);
                    for r in records {
                        if let Some(len) = h3_index_to_wkb(r.h3_index, &mut wkb_buf) {
                            values.push(wkb_buf[..len].to_vec());
                        } else {
                            values.push(Vec::new());
                        }
                    }
                    ColumnArray::Binary {
                        values,
                        validity: None,
                    }
                }
                _ => unreachable!(),
            };
            columns.push(col);
        }

        Ok(Self {
            schema,
            columns,
            num_rows: n,
        })
    }
}

/// Categorical output layout mode (`Wide` vs `Long`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoricalFormat {
    Wide,
    Long,
}

/// Whether a continuous-schema projection selects any of the `num_quantiles`
/// quantile columns (indices 10 and up). `None` selects every column.
pub fn quantiles_projected(projection: Option<&[usize]>, num_quantiles: usize) -> bool {
    num_quantiles > 0
        && projection.is_none_or(|cols| cols.iter().any(|&c| (10..10 + num_quantiles).contains(&c)))
}

/// Streaming iterator yielding [`RecordBatch`]es for continuous H3 raster aggregation.
pub struct ContinuousBatchReader {
    streamer: MultiScanHorizonStreamer,
    quantiles: Vec<QuantileTarget>,
    projection: Option<Vec<usize>>,
    schema: Arc<ColumnSchema>,
    batch_size: usize,
    remaining_limit: Option<usize>,
    finished: bool,
}

impl ContinuousBatchReader {
    pub fn open(
        paths: &[PathBuf],
        config: &MultiResolutionConfig,
        projection: Option<Vec<usize>>,
        batch_size: usize,
        limit: Option<usize>,
    ) -> Result<Self> {
        let mosaic = Arc::new(MosaicReader::open(
            paths,
            config.bbox,
            config.custom_crs.as_deref(),
            config.overlap_rule,
        )?);
        // A quantile sketch is a boxed, growing bin map per cell; build them
        // only when a quantile column is actually projected.
        let streamer = if quantiles_projected(projection.as_deref(), config.quantiles.len()) {
            MultiScanHorizonStreamer::new_mosaic(mosaic, config)?
        } else {
            let mut config = config.clone();
            config.quantiles.clear();
            MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?
        };
        Self::from_streamer(
            streamer,
            config.quantiles.clone(),
            projection,
            batch_size,
            limit,
        )
    }

    pub fn from_streamer(
        streamer: MultiScanHorizonStreamer,
        quantiles: Vec<QuantileTarget>,
        projection: Option<Vec<usize>>,
        batch_size: usize,
        limit: Option<usize>,
    ) -> Result<Self> {
        let full_schema = ColumnSchema::continuous(&quantiles);
        let schema = match &projection {
            Some(indices) => Arc::new(full_schema.project(indices)?),
            None => Arc::new(full_schema),
        };
        Ok(Self {
            streamer,
            quantiles,
            projection,
            schema,
            batch_size: batch_size.max(1),
            remaining_limit: limit,
            finished: false,
        })
    }

    pub fn schema(&self) -> Arc<ColumnSchema> {
        Arc::clone(&self.schema)
    }

    /// Whether the underlying streamer builds per-cell quantile sketches.
    pub fn tracks_quantiles(&self) -> bool {
        self.streamer.track_quantiles()
    }

    /// Valid sample points dropped by failed or out-of-domain coordinate
    /// transforms so far (see `transform_failures` on the streamers).
    pub fn transform_failures(&self) -> u64 {
        self.streamer.transform_failures()
    }
}

impl Iterator for ContinuousBatchReader {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if let Some(0) = self.remaining_limit {
            self.finished = true;
            return None;
        }

        let target = match self.remaining_limit {
            Some(rem) => self.batch_size.min(rem),
            None => self.batch_size,
        };

        let mut records = Vec::with_capacity(target);
        match self.streamer.drain_completed_into(target, |_, rec| {
            records.push(rec);
        }) {
            Ok(0) => {
                self.finished = true;
                None
            }
            Ok(count) => {
                if let Some(rem) = &mut self.remaining_limit {
                    *rem = rem.saturating_sub(count);
                }
                Some(RecordBatch::from_continuous_records(
                    &records,
                    &self.quantiles,
                    self.projection.as_deref(),
                ))
            }
            Err(e) => {
                self.finished = true;
                Some(Err(e))
            }
        }
    }
}

/// Streaming iterator yielding [`RecordBatch`]es for categorical H3 raster aggregation.
pub struct CategoricalBatchReader {
    streamer: MultiCategoricalHorizonStreamer,
    format: CategoricalFormat,
    projection: Option<Vec<usize>>,
    schema: Arc<ColumnSchema>,
    batch_size: usize,
    remaining_limit: Option<usize>,
    long_cursor: CategoricalLongCursor,
    finished: bool,
}

impl CategoricalBatchReader {
    pub fn open(
        paths: &[PathBuf],
        config: &MultiResolutionConfig,
        format: CategoricalFormat,
        projection: Option<Vec<usize>>,
        batch_size: usize,
        limit: Option<usize>,
    ) -> Result<Self> {
        let mosaic = Arc::new(MosaicReader::open(
            paths,
            config.bbox,
            config.custom_crs.as_deref(),
            config.overlap_rule,
        )?);
        let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, config)?;
        Self::from_streamer(streamer, format, projection, batch_size, limit)
    }

    pub fn from_streamer(
        streamer: MultiCategoricalHorizonStreamer,
        format: CategoricalFormat,
        projection: Option<Vec<usize>>,
        batch_size: usize,
        limit: Option<usize>,
    ) -> Result<Self> {
        let full_schema = match format {
            CategoricalFormat::Wide => ColumnSchema::categorical_wide(),
            CategoricalFormat::Long => ColumnSchema::categorical_long(),
        };
        let schema = match &projection {
            Some(indices) => Arc::new(full_schema.project(indices)?),
            None => Arc::new(full_schema),
        };
        // Long-format columns 7/8 are the entropy aliases; skip the per-cell
        // log pass when neither is projected.
        let with_entropy = projection
            .as_ref()
            .is_none_or(|cols| cols.iter().any(|&c| c == 7 || c == 8));
        Ok(Self {
            streamer,
            format,
            projection,
            schema,
            batch_size: batch_size.max(1),
            remaining_limit: limit,
            long_cursor: CategoricalLongCursor::new(with_entropy),
            finished: false,
        })
    }

    pub fn schema(&self) -> Arc<ColumnSchema> {
        Arc::clone(&self.schema)
    }

    /// Valid sample points dropped by failed or out-of-domain coordinate
    /// transforms so far (see `transform_failures` on the streamers).
    pub fn transform_failures(&self) -> u64 {
        self.streamer.transform_failures()
    }

    fn next_long_batch(&mut self, target: usize) -> Option<Result<RecordBatch>> {
        // Expand one cell at a time and only as far as this batch needs, so a
        // high-cardinality cell never materializes more rows than requested.
        let mut rows: Vec<CategoricalLongRow> = Vec::with_capacity(target);
        while rows.len() < target {
            if self.long_cursor.remaining() > 0 {
                self.long_cursor
                    .emit_into(target - rows.len(), |row| rows.push(row));
                continue;
            }
            if self.finished {
                break;
            }
            let cursor = &mut self.long_cursor;
            match self.streamer.drain_completed_into(1, |_, rec| {
                // Cells without counted pixels produce no long rows.
                if rec.accumulator.total_count > 0.0 {
                    cursor.load(&rec);
                }
            }) {
                Ok(0) => self.finished = true,
                Ok(_) => {}
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        }

        if rows.is_empty() {
            self.finished = true;
            return None;
        }

        let n = rows.len();
        if let Some(rem) = &mut self.remaining_limit {
            *rem = rem.saturating_sub(n);
        }

        let full_schema = ColumnSchema::categorical_long();
        let active_indices: Vec<usize> = match &self.projection {
            Some(indices) => indices.clone(),
            None => (0..full_schema.fields.len()).collect(),
        };
        let mut hex_buf = [0u8; 16];
        let mut wkb_buf: WkbBuf = [0u8; WKB_BUF_LEN];

        let mut columns = Vec::with_capacity(active_indices.len());
        for &col_idx in &active_indices {
            let col = match col_idx {
                0 => ColumnArray::UInt64 {
                    values: rows.iter().map(|r| r.cell_u64).collect(),
                    validity: None,
                },
                1 => ColumnArray::Utf8 {
                    values: rows
                        .iter()
                        .map(|r| {
                            let bytes = fast_hex_u64(r.cell_u64, &mut hex_buf);
                            std::str::from_utf8(bytes).unwrap_or_default().to_owned()
                        })
                        .collect(),
                    validity: None,
                },
                2 => ColumnArray::Int64 {
                    values: rows.iter().map(|r| r.category).collect(),
                    validity: None,
                },
                3 => ColumnArray::Float64 {
                    values: rows.iter().map(|r| r.count).collect(),
                    validity: None,
                },
                4 => ColumnArray::Float64 {
                    values: rows.iter().map(|r| r.fraction).collect(),
                    validity: None,
                },
                5 => ColumnArray::Float64 {
                    values: rows.iter().map(|r| r.total_count).collect(),
                    validity: None,
                },
                6 => ColumnArray::UInt8 {
                    values: rows.iter().map(|r| r.resolution).collect(),
                    validity: None,
                },
                7 | 8 => ColumnArray::Float64 {
                    values: rows.iter().map(|r| r.entropy).collect(),
                    validity: None,
                },
                9 | 10 => ColumnArray::Int64 {
                    values: rows.iter().map(|r| r.distinct_classes).collect(),
                    validity: None,
                },
                11 => ColumnArray::Binary {
                    values: rows
                        .iter()
                        .map(|r| {
                            if let Some(len) = h3_index_to_wkb(r.cell_u64, &mut wkb_buf) {
                                wkb_buf[..len].to_vec()
                            } else {
                                Vec::new()
                            }
                        })
                        .collect(),
                    validity: None,
                },
                _ => unreachable!(),
            };
            columns.push(col);
        }

        Some(Ok(RecordBatch {
            schema: Arc::clone(&self.schema),
            columns,
            num_rows: n,
        }))
    }
}

impl Iterator for CategoricalBatchReader {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(0) = self.remaining_limit {
            self.finished = true;
            return None;
        }
        let target = match self.remaining_limit {
            Some(rem) => self.batch_size.min(rem),
            None => self.batch_size,
        };

        match self.format {
            CategoricalFormat::Wide => {
                if self.finished {
                    return None;
                }
                let mut records = Vec::with_capacity(target);
                match self.streamer.drain_completed_into(target, |_, rec| {
                    records.push(rec);
                }) {
                    Ok(0) => {
                        self.finished = true;
                        None
                    }
                    Ok(count) => {
                        if let Some(rem) = &mut self.remaining_limit {
                            *rem = rem.saturating_sub(count);
                        }
                        Some(RecordBatch::from_categorical_wide_records(
                            &records,
                            self.projection.as_deref(),
                        ))
                    }
                    Err(e) => {
                        self.finished = true;
                        Some(Err(e))
                    }
                }
            }
            CategoricalFormat::Long => self.next_long_batch(target),
        }
    }
}
