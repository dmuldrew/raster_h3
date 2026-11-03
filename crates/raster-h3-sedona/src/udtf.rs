//! SQL table functions: `h3_raster_continuous_aggregate(source, resolution, ...)` and
//! `h3_raster_categorical_aggregate(source, resolution, ...)`.
//!
//! `source` is a path, glob or URL accepted by `raster-h3-core`. Every further integer
//! argument is an H3 resolution (default 8), and one optional string argument overrides
//! the raster's CRS (e.g. `'EPSG:4326'`). Use [`RasterH3Table`] with a configured provider
//! for the other options (nodata, quantiles, categorical format, ...).

use std::sync::Arc;

use datafusion::catalog::{TableFunctionArgs, TableFunctionImpl, TableProvider};
use datafusion::common::{plan_err, ScalarValue};
use datafusion::error::Result;
use datafusion::logical_expr::Expr;
use raster_h3_datafusion::{CategoricalRasterTableProvider, ContinuousRasterTableProvider};

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
        Ok(Arc::new(RasterH3Table::categorical(provider)))
    }
}

struct Args {
    source: String,
    resolutions: Vec<u8>,
    crs: Option<String>,
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
    let usage = "(source [, resolution ...] [, crs])";
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
                Some(crs) if args.crs.is_none() => args.crs = Some(crs.to_string()),
                _ => return plan_err!("{name} expects {usage}, got {e}"),
            },
        }
    }
    Ok(args)
}
