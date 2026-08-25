use std::fs::File;
use std::io::{Cursor, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use memmap2::Mmap;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::error::{RasterH3Error, Result};
use crate::raster::geotransform::GeoTransform;
use crate::raster::RasterChunk;

/// Parsed GeoTIFF metadata
#[derive(Debug, Clone)]
pub struct GeoTiffMetadata {
    pub width: u32,
    pub height: u32,
    pub geotransform: GeoTransform,
    pub nodata: Option<f64>,
    pub epsg: Option<u32>,
    pub proj_string: Option<String>,
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

/// Zero-copy memory-mapped GeoTIFF reader that decodes chunks on-demand
#[derive(Clone)]
pub struct GeoTiffStreamReader {
    pub file_path: PathBuf,
    pub mmap: Arc<Mmap>,
    pub metadata: GeoTiffMetadata,
    pub chunk_layout: ChunkLayout,
}

impl GeoTiffStreamReader {
    /// Open and memory-map a GeoTIFF file, decoding only header tags and layout metadata
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let file = File::open(&path_buf)?;
        let mmap = unsafe { Mmap::map(&file)? };
        #[cfg(unix)]
        let _ = mmap.advise(memmap2::Advice::Sequential);
        let mmap_arc = Arc::new(mmap);

        let cursor = Cursor::new(&mmap_arc[..]);
        let mut decoder = Decoder::new(cursor)?;

        let (width, height) = decoder.dimensions()?;
        let geotransform = Self::extract_geotransform(&mut decoder)?;
        let nodata = Self::extract_nodata(&mut decoder);
        let epsg = Self::extract_epsg(&mut decoder);

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
            proj_string: None,
        };

        Ok(Self {
            file_path: path_buf,
            mmap: mmap_arc,
            metadata,
            chunk_layout,
        })
    }

    /// Read and decode a single chunk on-demand directly from memory-mapped pages
    pub fn read_chunk(&self, chunk_index: u32) -> Result<(RasterChunk, DecodingResult)> {
        let cursor = Cursor::new(&self.mmap[..]);
        let mut decoder = Decoder::new(cursor)?;

        let chunk_bounds = self.chunk_layout.get_chunk_bounds(
            chunk_index,
            self.metadata.width,
            self.metadata.height,
        );

        let data = decoder.read_chunk(chunk_index)?;
        Ok((chunk_bounds, data))
    }

    /// Extract affine geotransform from tags
    fn extract_geotransform<R: std::io::Read + Seek>(decoder: &mut Decoder<R>) -> Result<GeoTransform> {
        if let Ok(matrix) = decoder.get_tag_f64_vec(Tag::Unknown(34264)) {
            if let Some(gt) = GeoTransform::from_model_transformation(&matrix) {
                return Ok(gt);
            }
        }

        let tiepoint_res = decoder.get_tag_f64_vec(Tag::Unknown(33922));
        let scale_res = decoder.get_tag_f64_vec(Tag::Unknown(33550));

        if let (Ok(tiepoint), Ok(scale)) = (tiepoint_res, scale_res) {
            if let Some(gt) = GeoTransform::from_tiepoint_and_scale(&tiepoint, &scale) {
                return Ok(gt);
            }
        }

        Ok(GeoTransform::default())
    }

    /// Extract NoData value from tag 42113
    fn extract_nodata<R: std::io::Read + Seek>(decoder: &mut Decoder<R>) -> Option<f64> {
        if let Ok(s) = decoder.get_tag_ascii_string(Tag::Unknown(42113)) {
            if let Ok(val) = s.trim().parse::<f64>() {
                return Some(val);
            }
        }
        None
    }

    /// Extract EPSG code from GeoKeyDirectoryTag (34735)
    fn extract_epsg<R: std::io::Read + Seek>(decoder: &mut Decoder<R>) -> Option<u32> {
        if let Ok(keys) = decoder.get_tag_u16_vec(Tag::Unknown(34735)) {
            if keys.len() >= 4 {
                let num_keys = keys[3] as usize;
                for i in 0..num_keys {
                    let offset = 4 + i * 4;
                    if offset + 3 < keys.len() {
                        let key_id = keys[offset];
                        let tiff_tag_loc = keys[offset + 1];
                        let val = keys[offset + 3];

                        if tiff_tag_loc == 0 {
                            if key_id == 3072 && val > 0 && val != 32767 {
                                return Some(val as u32);
                            }
                            if key_id == 2048 && val > 0 && val != 32767 {
                                return Some(val as u32);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}
