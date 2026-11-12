//! Vectorized DataFusion `ScalarUDFImpl` implementations for H3 cell operations.
//!
//! Operates directly on [`ColumnArray`] batches with full null-validity propagation.

use raster_h3_core::columnar::{ArrowDataType, ColumnArray};
use raster_h3_core::encoding::{fast_hex_u64, h3_index_to_wkb, parse_hex_u64, WkbBuf, WKB_BUF_LEN};
use raster_h3_core::error::{RasterH3Error, Result};
use raster_h3_core::scalar_ops::{
    h3_cell_to_parent_str, h3_cell_to_parent_u64, h3_get_resolution, h3_is_valid_str,
    h3_is_valid_u64, h3_to_lat, h3_to_lng,
};

/// Trait modeling DataFusion's `ScalarUDFImpl` for vectorized columnar execution.
pub trait ScalarUDFImpl: Send + Sync {
    /// Canonical function name in SQL.
    fn name(&self) -> &str;

    /// Optional SQL aliases (e.g. `h3_cell_to_geometry` for `h3_to_geometry`).
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// Resolve return type given input argument types.
    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType>;

    /// Execute the scalar function over a slice of columnar argument arrays.
    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray>;
}

/// `h3_to_string(UBIGINT) -> VARCHAR`
pub struct H3ToStringUdf;

impl ScalarUDFImpl for H3ToStringUdf {
    fn name(&self) -> &str {
        "h3_to_string"
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        if arg_types != [ArrowDataType::UInt64] {
            return Err(RasterH3Error::InvalidParameter(
                "h3_to_string expects (UInt64)".into(),
            ));
        }
        Ok(ArrowDataType::Utf8)
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        let Some(ColumnArray::UInt64 { values, validity }) = args.first() else {
            return Err(RasterH3Error::InvalidParameter(
                "h3_to_string requires a UInt64 column".into(),
            ));
        };
        let n = values.len();
        let mut out_vals = Vec::with_capacity(n);
        let mut out_valid = vec![true; n];
        let mut has_null = false;
        let mut buf = [0u8; 16];

        for i in 0..n {
            let is_valid = validity.as_ref().map(|v| v[i]).unwrap_or(true);
            if !is_valid || values[i] == 0 {
                out_vals.push(String::new());
                out_valid[i] = false;
                has_null = true;
            } else {
                let bytes = fast_hex_u64(values[i], &mut buf);
                out_vals.push(std::str::from_utf8(bytes).unwrap_or_default().to_owned());
            }
        }
        Ok(ColumnArray::Utf8 {
            values: out_vals,
            validity: if has_null { Some(out_valid) } else { None },
        })
    }
}

/// `string_to_h3(VARCHAR) -> UBIGINT`
pub struct StringToH3Udf;

impl ScalarUDFImpl for StringToH3Udf {
    fn name(&self) -> &str {
        "string_to_h3"
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        if arg_types != [ArrowDataType::Utf8] {
            return Err(RasterH3Error::InvalidParameter(
                "string_to_h3 expects (Utf8)".into(),
            ));
        }
        Ok(ArrowDataType::UInt64)
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        let Some(ColumnArray::Utf8 { values, validity }) = args.first() else {
            return Err(RasterH3Error::InvalidParameter(
                "string_to_h3 requires a Utf8 column".into(),
            ));
        };
        let out_vals: Vec<u64> = values
            .iter()
            .map(|s| parse_hex_u64(s).unwrap_or(0))
            .collect();
        Ok(ColumnArray::UInt64 {
            values: out_vals,
            validity: validity.clone(),
        })
    }
}

/// `h3_to_lat(UBIGINT) -> DOUBLE`
pub struct H3ToLatUdf;

impl ScalarUDFImpl for H3ToLatUdf {
    fn name(&self) -> &str {
        "h3_to_lat"
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        if arg_types != [ArrowDataType::UInt64] {
            return Err(RasterH3Error::InvalidParameter(
                "h3_to_lat expects (UInt64)".into(),
            ));
        }
        Ok(ArrowDataType::Float64)
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        let Some(ColumnArray::UInt64 { values, validity }) = args.first() else {
            return Err(RasterH3Error::InvalidParameter(
                "h3_to_lat requires a UInt64 column".into(),
            ));
        };
        let out_vals: Vec<f64> = values.iter().map(|&c| h3_to_lat(c)).collect();
        Ok(ColumnArray::Float64 {
            values: out_vals,
            validity: validity.clone(),
        })
    }
}

/// `h3_to_lng(UBIGINT) -> DOUBLE`
pub struct H3ToLngUdf;

impl ScalarUDFImpl for H3ToLngUdf {
    fn name(&self) -> &str {
        "h3_to_lng"
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        if arg_types != [ArrowDataType::UInt64] {
            return Err(RasterH3Error::InvalidParameter(
                "h3_to_lng expects (UInt64)".into(),
            ));
        }
        Ok(ArrowDataType::Float64)
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        let Some(ColumnArray::UInt64 { values, validity }) = args.first() else {
            return Err(RasterH3Error::InvalidParameter(
                "h3_to_lng requires a UInt64 column".into(),
            ));
        };
        let out_vals: Vec<f64> = values.iter().map(|&c| h3_to_lng(c)).collect();
        Ok(ColumnArray::Float64 {
            values: out_vals,
            validity: validity.clone(),
        })
    }
}

/// `h3_get_resolution(UBIGINT) -> BIGINT`
pub struct H3GetResolutionUdf;

impl ScalarUDFImpl for H3GetResolutionUdf {
    fn name(&self) -> &str {
        "h3_get_resolution"
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        if arg_types != [ArrowDataType::UInt64] {
            return Err(RasterH3Error::InvalidParameter(
                "h3_get_resolution expects (UInt64)".into(),
            ));
        }
        Ok(ArrowDataType::Int64)
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        let Some(ColumnArray::UInt64 { values, validity }) = args.first() else {
            return Err(RasterH3Error::InvalidParameter(
                "h3_get_resolution requires a UInt64 column".into(),
            ));
        };
        let out_vals: Vec<i64> = values.iter().map(|&c| h3_get_resolution(c)).collect();
        Ok(ColumnArray::Int64 {
            values: out_vals,
            validity: validity.clone(),
        })
    }
}

/// `h3_is_valid(UBIGINT | VARCHAR) -> BOOLEAN`
pub struct H3IsValidUdf;

impl ScalarUDFImpl for H3IsValidUdf {
    fn name(&self) -> &str {
        "h3_is_valid"
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        match arg_types {
            [ArrowDataType::UInt64] | [ArrowDataType::Utf8] => Ok(ArrowDataType::Boolean),
            _ => Err(RasterH3Error::InvalidParameter(
                "h3_is_valid expects (UInt64) or (Utf8)".into(),
            )),
        }
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        match args.first() {
            Some(ColumnArray::UInt64 { values, validity }) => {
                let out_vals: Vec<bool> = values.iter().map(|&c| h3_is_valid_u64(c)).collect();
                Ok(ColumnArray::Boolean {
                    values: out_vals,
                    validity: validity.clone(),
                })
            }
            Some(ColumnArray::Utf8 { values, validity }) => {
                let out_vals: Vec<bool> = values.iter().map(|s| h3_is_valid_str(s)).collect();
                Ok(ColumnArray::Boolean {
                    values: out_vals,
                    validity: validity.clone(),
                })
            }
            _ => Err(RasterH3Error::InvalidParameter(
                "h3_is_valid requires a UInt64 or Utf8 column".into(),
            )),
        }
    }
}

/// `h3_to_wkb(UBIGINT | VARCHAR) -> BINARY` (also aliased as `h3_to_geometry`, `h3_cell_to_geometry`)
pub struct H3ToWkbUdf;

impl ScalarUDFImpl for H3ToWkbUdf {
    fn name(&self) -> &str {
        "h3_to_wkb"
    }

    fn aliases(&self) -> &[&str] {
        &["h3_to_geometry", "h3_cell_to_geometry"]
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        match arg_types {
            [ArrowDataType::UInt64] | [ArrowDataType::Utf8] => Ok(ArrowDataType::Binary),
            _ => Err(RasterH3Error::InvalidParameter(
                "h3_to_wkb expects (UInt64) or (Utf8)".into(),
            )),
        }
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        let mut wkb_buf: WkbBuf = [0u8; WKB_BUF_LEN];
        match args.first() {
            Some(ColumnArray::UInt64 { values, validity }) => {
                let n = values.len();
                let mut out_vals = Vec::with_capacity(n);
                let mut out_valid = vec![true; n];
                let mut has_null = false;
                for i in 0..n {
                    let is_valid = validity.as_ref().map(|v| v[i]).unwrap_or(true);
                    if is_valid {
                        if let Some(len) = h3_index_to_wkb(values[i], &mut wkb_buf) {
                            out_vals.push(wkb_buf[..len].to_vec());
                            continue;
                        }
                    }
                    out_vals.push(Vec::new());
                    out_valid[i] = false;
                    has_null = true;
                }
                Ok(ColumnArray::Binary {
                    values: out_vals,
                    validity: if has_null { Some(out_valid) } else { None },
                })
            }
            Some(ColumnArray::Utf8 { values, validity }) => {
                let n = values.len();
                let mut out_vals = Vec::with_capacity(n);
                let mut out_valid = vec![true; n];
                let mut has_null = false;
                for i in 0..n {
                    let is_valid = validity.as_ref().map(|v| v[i]).unwrap_or(true);
                    if is_valid {
                        if let Some(cell) = parse_hex_u64(&values[i]) {
                            if let Some(len) = h3_index_to_wkb(cell, &mut wkb_buf) {
                                out_vals.push(wkb_buf[..len].to_vec());
                                continue;
                            }
                        }
                    }
                    out_vals.push(Vec::new());
                    out_valid[i] = false;
                    has_null = true;
                }
                Ok(ColumnArray::Binary {
                    values: out_vals,
                    validity: if has_null { Some(out_valid) } else { None },
                })
            }
            _ => Err(RasterH3Error::InvalidParameter(
                "h3_to_wkb requires a UInt64 or Utf8 column".into(),
            )),
        }
    }
}

/// `h3_cell_to_parent(UBIGINT | VARCHAR, BIGINT) -> UBIGINT | VARCHAR`
pub struct H3CellToParentUdf;

impl ScalarUDFImpl for H3CellToParentUdf {
    fn name(&self) -> &str {
        "h3_cell_to_parent"
    }

    fn return_type(&self, arg_types: &[ArrowDataType]) -> Result<ArrowDataType> {
        match arg_types {
            [ArrowDataType::UInt64, ArrowDataType::Int64] => Ok(ArrowDataType::UInt64),
            [ArrowDataType::Utf8, ArrowDataType::Int64] => Ok(ArrowDataType::Utf8),
            _ => Err(RasterH3Error::InvalidParameter(
                "h3_cell_to_parent expects (UInt64, Int64) or (Utf8, Int64)".into(),
            )),
        }
    }

    fn invoke(&self, args: &[ColumnArray]) -> Result<ColumnArray> {
        if args.len() != 2 {
            return Err(RasterH3Error::InvalidParameter(
                "h3_cell_to_parent requires 2 arguments".into(),
            ));
        }
        let ColumnArray::Int64 {
            values: res_vals,
            validity: res_valid,
        } = &args[1]
        else {
            return Err(RasterH3Error::InvalidParameter(
                "h3_cell_to_parent second argument must be Int64".into(),
            ));
        };

        match &args[0] {
            ColumnArray::UInt64 { values, validity } => {
                let n = values.len();
                let mut out_vals = Vec::with_capacity(n);
                let mut out_valid = vec![true; n];
                let mut has_null = false;
                for i in 0..n {
                    let v0 = validity.as_ref().map(|v| v[i]).unwrap_or(true);
                    let v1 = res_valid.as_ref().map(|v| v[i]).unwrap_or(true);
                    if v0 && v1 {
                        if let Some(p) = h3_cell_to_parent_u64(values[i], res_vals[i]) {
                            out_vals.push(p);
                            continue;
                        }
                    }
                    out_vals.push(0);
                    out_valid[i] = false;
                    has_null = true;
                }
                Ok(ColumnArray::UInt64 {
                    values: out_vals,
                    validity: if has_null { Some(out_valid) } else { None },
                })
            }
            ColumnArray::Utf8 { values, validity } => {
                let n = values.len();
                let mut out_vals = Vec::with_capacity(n);
                let mut out_valid = vec![true; n];
                let mut has_null = false;
                for i in 0..n {
                    let v0 = validity.as_ref().map(|v| v[i]).unwrap_or(true);
                    let v1 = res_valid.as_ref().map(|v| v[i]).unwrap_or(true);
                    if v0 && v1 {
                        if let Some(p) = h3_cell_to_parent_str(&values[i], res_vals[i]) {
                            out_vals.push(p);
                            continue;
                        }
                    }
                    out_vals.push(String::new());
                    out_valid[i] = false;
                    has_null = true;
                }
                Ok(ColumnArray::Utf8 {
                    values: out_vals,
                    validity: if has_null { Some(out_valid) } else { None },
                })
            }
            _ => Err(RasterH3Error::InvalidParameter(
                "h3_cell_to_parent first argument must be UInt64 or Utf8".into(),
            )),
        }
    }
}
