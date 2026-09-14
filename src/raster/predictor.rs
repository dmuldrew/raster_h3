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

/// Apply TIFF Horizontal Predictor on u8 samples with ARM NEON SIMD acceleration when spp == 1.
#[inline]
pub fn apply_horizontal_predictor_u8(
    data: &mut [u8],
    data_w: usize,
    data_h: usize,
    spp: usize,
) {
    if spp != 1 {
        apply_horizontal_predictor(data, data_w, data_h, spp);
        return;
    }

    #[cfg(target_arch = "aarch64")]
    {
        for r in 0..data_h {
            let row = &mut data[r * data_w..(r + 1) * data_w];
            unsafe {
                rev_hpredict_neon_u8_spp1(row);
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        apply_horizontal_predictor(data, data_w, data_h, spp);
    }
}

/// Apply TIFF Horizontal Predictor on u16 samples with ARM NEON SIMD acceleration when spp == 1.
#[inline]
pub fn apply_horizontal_predictor_u16(
    data: &mut [u16],
    data_w: usize,
    data_h: usize,
    spp: usize,
) {
    if spp != 1 {
        apply_horizontal_predictor(data, data_w, data_h, spp);
        return;
    }

    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        let chunks = data_w / 8;
        let zero = unsafe { vdupq_n_u16(0) };
        for r in 0..data_h {
            let row = &mut data[r * data_w..(r + 1) * data_w];
            let mut prev_carry = 0u16;
            let ptr = row.as_mut_ptr();
            unsafe {
                for i in 0..chunks {
                    let offset = i * 8;
                    let mut v = vld1q_u16(ptr.add(offset));
                    v = vaddq_u16(v, vextq_u16(zero, v, 7));
                    v = vaddq_u16(v, vextq_u16(zero, v, 6));
                    v = vaddq_u16(v, vextq_u16(zero, v, 4));
                    v = vaddq_u16(v, vdupq_n_u16(prev_carry));
                    vst1q_u16(ptr.add(offset), v);
                    prev_carry = vgetq_lane_u16::<7>(v);
                }
            }
            for col in (chunks * 8).max(1)..data_w {
                row[col] = row[col].wrapping_add(row[col - 1]);
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        apply_horizontal_predictor(data, data_w, data_h, spp);
    }
}

/// Trait abstracting endian decoding and photometric inversion for primitive TIFF samples
pub trait TiffSample: Copy + Default + 'static {
    const BYTES: usize = std::mem::size_of::<Self>();
    fn from_le_bytes(bytes: &[u8]) -> Self;
    fn from_be_bytes(bytes: &[u8]) -> Self;
    fn invert_white_is_zero(self) -> Self;
    fn apply_predictor(data: &mut [Self], data_w: usize, data_h: usize, spp: usize)
    where
        Self: WrappingAdd,
    {
        apply_horizontal_predictor(data, data_w, data_h, spp);
    }
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
    #[inline(always)]
    fn apply_predictor(data: &mut [Self], data_w: usize, data_h: usize, spp: usize) {
        apply_horizontal_predictor_u8(data, data_w, data_h, spp);
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
    ($t:ty, $inv:expr, $pred:expr) => {
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
            #[inline(always)]
            fn apply_predictor(data: &mut [Self], data_w: usize, data_h: usize, spp: usize) {
                $pred(data, data_w, data_h, spp);
            }
        }
    };
}

impl_tiff_int_sample!(u16, |x: u16| 65535 - x, apply_horizontal_predictor_u16);
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
        T::apply_predictor(dst, data_w, data_h, spp);
    }

    if photometric == PhotometricInterpretation::WhiteIsZero {
        for item in dst.iter_mut() {
            *item = item.invert_white_is_zero();
        }
    }

    Ok(())
}

/// NEON Kogge-Stone parallel prefix addition on 8-bit bytes with step 1.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn rev_hpredict_neon_u8_spp1(input: &mut [u8]) {
    use std::arch::aarch64::*;
    let len = input.len();
    let chunks = len / 16;
    let zero = vdupq_n_u8(0);
    let mut prev_carry = 0u8;

    let ptr = input.as_mut_ptr();
    for i in 0..chunks {
        let offset = i * 16;
        let mut v = vld1q_u8(ptr.add(offset));

        v = vaddq_u8(v, vextq_u8(zero, v, 15));
        v = vaddq_u8(v, vextq_u8(zero, v, 14));
        v = vaddq_u8(v, vextq_u8(zero, v, 12));
        v = vaddq_u8(v, vextq_u8(zero, v, 8));

        v = vaddq_u8(v, vdupq_n_u8(prev_carry));

        vst1q_u8(ptr.add(offset), v);
        prev_carry = vgetq_lane_u8::<15>(v);
    }

    for col in (chunks * 16).max(1)..len {
        input[col] = input[col].wrapping_add(input[col - 1]);
    }
}

/// NEON accelerated TIFF Predictor 3 for 32-bit floating point samples when spp == 1.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn fp_predict_f32_neon_spp1(input: &mut [u8], output: &mut [f32]) {
    use std::arch::aarch64::*;
    rev_hpredict_neon_u8_spp1(input);

    let plane_stride = input.len() / 4;
    let out_len = output.len();
    let chunks = out_len / 16;

    let p0 = input.as_ptr();
    let p1 = p0.add(plane_stride);
    let p2 = p0.add(plane_stride * 2);
    let p3 = p0.add(plane_stride * 3);
    let out_ptr = output.as_mut_ptr() as *mut u8;

    for i in 0..chunks {
        let offset = i * 16;
        let v0 = vld1q_u8(p0.add(offset));
        let v1 = vld1q_u8(p1.add(offset));
        let v2 = vld1q_u8(p2.add(offset));
        let v3 = vld1q_u8(p3.add(offset));

        // Little-endian memory representation for big-endian IEEE 754 f32 is [p3, p2, p1, p0]
        let interleaved = uint8x16x4_t(v3, v2, v1, v0);
        vst4q_u8(out_ptr.add(offset * 4), interleaved);
    }

    for i in (chunks * 16)..out_len {
        output[i] = f32::from_bits(u32::from_be_bytes([
            input[i],
            input[plane_stride + i],
            input[plane_stride * 2 + i],
            input[plane_stride * 3 + i],
        ]));
    }
}

/// NEON accelerated TIFF Predictor 3 for 64-bit floating point samples when spp == 1.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn fp_predict_f64_neon_spp1(input: &mut [u8], output: &mut [f64]) {
    use std::arch::aarch64::*;
    rev_hpredict_neon_u8_spp1(input);

    let plane_stride = input.len() / 8;
    let out_len = output.len();
    let chunks = out_len / 16;

    let p0 = input.as_ptr();
    let p1 = p0.add(plane_stride);
    let p2 = p0.add(plane_stride * 2);
    let p3 = p0.add(plane_stride * 3);
    let p4 = p0.add(plane_stride * 4);
    let p5 = p0.add(plane_stride * 5);
    let p6 = p0.add(plane_stride * 6);
    let p7 = p0.add(plane_stride * 7);
    let out_ptr = output.as_mut_ptr() as *mut u8;

    for i in 0..chunks {
        let offset = i * 16;
        let v0 = vld1q_u8(p0.add(offset));
        let v1 = vld1q_u8(p1.add(offset));
        let v2 = vld1q_u8(p2.add(offset));
        let v3 = vld1q_u8(p3.add(offset));
        let v4 = vld1q_u8(p4.add(offset));
        let v5 = vld1q_u8(p5.add(offset));
        let v6 = vld1q_u8(p6.add(offset));
        let v7 = vld1q_u8(p7.add(offset));

        // Interleave into 16-bit pairs: (v7, v6), (v5, v4), (v3, v2), (v1, v0)
        let p76_lo = vreinterpretq_u16_u8(vzip1q_u8(v7, v6));
        let p76_hi = vreinterpretq_u16_u8(vzip2q_u8(v7, v6));
        let p54_lo = vreinterpretq_u16_u8(vzip1q_u8(v5, v4));
        let p54_hi = vreinterpretq_u16_u8(vzip2q_u8(v5, v4));

        let p32_lo = vreinterpretq_u16_u8(vzip1q_u8(v3, v2));
        let p32_hi = vreinterpretq_u16_u8(vzip2q_u8(v3, v2));
        let p10_lo = vreinterpretq_u16_u8(vzip1q_u8(v1, v0));
        let p10_hi = vreinterpretq_u16_u8(vzip2q_u8(v1, v0));

        // Interleave 16-bit pairs into 32-bit words
        let w_lo_0 = vreinterpretq_u32_u16(vzip1q_u16(p76_lo, p54_lo));
        let w_lo_1 = vreinterpretq_u32_u16(vzip2q_u16(p76_lo, p54_lo));
        let w_hi_0 = vreinterpretq_u32_u16(vzip1q_u16(p32_lo, p10_lo));
        let w_hi_1 = vreinterpretq_u32_u16(vzip2q_u16(p32_lo, p10_lo));

        let w_lo_2 = vreinterpretq_u32_u16(vzip1q_u16(p76_hi, p54_hi));
        let w_lo_3 = vreinterpretq_u32_u16(vzip2q_u16(p76_hi, p54_hi));
        let w_hi_2 = vreinterpretq_u32_u16(vzip1q_u16(p32_hi, p10_hi));
        let w_hi_3 = vreinterpretq_u32_u16(vzip2q_u16(p32_hi, p10_hi));

        // Interleave 32-bit into 64-bit and store
        let d0 = vreinterpretq_u8_u32(vzip1q_u32(w_lo_0, w_hi_0));
        let d1 = vreinterpretq_u8_u32(vzip2q_u32(w_lo_0, w_hi_0));
        let d2 = vreinterpretq_u8_u32(vzip1q_u32(w_lo_1, w_hi_1));
        let d3 = vreinterpretq_u8_u32(vzip2q_u32(w_lo_1, w_hi_1));

        let d4 = vreinterpretq_u8_u32(vzip1q_u32(w_lo_2, w_hi_2));
        let d5 = vreinterpretq_u8_u32(vzip2q_u32(w_lo_2, w_hi_2));
        let d6 = vreinterpretq_u8_u32(vzip1q_u32(w_lo_3, w_hi_3));
        let d7 = vreinterpretq_u8_u32(vzip2q_u32(w_lo_3, w_hi_3));

        let dst_chunk = out_ptr.add(offset * 8);
        vst1q_u8(dst_chunk, d0);
        vst1q_u8(dst_chunk.add(16), d1);
        vst1q_u8(dst_chunk.add(32), d2);
        vst1q_u8(dst_chunk.add(48), d3);
        vst1q_u8(dst_chunk.add(64), d4);
        vst1q_u8(dst_chunk.add(80), d5);
        vst1q_u8(dst_chunk.add(96), d6);
        vst1q_u8(dst_chunk.add(112), d7);
    }

    for i in (chunks * 16)..out_len {
        output[i] = f64::from_bits(u64::from_be_bytes([
            input[i],
            input[plane_stride + i],
            input[plane_stride * 2 + i],
            input[plane_stride * 3 + i],
            input[plane_stride * 4 + i],
            input[plane_stride * 5 + i],
            input[plane_stride * 6 + i],
            input[plane_stride * 7 + i],
        ]));
    }
}

/// Decode TIFF floating point predictor for 32-bit floats with NEON acceleration when available.
#[inline]
pub fn decode_fp_predict_f32(input: &mut [u8], output: &mut [f32], spp: usize) {
    #[cfg(target_arch = "aarch64")]
    if spp == 1 {
        unsafe {
            fp_predict_f32_neon_spp1(input, output);
        }
        return;
    }

    fp_predict_f32(input, output, spp);
}

/// Decode TIFF floating point predictor for 64-bit floats with NEON acceleration when available.
#[inline]
pub fn decode_fp_predict_f64(input: &mut [u8], output: &mut [f64], spp: usize) {
    #[cfg(target_arch = "aarch64")]
    if spp == 1 {
        unsafe {
            fp_predict_f64_neon_spp1(input, output);
        }
        return;
    }

    fp_predict_f64(input, output, spp);
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
                decode_fp_predict_f32(s, d, spp);
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
                decode_fp_predict_f64(s, d, spp);
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

    #[test]
    fn test_decode_fp_predict_f32_parity() {
        let test_sizes = [1, 5, 15, 16, 17, 31, 32, 33, 64, 127, 256, 512];
        for &size in &test_sizes {
            let mut raw_bytes_scalar = vec![0u8; size * 4];
            for (i, b) in raw_bytes_scalar.iter_mut().enumerate() {
                *b = ((i * 37 + 11) % 256) as u8;
            }
            let mut raw_bytes_neon = raw_bytes_scalar.clone();

            let mut out_scalar = vec![0.0f32; size];
            let mut out_neon = vec![0.0f32; size];

            tiff::decoder::fp_predict_f32(&mut raw_bytes_scalar, &mut out_scalar, 1);
            decode_fp_predict_f32(&mut raw_bytes_neon, &mut out_neon, 1);

            assert_eq!(
                raw_bytes_scalar, raw_bytes_neon,
                "Byte prefix sum mismatch at size {}",
                size
            );

            for i in 0..size {
                assert_eq!(
                    out_scalar[i].to_bits(),
                    out_neon[i].to_bits(),
                    "Float mismatch at index {} for size {}",
                    i,
                    size
                );
            }
        }
    }

    #[test]
    fn test_decode_fp_predict_f64_parity() {
        let test_sizes = [1, 5, 15, 16, 17, 31, 32, 33, 64, 127, 256];
        for &size in &test_sizes {
            let mut raw_bytes_scalar = vec![0u8; size * 8];
            for (i, b) in raw_bytes_scalar.iter_mut().enumerate() {
                *b = ((i * 41 + 13) % 256) as u8;
            }
            let mut raw_bytes_neon = raw_bytes_scalar.clone();

            let mut out_scalar = vec![0.0f64; size];
            let mut out_neon = vec![0.0f64; size];

            tiff::decoder::fp_predict_f64(&mut raw_bytes_scalar, &mut out_scalar, 1);
            decode_fp_predict_f64(&mut raw_bytes_neon, &mut out_neon, 1);

            assert_eq!(
                raw_bytes_scalar, raw_bytes_neon,
                "f64 byte prefix sum mismatch at size {}",
                size
            );

            for i in 0..size {
                assert_eq!(
                    out_scalar[i].to_bits(),
                    out_neon[i].to_bits(),
                    "f64 float mismatch at index {} for size {}",
                    i,
                    size
                );
            }
        }
    }

    #[test]
    fn test_apply_horizontal_predictor_u8_parity() {
        let test_widths = [1, 7, 15, 16, 17, 32, 64, 128, 255, 256];
        for &w in &test_widths {
            let h = 3;
            let mut data_scalar = vec![0u8; w * h];
            for (i, b) in data_scalar.iter_mut().enumerate() {
                *b = ((i * 19 + 7) % 256) as u8;
            }
            let mut data_neon = data_scalar.clone();

            apply_horizontal_predictor(&mut data_scalar, w, h, 1);
            apply_horizontal_predictor_u8(&mut data_neon, w, h, 1);

            assert_eq!(
                data_scalar, data_neon,
                "u8 horizontal predictor mismatch at width {}",
                w
            );
        }
    }

    #[test]
    fn test_apply_horizontal_predictor_u16_parity() {
        let test_widths = [1, 7, 8, 9, 15, 16, 17, 32, 64, 128, 255, 256];
        for &w in &test_widths {
            let h = 3;
            let mut data_scalar = vec![0u16; w * h];
            for (i, b) in data_scalar.iter_mut().enumerate() {
                *b = ((i * 31 + 17) % 65536) as u16;
            }
            let mut data_neon = data_scalar.clone();

            apply_horizontal_predictor(&mut data_scalar, w, h, 1);
            apply_horizontal_predictor_u16(&mut data_neon, w, h, 1);

            assert_eq!(
                data_scalar, data_neon,
                "u16 horizontal predictor mismatch at width {}",
                w
            );
        }
    }
}
