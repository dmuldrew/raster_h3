//! Native typed views of decoded TIFF data. Windows borrow storage and retain
//! their physical row stride; no pixel values are copied by the built-in kernels.
use crate::error::{RasterH3Error, Result};
use tiff::decoder::DecodingResult;

#[derive(Clone, Copy, Debug)]
pub enum BorrowedSamples<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
    U64(&'a [u64]),
    I8(&'a [i8]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
}
impl<'a> From<&'a DecodingResult> for BorrowedSamples<'a> {
    fn from(data: &'a DecodingResult) -> Self {
        match data {
            DecodingResult::U8(v) => Self::U8(v),
            DecodingResult::U16(v) => Self::U16(v),
            DecodingResult::U32(v) => Self::U32(v),
            DecodingResult::U64(v) => Self::U64(v),
            DecodingResult::I8(v) => Self::I8(v),
            DecodingResult::I16(v) => Self::I16(v),
            DecodingResult::I32(v) => Self::I32(v),
            DecodingResult::I64(v) => Self::I64(v),
            DecodingResult::F32(v) => Self::F32(v),
            DecodingResult::F64(v) => Self::F64(v),
        }
    }
}
impl<'a> BorrowedSamples<'a> {
    pub fn window(self, start: usize, len: usize) -> Result<Self> {
        let end = start
            .checked_add(len)
            .ok_or_else(|| RasterH3Error::InvalidMetadata("sample window overflow".into()))?;
        match self {
            Self::U8(v) => v.get(start..end).map(Self::U8),
            Self::U16(v) => v.get(start..end).map(Self::U16),
            Self::U32(v) => v.get(start..end).map(Self::U32),
            Self::U64(v) => v.get(start..end).map(Self::U64),
            Self::I8(v) => v.get(start..end).map(Self::I8),
            Self::I16(v) => v.get(start..end).map(Self::I16),
            Self::I32(v) => v.get(start..end).map(Self::I32),
            Self::I64(v) => v.get(start..end).map(Self::I64),
            Self::F32(v) => v.get(start..end).map(Self::F32),
            Self::F64(v) => v.get(start..end).map(Self::F64),
        }
        .ok_or_else(|| {
            RasterH3Error::InvalidMetadata(
                "decoded chunk shorter than its declared dimensions".into(),
            )
        })
    }
    /// Compatibility fallback for external kernels that implement only process_chunk.
    pub fn to_owned(self) -> DecodingResult {
        match self {
            Self::U8(v) => DecodingResult::U8(v.to_vec()),
            Self::U16(v) => DecodingResult::U16(v.to_vec()),
            Self::U32(v) => DecodingResult::U32(v.to_vec()),
            Self::U64(v) => DecodingResult::U64(v.to_vec()),
            Self::I8(v) => DecodingResult::I8(v.to_vec()),
            Self::I16(v) => DecodingResult::I16(v.to_vec()),
            Self::I32(v) => DecodingResult::I32(v.to_vec()),
            Self::I64(v) => DecodingResult::I64(v.to_vec()),
            Self::F32(v) => DecodingResult::F32(v.to_vec()),
            Self::F64(v) => DecodingResult::F64(v.to_vec()),
        }
    }
    pub fn bytes(self) -> usize {
        match self {
            Self::U8(v) => std::mem::size_of_val(v),
            Self::U16(v) => std::mem::size_of_val(v),
            Self::U32(v) => std::mem::size_of_val(v),
            Self::U64(v) => std::mem::size_of_val(v),
            Self::I8(v) => std::mem::size_of_val(v),
            Self::I16(v) => std::mem::size_of_val(v),
            Self::I32(v) => std::mem::size_of_val(v),
            Self::I64(v) => std::mem::size_of_val(v),
            Self::F32(v) => std::mem::size_of_val(v),
            Self::F64(v) => std::mem::size_of_val(v),
        }
    }
    pub fn all_nodata(self, nodata: Option<f64>) -> bool {
        crate::dispatch_samples!(self, nodata, |slice, nd| {
            crate::aggregator::nodata::PixelValidity::new(nd).is_slice_all_nodata(slice)
        })
    }
}

#[macro_export]
macro_rules! dispatch_samples {
    ($data:expr, $nodata:expr, |$slice:ident, $nd:ident| $body:expr) => {
        match $data {
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::U8($slice) => {
                let $nd = <u8 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::U16($slice) => {
                let $nd = <u16 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::U32($slice) => {
                let $nd = <u32 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::U64($slice) => {
                let $nd = <u64 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::I8($slice) => {
                let $nd = <i8 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::I16($slice) => {
                let $nd = <i16 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::I32($slice) => {
                let $nd = <i32 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::I64($slice) => {
                let $nd = <i64 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::F32($slice) => {
                let $nd = <f32 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            $crate::aggregator::multi_horizon::borrowed::BorrowedSamples::F64($slice) => {
                let $nd = <f64 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
        }
    };
}
