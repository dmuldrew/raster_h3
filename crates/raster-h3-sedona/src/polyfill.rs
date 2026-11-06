//! `h3_polygon_to_cells(geometry, resolution [, containment])`: fill a polygon with H3 cells.

use std::sync::Arc;

use arrow::array::{Array, AsArray, ListBuilder, UInt64Builder};
use arrow::datatypes::{DataType, Field, FieldRef, Int64Type};
use datafusion::common::{exec_err, plan_err, DataFusionError, ScalarValue};
use datafusion::error::Result;
use datafusion::logical_expr::{
    ColumnarValue, ReturnFieldArgs, ScalarFunctionArgs, ScalarUDFImpl, Signature, Volatility,
};
use raster_h3_core::scalar_ops::{
    h3_polygon_to_cells_wkb, parse_containment_mode, ContainmentMode, PolygonToCellsError,
    H3_CONTAINMENT_MODES,
};
use rayon::prelude::*;

use crate::convert::GEOARROW_WKB_EXTENSION;
use crate::spatial::is_planar_lnglat;

const NAME: &str = "h3_polygon_to_cells";

/// `h3_polygon_to_cells(geometry, resolution [, containment])` -> `List<UInt64>`.
///
/// `geometry` is a WKB `POLYGON` / `MULTIPOLYGON` in lon/lat degrees (`Binary`, or a
/// `geoarrow.wkb` field whose CRS is `OGC:CRS84` / `EPSG:4326` or unset). Returns the cells
/// at `resolution` sorted ascending without duplicates, like DuckDB's function of the same
/// name. `containment` is one of `raster_h3_core::scalar_ops::H3_CONTAINMENT_MODES`
/// (default `contains_centroid`; `covers` keeps every cell that overlaps the polygon).
/// NULLs and errors follow DuckDB: a NULL argument, a geometry that is not a valid
/// `POLYGON` / `MULTIPOLYGON`, or a resolution outside `0..=15` gives NULL; an unknown
/// containment name or a polygon above the 10,000,000-cell limit fails the query.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct H3PolygonToCells {
    signature: Signature,
}

impl Default for H3PolygonToCells {
    fn default() -> Self {
        Self {
            signature: Signature::user_defined(Volatility::Immutable),
        }
    }
}

fn list_type() -> DataType {
    DataType::List(Arc::new(Field::new_list_field(DataType::UInt64, true)))
}

impl ScalarUDFImpl for H3PolygonToCells {
    fn name(&self) -> &str {
        NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        let usage = "expects (geometry, resolution [, containment])";
        if !(2..=3).contains(&arg_types.len()) {
            return plan_err!("{NAME} {usage}");
        }
        arg_types
            .iter()
            .enumerate()
            .map(|(i, t)| match (i, t) {
                (_, DataType::Null) => {
                    Ok([DataType::Binary, DataType::Int64, DataType::Utf8][i].clone())
                }
                (0, DataType::Binary | DataType::LargeBinary | DataType::BinaryView) => {
                    Ok(DataType::Binary)
                }
                (1, t) if t.is_integer() => Ok(DataType::Int64),
                (2, DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View) => {
                    Ok(DataType::Utf8)
                }
                (_, t) => plan_err!("{NAME} {usage}, got {t} for argument {}", i + 1),
            })
            .collect()
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(list_type())
    }

    fn return_field_from_args(&self, args: ReturnFieldArgs<'_>) -> Result<FieldRef> {
        // A geometry tagged with another CRS would be filled in the wrong coordinates.
        if let Some(geometry) = args.arg_fields.first() {
            let metadata = geometry.metadata();
            if metadata.get("ARROW:extension:name").map(String::as_str)
                == Some(GEOARROW_WKB_EXTENSION)
            {
                let crs = metadata
                    .get("ARROW:extension:metadata")
                    .map_or("", String::as_str);
                if !crs.is_empty() && crs != "{}" && !is_planar_lnglat(crs) {
                    return plan_err!(
                        "{NAME}: geometry must be lon/lat (OGC:CRS84 / EPSG:4326) with planar \
                         edges, got {crs}"
                    );
                }
            }
        }
        Ok(Arc::new(Field::new(NAME, list_type(), true)))
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let all_scalar = args
            .args
            .iter()
            .all(|a| matches!(a, ColumnarValue::Scalar(_)));
        let arrays = ColumnarValue::values_to_arrays(&args.args)?;
        let geometry = arrays[0].as_binary::<i32>();
        let resolution = arrays[1].as_primitive::<Int64Type>();
        let mode = arrays.get(2).map(|a| a.as_string::<i32>());

        // Polyfilling one row is far more work than scheduling it, so rows run in parallel on
        // the rayon pool (a batch of tracts typically arrives as one partition). Results are
        // assembled in row order, so the output and the first error reported are the same as
        // a sequential pass.
        let fill_row = |i: usize| -> Result<Option<Vec<u64>>> {
            if geometry.is_null(i) || resolution.is_null(i) || mode.is_some_and(|m| m.is_null(i)) {
                return Ok(None);
            }
            let containment = match mode {
                None => ContainmentMode::ContainsCentroid,
                Some(m) => parse_containment_mode(m.value(i)).ok_or_else(|| {
                    let names: Vec<_> = H3_CONTAINMENT_MODES.iter().map(|(n, _)| *n).collect();
                    DataFusionError::Execution(format!(
                        "{NAME}: unknown containment mode '{}' (expected one of {})",
                        m.value(i),
                        names.join(", ")
                    ))
                })?,
            };
            match h3_polygon_to_cells_wkb(geometry.value(i), resolution.value(i), containment) {
                Ok(cells) => Ok(Some(cells)),
                // As in DuckDB: a row that is not a polygon, or an out-of-range resolution,
                // gives NULL; only the cell limit fails the query.
                Err(
                    PolygonToCellsError::InvalidGeometry(_)
                    | PolygonToCellsError::InvalidResolution(_),
                ) => Ok(None),
                Err(e) => exec_err!("{NAME}: {e}"),
            }
        };
        let rows: Vec<Result<Option<Vec<u64>>>> = if geometry.len() > 1 {
            (0..geometry.len()).into_par_iter().map(fill_row).collect()
        } else {
            (0..geometry.len()).map(fill_row).collect()
        };

        let total = rows
            .iter()
            .map(|r| r.as_ref().map_or(0, |c| c.as_ref().map_or(0, Vec::len)))
            .sum();
        let mut out = ListBuilder::with_capacity(UInt64Builder::with_capacity(total), rows.len());
        for row in rows {
            match row? {
                Some(cells) => {
                    out.values().append_slice(&cells);
                    out.append(true);
                }
                None => out.append_null(),
            }
        }
        let out: Arc<dyn Array> = Arc::new(out.finish());
        if all_scalar {
            Ok(ColumnarValue::Scalar(ScalarValue::try_from_array(&out, 0)?))
        } else {
            Ok(ColumnarValue::Array(out))
        }
    }
}
