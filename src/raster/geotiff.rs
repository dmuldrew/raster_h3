use std::fs::File;
use std::io::{Cursor, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use memmap2::Mmap;
use tiff::decoder::{Decoder, DecodingBuffer, DecodingResult};
use tiff::tags::Tag;

use crate::error::Result;
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

/// Zero-copy memory-mapped GeoTIFF reader that decodes chunks on-demand
#[derive(Clone)]
pub struct GeoTiffStreamReader {
    pub file_path: PathBuf,
    pub mmap: Arc<Mmap>,
    pub metadata: GeoTiffMetadata,
    pub chunk_layout: ChunkLayout,
}

/// Persistent chunk decoder that reuses the TIFF decoder across reads.
/// This avoids re-parsing IFD headers, tag tables, and strip/tile offset
/// arrays on every chunk read — the single largest I/O optimization.
pub struct ChunkDecoder<'a> {
    decoder: Decoder<Cursor<&'a [u8]>>,
    chunk_layout: ChunkLayout,
    width: u32,
    height: u32,
}

impl<'a> ChunkDecoder<'a> {
    /// Read and decode a single chunk using the persistent decoder (no header re-parse)
    pub fn read_chunk(&mut self, chunk_index: u32) -> Result<(RasterChunk, DecodingResult)> {
        let chunk_bounds = self.chunk_layout.get_chunk_bounds(
            chunk_index,
            self.width,
            self.height,
        );

        let data = self.decoder.read_chunk(chunk_index)?;
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

        let data_dims = self.decoder.chunk_data_dimensions(chunk_index);
        let required_len = (data_dims.0 as usize) * (data_dims.1 as usize);

        let decoded_ok = match &mut buffer {
            DecodingResult::U8(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::U8(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::U16(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::U16(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::U32(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::U32(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::U64(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::U64(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I8(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::I8(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I16(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::I16(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I32(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::I32(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::I64(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::I64(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::F32(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0.0);
                    self.decoder
                        .read_chunk_to_buffer(DecodingBuffer::F32(&mut v[..required_len]), chunk_index, data_dims.0 as usize)
                        .is_ok()
                } else {
                    false
                }
            }
            DecodingResult::F64(ref mut v) => {
                if v.capacity() >= required_len {
                    v.resize(required_len, 0.0);
                    self.decoder
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
            let data = self.decoder.read_chunk(chunk_index)?;
            Ok((chunk_bounds, data))
        }
    }
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
        let (epsg, proj_string) = Self::extract_crs(&mut decoder);

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
        };

        Ok(Self {
            file_path: path_buf,
            mmap: mmap_arc,
            metadata,
            chunk_layout,
        })
    }

    /// Create a persistent ChunkDecoder that reuses the TIFF decoder for multiple chunk reads.
    /// The decoder parses the IFD once and then seeks directly to tile data on each read_chunk call.
    pub fn open_decoder(&self) -> Result<ChunkDecoder<'_>> {
        let cursor = Cursor::new(&self.mmap[..]);
        let decoder = Decoder::new(cursor)?;

        Ok(ChunkDecoder {
            decoder,
            chunk_layout: self.chunk_layout,
            width: self.metadata.width,
            height: self.metadata.height,
        })
    }

    /// Read and decode a single chunk on-demand directly from memory-mapped pages.
    /// NOTE: This creates a fresh decoder per call. For sequential reads, prefer open_decoder().
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
