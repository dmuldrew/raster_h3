//! Binary Columnar IPC Stream (`write_ipc_stream` / `read_ipc_stream`) and JSONL
//! (`JSONEachRow`) streaming serializers for [`RecordBatch`].
//!
//! Used by `raster-h3-cli` (for ClickHouse executable table functions and shell pipelines)
//! and `raster-h3-python` for zero-dependency binary batch exchange.

use std::io::{Read, Write};
use std::sync::Arc;

use crate::columnar::{ArrowDataType, ColumnArray, ColumnField, ColumnSchema, RecordBatch};
use crate::error::{RasterH3Error, Result};

const IPC_MAGIC: &[u8; 8] = b"RH3IPC01";

fn write_u8<W: Write>(w: &mut W, v: u8) -> Result<()> {
    w.write_all(&[v])?;
    Ok(())
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn write_u32<W: Write>(w: &mut W, v: u32) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn write_u64<W: Write>(w: &mut W, v: u64) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn encode_dtype(dt: ArrowDataType) -> u8 {
    match dt {
        ArrowDataType::UInt64 => 1,
        ArrowDataType::Int64 => 2,
        ArrowDataType::UInt8 => 3,
        ArrowDataType::Float64 => 4,
        ArrowDataType::Boolean => 5,
        ArrowDataType::Utf8 => 6,
        ArrowDataType::Binary => 7,
    }
}

fn decode_dtype(tag: u8) -> Result<ArrowDataType> {
    match tag {
        1 => Ok(ArrowDataType::UInt64),
        2 => Ok(ArrowDataType::Int64),
        3 => Ok(ArrowDataType::UInt8),
        4 => Ok(ArrowDataType::Float64),
        5 => Ok(ArrowDataType::Boolean),
        6 => Ok(ArrowDataType::Utf8),
        7 => Ok(ArrowDataType::Binary),
        other => Err(RasterH3Error::InvalidParameter(format!(
            "Unknown IPC data type tag: {}",
            other
        ))),
    }
}

fn write_validity<W: Write>(w: &mut W, validity: &Option<Vec<bool>>) -> Result<()> {
    match validity {
        None => write_u8(w, 0),
        Some(v) => {
            write_u8(w, 1)?;
            write_u32(w, v.len() as u32)?;
            let bytes: Vec<u8> = v.iter().map(|&b| u8::from(b)).collect();
            w.write_all(&bytes)?;
            Ok(())
        }
    }
}

fn read_validity<R: Read>(r: &mut R) -> Result<Option<Vec<bool>>> {
    let has = read_u8(r)?;
    if has == 0 {
        return Ok(None);
    }
    let len = read_u32(r)? as usize;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    Ok(Some(bytes.into_iter().map(|b| b != 0).collect()))
}

/// Write a stream of [`RecordBatch`]es in binary columnar IPC format to `writer`.
pub fn write_ipc_stream<W, I>(writer: &mut W, schema: &ColumnSchema, batches: I) -> Result<usize>
where
    W: Write,
    I: IntoIterator<Item = Result<RecordBatch>>,
{
    writer.write_all(IPC_MAGIC)?;
    write_u32(writer, schema.fields.len() as u32)?;
    for field in &schema.fields {
        let name_bytes = field.name.as_bytes();
        write_u32(writer, name_bytes.len() as u32)?;
        writer.write_all(name_bytes)?;
        write_u8(writer, encode_dtype(field.data_type))?;
        write_u8(writer, u8::from(field.nullable))?;
    }

    let mut total_rows = 0usize;
    for batch_res in batches {
        let batch = batch_res?;
        if batch.num_rows == 0 {
            continue;
        }
        write_u8(writer, 1)?; // RecordBatch marker
        write_u32(writer, batch.num_rows as u32)?;
        for col in &batch.columns {
            write_validity(writer, &col.validity().map(|s| s.to_vec()))?;
            match col {
                ColumnArray::UInt64 { values, .. } => {
                    for &v in values {
                        write_u64(writer, v)?;
                    }
                }
                ColumnArray::Int64 { values, .. } => {
                    for &v in values {
                        writer.write_all(&v.to_le_bytes())?;
                    }
                }
                ColumnArray::UInt8 { values, .. } => {
                    writer.write_all(values)?;
                }
                ColumnArray::Float64 { values, .. } => {
                    for &v in values {
                        writer.write_all(&v.to_le_bytes())?;
                    }
                }
                ColumnArray::Boolean { values, .. } => {
                    let bytes: Vec<u8> = values.iter().map(|&b| u8::from(b)).collect();
                    writer.write_all(&bytes)?;
                }
                ColumnArray::Utf8 { values, .. } => {
                    for s in values {
                        let b = s.as_bytes();
                        write_u32(writer, b.len() as u32)?;
                        writer.write_all(b)?;
                    }
                }
                ColumnArray::Binary { values, .. } => {
                    for b in values {
                        write_u32(writer, b.len() as u32)?;
                        writer.write_all(b)?;
                    }
                }
            }
        }
        total_rows += batch.num_rows;
    }

    write_u8(writer, 0)?; // End-of-stream marker
    writer.flush()?;
    Ok(total_rows)
}

/// Read a binary columnar IPC stream from `reader` into `(Arc<ColumnSchema>, Vec<RecordBatch>)`.
pub fn read_ipc_stream<R: Read>(reader: &mut R) -> Result<(Arc<ColumnSchema>, Vec<RecordBatch>)> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != IPC_MAGIC {
        return Err(RasterH3Error::InvalidParameter(
            "Invalid IPC stream magic header".into(),
        ));
    }

    let num_fields = read_u32(reader)? as usize;
    let mut fields = Vec::with_capacity(num_fields);
    for _ in 0..num_fields {
        let name_len = read_u32(reader)? as usize;
        let mut name_bytes = vec![0u8; name_len];
        reader.read_exact(&mut name_bytes)?;
        let name = String::from_utf8(name_bytes).map_err(|e| {
            RasterH3Error::InvalidParameter(format!("Invalid UTF-8 field name in IPC: {e}"))
        })?;
        let dt = decode_dtype(read_u8(reader)?)?;
        let nullable = read_u8(reader)? != 0;
        fields.push(ColumnField::new(name, dt, nullable));
    }
    let schema = Arc::new(ColumnSchema::new(fields));

    let mut batches = Vec::new();
    loop {
        let marker = read_u8(reader)?;
        if marker == 0 {
            break;
        }
        if marker != 1 {
            return Err(RasterH3Error::InvalidParameter(format!(
                "Invalid IPC batch marker: {}",
                marker
            )));
        }
        let num_rows = read_u32(reader)? as usize;
        let mut columns = Vec::with_capacity(schema.fields.len());

        for field in &schema.fields {
            let validity = read_validity(reader)?;
            let col = match field.data_type {
                ArrowDataType::UInt64 => {
                    let mut values = Vec::with_capacity(num_rows);
                    for _ in 0..num_rows {
                        values.push(read_u64(reader)?);
                    }
                    ColumnArray::UInt64 { values, validity }
                }
                ArrowDataType::Int64 => {
                    let mut values = Vec::with_capacity(num_rows);
                    for _ in 0..num_rows {
                        let mut b = [0u8; 8];
                        reader.read_exact(&mut b)?;
                        values.push(i64::from_le_bytes(b));
                    }
                    ColumnArray::Int64 { values, validity }
                }
                ArrowDataType::UInt8 => {
                    let mut values = vec![0u8; num_rows];
                    reader.read_exact(&mut values)?;
                    ColumnArray::UInt8 { values, validity }
                }
                ArrowDataType::Float64 => {
                    let mut values = Vec::with_capacity(num_rows);
                    for _ in 0..num_rows {
                        let mut b = [0u8; 8];
                        reader.read_exact(&mut b)?;
                        values.push(f64::from_le_bytes(b));
                    }
                    ColumnArray::Float64 { values, validity }
                }
                ArrowDataType::Boolean => {
                    let mut bytes = vec![0u8; num_rows];
                    reader.read_exact(&mut bytes)?;
                    let values = bytes.into_iter().map(|b| b != 0).collect();
                    ColumnArray::Boolean { values, validity }
                }
                ArrowDataType::Utf8 => {
                    let mut values = Vec::with_capacity(num_rows);
                    for _ in 0..num_rows {
                        let slen = read_u32(reader)? as usize;
                        let mut sb = vec![0u8; slen];
                        reader.read_exact(&mut sb)?;
                        values.push(String::from_utf8_lossy(&sb).into_owned());
                    }
                    ColumnArray::Utf8 { values, validity }
                }
                ArrowDataType::Binary => {
                    let mut values = Vec::with_capacity(num_rows);
                    for _ in 0..num_rows {
                        let blen = read_u32(reader)? as usize;
                        let mut bb = vec![0u8; blen];
                        reader.read_exact(&mut bb)?;
                        values.push(bb);
                    }
                    ColumnArray::Binary { values, validity }
                }
            };
            columns.push(col);
        }
        batches.push(RecordBatch::try_new(Arc::clone(&schema), columns)?);
    }

    Ok((schema, batches))
}

/// Write a stream of [`RecordBatch`]es as newline-delimited JSON (`JSONEachRow` / NDJSON),
/// ideal for ClickHouse executable table functions (`FORMAT JSONEachRow`) and CLI piping.
pub fn write_jsonl_stream<W, I>(writer: &mut W, batches: I) -> Result<usize>
where
    W: Write,
    I: IntoIterator<Item = Result<RecordBatch>>,
{
    let mut total_rows = 0usize;
    for batch_res in batches {
        let batch = batch_res?;
        for row in 0..batch.num_rows {
            let mut map = serde_json::Map::with_capacity(batch.columns.len());
            for (col_idx, field) in batch.schema.fields.iter().enumerate() {
                let col = &batch.columns[col_idx];
                if let Some(val) = col.validity() {
                    if !val[row] {
                        map.insert(field.name.clone(), serde_json::Value::Null);
                        continue;
                    }
                }
                let json_val = match col {
                    ColumnArray::UInt64 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Int64 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::UInt8 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Float64 { values, .. } => {
                        let v = values[row];
                        if v.is_finite() {
                            serde_json::json!(v)
                        } else {
                            serde_json::Value::Null
                        }
                    }
                    ColumnArray::Boolean { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Utf8 { values, .. } => serde_json::json!(values[row]),
                    ColumnArray::Binary { values, .. } => {
                        let mut hex = String::with_capacity(values[row].len() * 2);
                        for &b in &values[row] {
                            use std::fmt::Write as _;
                            let _ = write!(&mut hex, "{:02x}", b);
                        }
                        serde_json::Value::String(hex)
                    }
                };
                map.insert(field.name.clone(), json_val);
            }
            let line = serde_json::to_string(&serde_json::Value::Object(map))
                .map_err(|e| RasterH3Error::StreamFailed(e.to_string()))?;
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
            total_rows += 1;
        }
    }
    writer.flush()?;
    Ok(total_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_stream_and_jsonl_roundtrip() {
        let schema = Arc::new(ColumnSchema::new(vec![
            ColumnField::new("h3_index", ArrowDataType::UInt64, false),
            ColumnField::new("h3_hex", ArrowDataType::Utf8, false),
            ColumnField::new("mean", ArrowDataType::Float64, false),
            ColumnField::new("stddev", ArrowDataType::Float64, true),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                ColumnArray::UInt64 {
                    values: vec![0x8828308281fffff, 0x8828308283fffff],
                    validity: None,
                },
                ColumnArray::Utf8 {
                    values: vec!["8828308281fffff".into(), "8828308283fffff".into()],
                    validity: None,
                },
                ColumnArray::Float64 {
                    values: vec![10.5, 20.25],
                    validity: None,
                },
                ColumnArray::Float64 {
                    values: vec![0.0, 1.5],
                    validity: Some(vec![false, true]),
                },
            ],
        )
        .unwrap();

        let mut buf = Vec::new();
        let rows =
            write_ipc_stream(&mut buf, &schema, vec![Ok(batch.clone())]).expect("write ipc");
        assert_eq!(rows, 2);

        let mut cursor = std::io::Cursor::new(buf);
        let (out_schema, out_batches) = read_ipc_stream(&mut cursor).expect("read ipc");
        assert_eq!(*out_schema, *schema);
        assert_eq!(out_batches, vec![batch.clone()]);

        let mut jsonl_buf = Vec::new();
        let jrows = write_jsonl_stream(&mut jsonl_buf, vec![Ok(batch)]).expect("write jsonl");
        assert_eq!(jrows, 2);
        let jsonl_str = String::from_utf8(jsonl_buf).unwrap();
        let lines: Vec<&str> = jsonl_str.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"8828308281fffff\""));
        assert!(lines[0].contains("\"stddev\":null"));
    }
}
