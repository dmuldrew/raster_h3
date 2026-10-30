//! The `raster_h3` H3 scalar functions as DataFusion `ScalarUDF`s.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use arrow::datatypes::{DataType, Field, FieldRef};
use datafusion::common::{plan_err, ScalarValue};
use datafusion::error::Result;
use datafusion::logical_expr::{
    ColumnarValue, ReturnFieldArgs, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature,
    Volatility,
};
use raster_h3_core::columnar::ArrowDataType;
use raster_h3_datafusion::default_scalar_udfs;

use crate::convert::{from_column_array, geometry_field, to_arrow_type, to_column_array};

/// All `raster_h3` scalar functions, ready for `SessionContext::register_udf`.
pub fn scalar_udfs() -> Vec<ScalarUDF> {
    default_scalar_udfs()
        .into_iter()
        .map(|inner| ScalarUDF::new_from_impl(RasterH3ScalarUdf::new(inner)))
        .collect()
}

/// Adapts a `raster-h3-datafusion` scalar UDF to DataFusion's `ScalarUDFImpl`.
///
/// Arguments are coerced to the types the core functions take (`UInt64` cells, `Utf8`
/// hex strings, `Int64` resolutions), so SQL integer and string literals work directly.
/// Functions returning WKB return a `geoarrow.wkb` field.
pub struct RasterH3ScalarUdf {
    inner: Arc<dyn raster_h3_datafusion::ScalarUDFImpl>,
    aliases: Vec<String>,
    signature: Signature,
}

impl RasterH3ScalarUdf {
    pub fn new(inner: Arc<dyn raster_h3_datafusion::ScalarUDFImpl>) -> Self {
        let aliases = inner.aliases().iter().map(|a| a.to_string()).collect();
        Self {
            inner,
            aliases,
            signature: Signature::user_defined(Volatility::Immutable),
        }
    }

    fn core_return_type(&self, arg_types: &[DataType]) -> Result<ArrowDataType> {
        let core_types = arg_types
            .iter()
            .map(|t| match t {
                DataType::UInt64 => Ok(ArrowDataType::UInt64),
                DataType::Int64 => Ok(ArrowDataType::Int64),
                DataType::Utf8 => Ok(ArrowDataType::Utf8),
                other => plan_err!("{}: unsupported argument type {other}", self.name()),
            })
            .collect::<Result<Vec<_>>>()?;
        self.inner
            .return_type(&core_types)
            .or_else(|e| plan_err!("{e}"))
    }
}

impl PartialEq for RasterH3ScalarUdf {
    fn eq(&self, other: &Self) -> bool {
        self.inner.name() == other.inner.name()
    }
}

impl Eq for RasterH3ScalarUdf {}

impl Hash for RasterH3ScalarUdf {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.name().hash(state);
    }
}

impl std::fmt::Debug for RasterH3ScalarUdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RasterH3ScalarUdf")
            .field("name", &self.inner.name())
            .finish_non_exhaustive()
    }
}

impl ScalarUDFImpl for RasterH3ScalarUdf {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        let coerced = arg_types
            .iter()
            .enumerate()
            .map(|(i, t)| match t {
                // The first argument is a cell; later integers are resolutions.
                t if t.is_integer() || t.is_null() => Ok(if i == 0 {
                    DataType::UInt64
                } else {
                    DataType::Int64
                }),
                DataType::Utf8 | DataType::Utf8View | DataType::LargeUtf8 => Ok(DataType::Utf8),
                other => plan_err!("{}: unsupported argument type {other}", self.name()),
            })
            .collect::<Result<Vec<_>>>()?;
        // Reject wrong arity / combinations at planning time.
        self.core_return_type(&coerced)?;
        Ok(coerced)
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        Ok(to_arrow_type(self.core_return_type(arg_types)?))
    }

    fn return_field_from_args(&self, args: ReturnFieldArgs<'_>) -> Result<FieldRef> {
        let arg_types: Vec<DataType> = args
            .arg_fields
            .iter()
            .map(|f| f.data_type().clone())
            .collect();
        let return_type = self.return_type(&arg_types)?;
        Ok(Arc::new(if return_type == DataType::Binary {
            geometry_field(self.name(), true)
        } else {
            Field::new(self.name(), return_type, true)
        }))
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let all_scalar = args
            .args
            .iter()
            .all(|a| matches!(a, ColumnarValue::Scalar(_)));
        let arrays = ColumnarValue::values_to_arrays(&args.args)?;
        let columns = arrays
            .iter()
            .map(to_column_array)
            .collect::<Result<Vec<_>>>()?;
        let out = self
            .inner
            .invoke(&columns)
            .map_err(crate::convert::to_df_error)?;
        let out = from_column_array(out);
        if all_scalar {
            Ok(ColumnarValue::Scalar(ScalarValue::try_from_array(&out, 0)?))
        } else {
            Ok(ColumnarValue::Array(out))
        }
    }
}
