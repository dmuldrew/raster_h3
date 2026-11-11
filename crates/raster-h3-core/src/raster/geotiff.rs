//! Streaming GeoTIFF reader with on-demand chunk decoding and buffer recycling.
//!
//! This module provides a streaming GeoTIFF reader that decodes chunks (strips/tiles) on-demand
//! with buffer recycling, supporting DEFLATE, LZW, and uncompressed formats.
//! It supports both local files (via memory mapping) and remote cloud storage (via HTTP range requests).

use memmap2::Mmap;
use std::fs::File;
use std::io::{Cursor, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::{CompressionMethod, PhotometricInterpretation, Predictor, SampleFormat, Tag};

use crate::error::{RasterH3Error, Result};
use crate::raster::geotransform::GeoTransform;
use crate::raster::http_range::{
    is_remote_url, HttpRangeReader, RemoteHttpSource, RemoteSourceFactory,
};
use crate::raster::metadata::{
    extract_alpha_sample, extract_band_scaling, extract_crs, extract_geotransform, extract_nodata,
    BandScaling,
};
use crate::raster::predictor::{unpack_f32, unpack_f64, unpack_integer_samples, ChunkEncoding};
use crate::raster::RasterChunk;

/// TIFF byte order (Intel Little-Endian vs Motorola Big-Endian)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiffByteOrder {
    /// Little-endian byte order (`"II"`, Intel format), least significant byte first.
    LittleEndian,
    /// Big-endian byte order (`"MM"`, Motorola format), most significant byte first.
    BigEndian,
}

/// Parsed GeoTIFF metadata
#[derive(Debug, Clone)]
pub struct GeoTiffMetadata {
    /// Raster width in pixels.
    pub width: u32,
    /// Raster height in pixels.
    pub height: u32,
    /// Affine pixel-to-coordinate transform.
    pub geotransform: GeoTransform,
    /// NoData sentinel value if present in raster metadata.
    pub nodata: Option<f64>,
    /// Detected EPSG spatial reference code, if present.
    pub epsg: Option<u32>,
    /// Detected PROJ4 projection definition string, if present.
    pub proj_string: Option<String>,
    /// Number of bands or samples per pixel.
    pub samples_per_pixel: u16,
    /// Per-band GDAL `SCALE`/`OFFSET` calibration, indexed by zero-based
    /// band; empty when no band is calibrated.
    pub band_scaling: Vec<BandScaling>,
    /// Zero-based alpha sample (TIFF `ExtraSamples`); a pixel whose alpha
    /// is zero is transparent and has no observation.
    pub alpha_sample: Option<u16>,
}

impl GeoTiffMetadata {
    /// Calibration for the 1-based `band`, or identity when uncalibrated.
    #[inline]
    pub fn band_scaling(&self, band: usize) -> BandScaling {
        self.band_scaling
            .get(band.saturating_sub(1))
            .copied()
            .unwrap_or(BandScaling::IDENTITY)
    }
}

/// Chunk layout information (tiled vs striped)
#[derive(Debug, Clone, Copy)]
pub struct ChunkLayout {
    /// Dimension (width) of each strip or tile in pixels.
    pub chunk_width: u32,
    /// Dimension (height) of each strip or tile in pixels.
    pub chunk_height: u32,
    /// Grid layout column count (number of chunks across).
    pub chunks_across: u32,
    /// Grid layout row count (number of chunks down).
    pub chunks_down: u32,
    /// Total number of chunks in the raster grid.
    pub total_chunks: u32,
}

impl ChunkLayout {
    /// Create layout for tiled TIFFs
    ///
    /// Panics on zero tile dimensions or a chunk grid exceeding `u32::MAX`
    /// chunks; untrusted TIFF input is validated before reaching this API.
    pub fn new_tiled(
        image_width: u32,
        image_height: u32,
        tile_width: u32,
        tile_height: u32,
    ) -> Self {
        assert!(tile_width > 0 && tile_height > 0);
        let chunks_across = image_width.div_ceil(tile_width);
        let chunks_down = image_height.div_ceil(tile_height);
        Self {
            chunk_width: tile_width,
            chunk_height: tile_height,
            chunks_across,
            chunks_down,
            total_chunks: chunks_across
                .checked_mul(chunks_down)
                .expect("chunk grid exceeds u32 chunk count"),
        }
    }

    /// Create layout for striped TIFFs
    ///
    /// Panics on `rows_per_strip == 0`; untrusted TIFF input is validated
    /// before reaching this API.
    pub fn new_striped(image_width: u32, image_height: u32, rows_per_strip: u32) -> Self {
        assert!(rows_per_strip > 0);
        let chunks_down = image_height.div_ceil(rows_per_strip);
        Self {
            chunk_width: image_width,
            chunk_height: rows_per_strip,
            chunks_across: 1,
            chunks_down,
            total_chunks: chunks_down,
        }
    }

    /// Calculate grid bounds for a given chunk index
    pub fn get_chunk_bounds(
        &self,
        chunk_index: u32,
        image_width: u32,
        image_height: u32,
    ) -> RasterChunk {
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
    /// Local file backing accessed via memory mapping.
    Local(Arc<Mmap>),
    /// Remote file backing accessed via HTTP range requests.
    Remote(Arc<RemoteHttpSource>),
}

/// Chunk byte payload: zero-copy borrowed slice (local mmap) or owned byte vector (remote HTTP / cache)
#[derive(Debug, Clone)]
pub enum ChunkPayload<'a> {
    /// Zero-copy byte slice borrowed directly from memory-mapped storage (strips or tiles).
    Borrowed(&'a [u8]),
    /// Shared reference-counted byte buffer from cache or asynchronous prefetch queue.
    ArcOwned(Arc<Vec<u8>>),
    /// Owned byte buffer allocated for remote HTTP responses or decompressed payloads.
    Owned(Vec<u8>),
}

impl<'a> AsRef<[u8]> for ChunkPayload<'a> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Borrowed(s) => s,
            Self::ArcOwned(a) => a.as_slice(),
            Self::Owned(v) => v.as_slice(),
        }
    }
}

/// Unified chunk byte provider for ChunkDecoder: local slice or remote source with lifetime 'a
#[derive(Clone)]
pub enum DecoderSource<'a> {
    Local(&'a [u8]),
    Remote(Arc<RemoteHttpSource>),
}

impl<'a> DecoderSource<'a> {
    /// Read chunk byte payload with lifetime `'a`
    pub fn get_chunk_payload(
        &self,
        chunk_index: u32,
        info: &TiffChunkInfo,
    ) -> Result<ChunkPayload<'a>> {
        let idx = chunk_index as usize;
        let offset = *info
            .chunk_offsets
            .get(idx)
            .ok_or_else(|| RasterH3Error::InvalidMetadata("invalid chunk offset".into()))?;
        let len = *info
            .chunk_bytes
            .get(idx)
            .ok_or_else(|| RasterH3Error::InvalidMetadata("invalid chunk bytes".into()))?
            as usize;

        match self {
            DecoderSource::Local(mmap) => {
                let off = offset as usize;
                if off.checked_add(len).is_none_or(|end| end > mmap.len()) {
                    return Err(RasterH3Error::InvalidMetadata(
                        "chunk offset out of bounds".into(),
                    ));
                }
                Ok(ChunkPayload::Borrowed(&mmap[off..off + len]))
            }
            DecoderSource::Remote(remote) => {
                let bytes = remote.read_exact_range(offset, len)?;
                Ok(ChunkPayload::Owned(bytes))
            }
        }
    }
}

impl RasterSource {
    /// Read chunk byte payload for a specific chunk index using chunk offsets and byte counts
    pub fn get_chunk_payload(
        &self,
        chunk_index: u32,
        info: &TiffChunkInfo,
    ) -> Result<ChunkPayload<'_>> {
        let ds = match self {
            RasterSource::Local(mmap) => DecoderSource::Local(&mmap[..]),
            RasterSource::Remote(remote) => DecoderSource::Remote(Arc::clone(remote)),
        };
        ds.get_chunk_payload(chunk_index, info)
    }
}

/// TIFF chunk layout and compression metadata for SIMD-accelerated direct decoding
#[derive(Debug, Clone)]
pub struct TiffChunkInfo {
    /// Compression scheme used for chunk payloads (e.g., None, LZW, Deflate).
    pub compression: CompressionMethod,
    /// Differencing predictor applied prior to compression (e.g., None, Horizontal, FloatingPoint).
    pub predictor: Predictor,
    /// Organization of raster chunks within the TIFF file (strip vs tile).
    pub chunk_type: ChunkType,
    /// Byte offsets of each chunk within the TIFF file or stream.
    pub chunk_offsets: Arc<[u64]>,
    /// Compressed byte sizes for each chunk.
    pub chunk_bytes: Arc<[u64]>,
    /// Nominal (width, height) pixel dimensions of chunks.
    pub chunk_dimensions: (u32, u32),
    /// Number of bits per sample per band (e.g., 8, 16, 32, 64).
    pub bits_per_sample: u8,
    /// Data interpretation of samples (e.g., unsigned integer, signed integer, IEEE float).
    pub sample_format: SampleFormat,
    /// Endianness of numerical data within the TIFF file.
    pub byte_order: TiffByteOrder,
    /// Color space interpretation of sample values (e.g., BlackIsZero, RGB).
    pub photometric: PhotometricInterpretation,
}

/// Zero-copy memory-mapped or remote streaming GeoTIFF reader that decodes chunks on-demand
#[derive(Clone)]
pub struct GeoTiffStreamReader {
    /// Path or URI identifying the source GeoTIFF file.
    pub file_path: PathBuf,
    /// Backing data source providing raw byte access (local memory map or remote HTTP stream).
    pub source: RasterSource,
    /// Parsed GeoTIFF metadata including spatial reference, extent, and NoData value.
    pub metadata: GeoTiffMetadata,
    /// Grid layout partitioning the raster into discrete strips or tiles.
    pub chunk_layout: ChunkLayout,
    /// Pre-parsed chunk compression parameters and byte offsets for direct decoding.
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
        let data = match self {
            InnerDecoder::Local(d) => d.read_chunk(chunk)?,
            InnerDecoder::Remote(d) => d.read_chunk(chunk)?,
        };
        Ok(widen_f16(data))
    }

    /// Decode `chunk` into `buffer`; tiff reshapes it to the chunk's sample type and length.
    #[inline]
    fn read_chunk_to_buffer(
        &mut self,
        buffer: &mut DecodingResult,
        chunk: u32,
        width: usize,
    ) -> tiff::TiffResult<()> {
        match self {
            InnerDecoder::Local(d) => d.read_chunk_to_buffer(buffer, chunk, width)?,
            InnerDecoder::Remote(d) => d.read_chunk_to_buffer(buffer, chunk, width)?,
        }
        if matches!(buffer, DecodingResult::F16(_)) {
            *buffer = widen_f16(std::mem::replace(buffer, DecodingResult::U8(Vec::new())));
        }
        Ok(())
    }
}

/// Widen tiff's half-precision results to `F32`, so the rest of the pipeline only sees the
/// sample types it has native kernels for.
fn widen_f16(data: DecodingResult) -> DecodingResult {
    match data {
        DecodingResult::F16(v) => DecodingResult::F32(v.into_iter().map(f32::from).collect()),
        other => other,
    }
}

/// Why the accelerated decode path produced no samples.
#[derive(Debug)]
enum FastDecodeError {
    /// The accelerated kernels do not cover this layout; the generic TIFF
    /// decoder may still read it.
    Unsupported(RasterH3Error),
    /// The payload does not decode to the chunk's declared layout.
    Failed(RasterH3Error),
}

impl FastDecodeError {
    fn into_inner(self) -> RasterH3Error {
        match self {
            Self::Unsupported(e) | Self::Failed(e) => e,
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
    pub nodata: Option<f64>,
    chunk_info: Option<Arc<TiffChunkInfo>>,
    source: DecoderSource<'a>,
    libdeflater: Option<libdeflater::Decompressor>,
    lzw_decoder: Option<weezl::decode::Decoder>,
    decomp_scratch: Vec<u8>,
}

impl<'a> ChunkDecoder<'a> {
    /// Read chunk byte payload for a specific chunk index using decoder's backing source
    pub fn read_chunk_payload(&self, chunk_index: u32) -> Result<ChunkPayload<'a>> {
        let info = self
            .chunk_info
            .as_ref()
            .ok_or_else(|| RasterH3Error::InvalidMetadata("no chunk info".into()))?;
        self.source.get_chunk_payload(chunk_index, info)
    }

    /// Read and decode a single chunk using persistent fast decoder or fallback
    #[inline]
    pub fn read_chunk(&mut self, chunk_index: u32) -> Result<(RasterChunk, DecodingResult)> {
        self.read_chunk_with_payload(chunk_index, None, None)
    }

    /// Read and decode a single chunk into an existing buffer if possible, avoiding reallocations
    #[inline]
    pub fn read_chunk_into(
        &mut self,
        chunk_index: u32,
        buffer: DecodingResult,
    ) -> Result<(RasterChunk, DecodingResult)> {
        self.read_chunk_with_payload(chunk_index, None, Some(buffer))
    }

    /// Read and decode a chunk directly from an in-memory compressed byte payload
    /// (e.g. delivered by the asynchronous remote prefetch queue).
    #[inline]
    pub fn read_chunk_from_compressed_bytes(
        &mut self,
        chunk_index: u32,
        compressed_bytes: &[u8],
        target_buffer: Option<DecodingResult>,
    ) -> Result<(RasterChunk, DecodingResult)> {
        self.read_chunk_with_payload(chunk_index, Some(compressed_bytes), target_buffer)
    }

    /// Unified chunk decoding entry point:
    /// - If `provided_bytes` is Some (e.g. from async prefetch queue), decodes from it directly.
    /// - If `provided_bytes` is None, retrieves chunk payload via `self.source.get_chunk_payload`
    ///   (zero-copy slice for local memory maps, range request for remote COGs).
    /// - Uses SIMD Deflate or accelerated LZW first, populating `target_buffer` in-place if available.
    /// - Falls back to the standard TIFF decoder when the accelerated kernels do not cover the
    ///   chunk's layout, or when `provided_bytes` fails to decode (the chunk is then re-read
    ///   from the source).
    /// - A failed read of the source, or source bytes that do not decode to the chunk's declared
    ///   layout, is returned as an error.
    pub fn read_chunk_with_payload(
        &mut self,
        chunk_index: u32,
        provided_bytes: Option<&[u8]>,
        mut target_buffer: Option<DecodingResult>,
    ) -> Result<(RasterChunk, DecodingResult)> {
        let chunk_bounds = self
            .chunk_layout
            .get_chunk_bounds(chunk_index, self.width, self.height);

        // Check for sparse / unallocated chunk (e.g. in Cloud-Optimized GeoTIFFs where ocean/empty tiles have offset 0 or bytes 0)
        let is_sparse = if let Some(bytes) = provided_bytes {
            bytes.is_empty()
        } else if let Some(ref info) = self.chunk_info {
            let idx = chunk_index as usize;
            // An index past the offset/byte-count arrays means the arrays are
            // shorter than the declared chunk grid — malformed metadata, not a
            // legitimately absent chunk (which is recorded as offset/bytes 0).
            match (info.chunk_offsets.get(idx), info.chunk_bytes.get(idx)) {
                (Some(&offset), Some(&byte_count)) => offset == 0 || byte_count == 0,
                _ => {
                    return Err(RasterH3Error::InvalidMetadata(format!(
                        "chunk {chunk_index} is outside the {} entries in the TIFF offset/byte-count arrays",
                        info.chunk_offsets.len().min(info.chunk_bytes.len())
                    )));
                }
            }
        } else {
            false
        };

        if is_sparse {
            let spp = self.samples_per_pixel.max(1) as usize;
            let total_samples =
                (chunk_bounds.width as usize) * (chunk_bounds.height as usize) * spp;
            if let Some(mut buf) = target_buffer {
                match buf {
                    DecodingResult::U8(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as u8).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::U16(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as u16).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::U32(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as u32).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::U64(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as u64).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::I8(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as i8).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::I16(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as i16).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::I32(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as i32).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::I64(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as i64).unwrap_or(0);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::F32(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.map(|n| n as f32).unwrap_or(f32::NAN);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::F64(ref mut v) => {
                        v.clear();
                        let fill = self.nodata.unwrap_or(f64::NAN);
                        v.resize(total_samples, fill);
                    }
                    DecodingResult::F16(_) => {
                        let fill = self.nodata.map(|n| n as f32).unwrap_or(f32::NAN);
                        buf = DecodingResult::F32(vec![fill; total_samples]);
                    }
                }
                return Ok((chunk_bounds, buf));
            } else if let Some(ref info) = self.chunk_info {
                let res = match (info.sample_format, info.bits_per_sample) {
                    (SampleFormat::Uint, 8) => {
                        let fill = self.nodata.map(|n| n as u8).unwrap_or(0);
                        DecodingResult::U8(vec![fill; total_samples])
                    }
                    (SampleFormat::Uint, 16) => {
                        let fill = self.nodata.map(|n| n as u16).unwrap_or(0);
                        DecodingResult::U16(vec![fill; total_samples])
                    }
                    (SampleFormat::Uint, 32) => {
                        let fill = self.nodata.map(|n| n as u32).unwrap_or(0);
                        DecodingResult::U32(vec![fill; total_samples])
                    }
                    (SampleFormat::Uint, 64) => {
                        let fill = self.nodata.map(|n| n as u64).unwrap_or(0);
                        DecodingResult::U64(vec![fill; total_samples])
                    }
                    (SampleFormat::Int, 8) => {
                        let fill = self.nodata.map(|n| n as i8).unwrap_or(0);
                        DecodingResult::I8(vec![fill; total_samples])
                    }
                    (SampleFormat::Int, 16) => {
                        let fill = self.nodata.map(|n| n as i16).unwrap_or(0);
                        DecodingResult::I16(vec![fill; total_samples])
                    }
                    (SampleFormat::Int, 32) => {
                        let fill = self.nodata.map(|n| n as i32).unwrap_or(0);
                        DecodingResult::I32(vec![fill; total_samples])
                    }
                    (SampleFormat::Int, 64) => {
                        let fill = self.nodata.map(|n| n as i64).unwrap_or(0);
                        DecodingResult::I64(vec![fill; total_samples])
                    }
                    (SampleFormat::IEEEFP, 32) => {
                        let fill = self.nodata.map(|n| n as f32).unwrap_or(f32::NAN);
                        DecodingResult::F32(vec![fill; total_samples])
                    }
                    (SampleFormat::IEEEFP, 64) => {
                        let fill = self.nodata.unwrap_or(f64::NAN);
                        DecodingResult::F64(vec![fill; total_samples])
                    }
                    _ => {
                        let fill = self.nodata.map(|n| n as f32).unwrap_or(f32::NAN);
                        DecodingResult::F32(vec![fill; total_samples])
                    }
                };
                return Ok((chunk_bounds, res));
            } else {
                let fill = self.nodata.map(|n| n as f32).unwrap_or(f32::NAN);
                return Ok((chunk_bounds, DecodingResult::F32(vec![fill; total_samples])));
            }
        }

        // Only a payload the caller handed in may be replaced by a re-read: it is
        // a copy (e.g. a prefetched range) and the source still holds the chunk.
        let (fast_res, reread_on_failure) = if let Some(bytes) = provided_bytes {
            (
                self.fast_decode(chunk_index, bytes, target_buffer.as_mut()),
                true,
            )
        } else if let Some(ref info) = self.chunk_info {
            // A failed read is final. The generic decoder would only request the
            // same bytes again through a second code path.
            let payload = self.source.get_chunk_payload(chunk_index, info)?;
            (
                self.fast_decode(chunk_index, payload.as_ref(), target_buffer.as_mut()),
                false,
            )
        } else {
            (
                Err(FastDecodeError::Unsupported(
                    RasterH3Error::InvalidMetadata("no chunk info".into()),
                )),
                false,
            )
        };

        match fast_res {
            Ok(None) => return Ok((chunk_bounds, target_buffer.unwrap())),
            Ok(Some(new_buf)) => return Ok((chunk_bounds, new_buf)),
            // No accelerated kernel for this layout: the generic decoder handles it.
            Err(FastDecodeError::Unsupported(_)) => {}
            Err(FastDecodeError::Failed(_)) if reread_on_failure => {}
            // The source's own bytes do not decode to the declared layout. Report
            // that instead of masking it behind a second decode attempt.
            Err(FastDecodeError::Failed(err)) => return Err(err),
        }

        if let Some(buf) = target_buffer {
            self.read_chunk_fallback_into(chunk_index, chunk_bounds, buf)
        } else {
            let data = self.inner.read_chunk(chunk_index)?;
            Ok((chunk_bounds, data))
        }
    }

    fn read_chunk_fallback_into(
        &mut self,
        chunk_index: u32,
        chunk_bounds: RasterChunk,
        mut buffer: DecodingResult,
    ) -> Result<(RasterChunk, DecodingResult)> {
        let data_dims = self.inner.chunk_data_dimensions(chunk_index);
        if self
            .inner
            .read_chunk_to_buffer(&mut buffer, chunk_index, data_dims.0 as usize)
            .is_ok()
        {
            Ok((chunk_bounds, buffer))
        } else {
            let data = self.inner.read_chunk(chunk_index)?;
            Ok((chunk_bounds, data))
        }
    }

    #[inline]
    fn decode_chunk_bytes_into(
        libdeflater: Option<&mut libdeflater::Decompressor>,
        lzw_decoder: Option<&mut weezl::decode::Decoder>,
        compression: CompressionMethod,
        compressed: &[u8],
        out: &mut [u8],
    ) -> std::result::Result<(), FastDecodeError> {
        let unsupported =
            |why: &str| FastDecodeError::Unsupported(RasterH3Error::InvalidMetadata(why.into()));
        let failed = |why: String| FastDecodeError::Failed(RasterH3Error::InvalidMetadata(why));
        match compression {
            CompressionMethod::None => {
                // Uncompressed: the payload is the samples themselves. A strip or
                // tile shorter than its declared layout is malformed.
                let raw = compressed.get(..out.len()).ok_or_else(|| {
                    failed("uncompressed chunk shorter than its declared layout".into())
                })?;
                out.copy_from_slice(raw);
            }
            CompressionMethod::Deflate | CompressionMethod::OldDeflate => {
                let decompressor = libdeflater.ok_or_else(|| unsupported("no decompressor"))?;
                // zlib_decompress/deflate_decompress return the number of bytes
                // written; a chunk that decompresses short leaves stale bytes in
                // the buffer tail, so the output must exactly fill `out`.
                let decompressed = match decompressor.zlib_decompress(compressed, out) {
                    Ok(n) => n,
                    Err(zlib_err) => match decompressor.deflate_decompress(compressed, out) {
                        Ok(n) => n,
                        // The stream inflates past the declared layout (e.g. a padded
                        // final strip). libdeflate cannot stop early; the generic
                        // decoder reads just the bytes it needs.
                        Err(raw_err)
                            if zlib_err == libdeflater::DecompressionError::InsufficientSpace
                                || raw_err
                                    == libdeflater::DecompressionError::InsufficientSpace =>
                        {
                            return Err(unsupported(
                                "deflate chunk inflates past its declared layout",
                            ));
                        }
                        Err(_) => return Err(failed("decompress failed".into())),
                    },
                };
                if decompressed != out.len() {
                    return Err(failed(
                        "deflate chunk did not fill its declared layout".into(),
                    ));
                }
            }
            CompressionMethod::LZW => {
                let lzw = lzw_decoder.ok_or_else(|| unsupported("no lzw decompressor"))?;
                lzw.reset();
                let mut inp = compressed;
                let mut target_out = out;
                while !inp.is_empty() && !target_out.is_empty() {
                    let res = lzw.decode_bytes(inp, target_out);
                    inp = &inp[res.consumed_in..];
                    target_out = &mut target_out[res.consumed_out..];
                    match res.status {
                        Ok(weezl::LzwStatus::Done) | Ok(weezl::LzwStatus::NoProgress) => break,
                        Ok(weezl::LzwStatus::Ok) => {
                            if res.consumed_in == 0 && res.consumed_out == 0 {
                                break;
                            }
                        }
                        Err(e) => {
                            return Err(failed(format!("lzw error: {:?}", e)));
                        }
                    }
                }
                if !target_out.is_empty() {
                    return Err(failed("lzw chunk did not fill buffer".into()));
                }
            }
            _ => return Err(unsupported("unsupported fast compression")),
        }
        Ok(())
    }

    /// Attempt accelerated chunk decompression with libdeflater (Deflate) or weezl (LZW).
    /// If target_buffer is provided and of matching type, it will be populated in-place.
    /// Returns Ok(None) if target_buffer was populated in-place.
    /// Returns Ok(Some(new_result)) if a new DecodingResult was created.
    /// Returns Err(_) if the layout is unsupported or the payload fails to decode.
    fn decompress_chunk_fast(
        &mut self,
        chunk_index: u32,
        target_buffer: Option<&mut DecodingResult>,
    ) -> Result<Option<DecodingResult>> {
        let info = self
            .chunk_info
            .as_ref()
            .ok_or_else(|| RasterH3Error::InvalidMetadata("no chunk info".into()))?;

        let payload = self.source.get_chunk_payload(chunk_index, info)?;
        self.decompress_chunk_fast_bytes(chunk_index, payload.as_ref(), target_buffer)
    }

    /// Attempt accelerated chunk decompression with libdeflater (Deflate) or weezl (LZW)
    /// directly from a compressed byte slice (works for both local mmap slices and remote prefetched payloads).
    pub fn decompress_chunk_fast_bytes(
        &mut self,
        chunk_index: u32,
        compressed_slice: &[u8],
        target_buffer: Option<&mut DecodingResult>,
    ) -> Result<Option<DecodingResult>> {
        self.fast_decode(chunk_index, compressed_slice, target_buffer)
            .map_err(FastDecodeError::into_inner)
    }

    /// [`Self::decompress_chunk_fast_bytes`], telling a layout the accelerated
    /// kernels do not cover apart from a payload that fails to decode.
    fn fast_decode(
        &mut self,
        chunk_index: u32,
        compressed_slice: &[u8],
        target_buffer: Option<&mut DecodingResult>,
    ) -> std::result::Result<Option<DecodingResult>, FastDecodeError> {
        let unsupported =
            |why: &str| FastDecodeError::Unsupported(RasterH3Error::InvalidMetadata(why.into()));
        let info = self
            .chunk_info
            .as_ref()
            .ok_or_else(|| unsupported("no chunk info"))?;

        let (chunk_w, chunk_h) = info.chunk_dimensions;
        let bounds = self
            .chunk_layout
            .get_chunk_bounds(chunk_index, self.width, self.height);
        let data_w = bounds.width as usize;
        let data_h = bounds.height as usize;
        let spp = (self.samples_per_pixel.max(1)) as usize;
        let byte_len = (info.bits_per_sample as usize / 8).max(1);

        let raw_chunk_bytes = match info.chunk_type {
            ChunkType::Tile => (chunk_w as usize) * (chunk_h as usize) * spp * byte_len,
            ChunkType::Strip => (chunk_w as usize) * data_h * spp * byte_len,
        };

        let total_samples = data_w * data_h * spp;
        let tile_w = chunk_w as usize;

        #[cfg(target_endian = "little")]
        let is_native_endian = info.byte_order == TiffByteOrder::LittleEndian;
        #[cfg(not(target_endian = "little"))]
        let is_native_endian = info.byte_order == TiffByteOrder::BigEndian;

        // Decoding straight into the sample buffer needs the stored chunk to hold
        // exactly the samples wanted. A tile is always stored whole, so one
        // clipped by the right or bottom image edge goes through the scratch
        // buffer, which drops the padding.
        let stored_h = match info.chunk_type {
            ChunkType::Tile => chunk_h as usize,
            ChunkType::Strip => data_h,
        };
        let can_direct_decode = tile_w == data_w
            && stored_h == data_h
            && info.predictor == Predictor::None
            && is_native_endian
            && info.photometric != PhotometricInterpretation::WhiteIsZero;

        if can_direct_decode {
            macro_rules! direct_decode {
                ($variant:ident, $type:ty, $default:expr) => {{
                    let byte_len = total_samples * std::mem::size_of::<$type>();
                    match target_buffer {
                        Some(DecodingResult::$variant(ref mut v)) => {
                            v.resize(total_samples, $default);
                            // SAFETY: `v` holds exactly `total_samples` elements of `$type`
                            // (a primitive number), so its buffer spans `byte_len` bytes;
                            // `u8` needs no alignment, every byte pattern is a valid
                            // `$type`, and `v` is not otherwise used while `b` lives.
                            let b = unsafe {
                                std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, byte_len)
                            };
                            Self::decode_chunk_bytes_into(
                                self.libdeflater.as_mut(),
                                self.lzw_decoder.as_mut(),
                                info.compression,
                                compressed_slice,
                                b,
                            )?;
                            return Ok(None);
                        }
                        _ => {
                            let mut v = vec![$default; total_samples];
                            // SAFETY: `v` holds exactly `total_samples` elements of `$type`
                            // (a primitive number), so its buffer spans `byte_len` bytes;
                            // `u8` needs no alignment, every byte pattern is a valid
                            // `$type`, and `v` is not otherwise used while `b` lives.
                            let b = unsafe {
                                std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, byte_len)
                            };
                            Self::decode_chunk_bytes_into(
                                self.libdeflater.as_mut(),
                                self.lzw_decoder.as_mut(),
                                info.compression,
                                compressed_slice,
                                b,
                            )?;
                            return Ok(Some(DecodingResult::$variant(v)));
                        }
                    }
                }};
            }

            match (info.sample_format, info.bits_per_sample) {
                (SampleFormat::Int, 16) => direct_decode!(I16, i16, 0),
                (SampleFormat::Uint, 16) => direct_decode!(U16, u16, 0),
                (SampleFormat::IEEEFP, 32) => direct_decode!(F32, f32, 0.0),
                (SampleFormat::Uint, 8) => direct_decode!(U8, u8, 0),
                (SampleFormat::Uint, 32) => direct_decode!(U32, u32, 0),
                (SampleFormat::Int, 32) => direct_decode!(I32, i32, 0),
                _ => {}
            }
        }

        if self.decomp_scratch.len() < raw_chunk_bytes {
            self.decomp_scratch.resize(raw_chunk_bytes, 0);
        }

        let decomp_slice = &mut self.decomp_scratch[..raw_chunk_bytes];
        Self::decode_chunk_bytes_into(
            self.libdeflater.as_mut(),
            self.lzw_decoder.as_mut(),
            info.compression,
            compressed_slice,
            decomp_slice,
        )?;

        let encoding = ChunkEncoding {
            tile_w,
            data_w,
            data_h,
            spp,
            byte_order: info.byte_order,
            predictor: info.predictor,
            photometric: info.photometric,
        };
        // The float kernels reverse only the floating-point predictor; any other
        // unpack error means the decoded bytes do not match the declared layout.
        let float_predictor_supported =
            matches!(info.predictor, Predictor::None | Predictor::FloatingPoint);
        let float_err = |e: RasterH3Error| {
            if float_predictor_supported {
                FastDecodeError::Failed(e)
            } else {
                FastDecodeError::Unsupported(e)
            }
        };
        macro_rules! unpack_int_branch {
            ($variant:ident, $t:ty) => {
                match target_buffer {
                    Some(DecodingResult::$variant(ref mut v)) => {
                        v.resize(total_samples, 0);
                        unpack_integer_samples(decomp_slice, &mut v[..total_samples], encoding)
                            .map_err(FastDecodeError::Failed)?;
                        Ok(None)
                    }
                    _ => {
                        let mut v = vec![0 as $t; total_samples];
                        unpack_integer_samples(decomp_slice, &mut v, encoding)
                            .map_err(FastDecodeError::Failed)?;
                        Ok(Some(DecodingResult::$variant(v)))
                    }
                }
            };
        }

        match (info.sample_format, info.bits_per_sample) {
            (SampleFormat::Uint, 8) => unpack_int_branch!(U8, u8),
            (SampleFormat::Uint, 16) => unpack_int_branch!(U16, u16),
            (SampleFormat::Uint, 32) => unpack_int_branch!(U32, u32),
            (SampleFormat::Uint, 64) => unpack_int_branch!(U64, u64),
            (SampleFormat::Int, 8) => unpack_int_branch!(I8, i8),
            (SampleFormat::Int, 16) => unpack_int_branch!(I16, i16),
            (SampleFormat::Int, 32) => unpack_int_branch!(I32, i32),
            (SampleFormat::Int, 64) => unpack_int_branch!(I64, i64),
            (SampleFormat::IEEEFP, 32) => match target_buffer {
                Some(DecodingResult::F32(ref mut v)) => {
                    v.resize(total_samples, 0.0);
                    unpack_f32(decomp_slice, &mut v[..total_samples], encoding)
                        .map_err(float_err)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0.0f32; total_samples];
                    unpack_f32(decomp_slice, &mut v, encoding).map_err(float_err)?;
                    Ok(Some(DecodingResult::F32(v)))
                }
            },
            (SampleFormat::IEEEFP, 64) => match target_buffer {
                Some(DecodingResult::F64(ref mut v)) => {
                    v.resize(total_samples, 0.0);
                    unpack_f64(decomp_slice, &mut v[..total_samples], encoding)
                        .map_err(float_err)?;
                    Ok(None)
                }
                _ => {
                    let mut v = vec![0.0f64; total_samples];
                    unpack_f64(decomp_slice, &mut v, encoding).map_err(float_err)?;
                    Ok(Some(DecodingResult::F64(v)))
                }
            },
            _ => Err(unsupported("unsupported format")),
        }
    }

    /// Backward-compatible alias for decompress_chunk_fast
    #[inline]
    pub fn decompress_chunk_simd(
        &mut self,
        chunk_index: u32,
        target_buffer: Option<&mut DecodingResult>,
    ) -> Result<Option<DecodingResult>> {
        self.decompress_chunk_fast(chunk_index, target_buffer)
    }
}

impl GeoTiffStreamReader {
    /// Expected decoded size of one full chunk. Without parsed chunk info the
    /// sample width is unknown, so assume the widest (8 bytes).
    pub fn decoded_chunk_bytes(&self) -> usize {
        let bytes_per_sample = self
            .chunk_info
            .as_ref()
            .map_or(8, |info| (info.bits_per_sample as usize).div_ceil(8).max(1));
        (self.chunk_layout.chunk_width as usize)
            .saturating_mul(self.chunk_layout.chunk_height as usize)
            .saturating_mul(self.metadata.samples_per_pixel.max(1) as usize)
            .saturating_mul(bytes_per_sample)
    }

    /// Unallocated TIFF chunks contain no observations, regardless of nodata overrides.
    pub fn is_sparse_chunk(&self, chunk_index: u32) -> bool {
        self.chunk_info.as_ref().is_some_and(|info| {
            let index = chunk_index as usize;
            info.chunk_offsets.get(index) == Some(&0) || info.chunk_bytes.get(index) == Some(&0)
        })
    }

    /// Backward-compatible accessor for memory-mapped buffer if the source is local
    pub fn mmap(&self) -> Option<&Arc<Mmap>> {
        match &self.source {
            RasterSource::Local(mmap) => Some(mmap),
            RasterSource::Remote(_) => None,
        }
    }

    /// Check if this reader streams from a remote HTTP / HTTPS / S3 source
    pub fn is_remote(&self) -> bool {
        matches!(self.source, RasterSource::Remote(_))
    }

    /// Return reference to remote source if remote
    pub fn remote_source(&self) -> Option<&Arc<RemoteHttpSource>> {
        match &self.source {
            RasterSource::Remote(r) => Some(r),
            RasterSource::Local(_) => None,
        }
    }

    /// Read chunk byte payload for a specific chunk index (zero-copy borrowed slice for local mmap, fetched bytes for remote)
    pub fn get_chunk_payload(&self, chunk_index: u32) -> Result<ChunkPayload<'_>> {
        let info = self
            .chunk_info
            .as_ref()
            .ok_or_else(|| RasterH3Error::InvalidMetadata("no chunk info".into()))?;
        self.source.get_chunk_payload(chunk_index, info)
    }

    /// Extract common GeoTIFF metadata and chunk layout from an initialized Decoder
    fn parse_metadata_and_layout<R: std::io::Read + Seek>(
        decoder: &mut Decoder<R>,
    ) -> Result<(GeoTiffMetadata, ChunkLayout, Option<Arc<TiffChunkInfo>>)> {
        let (width, height) = decoder.dimensions()?;
        let geotransform = extract_geotransform(decoder)?;
        let nodata = extract_nodata(decoder);
        let (epsg, proj_string) = extract_crs(decoder);
        let samples_per_pixel = decoder
            .get_tag_u32(Tag::SamplesPerPixel)
            .or_else(|_| decoder.get_tag_u32(Tag::Unknown(277)))
            .map(|v| v as u16)
            .unwrap_or(1);
        // PlanarConfiguration=Planar stores each band in its own strips/tiles,
        // so the offset/bytecount arrays hold samples_per_pixel × more entries
        // than this layout's chunk indexing models. Reject it rather than
        // silently decode the wrong samples.
        let planar_config = decoder.get_tag_u32(Tag::PlanarConfiguration).unwrap_or(1);
        if planar_config != 1 && samples_per_pixel > 1 {
            return Err(RasterH3Error::InvalidMetadata(format!(
                "unsupported PlanarConfiguration {planar_config} for a {samples_per_pixel}-band raster: only chunky (interleaved) layouts are supported"
            )));
        }
        let band_scaling = extract_band_scaling(decoder, samples_per_pixel)?;
        let alpha_sample = extract_alpha_sample(decoder, samples_per_pixel);

        let (chunk_w, chunk_h) = decoder.chunk_dimensions();
        if chunk_w == 0 || chunk_h == 0 {
            return Err(RasterH3Error::InvalidMetadata(format!(
                "TIFF declares a zero-sized chunk ({chunk_w}x{chunk_h})"
            )));
        }
        let chunks_across = width.div_ceil(chunk_w);
        let chunks_down = height.div_ceil(chunk_h);
        let total_chunks = chunks_across.checked_mul(chunks_down).ok_or_else(|| {
            RasterH3Error::InvalidMetadata(format!(
                "TIFF chunk grid {chunks_across}x{chunks_down} exceeds u32 chunk count"
            ))
        })?;

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
            band_scaling,
            alpha_sample,
        };

        let compression = match decoder.get_tag_u32(Tag::Compression) {
            // Keep unrecognised codes as `Unknown` so they are never read as raw samples.
            Ok(v) => CompressionMethod::from_u16_exhaustive(v as u16),
            Err(_) => CompressionMethod::None,
        };

        let predictor = match decoder.get_tag_u32(Tag::Predictor) {
            // An unrecognised predictor means the decoder would silently
            // interpret delta-encoded samples as literal values.
            Ok(v) => Predictor::from_u16(v as u16).ok_or_else(|| {
                RasterH3Error::InvalidMetadata(format!("unrecognised Predictor tag value {v}"))
            })?,
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

        let sample_format = match decoder.get_tag_u32(Tag::SampleFormat).or_else(|_| {
            decoder
                .get_tag_u16_vec(Tag::SampleFormat)
                .map(|v| v.first().copied().unwrap_or(1) as u32)
        }) {
            Ok(v) => SampleFormat::from_u16(v as u16).ok_or_else(|| {
                RasterH3Error::InvalidMetadata(format!("unrecognised SampleFormat tag value {v}"))
            })?,
            Err(_) => SampleFormat::Uint,
        };

        let photometric = match decoder.get_tag_u32(Tag::PhotometricInterpretation) {
            // Coercing an unrecognised photometric to BlackIsZero inverts the
            // WhiteIsZero/BlackIsZero decision and reads colour-modelled data
            // (YCbCr, palette indices) as literal samples.
            Ok(v) => PhotometricInterpretation::from_u16(v as u16).ok_or_else(|| {
                RasterH3Error::InvalidMetadata(format!(
                    "unrecognised PhotometricInterpretation tag value {v}"
                ))
            })?,
            Err(_) => PhotometricInterpretation::BlackIsZero,
        };

        let byte_order = if format!("{:?}", decoder.byte_order()).contains("BigEndian") {
            TiffByteOrder::BigEndian
        } else {
            TiffByteOrder::LittleEndian
        };

        let (offsets, bytes) = match chunk_type {
            ChunkType::Tile => {
                let offs = decoder
                    .find_tag_unsigned_vec::<u64>(Tag::TileOffsets)
                    .ok()
                    .flatten();
                let bts = decoder
                    .find_tag_unsigned_vec::<u64>(Tag::TileByteCounts)
                    .ok()
                    .flatten();
                (offs, bts)
            }
            ChunkType::Strip => {
                let offs = decoder
                    .find_tag_unsigned_vec::<u64>(Tag::StripOffsets)
                    .ok()
                    .flatten();
                let bts = decoder
                    .find_tag_unsigned_vec::<u64>(Tag::StripByteCounts)
                    .ok()
                    .flatten();
                (offs, bts)
            }
        };

        let chunk_info = match (offsets, bytes) {
            (Some(offs), Some(bts)) if offs.len() == bts.len() => {
                // A short array would make every chunk past its end silently
                // read as sparse (nodata fill) instead of surfacing the
                // malformation.
                if offs.len() < total_chunks as usize {
                    return Err(RasterH3Error::InvalidMetadata(format!(
                        "TIFF offset/byte-count arrays hold {} entries but the image grid declares {total_chunks} chunks",
                        offs.len()
                    )));
                }
                Some(Arc::new(TiffChunkInfo {
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
                }))
            }
            _ => None,
        };

        Ok((metadata, chunk_layout, chunk_info))
    }

    /// Open and decode metadata for a local file (via mmap) or a remote URL (via HTTP Range)
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_remote_source_factory(path, None)
    }

    /// Open a raster source, using remote_source_factory to obtain the byte
    /// source when set (e.g. a host file system); otherwise remote URLs use
    /// the built-in HTTP range client and local paths use mmap.
    pub fn open_with_remote_source_factory<P: AsRef<Path>>(
        path: P,
        remote_source_factory: Option<&RemoteSourceFactory>,
    ) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let path_str = path_buf.to_string_lossy();

        if let Some(factory) = remote_source_factory {
            let remote = factory.open(&path_str)?;
            let reader = HttpRangeReader::new(Arc::clone(&remote));
            let mut decoder = Decoder::new(reader)?;
            let (metadata, chunk_layout, chunk_info) =
                Self::parse_metadata_and_layout(&mut decoder)?;

            return Ok(Self {
                file_path: path_buf,
                source: RasterSource::Remote(remote),
                metadata,
                chunk_layout,
                chunk_info,
            });
        }

        if is_remote_url(&path_str) {
            let remote = Arc::new(RemoteHttpSource::open(&path_str)?);
            let reader = HttpRangeReader::new(Arc::clone(&remote));
            let mut decoder = Decoder::new(reader)?;
            let (metadata, chunk_layout, chunk_info) =
                Self::parse_metadata_and_layout(&mut decoder)?;

            return Ok(Self {
                file_path: path_buf,
                source: RasterSource::Remote(remote),
                metadata,
                chunk_layout,
                chunk_info,
            });
        }

        let file = File::open(&path_buf)?;
        // SAFETY: the map is read-only, and GeoTIFFs are treated as immutable inputs.
        // If another process truncates or rewrites the file while it is mapped,
        // reads may fault or see torn data. That is the standard `mmap` caveat,
        // accepted here for zero-copy reads.
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

        let libdeflater = if self.chunk_info.as_ref().is_some_and(|info| {
            matches!(
                info.compression,
                CompressionMethod::Deflate | CompressionMethod::OldDeflate
            )
        }) {
            Some(libdeflater::Decompressor::new())
        } else {
            None
        };

        let lzw_decoder = if self
            .chunk_info
            .as_ref()
            .is_some_and(|info| matches!(info.compression, CompressionMethod::LZW))
        {
            Some(weezl::decode::Decoder::with_tiff_size_switch(
                weezl::BitOrder::Msb,
                8,
            ))
        } else {
            None
        };

        let source = match &self.source {
            RasterSource::Local(mmap) => DecoderSource::Local(&mmap[..]),
            RasterSource::Remote(remote) => DecoderSource::Remote(Arc::clone(remote)),
        };

        Ok(ChunkDecoder {
            inner,
            chunk_layout: self.chunk_layout,
            width: self.metadata.width,
            height: self.metadata.height,
            samples_per_pixel: self.metadata.samples_per_pixel,
            nodata: self.metadata.nodata,
            chunk_info: self.chunk_info.clone(),
            source,
            libdeflater,
            lzw_decoder,
            decomp_scratch: Vec::new(),
        })
    }

    /// Read and decode a single chunk on-demand directly from memory-mapped pages or remote stream.
    /// NOTE: This creates a fresh decoder per call. For sequential reads, prefer open_decoder().
    pub fn read_chunk(&self, chunk_index: u32) -> Result<(RasterChunk, DecodingResult)> {
        let mut decoder = self.open_decoder()?;
        decoder.read_chunk(chunk_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deflate_compress(src: &[u8]) -> Vec<u8> {
        let mut compressor = libdeflater::Compressor::new(libdeflater::CompressionLvl::default());
        let mut out = vec![0u8; compressor.deflate_compress_bound(src.len())];
        let n = compressor.deflate_compress(src, &mut out).unwrap();
        out.truncate(n);
        out
    }

    // Regression: a deflate chunk decompressing short used to be accepted,
    // leaving stale bytes from the previous chunk in the buffer tail.
    #[test]
    fn deflate_short_fill_is_rejected() {
        let compressed = deflate_compress(&[1, 2, 3, 4]);
        let mut decompressor = libdeflater::Decompressor::new();
        let mut out = [0xAAu8; 16];
        assert!(ChunkDecoder::decode_chunk_bytes_into(
            Some(&mut decompressor),
            None,
            CompressionMethod::Deflate,
            &compressed,
            &mut out,
        )
        .is_err());
    }

    #[test]
    fn deflate_exact_fill_is_accepted() {
        let payload = [9u8; 16];
        let compressed = deflate_compress(&payload);
        let mut decompressor = libdeflater::Decompressor::new();
        let mut out = [0u8; 16];
        ChunkDecoder::decode_chunk_bytes_into(
            Some(&mut decompressor),
            None,
            CompressionMethod::Deflate,
            &compressed,
            &mut out,
        )
        .expect("exact-fill deflate chunk decodes");
        assert_eq!(out, payload);
    }

    /// Encode a minimal single-band GeoTIFF, applying `overrides` to the IFD
    /// tags between the encoder defaults and `write_data`.
    fn encode_gray_geotiff(
        width: u32,
        height: u32,
        compression: tiff::encoder::Compression,
        overrides: &[(Tag, u16)],
    ) -> tempfile::NamedTempFile {
        use tiff::encoder::{colortype::Gray8, TiffEncoder};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let file = File::create(tmp.path()).unwrap();
        let mut encoder = TiffEncoder::new(std::io::BufWriter::new(file))
            .unwrap()
            .with_compression(compression);
        let mut image = encoder.new_image::<Gray8>(width, height).unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[100.0f64, 100.0, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::ModelTiepointTag,
                &[0.0, 0.0, 0.0, -122.45, 37.80, 0.0][..],
            )
            .unwrap();
        for &(tag, value) in overrides {
            image.encoder().write_tag(tag, value).unwrap();
        }
        image
            .write_data(&vec![7u8; (width * height) as usize])
            .unwrap();
        tmp
    }

    #[test]
    fn short_offset_arrays_error_instead_of_sparse_fill() {
        // Packbits forces rows_per_strip = 1, giving one strip per row.
        let tmp = encode_gray_geotiff(32, 32, tiff::encoder::Compression::Packbits, &[]);
        let reader = GeoTiffStreamReader::open(tmp.path()).unwrap();
        let mut decoder = reader.open_decoder().unwrap();
        let info = decoder.chunk_info.clone().unwrap();
        assert!(info.chunk_offsets.len() > 1);

        // A TiffChunkInfo whose arrays are shorter than the declared grid must
        // not treat the tail chunks as sparse nodata fills.
        let mut bad = (*info).clone();
        bad.chunk_offsets = Arc::from(&bad.chunk_offsets[..1]);
        bad.chunk_bytes = Arc::from(&bad.chunk_bytes[..1]);
        decoder.chunk_info = Some(Arc::new(bad));
        let err = match decoder.read_chunk_with_payload(1, None, None) {
            Ok(_) => panic!("truncated offset arrays must error"),
            Err(e) => e,
        };
        assert!(
            matches!(err, RasterH3Error::InvalidMetadata(_)),
            "unexpected error: {err}"
        );
    }
}
