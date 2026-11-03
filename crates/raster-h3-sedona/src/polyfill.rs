//! `h3_polygon_to_cells(geometry, resolution [, containment])`: fill a polygon with H3 cells.

use std::sync::Arc;

use arrow::array::{Array, AsArray, ListBuilder, UInt64Builder};
use arrow::datatypes::{DataType, Field, FieldRef, Int64Type};
use datafusion::common::{exec_err, plan_err, ScalarValue};
use datafusion::error::Result;
use datafusion::logical_expr::{
    ColumnarValue, ReturnFieldArgs, ScalarFunctionArgs, ScalarUDFImpl, Signature, Volatility,
};
use raster_h3_core::scalar_ops::{
    h3_polygon_to_cells_wkb, parse_containment_mode, ContainmentMode, H3_CONTAINMENT_MODES,
};

use crate::convert::GEOARROW_WKB_EXTENSION;
use crate::spatial::is_planar_lnglat;

const NAME: &str = "h3_polygon_to_cells";

/// `h3_polygon_to_cells(geometry, resolution [, containment])` -> `List<UInt64>`.
///
/// `geometry` is a WKB `POLYGON` / `MULTIPOLYGON` in lon/lat degrees (`Binary`, or a
/// `geoarrow.wkb` field whose CRS is `OGC:CRS84` / `EPSG:4326` or unset). Returns the cells
/// at `resolution` sorted ascending without duplicates, like DuckDB's function of the same
/// name. `containment` is one of `raster_h3_core::scalar_ops::H3_CONTAINMENT_MODES`
/// (default `contains_centroid`; `covers` keeps every cell that overlaps the polygon). A
/// NULL argument gives NULL; an invalid polygon, resolution or mode is an error.
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

        let mut out = ListBuilder::new(UInt64Builder::new());
        for i in 0..geometry.len() {
            if geometry.is_null(i) || resolution.is_null(i) || mode.is_some_and(|m| m.is_null(i)) {
                out.append_null();
                continue;
            }
            let containment = match mode {
                None => ContainmentMode::ContainsCentroid,
                Some(m) => match parse_containment_mode(m.value(i)) {
                    Some(c) => c,
                    None => {
                        let names: Vec<_> = H3_CONTAINMENT_MODES.iter().map(|(n, _)| *n).collect();
                        return exec_err!(
                            "{NAME}: unknown containment mode '{}' (expected one of {})",
                            m.value(i),
                            names.join(", ")
                        );
                    }
                },
            };
            match h3_polygon_to_cells_wkb(geometry.value(i), resolution.value(i), containment) {
                Ok(cells) => {
                    out.values().append_slice(&cells);
                    out.append(true);
                }
                Err(e) => return exec_err!("{NAME}: {e}"),
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
