//! The `raster_h3` scalar functions as SedonaDB scalar kernels.
//!
//! [`UdfKernel`] runs one of `raster_h3_sedona::scalar_udfs()` (a DataFusion `ScalarUDF`)
//! as a `SedonaScalarKernel`, exported through SedonaDB's C ABI as a
//! `sedonadb_scalar_kernel` capsule.

use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Field};
use datafusion::common::config::ConfigOptions;
use datafusion::error::Result;
use datafusion::logical_expr::{ColumnarValue, ScalarFunctionArgs, ScalarUDF};
use pyo3::prelude::*;
use pyo3::types::{PyCapsule, PyList};
use sedona_expr::scalar_udf::{ScalarKernelRef, SedonaScalarKernel};
use sedona_extension::extension::SedonaCScalarKernel;
use sedona_extension::scalar_kernel::ExportedScalarKernel;
use sedona_schema::crs::lnglat;
use sedona_schema::datatypes::{Edges, SedonaType};

/// A DataFusion UDF run as a SedonaDB kernel.
///
/// Its overloads are whatever the UDF's `coerce_types` accepts. Geometry arguments must be
/// planar lon/lat (`OGC:CRS84` / `EPSG:4326`) or have no CRS, and geometry results are
/// `OGC:CRS84`.
#[derive(Debug)]
struct UdfKernel {
    udf: ScalarUDF,
}

impl UdfKernel {
    /// The storage types to call the UDF with, or `None` if it does not take `args`.
    fn coerced(&self, args: &[SedonaType]) -> Option<Vec<DataType>> {
        let lnglat = lnglat();
        for arg in args {
            match arg {
                SedonaType::Arrow(_) => {}
                SedonaType::Wkb(edges, crs)
                | SedonaType::WkbLarge(edges, crs)
                | SedonaType::WkbView(edges, crs) => {
                    if *edges != Edges::Planar || !(crs.is_none() || *crs == lnglat) {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        let storage: Vec<DataType> = args.iter().map(|a| a.storage_type().clone()).collect();
        self.udf.inner().coerce_types(&storage).ok()
    }

    fn sedona_return_type(&self, coerced: &[DataType]) -> Option<SedonaType> {
        Some(match self.udf.return_type(coerced).ok()? {
            DataType::Binary => SedonaType::Wkb(Edges::Planar, lnglat()),
            other => SedonaType::Arrow(other),
        })
    }

    fn invoke(
        &self,
        arg_types: &[SedonaType],
        args: &[ColumnarValue],
        rows: usize,
    ) -> Result<ColumnarValue> {
        let coerced = self.coerced(arg_types).ok_or_else(|| {
            datafusion::error::DataFusionError::Plan(format!(
                "{}: unsupported argument types {arg_types:?}",
                self.udf.name()
            ))
        })?;
        let args = args
            .iter()
            .zip(&coerced)
            .map(|(a, t)| a.cast_to(t, None))
            .collect::<Result<Vec<_>>>()?;
        let return_type = self.udf.return_type(&coerced)?;
        self.udf.invoke_with_args(ScalarFunctionArgs {
            args,
            arg_fields: coerced
                .iter()
                .enumerate()
                .map(|(i, t)| Arc::new(Field::new(format!("arg{i}"), t.clone(), true)))
                .collect(),
            number_rows: rows,
            return_field: Arc::new(Field::new(self.udf.name(), return_type, true)),
            config_options: Arc::new(ConfigOptions::default()),
        })
    }
}

impl SedonaScalarKernel for UdfKernel {
    fn return_type(&self, args: &[SedonaType]) -> Result<Option<SedonaType>> {
        Ok(self
            .coerced(args)
            .and_then(|coerced| self.sedona_return_type(&coerced)))
    }

    fn invoke_batch(
        &self,
        arg_types: &[SedonaType],
        args: &[ColumnarValue],
    ) -> Result<ColumnarValue> {
        let rows = args
            .iter()
            .find_map(|a| match a {
                ColumnarValue::Array(array) => Some(array.len()),
                ColumnarValue::Scalar(_) => None,
            })
            .unwrap_or(1);
        self.invoke(arg_types, args, rows)
    }

    fn invoke_batch_from_args(
        &self,
        arg_types: &[SedonaType],
        args: &[ColumnarValue],
        _return_type: &SedonaType,
        num_rows: usize,
        _config_options: Option<&ConfigOptions>,
    ) -> Result<ColumnarValue> {
        self.invoke(arg_types, args, num_rows)
    }
}

/// One `raster_h3` SQL function (or alias) for `SedonaContext.register`.
///
/// Implements SedonaDB's `__sedonadb_scalar_udf__` protocol: a list holding one
/// `sedonadb_scalar_kernel` capsule whose kernel accepts every argument type the function
/// does.
#[pyclass(name = "ScalarFunction", module = "raster_h3_sedona", frozen)]
pub(crate) struct PyScalarFunction {
    name: String,
    udf: ScalarUDF,
}

#[pymethods]
impl PyScalarFunction {
    /// The SQL name it registers under.
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    fn __sedonadb_scalar_udf__<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Vec<Bound<'py, PyCapsule>>> {
        let kernel: ScalarKernelRef = Arc::new(UdfKernel {
            udf: self.udf.clone(),
        });
        let kernel: SedonaCScalarKernel = ExportedScalarKernel::from(kernel)
            .with_function_name(&self.name)
            .into();
        Ok(vec![PyCapsule::new_with_value(
            py,
            kernel,
            c"sedonadb_scalar_kernel",
        )?])
    }

    fn __repr__(&self) -> String {
        format!("ScalarFunction({:?})", self.name)
    }
}

/// Every `raster_h3` SQL function, under its name and each alias.
///
/// `SedonaContext.register(scalar_functions())` registers all of them (the
/// `__sedonadb_extension__` protocol); iterate to register a subset.
#[pyclass(name = "ScalarFunctions", module = "raster_h3_sedona", frozen)]
pub(crate) struct PyScalarFunctions {
    functions: Vec<Py<PyScalarFunction>>,
}

#[pymethods]
impl PyScalarFunctions {
    fn __sedonadb_extension__(&self, py: Python<'_>, ctx: &Bound<'_, PyAny>) -> PyResult<()> {
        for f in &self.functions {
            ctx.call_method1("register", (f.clone_ref(py),))?;
        }
        Ok(())
    }

    fn __len__(&self) -> usize {
        self.functions.len()
    }

    fn __iter__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let list = PyList::new(py, self.functions.iter().map(|f| f.clone_ref(py)))?;
        Ok(list.as_any().try_iter()?.into_any())
    }

    /// The SQL names, in registration order.
    #[getter]
    fn names(&self, py: Python<'_>) -> Vec<String> {
        self.functions
            .iter()
            .map(|f| f.borrow(py).name.clone())
            .collect()
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!("ScalarFunctions({:?})", self.names(py))
    }
}

/// The `raster_h3` SQL functions (`h3_polygon_to_cells`, `h3_to_wkb`, `h3_cell_to_parent`,
/// ...) for SedonaDB: `sd.register(raster_h3_sedona.scalar_functions())`.
#[pyfunction]
pub(crate) fn scalar_functions(py: Python<'_>) -> PyResult<PyScalarFunctions> {
    let mut functions = Vec::new();
    for udf in raster_h3_sedona::scalar_udfs() {
        let names = std::iter::once(udf.name().to_string()).chain(udf.aliases().iter().cloned());
        for name in names {
            functions.push(Py::new(
                py,
                PyScalarFunction {
                    name,
                    udf: udf.clone(),
                },
            )?);
        }
    }
    Ok(PyScalarFunctions { functions })
}
