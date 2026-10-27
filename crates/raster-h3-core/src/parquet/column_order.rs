//! Footer fix-up that keeps exported Parquet readable by Polars.
//!
//! parquet 60 writes `ColumnOrder::IEEE_754_TOTAL_ORDER` for every FLOAT/DOUBLE leaf and has
//! no writer property to opt out. Polars' native reader rejects that union variant ("Invalid
//! thrift: bad data") and PyArrow ignores those columns' min/max statistics. The writer still
//! skips NaN in statistics, so for our NaN-free, finite aggregates the type-defined order
//! describes the same min/max, and we rewrite the footer to declare it.
//!
//! In the Thrift compact encoding each `ColumnOrder` is a one-field union holding an empty
//! struct: `[0x1C, 0x00, 0x00]` for `TYPE_ORDER` (field 1) and `[0x2C, 0x00, 0x00]` for
//! `IEEE_754_TOTAL_ORDER` (field 2). Without encryption, `column_orders` (field 7) is the last
//! field of `FileMetaData`, so the footer ends with the list elements and the struct's stop
//! byte. The swap keeps the footer length, so it is patched in place.

use std::io::{self, Read, Seek, SeekFrom, Write};

use parquet::basic::ColumnOrder;

const TYPE_ORDER: u8 = 0x1C;
const IEEE_754_TOTAL_ORDER: u8 = 0x2C;
const ELEMENT_LEN: usize = 3;

/// Rewrites every `IEEE_754_TOTAL_ORDER` entry in a closed Parquet file's footer as
/// `TYPE_ORDER`. `orders` is the file's `column_orders` as reported by the writer; the footer
/// bytes must match it exactly, otherwise nothing is changed and an error is returned.
pub fn restore_type_defined_float_orders<F: Read + Write + Seek>(
    file: &mut F,
    orders: Option<&Vec<ColumnOrder>>,
) -> io::Result<()> {
    let Some(orders) = orders else {
        return Ok(());
    };
    if !orders.contains(&ColumnOrder::IEEE_754_TOTAL_ORDER) {
        return Ok(());
    }

    let file_len = file.seek(SeekFrom::End(0))?;
    let mut tail = [0u8; 8];
    file.seek(SeekFrom::End(-8))?;
    file.read_exact(&mut tail)?;
    if &tail[4..] != b"PAR1" {
        return Err(layout_error("missing PAR1 footer magic"));
    }
    let footer_len = u64::from(u32::from_le_bytes(tail[..4].try_into().unwrap()));
    let footer_start = file_len
        .checked_sub(8 + footer_len)
        .ok_or_else(|| layout_error("footer length exceeds file size"))?;
    let mut footer = vec![0u8; footer_len as usize];
    file.seek(SeekFrom::Start(footer_start))?;
    file.read_exact(&mut footer)?;

    let elements_len = orders.len() * ELEMENT_LEN;
    let header = list_header(orders.len());
    // Field header (short form, list type) + list header + elements + FileMetaData stop.
    let needed = 1 + header.len() + elements_len + 1;
    if footer.len() < needed || footer[footer.len() - 1] != 0 {
        return Err(layout_error("footer does not end with column_orders"));
    }
    let elements_start = footer.len() - 1 - elements_len;
    let header_start = elements_start - header.len();
    let field = footer[header_start - 1];
    if footer[header_start..elements_start] != header[..] || field & 0x0F != 0x09 || field >> 4 == 0
    {
        return Err(layout_error("column_orders list header not found"));
    }

    let mut patched = false;
    for (i, order) in orders.iter().enumerate() {
        let at = elements_start + i * ELEMENT_LEN;
        let element = &mut footer[at..at + ELEMENT_LEN];
        let expected = match order {
            ColumnOrder::IEEE_754_TOTAL_ORDER => IEEE_754_TOTAL_ORDER,
            ColumnOrder::TYPE_DEFINED_ORDER(_) => TYPE_ORDER,
            other => return Err(layout_error(&format!("unexpected column order {other:?}"))),
        };
        if element != [expected, 0, 0] {
            return Err(layout_error(
                "column_orders entry does not match the writer",
            ));
        }
        if expected == IEEE_754_TOTAL_ORDER {
            element[0] = TYPE_ORDER;
            patched = true;
        }
    }

    if patched {
        file.seek(SeekFrom::Start(footer_start + elements_start as u64))?;
        file.write_all(&footer[elements_start..elements_start + elements_len])?;
        file.flush()?;
    }
    Ok(())
}

/// Compact-protocol list header for `len` struct elements.
fn list_header(len: usize) -> Vec<u8> {
    if len < 15 {
        return vec![((len as u8) << 4) | 0x0C];
    }
    let mut out = vec![0xFC];
    let mut n = len as u64;
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    out
}

fn layout_error(detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("cannot rewrite Parquet column orders for Polars compatibility: {detail}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::basic::SortOrder;
    use std::io::Cursor;

    fn footer_file(field: u8, orders: &[u8]) -> Cursor<Vec<u8>> {
        let mut footer = vec![0x15, 0x04, field];
        footer.extend(list_header(orders.len()));
        for &o in orders {
            footer.extend([o, 0, 0]);
        }
        footer.push(0);
        let mut bytes = b"PAR1".to_vec();
        bytes.extend(&footer);
        bytes.extend((footer.len() as u32).to_le_bytes());
        bytes.extend(b"PAR1");
        Cursor::new(bytes)
    }

    #[test]
    fn rewrites_only_float_orders() {
        let orders = vec![
            ColumnOrder::TYPE_DEFINED_ORDER(SortOrder::SIGNED),
            ColumnOrder::IEEE_754_TOTAL_ORDER,
            ColumnOrder::IEEE_754_TOTAL_ORDER,
        ];
        let mut file = footer_file(
            0x19,
            &[TYPE_ORDER, IEEE_754_TOTAL_ORDER, IEEE_754_TOTAL_ORDER],
        );
        restore_type_defined_float_orders(&mut file, Some(&orders)).unwrap();
        assert_eq!(
            file.into_inner(),
            footer_file(0x19, &[TYPE_ORDER; 3]).into_inner()
        );
    }

    #[test]
    fn long_list_header() {
        let orders = vec![ColumnOrder::IEEE_754_TOTAL_ORDER; 20];
        let mut file = footer_file(0x19, &[IEEE_754_TOTAL_ORDER; 20]);
        restore_type_defined_float_orders(&mut file, Some(&orders)).unwrap();
        assert_eq!(
            file.into_inner(),
            footer_file(0x19, &[TYPE_ORDER; 20]).into_inner()
        );
    }

    #[test]
    fn mismatched_footer_is_left_untouched() {
        let orders = vec![ColumnOrder::IEEE_754_TOTAL_ORDER];
        let mut file = footer_file(0x19, &[TYPE_ORDER]);
        let before = file.get_ref().clone();
        assert!(restore_type_defined_float_orders(&mut file, Some(&orders)).is_err());
        assert_eq!(file.into_inner(), before);
    }
}
