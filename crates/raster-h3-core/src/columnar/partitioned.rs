//! Columnar output and merging for partitioned scans
//! ([`MultiResolutionConfig::partition`], see
//! [`partition`](crate::aggregator::multi_horizon::partition)).
//!
//! A partitioned scan's batches have the normal output schema plus a nullable
//! [`PARTIAL_STATE_COLUMN`] (`Binary`):
//!
//! - rows where it is null are final (interior) cells, already filtered;
//! - rows where it is set are this partition's share of a boundary cell. Their statistics
//!   columns describe only that share, and they are not filtered.
//!
//! Collect the partial rows of every partition (grouped by `h3_index`, in any order) and
//! pass their `resolution`, `h3_index` and `partial_state` columns to
//! [`merge_partial_batches`] for the final rows of those cells.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use crate::aggregator::multi_horizon::{
    encode_partial_state, MultiCategoricalHorizonStreamer, MultiCategoricalRecord,
    MultiContinuousRecord, MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
use crate::error::{RasterH3Error, Result};
use crate::raster::mosaic::MosaicReader;

use super::{
    quantiles_projected, ArrowDataType, ColumnArray, ColumnField, ColumnSchema, RecordBatch,
};

/// Name of the column holding a boundary cell's encoded partial state (null for final rows).
pub const PARTIAL_STATE_COLUMN: &str = "partial_state";

/// `schema` with the nullable [`PARTIAL_STATE_COLUMN`] appended.
pub fn with_partial_state_field(schema: &ColumnSchema) -> ColumnSchema {
    let mut fields = schema.fields.clone();
    fields.push(ColumnField::new(
        PARTIAL_STATE_COLUMN,
        ArrowDataType::Binary,
        true,
    ));
    ColumnSchema::new(fields)
}

/// Output schema of a partitioned scan: the scan's full base schema projected by
/// `projection`, plus [`PARTIAL_STATE_COLUMN`]. Fails when the projection drops a column
/// that partial rows are merged by (`h3_index`, `resolution`).
pub fn partitioned_output_schema(
    full: ColumnSchema,
    projection: Option<&[usize]>,
) -> Result<ColumnSchema> {
    let index_col = full
        .fields
        .first()
        .map(|f| f.name.as_str())
        .unwrap_or("h3_index");
    let schema = match projection {
        Some(indices) => full.project(indices)?,
        None => full.clone(),
    };
    for key in [index_col, "resolution"] {
        if schema.index_of(key).is_none() {
            return Err(RasterH3Error::InvalidParameter(format!(
                "partitioned scans must output the '{key}' column: partial rows are merged by \
                 resolution and {index_col}"
            )));
        }
    }
    Ok(with_partial_state_field(&schema))
}

enum Streamer {
    Continuous {
        streamer: MultiScanHorizonStreamer,
        quantiles: Vec<QuantileTarget>,
    },
    Categorical(MultiCategoricalHorizonStreamer),
}

/// Streaming reader of a partitioned scan: final rows and partial rows, see the
/// [module documentation](self). Categorical output uses the wide format.
pub struct PartitionedBatchReader {
    streamer: Streamer,
    projection: Option<Vec<usize>>,
    schema: Arc<ColumnSchema>,
    batch_size: usize,
    pending: VecDeque<RecordBatch>,
    finished: bool,
}

impl PartitionedBatchReader {
    /// Open a continuous partitioned scan of the mosaic `paths` (`config.partition` names
    /// the partition; without one, the whole mosaic is a single partition and every row is
    /// final).
    pub fn open_continuous(
        paths: &[PathBuf],
        config: &MultiResolutionConfig,
        projection: Option<Vec<usize>>,
        batch_size: usize,
    ) -> Result<Self> {
        let full = ColumnSchema::continuous_for(config.grid, &config.quantiles);
        let schema = partitioned_output_schema(full, projection.as_deref())?;
        // Sketches are only built when a quantile column is output, exactly as the
        // unpartitioned reader does, so merged results match it.
        let mut config = config.clone();
        if !quantiles_projected(projection.as_deref(), config.quantiles.len()) {
            config.quantiles.clear();
        }
        let streamer = MultiScanHorizonStreamer::new_mosaic(open_mosaic(paths, &config)?, &config)?;
        Ok(Self::new(
            Streamer::Continuous {
                streamer,
                quantiles: config.quantiles,
            },
            projection,
            schema,
            batch_size,
        ))
    }

    /// Open a categorical (wide format) partitioned scan; see [`Self::open_continuous`].
    pub fn open_categorical(
        paths: &[PathBuf],
        config: &MultiResolutionConfig,
        projection: Option<Vec<usize>>,
        batch_size: usize,
    ) -> Result<Self> {
        let schema = partitioned_output_schema(
            ColumnSchema::categorical_wide_for(config.grid),
            projection.as_deref(),
        )?;
        let streamer =
            MultiCategoricalHorizonStreamer::new_mosaic(open_mosaic(paths, config)?, config)?;
        Ok(Self::new(
            Streamer::Categorical(streamer),
            projection,
            schema,
            batch_size,
        ))
    }

    fn new(
        streamer: Streamer,
        projection: Option<Vec<usize>>,
        schema: ColumnSchema,
        batch_size: usize,
    ) -> Self {
        Self {
            streamer,
            projection,
            schema: Arc::new(schema),
            batch_size: batch_size.max(1),
            pending: VecDeque::new(),
            finished: false,
        }
    }

    /// Output schema: the projected columns plus [`PARTIAL_STATE_COLUMN`].
    pub fn schema(&self) -> Arc<ColumnSchema> {
        Arc::clone(&self.schema)
    }

    /// Valid sample points dropped by failed coordinate transforms so far.
    pub fn transform_failures(&self) -> u64 {
        match &self.streamer {
            Streamer::Continuous { streamer, .. } => streamer.transform_failures(),
            Streamer::Categorical(streamer) => streamer.transform_failures(),
        }
    }

    /// `(interior, boundary)` cells so far; `None` for a scan without a partition.
    pub fn partition_cell_counts(&self) -> Option<(u64, u64)> {
        match &self.streamer {
            Streamer::Continuous { streamer, .. } => streamer.partition_cell_counts(),
            Streamer::Categorical(streamer) => streamer.partition_cell_counts(),
        }
    }

    /// Drain up to one batch of records and partials, queueing their batches.
    fn fill(&mut self) -> Result<()> {
        let projection = self.projection.as_deref();
        let schema = Arc::clone(&self.schema);
        let (finals, partials) = match &mut self.streamer {
            Streamer::Continuous {
                streamer,
                quantiles,
            } => {
                let (mut records, mut partials, mut states) = (Vec::new(), Vec::new(), Vec::new());
                let n = streamer.drain_partitioned_into(
                    self.batch_size,
                    |_, r| records.push(r),
                    |p| {
                        states.push(encode_partial_state(&p.accumulator));
                        partials.push(MultiContinuousRecord {
                            resolution: p.resolution,
                            h3_index: p.h3_index,
                            accumulator: p.accumulator,
                        });
                    },
                )?;
                if n == 0 {
                    self.finished = true;
                    return Ok(());
                }
                let grid = streamer.grid();
                let finals = (!records.is_empty())
                    .then(|| {
                        RecordBatch::from_continuous_records_for(
                            grid, &records, quantiles, projection,
                        )
                    })
                    .transpose()?;
                let partials = (!partials.is_empty())
                    .then(|| {
                        RecordBatch::from_continuous_records_for(
                            grid, &partials, quantiles, projection,
                        )
                        .map(|b| (b, states))
                    })
                    .transpose()?;
                (finals, partials)
            }
            Streamer::Categorical(streamer) => {
                let (mut records, mut partials, mut states) = (Vec::new(), Vec::new(), Vec::new());
                let n = streamer.drain_partitioned_into(
                    self.batch_size,
                    |_, r| records.push(r),
                    |p| {
                        states.push(encode_partial_state(&p.accumulator));
                        partials.push(MultiCategoricalRecord {
                            resolution: p.resolution,
                            h3_index: p.h3_index,
                            accumulator: p.accumulator,
                        });
                    },
                )?;
                if n == 0 {
                    self.finished = true;
                    return Ok(());
                }
                let grid = streamer.grid();
                let finals = (!records.is_empty())
                    .then(|| {
                        RecordBatch::from_categorical_wide_records_for(grid, &records, projection)
                    })
                    .transpose()?;
                let partials = (!partials.is_empty())
                    .then(|| {
                        RecordBatch::from_categorical_wide_records_for(grid, &partials, projection)
                            .map(|b| (b, states))
                    })
                    .transpose()?;
                (finals, partials)
            }
        };
        if let Some(batch) = finals {
            let nulls = ColumnArray::Binary {
                values: vec![Vec::new(); batch.num_rows],
                validity: Some(vec![false; batch.num_rows]),
            };
            self.pending
                .push_back(append_column(batch, nulls, &schema)?);
        }
        if let Some((batch, states)) = partials {
            let states = ColumnArray::Binary {
                values: states,
                validity: None,
            };
            self.pending
                .push_back(append_column(batch, states, &schema)?);
        }
        Ok(())
    }
}

fn open_mosaic(paths: &[PathBuf], config: &MultiResolutionConfig) -> Result<Arc<MosaicReader>> {
    Ok(Arc::new(MosaicReader::open(
        paths,
        config.bbox,
        config.custom_crs.as_deref(),
        config.overlap_rule,
    )?))
}

fn append_column(
    batch: RecordBatch,
    column: ColumnArray,
    schema: &Arc<ColumnSchema>,
) -> Result<RecordBatch> {
    let mut columns = batch.columns;
    columns.push(column);
    RecordBatch::try_new(Arc::clone(schema), columns)
}

impl Iterator for PartitionedBatchReader {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(batch) = self.pending.pop_front() {
                return Some(Ok(batch));
            }
            if self.finished {
                return None;
            }
            if let Err(e) = self.fill() {
                self.finished = true;
                return Some(Err(e));
            }
        }
    }
}

/// Typed key columns of a batch of partial rows.
struct PartialColumns<'a> {
    resolution: &'a [u8],
    h3_index: &'a [u64],
    states: &'a [Vec<u8>],
    validity: Option<&'a [bool]>,
}

fn partial_columns(batch: &RecordBatch) -> Result<PartialColumns<'_>> {
    let missing = |name: &str| {
        RasterH3Error::InvalidParameter(format!(
            "partial rows need a '{name}' column (resolution: UInt8, h3_index: UInt64, {PARTIAL_STATE_COLUMN}: Binary)"
        ))
    };
    let resolution = match batch.column_by_name("resolution") {
        Some(ColumnArray::UInt8 {
            values,
            validity: None,
        }) => values.as_slice(),
        Some(ColumnArray::UInt8 {
            values,
            validity: Some(v),
        }) if v.iter().all(|&b| b) => values.as_slice(),
        _ => return Err(missing("resolution")),
    };
    let h3 = match batch
        .column_by_name("h3_index")
        .or_else(|| batch.column_by_name("a5_index"))
    {
        Some(ColumnArray::UInt64 {
            values,
            validity: None,
        }) => values.as_slice(),
        Some(ColumnArray::UInt64 {
            values,
            validity: Some(v),
        }) if v.iter().all(|&b| b) => values.as_slice(),
        _ => return Err(missing("h3_index")),
    };
    let (states, validity) = match batch.column_by_name(PARTIAL_STATE_COLUMN) {
        Some(ColumnArray::Binary { values, validity }) => (values.as_slice(), validity.as_deref()),
        _ => return Err(missing(PARTIAL_STATE_COLUMN)),
    };
    Ok(PartialColumns {
        resolution,
        h3_index: h3,
        states,
        validity,
    })
}

/// Merge the partial rows of all partitions of a scan into final rows.
///
/// `batches` hold `resolution` (UInt8), `h3_index` (UInt64) and [`PARTIAL_STATE_COLUMN`]
/// (Binary, never null) columns; other columns are ignored. Every partial row of a cell
/// must be in `batches`. `config` must match the scan's (for its filters and, for
/// continuous scans, its quantiles), and `projection` selects the output columns of the
/// scan's base schema, like the scan's own projection. Returns the output schema and
/// batches of up to `batch_size` rows, sorted by `(resolution, h3_index)`.
pub fn merge_partial_batches(
    batches: &[RecordBatch],
    categorical: bool,
    config: &MultiResolutionConfig,
    projection: Option<&[usize]>,
    batch_size: usize,
) -> Result<(Arc<ColumnSchema>, Vec<RecordBatch>)> {
    let limit = config.aggregation_budget_bytes;
    let batch_size = batch_size.max(1);
    let null_state = || {
        RasterH3Error::InvalidParameter(format!(
            "null {PARTIAL_STATE_COLUMN}: pass only partial rows; rows without a state are already final"
        ))
    };
    macro_rules! merge_all {
        ($merger:expr) => {{
            let mut merger = $merger;
            for batch in batches {
                let cols = partial_columns(batch)?;
                for i in 0..batch.num_rows {
                    if cols.validity.is_some_and(|v| !v[i]) {
                        return Err(null_state());
                    }
                    merger.push_encoded(
                        cols.resolution[i],
                        cols.h3_index[i],
                        &cols.states[i],
                        limit,
                    )?;
                }
            }
            merger.finish()
        }};
    }
    if categorical {
        let full = ColumnSchema::categorical_wide_for(config.grid);
        let schema = Arc::new(match projection {
            Some(p) => full.project(p)?,
            None => full,
        });
        let records = merge_all!(MultiCategoricalHorizonStreamer::partial_merger(config));
        let out = records
            .chunks(batch_size)
            .map(|chunk| {
                RecordBatch::from_categorical_wide_records_for(config.grid, chunk, projection)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((schema, out))
    } else {
        let full = ColumnSchema::continuous_for(config.grid, &config.quantiles);
        let schema = Arc::new(match projection {
            Some(p) => full.project(p)?,
            None => full,
        });
        let mut config = config.clone();
        if !quantiles_projected(projection, config.quantiles.len()) {
            config.quantiles.clear();
        }
        let records = merge_all!(MultiScanHorizonStreamer::partial_merger(&config));
        let out = records
            .chunks(batch_size)
            .map(|chunk| {
                RecordBatch::from_continuous_records_for(
                    config.grid,
                    chunk,
                    &config.quantiles,
                    projection,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((schema, out))
    }
}
