//! NoData and decoding utilities for typed raster buffers.
//!
//! Provides safe casting between f64 metadata NoData values and native pixel types,
//! fast chunk-level NoData validation, and zero-cost static dispatch over `DecodingResult`.
//!
//! ### Floating-Point vs Integer Validity Invariant
//!
//! - **Floating-Point Values (`f32`, `f64`)**:
//!   NaN and infinite values are unconditionally invalid regardless of metadata.
//!   When an optional NoData marker is present, values are compared using an epsilon
//!   tolerance (`(val - nd).abs() < 1e-6` or exact equality `val == nd`) to account
//!   for slight IEEE 754 precision discrepancies between metadata representations
//!   and decoded raster buffers.
//!
//! - **Integer Values (`u8`..`u64`, `i8`..`i64`)**:
//!   Values are discrete and exact. Validity is evaluated strictly in native integer
//!   arithmetic (`val != nd`). Slices and samples must NOT be cast to `f64` during
//!   skipping checks or validity tests, because 64-bit integers (`u64`, `i64`) lose
//!   precision beyond 53 bits ($2^{53} \approx 9 \times 10^{15}$) when cast to `f64`,
//!   which could cause false positives or false negatives in NoData detection.

use tiff::decoder::DecodingResult;

/// Trait for safely casting an optional f64 NoData value to a typed native pixel value within valid range bounds
pub trait NodataCast: Copy + PartialEq + Send + Sync + 'static {
    fn from_nodata_f64(nodata: Option<f64>) -> Option<Self>;
}

macro_rules! impl_integer_nodata_cast {
    ($($ty:ty),+ $(,)?) => {
        $(impl NodataCast for $ty {
            #[inline(always)]
            fn from_nodata_f64(nodata: Option<f64>) -> Option<Self> {
                nodata.and_then(|v| {
                    // The upper bound is exclusive: i64::MAX and u64::MAX round
                    // upward in f64, so an inclusive comparison accepts overflow.
                    if v.is_finite()
                        && v.fract() == 0.0
                        && v >= <$ty>::MIN as f64
                        && v < (<$ty>::MAX as f64) + 1.0
                    {
                        Some(v as $ty)
                    } else {
                        None
                    }
                })
            }
        })+
    };
}

impl_integer_nodata_cast!(u8, u16, u32, u64, i8, i16, i32, i64);

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

/// Trait defining unified NoData validity checking across all raster processing pipelines.
///
/// Ensures chunk-level skipping, row-level skipping, scalar sample processing,
/// span accumulation, and categorical classification share identical validity semantics.
pub trait NoDataRule<T>: Copy + Send + Sync + 'static {
    /// Return true if the value represents valid raster data.
    fn is_valid(self, val: T) -> bool;

    /// Return true if every element in the slice is NoData / invalid.
    fn is_slice_all_nodata(self, slice: &[T]) -> bool;
}

/// Trait implemented by native raster pixel types for unified NoData checking.
pub trait NativeNoData: Copy + PartialEq + Send + Sync + 'static {
    fn is_valid_pixel(self, marker: Option<Self>) -> bool;
    fn is_slice_all_nodata(slice: &[Self], marker: Option<Self>) -> bool;
}

/// Unified NoData validity rule wrapper around an optional typed marker value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelValidity<T> {
    pub marker: Option<T>,
}

impl<T> PixelValidity<T> {
    #[inline(always)]
    pub const fn new(marker: Option<T>) -> Self {
        Self { marker }
    }
}

macro_rules! impl_integer_native_nodata {
    ($($ty:ty),+ $(,)?) => {
        $(impl NativeNoData for $ty {
            #[inline(always)]
            fn is_valid_pixel(self, marker: Option<Self>) -> bool {
                match marker {
                    Some(nd) => self != nd,
                    None => true,
                }
            }

            #[inline(always)]
            fn is_slice_all_nodata(slice: &[Self], marker: Option<Self>) -> bool {
                match marker {
                    Some(nd) => {
                        if slice.is_empty() {
                            return true;
                        }
                        let len = slice.len();
                        if slice[0] != nd || slice[len / 2] != nd || slice[len - 1] != nd {
                            return false;
                        }
                        slice.iter().all(|&val| val == nd)
                    }
                    None => false,
                }
            }
        })+
    };
}

impl_integer_native_nodata!(u8, u16, u32, u64, i8, i16, i32, i64);

impl NativeNoData for f32 {
    #[inline(always)]
    fn is_valid_pixel(self, marker: Option<Self>) -> bool {
        if !self.is_finite() {
            return false;
        }
        if let Some(nd) = marker {
            if self == nd || (self - nd).abs() < 1e-6 {
                return false;
            }
        }
        true
    }

    #[inline(always)]
    fn is_slice_all_nodata(slice: &[Self], marker: Option<Self>) -> bool {
        if slice.is_empty() {
            return true;
        }
        match marker {
            Some(nd) => {
                let len = slice.len();
                let is_nd = |v: f32| !v.is_finite() || v == nd || (v - nd).abs() < 1e-6;
                if !is_nd(slice[0]) || !is_nd(slice[len / 2]) || !is_nd(slice[len - 1]) {
                    return false;
                }
                slice.iter().all(|&v| is_nd(v))
            }
            None => slice.iter().all(|&v| !v.is_finite()),
        }
    }
}

impl NativeNoData for f64 {
    #[inline(always)]
    fn is_valid_pixel(self, marker: Option<Self>) -> bool {
        if !self.is_finite() {
            return false;
        }
        if let Some(nd) = marker {
            if self == nd || (self - nd).abs() < 1e-6 {
                return false;
            }
        }
        true
    }

    #[inline(always)]
    fn is_slice_all_nodata(slice: &[Self], marker: Option<Self>) -> bool {
        if slice.is_empty() {
            return true;
        }
        match marker {
            Some(nd) => {
                let len = slice.len();
                let is_nd = |v: f64| !v.is_finite() || v == nd || (v - nd).abs() < 1e-6;
                if !is_nd(slice[0]) || !is_nd(slice[len / 2]) || !is_nd(slice[len - 1]) {
                    return false;
                }
                slice.iter().all(|&v| is_nd(v))
            }
            None => slice.iter().all(|&v| !v.is_finite()),
        }
    }
}

impl<T: NativeNoData> PixelValidity<T> {
    #[inline(always)]
    pub fn is_valid(self, val: T) -> bool {
        val.is_valid_pixel(self.marker)
    }

    #[inline(always)]
    pub fn is_slice_all_nodata(self, slice: &[T]) -> bool {
        T::is_slice_all_nodata(slice, self.marker)
    }
}

impl<T: NativeNoData> NoDataRule<T> for PixelValidity<T> {
    #[inline(always)]
    fn is_valid(self, val: T) -> bool {
        val.is_valid_pixel(self.marker)
    }

    #[inline(always)]
    fn is_slice_all_nodata(self, slice: &[T]) -> bool {
        T::is_slice_all_nodata(slice, self.marker)
    }
}

/// Fast check if an entire row slice consists purely of NoData values using native type comparison
#[inline(always)]
pub fn is_slice_all_native_nodata<T: NativeNoData>(slice: &[T], native_nodata: Option<T>) -> bool {
    T::is_slice_all_nodata(slice, native_nodata)
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

/// Fast check if an entire TIFF DecodingResult chunk is NoData / NaN using native type dispatch
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

/// Fast check if an entire TIFF DecodingResult chunk is NoData / NaN using native type dispatch
pub fn is_decoding_result_all_nodata(
    decoding_result: &DecodingResult,
    nodata: Option<f64>,
) -> bool {
    dispatch_decoding!(decoding_result, nodata, |slice, nd| {
        PixelValidity::new(nd).is_slice_all_nodata(slice)
    })
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
    fn integer_nodata_rejects_fractional_nonfinite_and_out_of_range_values() {
        macro_rules! check_invalid {
            ($ty:ty) => {
                assert_eq!(<$ty>::from_nodata_f64(Some(1.9)), None);
                assert_eq!(<$ty>::from_nodata_f64(Some(f64::NAN)), None);
                assert_eq!(<$ty>::from_nodata_f64(Some(f64::INFINITY)), None);
                assert_eq!(<$ty>::from_nodata_f64(Some(f64::NEG_INFINITY)), None);
                assert_eq!(
                    <$ty>::from_nodata_f64(Some((<$ty>::MAX as f64) + 1.0)),
                    None
                );
            };
        }
        check_invalid!(u8);
        check_invalid!(u16);
        check_invalid!(u32);
        check_invalid!(u64);
        check_invalid!(i8);
        check_invalid!(i16);
        check_invalid!(i32);
        check_invalid!(i64);
        assert_eq!(
            u64::from_nodata_f64(Some(2f64.powi(64) - 2048.0)),
            Some(u64::MAX - 2047)
        );
        assert_eq!(i64::from_nodata_f64(Some(-(2f64.powi(63)))), Some(i64::MIN));
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

    #[test]
    fn test_pixel_validity_large_integers_preserves_distinctions() {
        // Values > 2^53 that would collide if cast to f64
        let base = (1u64 << 54) + 100;
        let nd = base;
        let val_different = base + 1;

        // In f64, these might be equal due to loss of precision:
        assert_eq!(base as f64, (base + 1) as f64);

        // In PixelValidity, native u64 equality distinguishes them:
        let rule = PixelValidity::new(Some(nd));
        assert!(!rule.is_valid(nd));
        assert!(rule.is_valid(val_different));

        let slice = vec![val_different, val_different];
        assert!(!rule.is_slice_all_nodata(&slice));

        let nd_slice = vec![nd, nd, nd];
        assert!(rule.is_slice_all_nodata(&nd_slice));
    }

    #[test]
    fn test_pixel_validity_floats_epsilon_and_nonfinite() {
        let rule = PixelValidity::new(Some(-9999.0f32));
        assert!(!rule.is_valid(f32::NAN));
        assert!(!rule.is_valid(f32::INFINITY));
        assert!(!rule.is_valid(f32::NEG_INFINITY));
        assert!(!rule.is_valid(-9999.0f32));
        assert!(!rule.is_valid(-9999.0000001f32));
        assert!(rule.is_valid(0.0f32));
        assert!(rule.is_valid(42.5f32));

        let slice = vec![f32::NAN, -9999.0f32];
        assert!(rule.is_slice_all_nodata(&slice));

        let slice_valid = vec![f32::NAN, 1.0f32];
        assert!(!rule.is_slice_all_nodata(&slice_valid));
    }
}
