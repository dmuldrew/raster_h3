//! Order-independent row comparison between DataFusion plans and core batch readers.

use std::path::PathBuf;

use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::{
    CategoricalBatchReader, CategoricalFormat, ColumnArray, ContinuousBatchReader, RecordBatch,
};
use raster_h3_datafusion::ExecutionPlan;

/// One output row rendered as text, so whole tables compare with `assert_eq!`.
/// Floats keep 12 significant digits: shard merge order may differ between runs.
pub type Row = Vec<String>;

fn cell_text(col: &ColumnArray, i: usize) -> String {
    if let Some(validity) = col.validity() {
        if !validity[i] {
            return "NULL".into();
        }
    }
    match col {
        ColumnArray::UInt64 { values, .. } => values[i].to_string(),
        ColumnArray::Int64 { values, .. } => values[i].to_string(),
        ColumnArray::UInt8 { values, .. } => values[i].to_string(),
        ColumnArray::Float64 { values, .. } => {
            let v = values[i];
            if v == 0.0 {
                "0".into()
            } else {
                format!("{v:.11e}")
            }
        }
        ColumnArray::Boolean { values, .. } => values[i].to_string(),
        ColumnArray::Utf8 { values, .. } => values[i].clone(),
        ColumnArray::Binary { values, .. } => format!("{:?}", values[i]),
    }
}

/// Field names of the first batch followed by every row, sorted.
pub fn table(batches: &[RecordBatch]) -> (Vec<String>, Vec<Row>) {
    let names = batches
        .first()
        .map(|b| b.schema.fields.iter().map(|f| f.name.clone()).collect())
        .unwrap_or_default();
    let mut rows = Vec::new();
    for batch in batches {
        for i in 0..batch.num_rows {
            rows.push(batch.columns.iter().map(|c| cell_text(c, i)).collect());
        }
    }
    rows.sort();
    (names, rows)
}

pub fn plan_table(plan: Box<dyn ExecutionPlan>) -> (Vec<String>, Vec<Row>) {
    let batches: Vec<RecordBatch> = plan
        .execute()
        .unwrap()
        .collect::<raster_h3_core::error::Result<_>>()
        .unwrap();
    table(&batches)
}

pub fn core_continuous(paths: &[PathBuf], cfg: &MultiResolutionConfig) -> (Vec<String>, Vec<Row>) {
    let reader = ContinuousBatchReader::open(paths, cfg, None, 2048, None).unwrap();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    table(&batches)
}

pub fn core_categorical(
    paths: &[PathBuf],
    cfg: &MultiResolutionConfig,
    format: CategoricalFormat,
) -> (Vec<String>, Vec<Row>) {
    let reader = CategoricalBatchReader::open(paths, cfg, format, None, 2048, None).unwrap();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    table(&batches)
}

/// Value of column `name` in every row of `table`, parsed as `f64`.
pub fn column_f64(table: &(Vec<String>, Vec<Row>), name: &str) -> Vec<f64> {
    let idx = table
        .0
        .iter()
        .position(|n| n == name)
        .unwrap_or_else(|| panic!("no column '{name}' in {:?}", table.0));
    table.1.iter().map(|r| r[idx].parse().unwrap()).collect()
}

pub fn column_text<'a>(table: &'a (Vec<String>, Vec<Row>), name: &str) -> Vec<&'a str> {
    let idx = table
        .0
        .iter()
        .position(|n| n == name)
        .unwrap_or_else(|| panic!("no column '{name}' in {:?}", table.0));
    table.1.iter().map(|r| r[idx].as_str()).collect()
}
