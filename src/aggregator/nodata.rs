//! NoData and decoding utilities for typed raster buffers.
//!
//! Provides safe casting between f64 metadata NoData values and native pixel types,
//! fast chunk-level NoData validation, and zero-cost static dispatch over `DecodingResult`.

use tiff::decoder::DecodingResult;

/// Trait for safely casting an optional f64 NoData value to a typed native pixel value within valid range bounds
pub trait NodataCast: Copy + PartialEq + Send + Sync + 'static {
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self>;
}

impl NodataCast for u8 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| {
            if (0.0..=255.0).contains(&v) {
                Some(v as u8)
            } else {
                None
            }
        })
    }
}

impl NodataCast for u16 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| {
            if (0.0..=65535.0).contains(&v) {
                Some(v as u16)
            } else {
                None
            }
        })
    }
}

impl NodataCast for u32 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| {
            if v >= 0.0 && v <= u32::MAX as f64 {
                Some(v as u32)
            } else {
                None
            }
        })
    }
}

impl NodataCast for u64 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| if v >= 0.0 { Some(v as u64) } else { None })
    }
}

impl NodataCast for i8 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| {
            if (-128.0..=127.0).contains(&v) {
                Some(v as i8)
            } else {
                None
            }
        })
    }
}

impl NodataCast for i16 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| {
            if (-32768.0..=32767.0).contains(&v) {
                Some(v as i16)
            } else {
                None
            }
        })
    }
}

impl NodataCast for i32 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.and_then(|v| {
            if v >= i32::MIN as f64 && v <= i32::MAX as f64 {
                Some(v as i32)
            } else {
                None
            }
        })
    }
}

impl NodataCast for i64 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.map(|v| v as i64)
    }
}

impl NodataCast for f32 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata.map(|v| v as f32)
    }
}

impl NodataCast for f64 {
    #[inline(always)]
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
        nodata
    }
}

/// Fast check if an entire chunk slice is NoData / NaN
#[inline(always)]
pub fn is_chunk_all_nodata<T, F>(slice: &[T], nodata: Option<f64>, to_f64: F) -> bool
where
    T: Copy,
    F: Fn(T) -> f64,
{
    if slice.is_empty() {
        return true;
    }
    match nodata {
        Some(nd) => {
            let mid = slice.len() / 2;
            let last = slice.len() - 1;
            let s0 = to_f64(slice[0]);
            let s_mid = to_f64(slice[mid]);
            let s_last = to_f64(slice[last]);

            let is_nd = |v: f64| !v.is_finite() || (v - nd).abs() < 1e-6;
            if !is_nd(s0) || !is_nd(s_mid) || !is_nd(s_last) {
                return false;
            }
            slice.iter().all(|&x| is_nd(to_f64(x)))
        }
        None => slice.iter().all(|&x| !to_f64(x).is_finite()),
    }
}

/// Fast check if an entire TIFF DecodingResult chunk is NoData / NaN
pub fn is_decoding_result_all_nodata(
    decoding_result: &DecodingResult,
    nodata: Option<f64>,
) -> bool {
    match decoding_result {
        DecodingResult::U8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::U16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::U32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::U64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I8(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I16(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::I64(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::F32(slice) => is_chunk_all_nodata(slice, nodata, |x| x as f64),
        DecodingResult::F64(slice) => is_chunk_all_nodata(slice, nodata, |x| x),
    }
}

/// Unified macro to dispatch over a TIFF `DecodingResult` and cast an optional f64 `nodata`
/// value to the native slice type using `NodataCast`.
#[macro_export]
macro_rules! dispatch_decoding {
    ($dr:expr, $nodata:expr, |$slice:ident, $nd:ident| $body:expr) => {
        match $dr {
            tiff::decoder::DecodingResult::U8($slice) => {
                let $nd = <u8 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::U16($slice) => {
                let $nd = <u16 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::U32($slice) => {
                let $nd = <u32 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::U64($slice) => {
                let $nd = <u64 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::I8($slice) => {
                let $nd = <i8 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::I16($slice) => {
                let $nd = <i16 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::I32($slice) => {
                let $nd = <i32 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::I64($slice) => {
                let $nd = <i64 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::F32($slice) => {
                let $nd = <f32 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
            tiff::decoder::DecodingResult::F64($slice) => {
                let $nd = <f64 as $crate::aggregator::nodata::NodataCast>::from_nodata_f64($nodata);
                $body
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nodata_cast_ranges() {
        // u8: 0..=255
        assert_eq!(u8::from_nodata_f64(Some(0.0)), Some(0));
        assert_eq!(u8::from_nodata_f64(Some(255.0)), Some(255));
        assert_eq!(u8::from_nodata_f64(Some(256.0)), None);
        assert_eq!(u8::from_nodata_f64(Some(-1.0)), None);
        assert_eq!(u8::from_nodata_f64(None), None);

        // i8: -128..=127
        assert_eq!(i8::from_nodata_f64(Some(-128.0)), Some(-128));
        assert_eq!(i8::from_nodata_f64(Some(127.0)), Some(127));
        assert_eq!(i8::from_nodata_f64(Some(128.0)), None);

        // u16: 0..=65535
        assert_eq!(u16::from_nodata_f64(Some(65535.0)), Some(65535));
        assert_eq!(u16::from_nodata_f64(Some(65536.0)), None);

        // f32 and f64
        assert_eq!(f32::from_nodata_f64(Some(-9999.0)), Some(-9999.0f32));
        assert_eq!(f64::from_nodata_f64(Some(-9999.0)), Some(-9999.0f64));
    }

    #[test]
    fn test_is_decoding_result_all_nodata() {
        let dr_u8 = DecodingResult::U8(vec![255, 255, 255]);
        assert!(is_decoding_result_all_nodata(&dr_u8, Some(255.0)));
        assert!(!is_decoding_result_all_nodata(&dr_u8, Some(0.0)));

        let dr_f32 = DecodingResult::F32(vec![f32::NAN, f32::NAN]);
        assert!(is_decoding_result_all_nodata(&dr_f32, None));

        let dr_mixed = DecodingResult::F32(vec![f32::NAN, 1.0]);
        assert!(!is_decoding_result_all_nodata(&dr_mixed, None));
    }
}
