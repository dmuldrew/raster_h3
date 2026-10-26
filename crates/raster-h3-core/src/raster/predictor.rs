//! TIFF Predictor Decoding and Sample Unpacking Utilities
//!
//! Centralizes TIFF Predictor 2 (Horizontal Differencing), Predictor 3 (Floating Point Differencing),
//! and sample unpacking across integer and floating-point types.

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
pub fn apply_horizontal_predictor_u8(data: &mut [u8], data_w: usize, data_h: usize, spp: usize) {
    if spp != 1 {
        apply_horizontal_predictor(data, data_w, data_h, spp);
        return;
    }

    #[cfg(target_arch = "aarch64")]
    {
        for r in 0..data_h {
            let row = &mut data[r * data_w..(r + 1) * data_w];
            // SAFETY: NEON is a baseline feature of every aarch64 target, and the
            // kernel only touches `row` within its bounds.
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
pub fn apply_horizontal_predictor_u16(data: &mut [u16], data_w: usize, data_h: usize, spp: usize) {
    if spp != 1 {
        apply_horizontal_predictor(data, data_w, data_h, spp);
        return;
    }

    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        let chunks = data_w / 8;
        // SAFETY: NEON is a baseline feature of every aarch64 target.
        let zero = unsafe { vdupq_n_u16(0) };
        for r in 0..data_h {
            let row = &mut data[r * data_w..(r + 1) * data_w];
            let mut prev_carry = 0u16;
            let ptr = row.as_mut_ptr();
            // SAFETY: NEON is baseline on aarch64. `row` has `data_w` elements and
            // each iteration loads/stores the 8 `u16`s at `offset..offset + 8`,
            // where `offset + 8 <= chunks * 8 <= data_w`; `vld1q`/`vst1q` only
            // need `u16` alignment, which `row`'s pointer has.
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
///
/// Sealed: [`unpack_integer_samples`] memcpys raw bytes into `[Self]`, which is
/// only sound for primitive integers (every bit pattern valid, `BYTES` equal to
/// `size_of::<Self>()`), so outside implementations are not allowed.
pub trait TiffSample: sealed::Sealed + Copy + Default + 'static {
    const BYTES: usize = size_of::<Self>();
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

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for i8 {}
    impl Sealed for u16 {}
    impl Sealed for i16 {}
    impl Sealed for u32 {}
    impl Sealed for i32 {}
    impl Sealed for u64 {}
    impl Sealed for i64 {}
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

/// Layout and encoding of one decompressed TIFF chunk.
#[derive(Debug, Clone, Copy)]
pub struct ChunkEncoding {
    /// Allocated tile width in pixels; the source row stride.
    pub tile_w: usize,
    /// Valid pixel columns to copy from each row.
    pub data_w: usize,
    /// Valid rows to copy.
    pub data_h: usize,
    /// Interleaved samples per pixel.
    pub spp: usize,
    pub byte_order: TiffByteOrder,
    pub predictor: Predictor,
    pub photometric: PhotometricInterpretation,
}

impl ChunkEncoding {
    fn validate(self, src_len: usize, dst_len: usize, sample_bytes: usize) -> Result<()> {
        let invalid = || {
            RasterH3Error::InvalidMetadata("invalid or truncated decoded TIFF chunk layout".into())
        };
        if self.spp == 0 || self.data_w == 0 || self.data_h == 0 || self.data_w > self.tile_w {
            return Err(invalid());
        }
        let src_stride = self
            .tile_w
            .checked_mul(self.spp)
            .and_then(|n| n.checked_mul(sample_bytes))
            .ok_or_else(invalid)?;
        let dst_stride = self.data_w.checked_mul(self.spp).ok_or_else(invalid)?;
        let dst_bytes = dst_stride.checked_mul(sample_bytes).ok_or_else(invalid)?;
        let samples = dst_stride.checked_mul(self.data_h).ok_or_else(invalid)?;
        // Predictor 3 needs all byte planes, including the padding in the last row.
        let last_row = if self.predictor == Predictor::FloatingPoint {
            src_stride
        } else {
            dst_bytes
        };
        let required = src_stride
            .checked_mul(self.data_h - 1)
            .and_then(|n| n.checked_add(last_row))
            .ok_or_else(invalid)?;
        if src_len < required || dst_len < samples {
            return Err(invalid());
        }
        Ok(())
    }
}

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
    encoding: ChunkEncoding,
) -> Result<()> {
    encoding.validate(src.len(), dst.len(), T::BYTES)?;
    let ChunkEncoding {
        tile_w,
        data_w,
        data_h,
        spp,
        byte_order,
        predictor,
        photometric,
    } = encoding;
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
            let total_samples = data_w * data_h * spp;
            let s = &src[..total_samples * sample_bytes];
            let d = &mut dst[..total_samples];
            // SAFETY: both slices were bounds-checked above to span exactly
            // `total_samples * sample_bytes` bytes (`TiffSample` is sealed to
            // primitive integers, so `BYTES == size_of::<T>()`); they are distinct
            // borrows so cannot overlap; `u8` copies need no alignment; and every
            // byte pattern is a valid primitive integer.
            unsafe {
                std::ptr::copy_nonoverlapping(s.as_ptr(), d.as_mut_ptr() as *mut u8, s.len());
            }
        } else {
            for r in 0..data_h {
                let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                // SAFETY: `s` is `dst_stride_bytes` long and `d` holds `dst_stride`
                // samples of `sample_bytes == size_of::<T>()` each, so both span
                // `dst_stride_bytes`; distinct borrows, byte-wise copy, and any bit
                // pattern is a valid primitive integer.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        s.as_ptr(),
                        d.as_mut_ptr() as *mut u8,
                        dst_stride_bytes,
                    );
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
///
/// # Safety
/// The CPU must support NEON (always true on aarch64 targets). Memory accesses
/// stay within `input`, so there are no other requirements.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn rev_hpredict_neon_u8_spp1(input: &mut [u8]) {
    use std::arch::aarch64::*;
    let len = input.len();
    let chunks = len / 16;
    // SAFETY: NEON availability is this function's precondition.
    let zero = unsafe { vdupq_n_u8(0) };
    let mut prev_carry = 0u8;

    let ptr = input.as_mut_ptr();
    // SAFETY: NEON is available (precondition). Each iteration loads and stores
    // the 16 bytes at `offset..offset + 16 <= chunks * 16 <= len`, all inside
    // `input`, and `u8` vectors need no alignment.
    unsafe {
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
    }

    for col in (chunks * 16).max(1)..len {
        input[col] = input[col].wrapping_add(input[col - 1]);
    }
}

/// NEON accelerated TIFF Predictor 3 for 32-bit floating point samples when spp == 1.
///
/// # Safety
/// The CPU must support NEON, and `output.len() <= input.len() / 4` (each of the
/// four byte planes in `input` must hold at least one byte per output sample).
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn fp_predict_f32_neon_spp1(input: &mut [u8], output: &mut [f32]) {
    use std::arch::aarch64::*;
    debug_assert!(output.len() <= input.len() / 4);
    // SAFETY: NEON availability is forwarded from our own precondition.
    unsafe { rev_hpredict_neon_u8_spp1(input) };

    let plane_stride = input.len() / 4;
    let out_len = output.len();
    let chunks = out_len / 16;

    // SAFETY: NEON is available (precondition). Plane `k` starts at
    // `k * plane_stride` and reads `offset..offset + 16 <= chunks * 16 <= out_len
    // <= plane_stride` (precondition), so every load stays inside `input`. Each
    // store writes 64 bytes at `offset * 4`, ending at or before `out_len * 4`,
    // the byte length of `output`. Byte vectors need no alignment, and any byte
    // pattern is a valid `f32`.
    unsafe {
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
///
/// # Safety
/// The CPU must support NEON, and `output.len() <= input.len() / 8` (each of the
/// eight byte planes in `input` must hold at least one byte per output sample).
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn fp_predict_f64_neon_spp1(input: &mut [u8], output: &mut [f64]) {
    use std::arch::aarch64::*;
    debug_assert!(output.len() <= input.len() / 8);
    // SAFETY: NEON availability is forwarded from our own precondition.
    unsafe { rev_hpredict_neon_u8_spp1(input) };

    let plane_stride = input.len() / 8;
    let out_len = output.len();
    let chunks = out_len / 16;

    // SAFETY: NEON is available (precondition). Plane `k` starts at
    // `k * plane_stride` and reads `offset..offset + 16 <= chunks * 16 <= out_len
    // <= plane_stride` (precondition), so every load stays inside `input`. Each
    // iteration stores 128 bytes at `offset * 8`, ending at or before
    // `out_len * 8`, the byte length of `output`. Byte vectors need no alignment,
    // and any byte pattern is a valid `f64`.
    unsafe {
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

/// Scalar TIFF Predictor 3 for 32-bit floats: undo byte-wise horizontal differencing, then
/// reassemble each float from its four big-endian byte planes.
///
/// Vendored from `tiff` 0.9, which stopped exporting it in 0.11.
pub(crate) fn fp_predict_f32(input: &mut [u8], output: &mut [f32], spp: usize) {
    for i in spp..input.len() {
        input[i] = input[i].wrapping_add(input[i - spp]);
    }
    let plane = input.len() / 4;
    for (i, out) in output.iter_mut().enumerate() {
        *out = f32::from_be_bytes([
            input[i],
            input[plane + i],
            input[plane * 2 + i],
            input[plane * 3 + i],
        ]);
    }
}

/// Scalar TIFF Predictor 3 for 64-bit floats; see [`fp_predict_f32`].
pub(crate) fn fp_predict_f64(input: &mut [u8], output: &mut [f64], spp: usize) {
    for i in spp..input.len() {
        input[i] = input[i].wrapping_add(input[i - spp]);
    }
    let plane = input.len() / 8;
    for (i, out) in output.iter_mut().enumerate() {
        *out = f64::from_be_bytes(std::array::from_fn(|b| input[plane * b + i]));
    }
}

/// Decode TIFF floating point predictor for 32-bit floats with NEON acceleration when available.
#[inline]
pub fn decode_fp_predict_f32(input: &mut [u8], output: &mut [f32], spp: usize) {
    // Shorter inputs fall through to the scalar path, which bounds-checks.
    #[cfg(target_arch = "aarch64")]
    if spp == 1 && output.len() <= input.len() / 4 {
        // SAFETY: NEON is baseline on aarch64 and the length precondition was
        // just checked.
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
    // Shorter inputs fall through to the scalar path, which bounds-checks.
    #[cfg(target_arch = "aarch64")]
    if spp == 1 && output.len() <= input.len() / 8 {
        // SAFETY: NEON is baseline on aarch64 and the length precondition was
        // just checked.
        unsafe {
            fp_predict_f64_neon_spp1(input, output);
        }
        return;
    }

    fp_predict_f64(input, output, spp);
}

/// Unpack 32-bit floating point samples from decompressed TIFF chunk bytes.
pub fn unpack_f32(src: &mut [u8], dst: &mut [f32], encoding: ChunkEncoding) -> Result<()> {
    encoding.validate(src.len(), dst.len(), 4)?;
    let ChunkEncoding {
        tile_w,
        data_w,
        data_h,
        spp,
        byte_order,
        predictor,
        photometric,
    } = encoding;
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
                    let total_samples = data_w * data_h * spp;
                    let s = &src[..total_samples * 4];
                    let d = &mut dst[..total_samples];
                    // SAFETY: both slices were bounds-checked above to span
                    // `total_samples * 4` bytes; distinct borrows cannot overlap;
                    // the copy is byte-wise; any bit pattern is a valid `f32`.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            s.as_ptr(),
                            d.as_mut_ptr() as *mut u8,
                            s.len(),
                        );
                    }
                } else {
                    for r in 0..data_h {
                        let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                        let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                        // SAFETY: `s` and `d` both span `dst_stride_bytes` bytes
                        // (slice-checked above), don't overlap, and any bit pattern
                        // is a valid `f32`.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                s.as_ptr(),
                                d.as_mut_ptr() as *mut u8,
                                dst_stride_bytes,
                            );
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
                                    s[i * 4],
                                    s[i * 4 + 1],
                                    s[i * 4 + 2],
                                    s[i * 4 + 3],
                                ]));
                            }
                        }
                        TiffByteOrder::BigEndian => {
                            for (i, item) in d.iter_mut().enumerate() {
                                *item = f32::from_bits(u32::from_be_bytes([
                                    s[i * 4],
                                    s[i * 4 + 1],
                                    s[i * 4 + 2],
                                    s[i * 4 + 3],
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
pub fn unpack_f64(src: &mut [u8], dst: &mut [f64], encoding: ChunkEncoding) -> Result<()> {
    encoding.validate(src.len(), dst.len(), 8)?;
    let ChunkEncoding {
        tile_w,
        data_w,
        data_h,
        spp,
        byte_order,
        predictor,
        photometric,
    } = encoding;
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
                    let total_samples = data_w * data_h * spp;
                    let s = &src[..total_samples * 8];
                    let d = &mut dst[..total_samples];
                    // SAFETY: both slices were bounds-checked above to span
                    // `total_samples * 8` bytes; distinct borrows cannot overlap;
                    // the copy is byte-wise; any bit pattern is a valid `f64`.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            s.as_ptr(),
                            d.as_mut_ptr() as *mut u8,
                            s.len(),
                        );
                    }
                } else {
                    for r in 0..data_h {
                        let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride_bytes];
                        let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                        // SAFETY: `s` and `d` both span `dst_stride_bytes` bytes
                        // (slice-checked above), don't overlap, and any bit pattern
                        // is a valid `f64`.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                s.as_ptr(),
                                d.as_mut_ptr() as *mut u8,
                                dst_stride_bytes,
                            );
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
                                    s[i * 8],
                                    s[i * 8 + 1],
                                    s[i * 8 + 2],
                                    s[i * 8 + 3],
                                    s[i * 8 + 4],
                                    s[i * 8 + 5],
                                    s[i * 8 + 6],
                                    s[i * 8 + 7],
                                ]));
                            }
                        }
                        TiffByteOrder::BigEndian => {
                            for (i, item) in d.iter_mut().enumerate() {
                                *item = f64::from_bits(u64::from_be_bytes([
                                    s[i * 8],
                                    s[i * 8 + 1],
                                    s[i * 8 + 2],
                                    s[i * 8 + 3],
                                    s[i * 8 + 4],
                                    s[i * 8 + 5],
                                    s[i * 8 + 6],
                                    s[i * 8 + 7],
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
    fn unpack_rejects_truncated_buffers_and_invalid_layouts() {
        let base = ChunkEncoding {
            tile_w: 1,
            data_w: 1,
            data_h: 1,
            spp: 1,
            byte_order: TiffByteOrder::LittleEndian,
            predictor: Predictor::None,
            photometric: PhotometricInterpretation::BlackIsZero,
        };
        for encoding in [
            base,
            ChunkEncoding { spp: 0, ..base },
            ChunkEncoding { data_w: 2, ..base },
            ChunkEncoding {
                tile_w: usize::MAX,
                spp: 2,
                ..base
            },
            ChunkEncoding {
                data_h: usize::MAX,
                ..base
            },
            ChunkEncoding {
                predictor: Predictor::FloatingPoint,
                ..base
            },
        ] {
            assert!(unpack_integer_samples(&[0], &mut [0u16], encoding).is_err());
            assert!(unpack_f32(&mut [0; 3], &mut [0.0], encoding).is_err());
            // Reproduces the first truncated-input failure found by the fuzz target.
            assert!(unpack_f64(&mut [0, 8, 0, 0, 37, 0], &mut [0.0], encoding).is_err());
        }
        assert!(unpack_integer_samples::<u16>(&[0; 2], &mut [], base).is_err());
        assert!(unpack_f32(&mut [0; 4], &mut [], base).is_err());
        assert!(unpack_f64(&mut [0; 8], &mut [], base).is_err());
    }

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
            30, 40, 99, 99, // row 1: keep [30, 40]
        ];
        let mut dst = vec![0u8; 4];
        unpack_integer_samples(
            &tile,
            &mut dst,
            ChunkEncoding {
                tile_w: 4,
                data_w: 2,
                data_h: 2,
                spp: 1,
                byte_order: TiffByteOrder::LittleEndian,
                predictor: Predictor::None,
                photometric: PhotometricInterpretation::WhiteIsZero,
            },
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
            ChunkEncoding {
                tile_w: 2,
                data_w: 2,
                data_h: 1,
                spp: 1,
                byte_order: TiffByteOrder::LittleEndian,
                predictor: Predictor::Horizontal,
                photometric: PhotometricInterpretation::BlackIsZero,
            },
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

            fp_predict_f32(&mut raw_bytes_scalar, &mut out_scalar, 1);
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

            fp_predict_f64(&mut raw_bytes_scalar, &mut out_scalar, 1);
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
