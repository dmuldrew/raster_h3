//! SQL table functions: `h3_raster_continuous_aggregate(source, resolution, ...)` and
//! `h3_raster_categorical_aggregate(source, resolution, ...)`.
//!
//! `source` is a path, glob or URL accepted by `raster-h3-core`. Every further integer
//! argument is an H3 resolution (default 8), and one optional string argument overrides
//! the raster's CRS (e.g. `'EPSG:4326'`). A string argument holding a partition spec (a JSON
//! object from `h3_raster_plan_partitions`) scans only that partition, adding a
//! `partial_state` column (see [`crate::merge`]). Use [`RasterH3Table`] with a configured
//! provider for the other options (nodata, quantiles, categorical format, ...).
//!
//! `h3_raster_plan_partitions(source [, target_chunks] [, crs])` splits a scan into
//! partitions of about `target_chunks` TIFF chunks (default 256): one row per partition
//! with `partition_index`, `partition_count` and `spec`.

use std::sync::Arc;

use crate::scan::{CategoricalRasterTableProvider, ContinuousRasterTableProvider, ScanPartition};
use arrow::array::{Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::catalog::{TableFunctionArgs, TableFunctionImpl, TableProvider};
use datafusion::common::{plan_err, ScalarValue};
use datafusion::datasource::MemTable;
use datafusion::error::Result;
use datafusion::logical_expr::Expr;

use crate::convert::to_df_error;
use crate::table::RasterH3Table;

/// `h3_raster_continuous_aggregate`
#[derive(Debug, Default)]
pub struct ContinuousAggregateFunction;

impl TableFunctionImpl for ContinuousAggregateFunction {
    fn call_with_args(&self, args: TableFunctionArgs<'_, '_>) -> Result<Arc<dyn TableProvider>> {
        let args = parse_args("h3_raster_continuous_aggregate", args.exprs())?;
        let mut provider = ContinuousRasterTableProvider::try_new(&args.source, args.resolutions)
            .map_err(to_df_error)?;
        if let Some(crs) = args.crs {
            provider = provider.with_crs(crs);
        }
        if let Some(partition) = args.partition {
            provider = provider.with_partition(partition);
        }
        Ok(Arc::new(RasterH3Table::continuous(provider)))
    }
}

/// `h3_raster_categorical_aggregate` (wide format)
#[derive(Debug, Default)]
pub struct CategoricalAggregateFunction;

impl TableFunctionImpl for CategoricalAggregateFunction {
    fn call_with_args(&self, args: TableFunctionArgs<'_, '_>) -> Result<Arc<dyn TableProvider>> {
        let args = parse_args("h3_raster_categorical_aggregate", args.exprs())?;
        let mut provider = CategoricalRasterTableProvider::try_new(&args.source, args.resolutions)
            .map_err(to_df_error)?;
        if let Some(crs) = args.crs {
            provider = provider.with_crs(crs);
        }
        if let Some(partition) = args.partition {
            provider = provider.with_partition(partition);
        }
        Ok(Arc::new(RasterH3Table::categorical(provider)))
    }
}

/// `h3_raster_plan_partitions`
#[derive(Debug, Default)]
pub struct PlanPartitionsFunction;

impl TableFunctionImpl for PlanPartitionsFunction {
    fn call_with_args(&self, args: TableFunctionArgs<'_, '_>) -> Result<Arc<dyn TableProvider>> {
        let name = "h3_raster_plan_partitions";
        let usage = "(source [, target_chunks] [, crs])";
        let exprs = args.exprs();
        let Some(source) = exprs.first().and_then(string_literal) else {
            return plan_err!("{name} expects {usage}");
        };
        let mut target_chunks = 256usize;
        let mut crs = None;
        for e in &exprs[1..] {
            match e {
                Expr::Literal(v, _) if v.data_type().is_integer() => {
                    match v.cast_to(&DataType::Int64)? {
                        ScalarValue::Int64(Some(n)) if n > 0 => target_chunks = n as usize,
                        _ => return plan_err!("{name}: target_chunks must be at least 1, got {v}"),
                    }
                }
                e => match string_literal(e) {
                    Some(s) if crs.is_none() => crs = Some(s.to_string()),
                    _ => return plan_err!("{name} expects {usage}, got {e}"),
                },
            }
        }
        // Plan with the same mosaic options the aggregate functions use, so specs match.
        let mut provider =
            ContinuousRasterTableProvider::try_new(source, vec![8]).map_err(to_df_error)?;
        if let Some(crs) = crs {
            provider = provider.with_crs(crs);
        }
        let plan = provider
            .plan_partitions(target_chunks)
            .map_err(to_df_error)?;
        let schema = Arc::new(Schema::new(vec![
            Field::new("partition_index", DataType::Int64, false),
            Field::new("partition_count", DataType::Int64, false),
            Field::new("spec", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from_iter_values(
                    plan.iter().map(|p| i64::from(p.index)),
                )),
                Arc::new(Int64Array::from_iter_values(
                    plan.iter().map(|p| i64::from(p.count)),
                )),
                Arc::new(StringArray::from_iter_values(
                    plan.iter().map(|p| p.to_json()),
                )),
            ],
        )?;
        Ok(Arc::new(MemTable::try_new(schema, vec![vec![batch]])?))
    }
}

struct Args {
    source: String,
    resolutions: Vec<u8>,
    crs: Option<String>,
    partition: Option<ScanPartition>,
}

fn string_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Literal(
            ScalarValue::Utf8(Some(s))
            | ScalarValue::Utf8View(Some(s))
            | ScalarValue::LargeUtf8(Some(s)),
            _,
        ) => Some(s),
        _ => None,
    }
}

fn parse_args(name: &str, exprs: &[Expr]) -> Result<Args> {
    let usage = "(source [, resolution ...] [, crs] [, partition spec])";
    let Some((first, rest)) = exprs.split_first() else {
        return plan_err!("{name} expects {usage}");
    };
    let Some(source) = string_literal(first) else {
        return plan_err!("{name}: source must be a string literal, got {first}");
    };
    let mut args = Args {
        source: source.to_string(),
        resolutions: Vec::new(),
        crs: None,
        partition: None,
    };
    for e in rest {
        match e {
            Expr::Literal(v, _) if v.data_type().is_integer() => {
                match v.cast_to(&arrow::datatypes::DataType::Int64)? {
                    ScalarValue::Int64(Some(r)) if (0..=15).contains(&r) => {
                        args.resolutions.push(r as u8)
                    }
                    _ => return plan_err!("{name}: resolution must be between 0 and 15, got {v}"),
                }
            }
            e => match string_literal(e) {
                // Partition specs are JSON objects; CRS strings never start with `{`.
                Some(spec) if spec.trim_start().starts_with('{') && args.partition.is_none() => {
                    args.partition = Some(ScanPartition::from_json(spec).map_err(to_df_error)?)
                }
                Some(crs) if args.crs.is_none() => args.crs = Some(crs.to_string()),
                _ => return plan_err!("{name} expects {usage}, got {e}"),
            },
        }
    }
    Ok(args)
}
