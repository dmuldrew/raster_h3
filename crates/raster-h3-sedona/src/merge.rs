//! Aggregates that merge the partial cells of partitioned scans:
//! `h3_merge_partials(partial_state)` (continuous) and
//! `h3_merge_categorical_partials(partial_state)` (categorical).
//!
//! Group the partial rows of every partition by cell and aggregate their `partial_state`:
//!
//! ```sql
//! SELECT h3_index, resolution, h3_merge_partials(partial_state) AS cell
//! FROM partial_rows
//! GROUP BY h3_index, resolution
//! ```
//!
//! Each returns a struct with the statistics columns a scan outputs (for example `mean`,
//! `stddev`, `count`, `min`, `max`, `sum`), computed exactly as the scan computes them.
//! Null states are skipped, like any SQL aggregate. Thresholds such as `count >= 4` belong
//! in a filter on the merged result. The intermediate aggregation state is the encoded
//! partial state itself, so DataFusion's multi-phase aggregation can spread the merge across
//! threads.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, StructArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, FieldRef, Fields};
use datafusion::common::{exec_err, ScalarValue};
use datafusion::error::Result;
use datafusion::logical_expr::function::{AccumulatorArgs, StateFieldsArgs};
use datafusion::logical_expr::{
    Accumulator, AggregateUDF, AggregateUDFImpl, Signature, TypeSignature, Volatility,
};
use raster_h3_core::aggregator::accumulator::H3Accumulator;
use raster_h3_core::aggregator::categorical::CategoricalAccumulator;
use raster_h3_core::aggregator::multi_horizon::{
    decode_partial_state, encode_partial_state, AccumulatorMerge, MultiCategoricalRecord,
    MultiContinuousRecord,
};
use raster_h3_core::columnar::{ColumnSchema, RecordBatch};

use crate::convert::{from_column_array, to_arrow_type, to_df_error};

/// Upper bound on one decoded partial state, guarding against corrupt input.
const STATE_LIMIT: usize = 64 << 20;

/// Statistics columns of a continuous scan returned by `h3_merge_partials`.
const CONTINUOUS_COLUMNS: &[&str] = &["mean", "stddev", "count", "min", "max", "sum"];

/// Statistics columns of a wide categorical scan returned by `h3_merge_categorical_partials`.
const CATEGORICAL_COLUMNS: &[&str] = &[
    "majority_class",
    "majority_fraction",
    "majority_count",
    "unique_classes",
    "total_count",
    "histogram",
    "shannon_entropy",
    "entropy",
    "distinct_classes",
];

/// The two merge aggregates, ready for `SessionContext::register_udaf`.
pub fn merge_udafs() -> Vec<AggregateUDF> {
    vec![
        AggregateUDF::new_from_impl(MergePartials::new(Kind::Continuous)),
        AggregateUDF::new_from_impl(MergePartials::new(Kind::Categorical)),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Continuous,
    Categorical,
}

impl Kind {
    fn schema(self) -> ColumnSchema {
        match self {
            Kind::Continuous => ColumnSchema::continuous(&[]),
            Kind::Categorical => ColumnSchema::categorical_wide(),
        }
    }

    fn columns(self) -> &'static [&'static str] {
        match self {
            Kind::Continuous => CONTINUOUS_COLUMNS,
            Kind::Categorical => CATEGORICAL_COLUMNS,
        }
    }

    /// Indices of the returned columns in the scan schema.
    fn projection(self) -> Vec<usize> {
        let schema = self.schema();
        self.columns()
            .iter()
            .map(|c| {
                schema
                    .index_of(c)
                    .expect("statistics column in scan schema")
            })
            .collect()
    }

    fn struct_fields(self) -> Fields {
        let schema = self.schema();
        self.projection()
            .into_iter()
            .map(|i| {
                let f = &schema.fields[i];
                Field::new(&f.name, to_arrow_type(f.data_type), true)
            })
            .collect()
    }
}

/// `h3_merge_partials` / `h3_merge_categorical_partials`.
#[derive(Debug)]
pub struct MergePartials {
    kind: Kind,
    signature: Signature,
}

impl MergePartials {
    fn new(kind: Kind) -> Self {
        let binary = [
            DataType::Binary,
            DataType::LargeBinary,
            DataType::BinaryView,
        ]
        .into_iter()
        .map(|t| TypeSignature::Exact(vec![t]))
        .collect();
        Self {
            kind,
            signature: Signature::one_of(binary, Volatility::Immutable),
        }
    }
}

impl PartialEq for MergePartials {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl Eq for MergePartials {}

impl Hash for MergePartials {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
    }
}

impl AggregateUDFImpl for MergePartials {
    fn name(&self) -> &str {
        match self.kind {
            Kind::Continuous => "h3_merge_partials",
            Kind::Categorical => "h3_merge_categorical_partials",
        }
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Struct(self.kind.struct_fields()))
    }

    fn accumulator(&self, _args: AccumulatorArgs) -> Result<Box<dyn Accumulator>> {
        Ok(Box::new(MergeAccumulator {
            kind: self.kind,
            state: State::Empty,
        }))
    }

    fn state_fields(&self, args: StateFieldsArgs) -> Result<Vec<FieldRef>> {
        Ok(vec![Arc::new(Field::new(
            format!("{}[partial_state]", args.name),
            DataType::Binary,
            true,
        ))])
    }
}

#[derive(Debug)]
enum State {
    Empty,
    Continuous(H3Accumulator),
    Categorical(Box<CategoricalAccumulator>),
}

#[derive(Debug)]
struct MergeAccumulator {
    kind: Kind,
    state: State,
}

impl MergeAccumulator {
    /// Merge every non-null encoded state of `array` (any binary type).
    fn merge_states(&mut self, array: &ArrayRef) -> Result<()> {
        let array = cast(array, &DataType::Binary)?;
        let states = array.as_binary::<i32>();
        for i in 0..states.len() {
            if states.is_null(i) {
                continue;
            }
            let bytes = states.value(i);
            match (self.kind, &mut self.state) {
                (Kind::Continuous, State::Continuous(acc)) => {
                    let other: H3Accumulator =
                        decode_partial_state(bytes, STATE_LIMIT).map_err(to_df_error)?;
                    acc.merge(&other);
                }
                (Kind::Continuous, state) => {
                    *state = State::Continuous(
                        decode_partial_state(bytes, STATE_LIMIT).map_err(to_df_error)?,
                    );
                }
                (Kind::Categorical, State::Categorical(acc)) => {
                    let other: CategoricalAccumulator =
                        decode_partial_state(bytes, STATE_LIMIT).map_err(to_df_error)?;
                    acc.merge(&other);
                }
                (Kind::Categorical, state) => {
                    *state = State::Categorical(Box::new(
                        decode_partial_state(bytes, STATE_LIMIT).map_err(to_df_error)?,
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Accumulator for MergeAccumulator {
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        match values.first() {
            Some(array) => self.merge_states(array),
            None => exec_err!("partial-state merge takes one argument"),
        }
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        self.update_batch(states)
    }

    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        let bytes = match &self.state {
            State::Empty => None,
            State::Continuous(acc) => Some(encode_partial_state(acc)),
            State::Categorical(acc) => Some(encode_partial_state(acc.as_ref())),
        };
        Ok(vec![ScalarValue::Binary(bytes)])
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        let fields = self.kind.struct_fields();
        let projection = self.kind.projection();
        // Build the scan's own output row for the merged state, so every statistic is
        // computed exactly as a scan computes it. The cell id does not affect them.
        let batch = match std::mem::replace(&mut self.state, State::Empty) {
            State::Empty => {
                return ScalarValue::try_from(&DataType::Struct(fields));
            }
            State::Continuous(accumulator) => RecordBatch::from_continuous_records(
                &[MultiContinuousRecord {
                    resolution: 0,
                    h3_index: 0,
                    accumulator,
                }],
                &[],
                Some(&projection),
            ),
            State::Categorical(accumulator) => RecordBatch::from_categorical_wide_records(
                &[MultiCategoricalRecord {
                    resolution: 0,
                    h3_index: 0,
                    accumulator: *accumulator,
                }],
                Some(&projection),
            ),
        }
        .map_err(to_df_error)?;
        let columns: Vec<ArrayRef> = batch.columns.into_iter().map(from_column_array).collect();
        // The struct's fields are all nullable; columns may be declared non-null.
        let columns = columns
            .into_iter()
            .zip(fields.iter())
            .map(|(c, f)| cast(&c, f.data_type()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ScalarValue::Struct(Arc::new(StructArray::try_new(
            fields, columns, None,
        )?)))
    }

    fn size(&self) -> usize {
        size_of::<Self>()
            + match &self.state {
                State::Empty => 0,
                State::Continuous(acc) => acc.heap_bytes(),
                State::Categorical(acc) => acc.heap_bytes(),
            }
    }
}
