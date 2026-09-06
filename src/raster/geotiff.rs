use std::fs::File;
use std::io::{Cursor, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use memmap2::Mmap;
use tiff::decoder::{
    fp_predict_f32, fp_predict_f64, ChunkType, Decoder, DecodingBuffer, DecodingResult,
};
use tiff::tags::{
    CompressionMethod, PhotometricInterpretation, Predictor, SampleFormat, Tag,
};

use crate::error::{RasterH3Error, Result};
use crate::raster::geotransform::GeoTransform;
use crate::raster::http_range::{is_remote_url, HttpRangeReader, RemoteHttpSource};
use crate::raster::RasterChunk;

/// TIFF byte order (Intel Little-Endian vs Motorola Big-Endian)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiffByteOrder {
    LittleEndian,
    BigEndian,
}

/// Parsed GeoTIFF metadata
#[derive(Debug, Clone)]
pub struct GeoTiffMetadata {
    pub width: u32,
    pub height: u32,
    pub geotransform: GeoTransform,
    pub nodata: Option<f64>,
    pub epsg: Option<u32>,
    pub proj_string: Option<String>,
    pub samples_per_pixel: u16,
}

/// Chunk layout information (tiled vs striped)
#[derive(Debug, Clone, Copy)]
pub struct ChunkLayout {
    pub chunk_width: u32,
    pub chunk_height: u32,
    pub chunks_across: u32,
    pub chunks_down: u32,
    pub total_chunks: u32,
}

impl ChunkLayout {
    /// Create layout for tiled TIFFs
    pub fn new_tiled(image_width: u32, image_height: u32, tile_width: u32, tile_height: u32) -> Self {
        let chunks_across = (image_width + tile_width - 1) / tile_width;
        let chunks_down = (image_height + tile_height - 1) / tile_height;
        Self {
            chunk_width: tile_width,
            chunk_height: tile_height,
            chunks_across,
            chunks_down,
            total_chunks: chunks_across * chunks_down,
        }
    }

    /// Create layout for striped TIFFs
    pub fn new_striped(image_width: u32, image_height: u32, rows_per_strip: u32) -> Self {
        let chunks_down = (image_height + rows_per_strip - 1) / rows_per_strip;
        Self {
            chunk_width: image_width,
            chunk_height: rows_per_strip,
            chunks_across: 1,
            chunks_down,
            total_chunks: chunks_down,
        }
    }

    /// Calculate grid bounds for a given chunk index
    pub fn get_chunk_bounds(&self, chunk_index: u32, image_width: u32, image_height: u32) -> RasterChunk {
        let chunk_col = chunk_index % self.chunks_across;
        let chunk_row = chunk_index / self.chunks_across;

        let col_offset = chunk_col * self.chunk_width;
        let row_offset = chunk_row * self.chunk_height;

        let width = (image_width.saturating_sub(col_offset)).min(self.chunk_width);
        let height = (image_height.saturating_sub(row_offset)).min(self.chunk_height);

        RasterChunk {
            col_offset,
            row_offset,
            width,
            height,
        }
    }
}

/// Backing storage for a GeoTIFF: local memory-mapped file or remote HTTP/S3 stream
#[derive(Clone)]
pub enum RasterSource {
    Local(Arc<Mmap>),
    Remote(Arc<RemoteHttpSource>),
}

/// TIFF chunk layout and compression metadata for SIMD-accelerated direct decoding
#[derive(Debug, Clone)]
pub struct TiffChunkInfo {
    pub compression: CompressionMethod,
    pub predictor: Predictor,
    pub chunk_type: ChunkType,
    pub chunk_offsets: Arc<[u64]>,
    pub chunk_bytes: Arc<[u64]>,
    pub chunk_dimensions: (u32, u32),
    pub bits_per_sample: u8,
    pub sample_format: SampleFormat,
    pub byte_order: TiffByteOrder,
    pub photometric: PhotometricInterpretation,
}

/// Zero-copy memory-mapped or remote streaming GeoTIFF reader that decodes chunks on-demand
#[derive(Clone)]
pub struct GeoTiffStreamReader {
    pub file_path: PathBuf,
    pub source: RasterSource,
    pub metadata: GeoTiffMetadata,
    pub chunk_layout: ChunkLayout,
    pub chunk_info: Option<Arc<TiffChunkInfo>>,
}

/// Internal decoder variant: Cursor over memory-mapped slice or streaming HttpRangeReader
pub enum InnerDecoder<'a> {
    Local(Decoder<Cursor<&'a [u8]>>),
    Remote(Decoder<HttpRangeReader>),
}

impl<'a> InnerDecoder<'a> {
    #[inline]
    fn chunk_data_dimensions(&mut self, chunk: u32) -> (u32, u32) {
        match self {
            InnerDecoder::Local(d) => d.chunk_data_dimensions(chunk),
            InnerDecoder::Remote(d) => d.chunk_data_dimensions(chunk),
        }
    }

    #[inline]
    fn read_chunk(&mut self, chunk: u32) -> Result<DecodingResult> {
        match self {
            InnerDecoder::Local(d) => Ok(d.read_chunk(chunk)?),
            InnerDecoder::Remote(d) => Ok(d.read_chunk(chunk)?),
        }
    }

    #[inline]
    fn read_chunk_to_buffer(
        &mut self,
        buffer: DecodingBuffer<'_>,
        chunk: u32,
        width: usize,
    ) -> tiff::TiffResult<()> {
        match self {
            InnerDecoder::Local(d) => d.read_chunk_to_buffer(buffer, chunk, width),
            InnerDecoder::Remote(d) => d.read_chunk_to_buffer(buffer, chunk, width),
        }
    }
}

/// Persistent chunk decoder that reuses the TIFF decoder across reads.
/// This avoids re-parsing IFD headers, tag tables, and strip/tile offset
/// arrays on every chunk read — the single largest I/O optimization.
pub struct ChunkDecoder<'a> {
    inner: InnerDecoder<'a>,
    chunk_layout: ChunkLayout,
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    chunk_info: Option<Arc<TiffChunkInfo>>,
    mmap: Option<&'a [u8]>,
    libdeflater: Option<libdeflater::Decompressor>,
    decomp_scratch: Vec<u8>,
}

impl<'a> ChunkDecoder<'a> {
    /// Read and decode a single chunk using persistent decoder or SIMD libdeflater
    pub fn read_chunk(&mut self, chunk_index: u32) -> Result<(RasterChunk, DecodingResult)> {
        let chunk_bounds = self.chunk_layout.get_chunk_bounds(
            chunk_index,
            self.width,
            self.height,
        );

        if let Ok(Some(data)) = self.decompress_chunk_simd(chunk_index, None) {
            return Ok((chunk_bounds, data));
        }

        let data = self.inner.read_chunk(chunk_index)?;
        Ok((chunk_bounds, data))
    }

    /// Read and decode a single chunk into an existing buffer if possible, avoiding reallocations
    pub fn read_chunk_into(
        &mut self,
        chunk_index: u32,
        mut buffer: DecodingResult,
    ) -> Result<(RasterChunk, DecodingResult)> {
        let chunk_bounds = self.chunk_layout.get_chunk_bounds(
            chunk_index,
            self.width,
            self.height,
        );

        match self.decompress_chunk_simd(chunk_index, Some(&mut buffer)) {
            Ok(None) => return Ok((chunk_bounds, buffer)),
            Ok(Some(new_buf)) => return Ok((chunk_bounds, new_buf)),
            Err(_) => {}
        }

        let data_dims = self.inner.chunk_data_dimensions(chunk_index);
        let spp = self.samples_per_pixel.max(1) as usize;
        let required_len = (data_dims.0 as usize) * (data_dims.1 as usize) * spp;

        let decoded_ok = match &mut buffer {
            DecodingResult::U8(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::U8(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::U16(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::U16(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::U32(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::U32(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::U64(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::U64(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I8(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::I8(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I16(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::I16(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I32(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::I32(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I64(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::I64(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::F32(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0.0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::F32(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::F64(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0.0);
                    self.inner
                        .read_chunk_to_buffer(DecodingBuffer::F64(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
        };

        if decoded_ok {
            Ok((chunk_bounds, buffer))
        } else {
            let data = self.inner.read_chunk(chunk_index)?;
            Ok((chunk_bounds, data))
        }
    }

    /// Attempt SIMD-accelerated chunk decompression with libdeflater.
    /// If target_buffer is provided and of matching type, it will be populated in-place.
    /// Returns Ok(None) if target_buffer was populated in-place.
    /// Returns Ok(Some(new_result)) if a new DecodingResult was created.
    /// Returns Err(_) if anything is unsupported or failed, triggering standard fallback.
    fn decompress_chunk_simd(
        &mut self,
        chunk_index: u32,
        target_buffer: Option<&mut DecodingResult>,
    ) -> Result<Option<DecodingResult>> {
        let info = self
            .chunk_info
            .as_ref()
            .ok_or_else(|| RasterH3Error::InvalidMetadata("no chunk info".into()))?;

        if !matches!(
            info.compression,
            CompressionMethod::Deflate | CompressionMethod::OldDeflate
        ) {
            return Err(RasterH3Error::InvalidMetadata("not deflate".into()));
        }

        let mmap = self
            .mmap
            .ok_or_else(|| RasterH3Error::InvalidMetadata("no mmap source".into()))?;

        let decompressor = self
            .libdeflater
            .as_mut()
            .ok_or_else(|| RasterH3Error::InvalidMetadata("no decompressor".into()))?;

        let idx = chunk_index as usize;
        let offset = *info
            .chunk_offsets
            .get(idx)
            .ok_or_else(|| RasterH3Error::InvalidMetadata("invalid chunk offset".into()))?
            as usize;
        let compressed_len = *info
            .chunk_bytes
            .get(idx)
            .ok_or_else(|| RasterH3Error::InvalidMetadata("invalid chunk bytes".into()))?
            as usize;

        if offset.checked_add(compressed_len).map_or(true, |end| end > mmap.len()) {
            return Err(RasterH3Error::InvalidMetadata("chunk offset out of bounds".into()));
        }

        let compressed_slice = &mmap[offset..offset + compressed_len];
        let (chunk_w, chunk_h) = info.chunk_dimensions;
        let bounds = self.chunk_layout.get_chunk_bounds(chunk_index, self.width, self.height);
        let data_w = bounds.width as usize;
        let data_h = bounds.height as usize;
        let spp = (self.samples_per_pixel.max(1)) as usize;
        let byte_len = (info.bits_per_sample as usize / 8).max(1);

        let raw_chunk_bytes = match info.chunk_type {
            ChunkType::Tile => (chunk_w as usize) * (chunk_h as usize) * spp * byte_len,
            ChunkType::Strip => (chunk_w as usize) * data_h * spp * byte_len,
        };

        if self.decomp_scratch.len() < raw_chunk_bytes {
            self.decomp_scratch.resize(raw_chunk_bytes, 0);
        }

        let decomp_slice = &mut self.decomp_scratch[..raw_chunk_bytes];
        let decomp_res = decompressor.zlib_decompress(compressed_slice, decomp_slice);
        let decomp_ok = match decomp_res {
            Ok(_) => true,
            Err(_) => decompressor.deflate_decompress(compressed_slice, decomp_slice).is_ok(),
        };

        if !decomp_ok {
            return Err(RasterH3Error::InvalidMetadata("decompress failed".into()));
        }

        let total_samples = data_w * data_h * spp;
        let tile_w = chunk_w as usize;

        match (info.sample_format, info.bits_per_sample) {
            (SampleFormat::Uint, 8) => match target_buffer {
                Some(DecodingResult::U8(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_u8(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0u8; total_samples];
                    Self::unpack_u8(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::U8(v)))
                }
            },
            (SampleFormat::Uint, 16) => match target_buffer {
                Some(DecodingResult::U16(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_u16(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0u16; total_samples];
                    Self::unpack_u16(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::U16(v)))
                }
            },
            (SampleFormat::Uint, 32) => match target_buffer {
                Some(DecodingResult::U32(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_u32(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0u32; total_samples];
                    Self::unpack_u32(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::U32(v)))
                }
            },
            (SampleFormat::Uint, 64) => match target_buffer {
                Some(DecodingResult::U64(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_u64(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0u64; total_samples];
                    Self::unpack_u64(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::U64(v)))
                }
            },
            (SampleFormat::Int, 8) => match target_buffer {
                Some(DecodingResult::I8(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_i8(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0i8; total_samples];
                    Self::unpack_i8(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::I8(v)))
                }
            },
            (SampleFormat::Int, 16) => match target_buffer {
                Some(DecodingResult::I16(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_i16(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0i16; total_samples];
                    Self::unpack_i16(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::I16(v)))
                }
            },
            (SampleFormat::Int, 32) => match target_buffer {
                Some(DecodingResult::I32(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_i32(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0i32; total_samples];
                    Self::unpack_i32(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::I32(v)))
                }
            },
            (SampleFormat::Int, 64) => match target_buffer {
                Some(DecodingResult::I64(ref mut v)) => {
                    v.resize(total_samples, 0);
                    Self::unpack_i64(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0i64; total_samples];
                    Self::unpack_i64(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::I64(v)))
                }
            },
            (SampleFormat::IEEEFP, 32) => match target_buffer {
                Some(DecodingResult::F32(ref mut v)) => {
                    v.resize(total_samples, 0.0);
                    Self::unpack_f32(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0.0f32; total_samples];
                    Self::unpack_f32(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::F32(v)))
                }
            },
            (SampleFormat::IEEEFP, 64) => match target_buffer {
                Some(DecodingResult::F64(ref mut v)) => {
                    v.resize(total_samples, 0.0);
                    Self::unpack_f64(decomp_slice, &mut v[..total_samples], tile_w, data_w, data_h, spp, info)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0.0f64; total_samples];
                    Self::unpack_f64(decomp_slice, &mut v, tile_w, data_w, data_h, spp, info)?;
                    Ok(Some(DecodingResult::F64(v)))
                }
            },
            _ => Err(RasterH3Error::InvalidMetadata("unsupported format".into())),
        }
    }

    fn unpack_u8(
        src: &[u8],
        dst: &mut [u8],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride = tile_w * spp;
        let dst_stride = data_w * spp;
        if src_stride == dst_stride {
            dst.copy_from_slice(&src[..data_w * data_h * spp]);
        } else {
            for r in 0..data_h {
                let s = &src[r * src_stride..r * src_stride + dst_stride];
                let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                d.copy_from_slice(s);
            }
        }
        if info.predictor == Predictor::Horizontal {
            for r in 0..data_h {
                let row = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                for col in spp..row.len() {
                    row[col] = row[col].wrapping_add(row[col - spp]);
                }
            }
        }
        if info.photometric == PhotometricInterpretation::WhiteIsZero {
            for val in dst.iter_mut() {
                *val = 255 - *val;
            }
        }
        Ok(())
    }

    fn unpack_i8(
        src: &[u8],
        dst: &mut [i8],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride = tile_w * spp;
        let dst_stride = data_w * spp;
        for r in 0..data_h {
            let s = &src[r * src_stride..r * src_stride + dst_stride];
            let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
            for (i, item) in d.iter_mut().enumerate() {
                *item = s[i] as i8;
            }
        }
        if info.predictor == Predictor::Horizontal {
            for r in 0..data_h {
                let row = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                for col in spp..row.len() {
                    row[col] = row[col].wrapping_add(row[col - spp]);
                }
            }
        }
        Ok(())
    }

    fn unpack_u16(
        src: &[u8],
        dst: &mut [u16],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 2;
        let dst_stride = data_w * spp;
        for r in 0..data_h {
            let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 2];
            let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
            match info.byte_order {
                TiffByteOrder::LittleEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = u16::from_le_bytes([s[i * 2], s[i * 2 + 1]]);
                    }
                }
                TiffByteOrder::BigEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = u16::from_be_bytes([s[i * 2], s[i * 2 + 1]]);
                    }
                }
            }
            if info.predictor == Predictor::Horizontal {
                for col in spp..d.len() {
                    d[col] = d[col].wrapping_add(d[col - spp]);
                }
            }
            if info.photometric == PhotometricInterpretation::WhiteIsZero {
                for item in d.iter_mut() {
                    *item = 65535 - *item;
                }
            }
        }
        Ok(())
    }

    fn unpack_i16(
        src: &[u8],
        dst: &mut [i16],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 2;
        let dst_stride = data_w * spp;
        for r in 0..data_h {
            let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 2];
            let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
            match info.byte_order {
                TiffByteOrder::LittleEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = i16::from_le_bytes([s[i * 2], s[i * 2 + 1]]);
                    }
                }
                TiffByteOrder::BigEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = i16::from_be_bytes([s[i * 2], s[i * 2 + 1]]);
                    }
                }
            }
            if info.predictor == Predictor::Horizontal {
                for col in spp..d.len() {
                    d[col] = d[col].wrapping_add(d[col - spp]);
                }
            }
        }
        Ok(())
    }

    fn unpack_u32(
        src: &[u8],
        dst: &mut [u32],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 4;
        let dst_stride = data_w * spp;
        for r in 0..data_h {
            let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 4];
            let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
            match info.byte_order {
                TiffByteOrder::LittleEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = u32::from_le_bytes([s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3]]);
                    }
                }
                TiffByteOrder::BigEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = u32::from_be_bytes([s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3]]);
                    }
                }
            }
            if info.predictor == Predictor::Horizontal {
                for col in spp..d.len() {
                    d[col] = d[col].wrapping_add(d[col - spp]);
                }
            }
            if info.photometric == PhotometricInterpretation::WhiteIsZero {
                for item in d.iter_mut() {
                    *item = u32::MAX - *item;
                }
            }
        }
        Ok(())
    }

    fn unpack_i32(
        src: &[u8],
        dst: &mut [i32],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 4;
        let dst_stride = data_w * spp;
        for r in 0..data_h {
            let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 4];
            let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
            match info.byte_order {
                TiffByteOrder::LittleEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = i32::from_le_bytes([s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3]]);
                    }
                }
                TiffByteOrder::BigEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = i32::from_be_bytes([s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3]]);
                    }
                }
            }
            if info.predictor == Predictor::Horizontal {
                for col in spp..d.len() {
                    d[col] = d[col].wrapping_add(d[col - spp]);
                }
            }
        }
        Ok(())
    }

    fn unpack_u64(
        src: &[u8],
        dst: &mut [u64],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 8;
        let dst_stride = data_w * spp;
        for r in 0..data_h {
            let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 8];
            let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
            match info.byte_order {
                TiffByteOrder::LittleEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = u64::from_le_bytes([
                            s[i * 8], s[i * 8 + 1], s[i * 8 + 2], s[i * 8 + 3],
                            s[i * 8 + 4], s[i * 8 + 5], s[i * 8 + 6], s[i * 8 + 7],
                        ]);
                    }
                }
                TiffByteOrder::BigEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = u64::from_be_bytes([
                            s[i * 8], s[i * 8 + 1], s[i * 8 + 2], s[i * 8 + 3],
                            s[i * 8 + 4], s[i * 8 + 5], s[i * 8 + 6], s[i * 8 + 7],
                        ]);
                    }
                }
            }
            if info.predictor == Predictor::Horizontal {
                for col in spp..d.len() {
                    d[col] = d[col].wrapping_add(d[col - spp]);
                }
            }
            if info.photometric == PhotometricInterpretation::WhiteIsZero {
                for item in d.iter_mut() {
                    *item = u64::MAX - *item;
                }
            }
        }
        Ok(())
    }

    fn unpack_i64(
        src: &[u8],
        dst: &mut [i64],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 8;
        let dst_stride = data_w * spp;
        for r in 0..data_h {
            let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 8];
            let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
            match info.byte_order {
                TiffByteOrder::LittleEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = i64::from_le_bytes([
                            s[i * 8], s[i * 8 + 1], s[i * 8 + 2], s[i * 8 + 3],
                            s[i * 8 + 4], s[i * 8 + 5], s[i * 8 + 6], s[i * 8 + 7],
                        ]);
                    }
                }
                TiffByteOrder::BigEndian => {
                    for (i, item) in d.iter_mut().enumerate() {
                        *item = i64::from_be_bytes([
                            s[i * 8], s[i * 8 + 1], s[i * 8 + 2], s[i * 8 + 3],
                            s[i * 8 + 4], s[i * 8 + 5], s[i * 8 + 6], s[i * 8 + 7],
                        ]);
                    }
                }
            }
            if info.predictor == Predictor::Horizontal {
                for col in spp..d.len() {
                    d[col] = d[col].wrapping_add(d[col - spp]);
                }
            }
        }
        Ok(())
    }

    fn unpack_f32(
        src: &mut [u8],
        dst: &mut [f32],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 4;
        let dst_stride = data_w * spp;
        match info.predictor {
            Predictor::FloatingPoint => {
                for r in 0..data_h {
                    let s = &mut src[r * src_stride_bytes..(r + 1) * src_stride_bytes];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    fp_predict_f32(s, d, spp);
                    if info.photometric == PhotometricInterpretation::WhiteIsZero {
                        for item in d.iter_mut() {
                            *item = 1.0 - *item;
                        }
                    }
                }
            }
            Predictor::None => {
                for r in 0..data_h {
                    let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 4];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    match info.byte_order {
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
                    if info.photometric == PhotometricInterpretation::WhiteIsZero {
                        for item in d.iter_mut() {
                            *item = 1.0 - *item;
                        }
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

    fn unpack_f64(
        src: &mut [u8],
        dst: &mut [f64],
        tile_w: usize,
        data_w: usize,
        data_h: usize,
        spp: usize,
        info: &TiffChunkInfo,
    ) -> Result<()> {
        let src_stride_bytes = tile_w * spp * 8;
        let dst_stride = data_w * spp;
        match info.predictor {
            Predictor::FloatingPoint => {
                for r in 0..data_h {
                    let s = &mut src[r * src_stride_bytes..(r + 1) * src_stride_bytes];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    fp_predict_f64(s, d, spp);
                    if info.photometric == PhotometricInterpretation::WhiteIsZero {
                        for item in d.iter_mut() {
                            *item = 1.0 - *item;
                        }
                    }
                }
            }
            Predictor::None => {
                for r in 0..data_h {
                    let s = &src[r * src_stride_bytes..r * src_stride_bytes + dst_stride * 8];
                    let d = &mut dst[r * dst_stride..(r + 1) * dst_stride];
                    match info.byte_order {
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
                    if info.photometric == PhotometricInterpretation::WhiteIsZero {
                        for item in d.iter_mut() {
                            *item = 1.0 - *item;
                        }
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
}

impl GeoTiffStreamReader {
    /// Backward-compatible accessor for memory-mapped buffer if the source is local
    pub fn mmap(&self) -> Option<&Arc<Mmap>> {
        match &self.source {
            RasterSource::Local(mmap) => Some(mmap),
            RasterSource::Remote(_) => None,
        }
    }

    /// Extract common GeoTIFF metadata and chunk layout from an initialized Decoder
    fn parse_metadata_and_layout<R: std::io::Read + Seek>(
        decoder: &mut Decoder<R>,
    ) -> Result<(GeoTiffMetadata, ChunkLayout, Option<Arc<TiffChunkInfo>>)> {
        let (width, height) = decoder.dimensions()?;
        let geotransform = Self::extract_geotransform(decoder)?;
        let nodata = Self::extract_nodata(decoder);
        let (epsg, proj_string) = Self::extract_crs(decoder);
        let samples_per_pixel = decoder
            .get_tag_u32(Tag::SamplesPerPixel)
            .or_else(|_| decoder.get_tag_u32(Tag::Unknown(277)))
            .map(|v| v as u16)
            .unwrap_or(1);

        let (chunk_w, chunk_h) = decoder.chunk_dimensions();
        let chunks_across = (width + chunk_w - 1) / chunk_w;
        let chunks_down = (height + chunk_h - 1) / chunk_h;
        let total_chunks = chunks_across * chunks_down;

        let chunk_layout = ChunkLayout {
            chunk_width: chunk_w,
            chunk_height: chunk_h,
            chunks_across,
            chunks_down,
            total_chunks,
        };

        let metadata = GeoTiffMetadata {
            width,
            height,
            geotransform,
            nodata,
            epsg,
            proj_string,
            samples_per_pixel,
        };

        let compression = match decoder.get_tag_u32(Tag::Compression) {
            Ok(v) => CompressionMethod::from_u16(v as u16).unwrap_or(CompressionMethod::None),
            Err(_) => CompressionMethod::None,
        };

        let predictor = match decoder.get_tag_u32(Tag::Predictor) {
            Ok(v) => Predictor::from_u16(v as u16).unwrap_or(Predictor::None),
            Err(_) => Predictor::None,
        };

        let chunk_type = decoder.get_chunk_type();

        let bits_per_sample = decoder
            .get_tag_u32(Tag::BitsPerSample)
            .or_else(|_| {
                decoder
                    .get_tag_u16_vec(Tag::BitsPerSample)
                    .map(|v| v.first().copied().unwrap_or(8) as u32)
            })
            .unwrap_or(8) as u8;

        let sample_format = match decoder
            .get_tag_u32(Tag::SampleFormat)
            .or_else(|_| {
                decoder
                    .get_tag_u16_vec(Tag::SampleFormat)
                    .map(|v| v.first().copied().unwrap_or(1) as u32)
            }) {
            Ok(v) => SampleFormat::from_u16(v as u16).unwrap_or(SampleFormat::Uint),
            Err(_) => SampleFormat::Uint,
        };

        let photometric = match decoder.get_tag_u32(Tag::PhotometricInterpretation) {
            Ok(v) => PhotometricInterpretation::from_u16(v as u16)
                .unwrap_or(PhotometricInterpretation::BlackIsZero),
            Err(_) => PhotometricInterpretation::BlackIsZero,
        };

        let byte_order = if format!("{:?}", decoder.byte_order()).contains("BigEndian") {
            TiffByteOrder::BigEndian
        } else {
            TiffByteOrder::LittleEndian
        };

        let (offsets, bytes) = match chunk_type {
            ChunkType::Tile => {
                let offs = decoder.find_tag_unsigned_vec::<u64>(Tag::TileOffsets).ok().flatten();
                let bts = decoder.find_tag_unsigned_vec::<u64>(Tag::TileByteCounts).ok().flatten();
                (offs, bts)
            }
            ChunkType::Strip => {
                let offs = decoder.find_tag_unsigned_vec::<u64>(Tag::StripOffsets).ok().flatten();
                let bts = decoder.find_tag_unsigned_vec::<u64>(Tag::StripByteCounts).ok().flatten();
                (offs, bts)
            }
        };

        let chunk_info = match (offsets, bytes) {
            (Some(offs), Some(bts)) if offs.len() == bts.len() => Some(Arc::new(TiffChunkInfo {
                compression,
                predictor,
                chunk_type,
                chunk_offsets: offs.into(),
                chunk_bytes: bts.into(),
                chunk_dimensions: (chunk_w, chunk_h),
                bits_per_sample,
                sample_format,
                byte_order,
                photometric,
            })),
            _ => None,
        };

        Ok((metadata, chunk_layout, chunk_info))
    }

    /// Open and decode metadata for a local file (via mmap) or a remote URL (via HTTP Range)
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let path_str = path_buf.to_string_lossy();

        if is_remote_url(&path_str) {
            let remote = Arc::new(RemoteHttpSource::open(&path_str)?);
            let reader = HttpRangeReader::new(Arc::clone(&remote));
            let mut decoder = Decoder::new(reader)?;
            let (metadata, chunk_layout, chunk_info) = Self::parse_metadata_and_layout(&mut decoder)?;

            return Ok(Self {
                file_path: path_buf,
                source: RasterSource::Remote(remote),
                metadata,
                chunk_layout,
                chunk_info,
            });
        }

        let file = File::open(&path_buf)?;
        let mmap = unsafe { Mmap::map(&file)? };
        #[cfg(unix)]
        let _ = mmap.advise(memmap2::Advice::Sequential);
        let mmap_arc = Arc::new(mmap);

        let cursor = Cursor::new(&mmap_arc[..]);
        let mut decoder = Decoder::new(cursor)?;
        let (metadata, chunk_layout, chunk_info) = Self::parse_metadata_and_layout(&mut decoder)?;

        Ok(Self {
            file_path: path_buf,
            source: RasterSource::Local(mmap_arc),
            metadata,
            chunk_layout,
            chunk_info,
        })
    }

    /// Create a persistent ChunkDecoder that reuses the TIFF decoder for multiple chunk reads.
    /// The decoder parses the IFD once and then seeks directly to tile data on each read_chunk call.
    pub fn open_decoder(&self) -> Result<ChunkDecoder<'_>> {
        let inner = match &self.source {
            RasterSource::Local(mmap) => {
                let cursor = Cursor::new(&mmap[..]);
                let decoder = Decoder::new(cursor)?;
                InnerDecoder::Local(decoder)
            }
            RasterSource::Remote(remote) => {
                let reader = HttpRangeReader::new(Arc::clone(remote));
                let decoder = Decoder::new(reader)?;
                InnerDecoder::Remote(decoder)
            }
        };

        let mmap = match &self.source {
            RasterSource::Local(mmap) => Some(&mmap[..]),
            RasterSource::Remote(_) => None,
        };

        let libdeflater = if self.chunk_info.as_ref().map_or(false, |info| {
            matches!(
                info.compression,
                CompressionMethod::Deflate | CompressionMethod::OldDeflate
            )
        }) && mmap.is_some()
        {
            Some(libdeflater::Decompressor::new())
        } else {
            None
        };

        Ok(ChunkDecoder {
            inner,
            chunk_layout: self.chunk_layout,
            width: self.metadata.width,
            height: self.metadata.height,
            samples_per_pixel: self.metadata.samples_per_pixel,
            chunk_info: self.chunk_info.clone(),
            mmap,
            libdeflater,
            decomp_scratch: Vec::new(),
        })
    }

    /// Read and decode a single chunk on-demand directly from memory-mapped pages or remote stream.
    /// NOTE: This creates a fresh decoder per call. For sequential reads, prefer open_decoder().
    pub fn read_chunk(&self, chunk_index: u32) -> Result<(RasterChunk, DecodingResult)> {
        let mut decoder = self.open_decoder()?;
        decoder.read_chunk(chunk_index)
    }

    /// Extract affine geotransform from tags
    fn extract_geotransform<R: std::io::Read + Seek>(decoder: &mut Decoder<R>) -> Result<GeoTransform> {
        let matrix_res = decoder
            .get_tag_f64_vec(Tag::ModelTransformationTag)
            .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(34264)));
        if let Ok(matrix) = matrix_res {
            if let Some(gt) = GeoTransform::from_model_transformation(&matrix) {
                return Ok(gt);
            }
        }

        let tiepoint_res = decoder
            .get_tag_f64_vec(Tag::ModelTiepointTag)
            .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(33922)));
        let scale_res = decoder
            .get_tag_f64_vec(Tag::ModelPixelScaleTag)
            .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(33550)));

        if let (Ok(tiepoint), Ok(scale)) = (tiepoint_res, scale_res) {
            if let Some(gt) = GeoTransform::from_tiepoint_and_scale(&tiepoint, &scale) {
                return Ok(gt);
            }
        }

        Ok(GeoTransform::default())
    }

    /// Extract NoData value from tag 42113 / GdalNodata
    fn extract_nodata<R: std::io::Read + Seek>(decoder: &mut Decoder<R>) -> Option<f64> {
        let s_res = decoder
            .get_tag_ascii_string(Tag::GdalNodata)
            .or_else(|_| decoder.get_tag_ascii_string(Tag::Unknown(42113)));
        if let Ok(s) = s_res {
            if let Ok(val) = s.trim().parse::<f64>() {
                return Some(val);
            }
        }
        None
    }

    /// Extract CRS: returns (Option<epsg>, Option<proj_string>)
    fn extract_crs<R: std::io::Read + Seek>(decoder: &mut Decoder<R>) -> (Option<u32>, Option<String>) {
        let keys_res = decoder
            .get_tag_u16_vec(Tag::GeoKeyDirectoryTag)
            .or_else(|_| decoder.get_tag_u16_vec(Tag::Unknown(34735)));

        let mut proj_epsg: Option<u32> = None;
        let mut geo_epsg: Option<u32> = None;
        let mut is_user_defined = false;
        let mut coord_trans: Option<u16> = None;
        let mut double_param_keys = std::collections::HashMap::new();

        if let Ok(keys) = keys_res {
            if keys.len() >= 4 {
                let num_keys = keys[3] as usize;
                for i in 0..num_keys {
                    let offset = 4 + i * 4;
                    if offset + 3 < keys.len() {
                        let key_id = keys[offset];
                        let tiff_tag_loc = keys[offset + 1];
                        let _count = keys[offset + 2];
                        let val_or_offset = keys[offset + 3];

                        if tiff_tag_loc == 0 {
                            if key_id == 3072 {
                                if val_or_offset == 32767 {
                                    is_user_defined = true;
                                } else if val_or_offset > 0 {
                                    proj_epsg = Some(val_or_offset as u32);
                                }
                            } else if key_id == 2048 {
                                if val_or_offset > 0 && val_or_offset != 32767 {
                                    geo_epsg = Some(val_or_offset as u32);
                                }
                            } else if key_id == 3075 {
                                coord_trans = Some(val_or_offset);
                            }
                        } else if tiff_tag_loc == 34736 {
                            // Points to GeoDoubleParamsTag index (0-indexed or 1-indexed)
                            double_param_keys.insert(key_id, val_or_offset as usize);
                        }
                    }
                }
            }
        }

        if let Some(epsg) = proj_epsg {
            return (Some(epsg), None);
        }

        // If User-Defined or no standard projected EPSG, parse WKT from GeoAsciiParamsTag (34737)
        let ascii_res = decoder
            .get_tag_ascii_string(Tag::GeoAsciiParamsTag)
            .or_else(|_| decoder.get_tag_ascii_string(Tag::Unknown(34737)));

        if let Ok(ascii_str) = ascii_res {
            if let Some(proj_str) = parse_wkt_or_ascii_to_proj(&ascii_str) {
                return (None, Some(proj_str));
            }
        }

        // Fallback to GeoDoubleParamsTag (34736) with GeoKey parameters
        if let Some(trans_id) = coord_trans {
            let doubles_res = decoder
                .get_tag_f64_vec(Tag::GeoDoubleParamsTag)
                .or_else(|_| decoder.get_tag_f64_vec(Tag::Unknown(34736)));

            if let Ok(doubles) = doubles_res {
                let get_double = |key: u16| -> Option<f64> {
                    double_param_keys.get(&key).and_then(|&idx| doubles.get(idx).copied())
                };

                let datum_str = if geo_epsg == Some(4269) {
                    "+datum=NAD83"
                } else {
                    "+datum=WGS84"
                };

                if trans_id == 11 {
                    // CT_AlbersEqualArea
                    let lat_1 = get_double(3078).unwrap_or(0.0);
                    let lat_2 = get_double(3079).unwrap_or(0.0);
                    let lon_0 = get_double(3080).or_else(|| get_double(3084)).unwrap_or(0.0);
                    let lat_0 = get_double(3081).or_else(|| get_double(3085)).unwrap_or(0.0);
                    let x_0 = get_double(3082).unwrap_or(0.0);
                    let y_0 = get_double(3083).unwrap_or(0.0);

                    let p_str = format!(
                        "+proj=aea +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
                        lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum_str
                    );
                    return (None, Some(p_str));
                } else if trans_id == 8 {
                    // CT_LambertConfConic_2SP
                    let lat_1 = get_double(3078).unwrap_or(0.0);
                    let lat_2 = get_double(3079).unwrap_or(0.0);
                    let lon_0 = get_double(3080).or_else(|| get_double(3084)).unwrap_or(0.0);
                    let lat_0 = get_double(3081).or_else(|| get_double(3085)).unwrap_or(0.0);
                    let x_0 = get_double(3082).unwrap_or(0.0);
                    let y_0 = get_double(3083).unwrap_or(0.0);

                    let p_str = format!(
                        "+proj=lcc +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
                        lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum_str
                    );
                    return (None, Some(p_str));
                } else if trans_id == 1 {
                    // CT_TransverseMercator
                    let scale = get_double(3076).unwrap_or(0.9996);
                    let lon_0 = get_double(3080).or_else(|| get_double(3084)).unwrap_or(0.0);
                    let lat_0 = get_double(3081).or_else(|| get_double(3085)).unwrap_or(0.0);
                    let x_0 = get_double(3082).unwrap_or(500000.0);
                    let y_0 = get_double(3083).unwrap_or(0.0);

                    let p_str = format!(
                        "+proj=tmerc +lat_0={} +lon_0={} +k={} +x_0={} +y_0={} {} +units=m +no_defs",
                        lat_0, lon_0, scale, x_0, y_0, datum_str
                    );
                    return (None, Some(p_str));
                }
            }
        }

        if is_user_defined {
            (None, None)
        } else {
            (geo_epsg, None)
        }
    }
}

/// Parse WKT string or ESRI PE string found in GeoAsciiParamsTag (34737) into a PROJ string
fn parse_wkt_or_ascii_to_proj(s: &str) -> Option<String> {
    let upper = s.to_uppercase();

    // Determine datum
    let datum = if upper.contains("D_NORTH_AMERICAN_1983") || upper.contains("NAD83") || upper.contains("GRS_1980") {
        "+datum=NAD83"
    } else if upper.contains("D_NORTH_AMERICAN_1927") || upper.contains("NAD27") || upper.contains("CLARKE_1866") {
        "+datum=NAD27"
    } else {
        "+datum=WGS84"
    };

    let extract_param = |name: &str| -> Option<f64> {
        let pattern = format!("PARAMETER[\"{}\",", name.to_uppercase());
        if let Some(pos) = upper.find(&pattern) {
            let start = pos + pattern.len();
            let sub = &s[start..];
            if let Some(end) = sub.find(']') {
                return sub[..end].trim().parse::<f64>().ok();
            }
        }
        None
    };

    if upper.contains("PROJECTION[\"ALBERS\"]") || upper.contains("ALBERS_EQUAL_AREA_CONIC") {
        let lat_1 = extract_param("STANDARD_PARALLEL_1").unwrap_or(0.0);
        let lat_2 = extract_param("STANDARD_PARALLEL_2").unwrap_or(0.0);
        let lat_0 = extract_param("LATITUDE_OF_ORIGIN").or_else(|| extract_param("LATITUDE_OF_CENTER")).unwrap_or(0.0);
        let lon_0 = extract_param("CENTRAL_MERIDIAN").or_else(|| extract_param("LONGITUDE_OF_CENTER")).unwrap_or(0.0);
        let x_0 = extract_param("FALSE_EASTING").unwrap_or(0.0);
        let y_0 = extract_param("FALSE_NORTHING").unwrap_or(0.0);

        return Some(format!(
            "+proj=aea +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
            lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum
        ));
    }

    if upper.contains("PROJECTION[\"LAMBERT_CONFORMAL_CONIC\"]") || upper.contains("LAMBERT_CONFORMAL_CONIC") {
        let lat_1 = extract_param("STANDARD_PARALLEL_1").unwrap_or(0.0);
        let lat_2 = extract_param("STANDARD_PARALLEL_2").unwrap_or(0.0);
        let lat_0 = extract_param("LATITUDE_OF_ORIGIN").or_else(|| extract_param("LATITUDE_OF_CENTER")).unwrap_or(0.0);
        let lon_0 = extract_param("CENTRAL_MERIDIAN").or_else(|| extract_param("LONGITUDE_OF_CENTER")).unwrap_or(0.0);
        let x_0 = extract_param("FALSE_EASTING").unwrap_or(0.0);
        let y_0 = extract_param("FALSE_NORTHING").unwrap_or(0.0);

        return Some(format!(
            "+proj=lcc +lat_1={} +lat_2={} +lat_0={} +lon_0={} +x_0={} +y_0={} {} +units=m +no_defs",
            lat_1, lat_2, lat_0, lon_0, x_0, y_0, datum
        ));
    }

    if upper.contains("PROJECTION[\"TRANSVERSE_MERCATOR\"]") || upper.contains("TRANSVERSE_MERCATOR") {
        let scale = extract_param("SCALE_FACTOR").unwrap_or(0.9996);
        let lat_0 = extract_param("LATITUDE_OF_ORIGIN").unwrap_or(0.0);
        let lon_0 = extract_param("CENTRAL_MERIDIAN").unwrap_or(0.0);
        let x_0 = extract_param("FALSE_EASTING").unwrap_or(500000.0);
        let y_0 = extract_param("FALSE_NORTHING").unwrap_or(0.0);

        return Some(format!(
            "+proj=tmerc +lat_0={} +lon_0={} +k={} +x_0={} +y_0={} {} +units=m +no_defs",
            lat_0, lon_0, scale, x_0, y_0, datum
        ));
    }

    None
}
