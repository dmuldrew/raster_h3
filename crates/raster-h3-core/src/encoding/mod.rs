//! Shared encoding and serialization utilities for H3 cell indices and geometries.
//!
//! Provides zero-allocation formatting for H3 hexadecimal cell strings and stack-allocated
//! OGC Well-Known Binary (WKB) 2D Polygon geometry serialization, plus a WKB reader for
//! polygonal inputs.

pub mod fast_hex;
pub mod wkb;
pub mod wkb_reader;

pub use fast_hex::{fast_hex_u64, parse_hex_u64};
pub use wkb::{cell_to_wkb, extend_wkb_bounds, h3_index_to_wkb, WkbBuf, WKB_BUF_LEN};
pub use wkb_reader::wkb_to_multipolygon;
