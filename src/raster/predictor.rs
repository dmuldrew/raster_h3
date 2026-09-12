//! TIFF Predictor Decoding and Sample Unpacking Utilities
//!
//! Centralizes TIFF Predictor 2 (Horizontal Differencing), Predictor 3 (Floating Point Differencing),
//! and sample unpacking across integer and floating-point types.

use tiff::decoder::{fp_predict_f32, fp_predict_f64};
use tiff::tags::{PhotometricInterpretation, Predictor};

use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::TiffByteOrder;

/// Trait for integer sample types that support wrapping addition for Horizontal Predictor decoding
pub trait WrappingAdd: Copy {
    fn wrapping_add(self, rhs: Self) -> Self;
}

macro_rules! impl_wrapping_add {
    ($($t:ty),*) => {
        $(
            impl WrappingAdd for $t {
                #[inline(always)]
                fn wrapping_add(self, rhs: Self) -> Self {
                    <$t>::wrapping_add(self, rhs)
                }
            }
        )*
    };
}

impl_wrapping_add!(u8, i8, u16, i16, u32, i32, u64, i64);

/// Apply TIFF Horizontal Predictor (Predictor 2) in-place along each row.
///
/// Each sample is reconstructed by adding the sample `spp` positions before it:
/// `row[col] = row[col].wrapping_add(row[col - spp])`
#[inline]
pub fn apply_horizontal_predictor<T: WrappingAdd>(
    data: &mut [T],
    data_w: usize,
    data_h: usize,
    spp: usize,
) {
    let stride = data_w * spp;
    for r in 0..data_h {
        let row = &mut data[r * stride..(r + 1) * stride];
        for col in spp..row.len() {
            row[col] = row[col].wrapping_add(row[col - spp]);
        }
    }
}

/// Trait abstracting endian decoding and photometric inversion for primitive TIFF samples
pub trait TiffSample: Copy + Default + 'static {
    const BYTES: usize = std::mem::size_of::<Self>();
    fn from_le_bytes(bytes: &[u8]) -> Self;
    fn from_be_bytes(bytes: &[u8]) -> Self;
    fn invert_white_is_zero(self) -> Self;
}

impl TiffSample for u8 {
    #[inline(always)]
    fn from_le_bytes(bytes: &[u8]) -> Self {
        bytes[0]
    }
    #[inline(always)]
    fn from_be_bytes(bytes: &[u8]) -> Self {
        bytes[0]
    }
    #[inline(always)]
    fn invert_white_is_zero(self) -> Self {
        255 - self
    }
}

impl TiffSample for i8 {
    #[inline(always)]
    fn from_le_bytes(bytes: &[u8]) -> Self {
        bytes[0] as i8
    }
    #[inline(always)]
    fn from_be_bytes(bytes: &[u8]) -> Self {
        bytes[0] as i8
    }
    #[inline(always)]
    fn invert_white_is_zero(self) -> Self {
        self
    }
}

macro_rules! impl_tiff_int_sample {
    ($t:ty, $inv:expr) => {
        impl TiffSample for $t {
            #[inline(always)]
            fn from_le_bytes(bytes: &[u8]) -> Self {
                <$t>::from_le_bytes(bytes.try_into().unwrap())
            }
            #[inline(always)]
            fn from_be_bytes(bytes: &[u8]) -> Self {
                <$t>::from_be_bytes(bytes.try_into().unwrap())
            }
            #[inline(always)]
            fn invert_white_is_zero(self) -> Self {
                $inv(self)
            }
        }
    };
}

impl_tiff_int_sample!(u16, |x: u16| 65535 - x);
impl_tiff_int_sample!(i16, |x: i16| x);
impl_tiff_int_sample!(u32, |x: u32| u32::MAX - x);
impl_tiff_int_sample!(i32, |x: i32| x);
impl_tiff_int_sample!(u64, |x: u64| u64::MAX - x);
impl_tiff_int_sample!(i64, |x: i64| x);

/// Unpack generic integer samples from decompressed TIFF chunk bytes.
///
/// Handles:
/// 1. Direct native-endian memcpy (single contiguous slice or row-by-row stride).
/// 2. Endian-swapping conversion if byte order differs from host.
/// 3. In-place horizontal predictor reversal.
/// 4. In-place WhiteIsZero photometric inversion.
pub fn unpack_integer_samples<T: TiffSample + WrappingAdd>(
    src: &[u8],
    dst: &mut [T],
    tile_w: usize,
    data_w: usize,
    data_h: usize,
    spp: usize,
    byte_order: TiffByteOrder,
    predictor: Predictor,
    photometric: PhotometricInterpretation,
) -> Result<()> {
    let sample_bytes = T::BYTES;
    let src_stride_bytes = tile_w * spp * sample_bytes;
    let dst_stride = data_w * spp;
    let dst_stride_bytes = dst_stride * sample_bytes;

    #[cfg(target_endian = "little")]
    let is_native_endian = sample_bytes == 1 || byte_order == TiffByteOrder::LittleEndian;
    #[cfg(not(target_endian = "little"))]
    let is_native_endian = sample_bytes == 1 || byte_order == TiffByteOrder::BigEndian;

    if is_native_endian {
        if src_stride_bytes == dst_stride_bytes {
            let total_bytes = data_w * data_h * spp * sample_bytes;
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr() as *mut u8, total_bytes);
            }
        } else {
            for r in 0..data_h {
                let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                unsafe {
                    std::ptr::copy_nonoverlapping(s.as_ptr(), d.as_mut_ptr() as *mut u8, dst_stride_bytes);
                }
            }
        }
    } else {
        match byte_order {
            TiffByteOrder::LittleEndian => {
                for r in 0..data_h {
                    let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    for (i, item) in d.iter_mut().enumerate() {
                        let offset = i * sample_bytes;
                        *item = T::from_le_bytes(&s[offset..offset + sample_bytes]);
                    }
                }
            }
            TiffByteOrder::BigEndian => {
                for r in 0..data_h {
                    let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    for (i, item) in d.iter_mut().enumerate() {
                        let offset = i * sample_bytes;
                        *item = T::from_be_bytes(&s[offset..offset + sample_bytes]);
                    }
                }
            }
        }
    }

    if predictor == Predictor::Horizontal {
        apply_horizontal_predictor(dst, data_w, data_h, spp);
    }

    if photometric == PhotometricInterpretation::WhiteIsZero {
        for item in dst.iter_mut() {
            *item = item.invert_white_is_zero();
        }
    }

    Ok(())
}

/// Unpack 32-bit floating point samples from decompressed TIFF chunk bytes.
pub fn unpack_f32(
    src: &mut [u8],
    dst: &mut [f32],
    tile_w: usize,
    data_w: usize,
    data_h: usize,
    spp: usize,
    byte_order: TiffByteOrder,
    predictor: Predictor,
    photometric: PhotometricInterpretation,
) -> Result<()> {
    let src_stride_bytes = tile_w * spp * 4;
    let dst_stride = data_w * spp;
    match predictor {
        Predictor::FloatingPoint => {
            for r in 0..data_h {
                let s = &mut src[r * src_stride_bytes..(r + 1) * src_stride_bytes];
                let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                fp_predict_f32(s, d, spp);
                if photometric == PhotometricInterpretation::WhiteIsZero {
                    for item in d.iter_mut() {
                        *item = 1.0 - *item;
                    }
                }
            }
        }
        Predictor::None => {
            let dst_stride_bytes = dst_stride * 4;

            #[cfg(target_endian = "little")]
            let is_native_endian = byte_order == TiffByteOrder::LittleEndian;
            #[cfg(not(target_endian = "little"))]
            let is_native_endian = byte_order == TiffByteOrder::BigEndian;

            if is_native_endian {
                if src_stride_bytes == dst_stride_bytes {
                    let total_bytes = data_w * data_h * spp * 4;
                    unsafe {
                        std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr() as *mut u8, total_bytes);
                    }
                } else {
                    for r in 0..data_h {
                        let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                        let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                        unsafe {
                            std::ptr::copy_nonoverlapping(s.as_ptr(), d.as_mut_ptr() as *mut u8, dst_stride_bytes);
                        }
                    }
                }
            } else {
                for r in 0..data_h {
                    let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 4];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    match byte_order {
                        TiffByteOrder::LittleEndian => {
                            for (i, item) in d.iter_mut().enumerate() {
                                *item = f32::from_bits(u32::from_le_bytes([
                                    s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3],
                                ]));
                            }
                        }
                        TiffByteOrder::BigEndian => {
                            for (i, item) in d.iter_mut().enumerate() {
                                *item = f32::from_bits(u32::from_be_bytes([
                                    s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3],
                                ]));
                            }
                        }
                    }
                }
            }
            if photometric == PhotometricInterpretation::WhiteIsZero {
                for item in dst.iter_mut() {
                    *item = 1.0 - *item;
                }
            }
        }
        Predictor::Horizontal => {
            return Err(RasterH3Error::InvalidMetadata(
                "horizontal predictor unsupported for f32".into(),
            ));
        }
        _ => {
            return Err(RasterH3Error::InvalidMetadata(
                "unsupported predictor for f32".into(),
            ));
        }
    }
    Ok(())
}

/// Unpack 64-bit floating point samples from decompressed TIFF chunk bytes.
pub fn unpack_f64(
    src: &mut [u8],
    dst: &mut [f64],
    tile_w: usize,
    data_w: usize,
    data_h: usize,
    spp: usize,
    byte_order: TiffByteOrder,
    predictor: Predictor,
    photometric: PhotometricInterpretation,
) -> Result<()> {
    let src_stride_bytes = tile_w * spp * 8;
    let dst_stride = data_w * spp;
    match predictor {
        Predictor::FloatingPoint => {
            for r in 0..data_h {
                let s = &mut src[r * src_stride_bytes..(r + 1) * src_stride_bytes];
                let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                fp_predict_f64(s, d, spp);
                if photometric == PhotometricInterpretation::WhiteIsZero {
                    for item in d.iter_mut() {
                        *item = 1.0 - *item;
                    }
                }
            }
        }
        Predictor::None => {
            let dst_stride_bytes = dst_stride * 8;

            #[cfg(target_endian = "little")]
            let is_native_endian = byte_order == TiffByteOrder::LittleEndian;
            #[cfg(not(target_endian = "little"))]
            let is_native_endian = byte_order == TiffByteOrder::BigEndian;

            if is_native_endian {
                if src_stride_bytes == dst_stride_bytes {
                    let total_bytes = data_w * data_h * spp * 8;
                    unsafe {
                        std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr() as *mut u8, total_bytes);
                    }
                } else {
                    for r in 0..data_h {
                        let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                        let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                        unsafe {
                            std::ptr::copy_nonoverlapping(s.as_ptr(), d.as_mut_ptr() as *mut u8, dst_stride_bytes);
                        }
                    }
                }
            } else {
                for r in 0..data_h {
                    let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 8];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    match byte_order {
                        TiffByteOrder::LittleEndian => {
                            for (i, item) in d.iter_mut().enumerate() {
                                *item = f64::from_bits(u64::from_le_bytes([
                                    s[i * 8], s[i * 8 + 1], s[i * 8 + 2], s[i * 8 + 3],
                                    s[i * 8 + 4], s[i * 8 + 5], s[i * 8 + 6], s[i * 8 + 7],
                                ]));
                            }
                        }
                        TiffByteOrder::BigEndian => {
                            for (i, item) in d.iter_mut().enumerate() {
                                *item = f64::from_bits(u64::from_be_bytes([
                                    s[i * 8], s[i * 8 + 1], s[i * 8 + 2], s[i * 8 + 3],
                                    s[i * 8 + 4], s[i * 8 + 5], s[i * 8 + 6], s[i * 8 + 7],
                                ]));
                            }
                        }
                    }
                }
            }
            if photometric == PhotometricInterpretation::WhiteIsZero {
                for item in dst.iter_mut() {
                    *item = 1.0 - *item;
                }
            }
        }
        Predictor::Horizontal => {
            return Err(RasterH3Error::InvalidMetadata(
                "horizontal predictor unsupported for f64".into(),
            ));
        }
        _ => {
            return Err(RasterH3Error::InvalidMetadata(
                "unsupported predictor for f64".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_horizontal_predictor_u8() {
        // Row 1: diffs [10, 5, 2] -> reconstructed [10, 15, 17]
        // Row 2: diffs [20, 250, 10] -> reconstructed [20, 14, 24] (wrapping add: 20+250 = 270 % 256 = 14)
        let mut data = vec![10u8, 5, 2, 20, 250, 10];
        apply_horizontal_predictor(&mut data, 3, 2, 1);
        assert_eq!(data, vec![10, 15, 17, 20, 14, 24]);
    }

    #[test]
    fn test_apply_horizontal_predictor_multi_channel() {
        // spp = 2 (interleaved R, G)
        // Row 1: R=[10, 3], G=[20, 4]
        // Flat input: [10, 20, 3, 4]
        // Expected: R=[10, 13], G=[20, 24] -> [10, 20, 13, 24]
        let mut data = vec![10u16, 20, 3, 4];
        apply_horizontal_predictor(&mut data, 2, 1, 2);
        assert_eq!(data, vec![10, 20, 13, 24]);
    }

    #[test]
    fn test_unpack_integer_samples_stride_and_white_is_zero() {
        // tile_w = 4, data_w = 2, data_h = 2, spp = 1 (tile has padding)
        let tile = vec![
            10u8, 20, 99, 99, // row 0: keep [10, 20]
            30, 40, 99, 99,   // row 1: keep [30, 40]
        ];
        let mut dst = vec![0u8; 4];
        unpack_integer_samples(
            &tile,
            &mut dst,
            4,
            2,
            2,
            1,
            TiffByteOrder::LittleEndian,
            Predictor::None,
            PhotometricInterpretation::WhiteIsZero,
        )
        .unwrap();

        // 255 - val: [255 - 10, 255 - 20, 255 - 30, 255 - 40]
        assert_eq!(dst, vec![245, 235, 225, 215]);
    }

    #[test]
    fn test_unpack_f32_invalid_predictor() {
        let mut src = vec![0u8; 16];
        let mut dst = vec![0.0f32; 4];
        let result = unpack_f32(
            &mut src,
            &mut dst,
            2,
            2,
            1,
            1,
            TiffByteOrder::LittleEndian,
            Predictor::Horizontal,
            PhotometricInterpretation::BlackIsZero,
        );
        assert!(result.is_err());
    }
}
