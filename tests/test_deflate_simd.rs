use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use tempfile::NamedTempFile;
use tiff::decoder::DecodingResult;
use tiff::encoder::colortype;
use tiff::encoder::compression::Deflate;
use tiff::encoder::TiffEncoder;
use tiff::tags::{CompressionMethod, Predictor};

use raster_h3::raster::geotiff::GeoTiffStreamReader;

/// Helper to write a custom tiled GeoTIFF with Deflate compression and optional predictor.
fn write_tiled_deflate_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    predictor: u16,
    data: &[f32],
) {
    let mut file = BufWriter::new(File::create(path).unwrap());

    // 1. TIFF Header (Little Endian, version 42, IFD offset placeholder)
    file.write_all(b"II\x2a\x00\x08\x00\x00\x00").unwrap();

    let tiles_across = (width + tile_w - 1) / tile_w;
    let tiles_down = (height + tile_h - 1) / tile_h;
    let total_tiles = tiles_across * tiles_down;

    // Write tile data starting at offset 4096.
    let tile_data_start = 4096u64;
    file.seek(SeekFrom::Start(tile_data_start)).unwrap();

    let mut tile_offsets = Vec::with_capacity(total_tiles as usize);
    let mut tile_byte_counts = Vec::with_capacity(total_tiles as usize);
    let mut compressor = libdeflater::Compressor::new(libdeflater::CompressionLvl::default());

    for tile_idx in 0..total_tiles {
        let tc = tile_idx % tiles_across;
        let tr = tile_idx / tiles_across;
        let x0 = tc * tile_w;
        let y0 = tr * tile_h;

        // Build raw tile buffer of size tile_w * tile_h * 4 bytes
        let mut raw_bytes = vec![0u8; (tile_w * tile_h * 4) as usize];
        for r in 0..tile_h {
            for c in 0..tile_w {
                let img_x = x0 + c;
                let img_y = y0 + r;
                let val = if img_x < width && img_y < height {
                    data[(img_y * width + img_x) as usize]
                } else {
                    0.0f32 // padding
                };
                let offset = ((r * tile_w + c) * 4) as usize;
                raw_bytes[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
            }
        }

        // Apply predictor if requested
        if predictor == 3 {
            let mut pred_bytes = vec![0u8; raw_bytes.len()];
            let w = tile_w as usize;
            for r in 0..tile_h as usize {
                let row_in = &raw_bytes[r * w * 4..(r + 1) * w * 4];
                let row_out = &mut pred_bytes[r * w * 4..(r + 1) * w * 4];
                for i in 0..w {
                    let be_bytes = f32::from_le_bytes([
                        row_in[i * 4],
                        row_in[i * 4 + 1],
                        row_in[i * 4 + 2],
                        row_in[i * 4 + 3],
                    ])
                    .to_bits()
                    .to_be_bytes();
                    row_out[i] = be_bytes[0];
                    row_out[w + i] = be_bytes[1];
                    row_out[2 * w + i] = be_bytes[2];
                    row_out[3 * w + i] = be_bytes[3];
                }
                for i in (1..row_out.len()).rev() {
                    row_out[i] = row_out[i].wrapping_sub(row_out[i - 1]);
                }
            }
            raw_bytes = pred_bytes;
        }

        // Compress tile with zlib framing
        let mut comp_buf = vec![0u8; raw_bytes.len() + 256];
        let comp_size = compressor.zlib_compress(&raw_bytes, &mut comp_buf).unwrap();

        let current_offset = file.stream_position().unwrap();
        file.write_all(&comp_buf[..comp_size]).unwrap();

        tile_offsets.push(current_offset);
        tile_byte_counts.push(comp_size as u64);
    }

    // Now write the metadata arrays right after the header (offset 8)
    file.seek(SeekFrom::Start(8)).unwrap();

    let tiepoint_offset = file.stream_position().unwrap();
    let tiepoint: [f64; 6] = [0.0, 0.0, 0.0, -122.45, 37.85, 0.0];
    for v in tiepoint {
        file.write_all(&v.to_le_bytes()).unwrap();
    }

    let scale_offset = file.stream_position().unwrap();
    let scale: [f64; 3] = [0.001, 0.001, 0.0];
    for v in scale {
        file.write_all(&v.to_le_bytes()).unwrap();
    }

    let tile_offsets_pos = file.stream_position().unwrap();
    for off in &tile_offsets {
        file.write_all(&(*off as u32).to_le_bytes()).unwrap();
    }

    let tile_byte_counts_pos = file.stream_position().unwrap();
    for cnt in &tile_byte_counts {
        file.write_all(&(*cnt as u32).to_le_bytes()).unwrap();
    }

    // IFD Directory
    let ifd_offset = file.stream_position().unwrap() as u32;

    // Update header pointer at offset 4
    file.seek(SeekFrom::Start(4)).unwrap();
    file.write_all(&ifd_offset.to_le_bytes()).unwrap();

    file.seek(SeekFrom::Start(ifd_offset as u64)).unwrap();

    // Helper to write a 12-byte IFD tag entry
    let write_tag = |f: &mut BufWriter<File>, tag: u16, typ: u16, count: u32, val_or_off: u32| {
        f.write_all(&tag.to_le_bytes()).unwrap();
        f.write_all(&typ.to_le_bytes()).unwrap();
        f.write_all(&count.to_le_bytes()).unwrap();
        f.write_all(&val_or_off.to_le_bytes()).unwrap();
    };

    let num_tags = 14u16;
    file.write_all(&num_tags.to_le_bytes()).unwrap();

    // Tags MUST be written in ascending tag order:
    // 256: ImageWidth (LONG)
    write_tag(&mut file, 256, 4, 1, width);
    // 257: ImageLength (LONG)
    write_tag(&mut file, 257, 4, 1, height);
    // 258: BitsPerSample (SHORT = 32)
    write_tag(&mut file, 258, 3, 1, 32);
    // 259: Compression (SHORT = 8 Deflate)
    write_tag(&mut file, 259, 3, 1, 8);
    // 262: PhotometricInterpretation (SHORT = 1 BlackIsZero)
    write_tag(&mut file, 262, 3, 1, 1);
    // 277: SamplesPerPixel (SHORT = 1)
    write_tag(&mut file, 277, 3, 1, 1);
    // 317: Predictor (SHORT)
    write_tag(&mut file, 317, 3, 1, predictor as u32);
    // 322: TileWidth (LONG)
    write_tag(&mut file, 322, 4, 1, tile_w);
    // 323: TileLength (LONG)
    write_tag(&mut file, 323, 4, 1, tile_h);
    // 324: TileOffsets (LONG)
    write_tag(&mut file, 324, 4, total_tiles, tile_offsets_pos as u32);
    // 325: TileByteCounts (LONG)
    write_tag(&mut file, 325, 4, total_tiles, tile_byte_counts_pos as u32);
    // 339: SampleFormat (SHORT = 3 IEEEFP)
    write_tag(&mut file, 339, 3, 1, 3);
    // 33550: ModelPixelScaleTag (DOUBLE, count=3)
    write_tag(&mut file, 33550, 12, 3, scale_offset as u32);
    // 33922: ModelTiepointTag (DOUBLE, count=6)
    write_tag(&mut file, 33922, 12, 6, tiepoint_offset as u32);

    // Next IFD offset = 0
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.flush().unwrap();
}

#[test]
fn test_deflate_striped_u8_parity() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();
    let width = 120u32;
    let height = 80u32;
    let mut ground_truth = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            ground_truth.push(((x * 7 + y * 13) % 256) as u8);
        }
    }

    {
        let file = File::create(path).unwrap();
        let mut encoder = TiffEncoder::new(BufWriter::new(file)).unwrap();
        let mut img = encoder
            .new_image_with_compression::<colortype::Gray8, Deflate>(width, height, Deflate::default())
            .unwrap();
        img.rows_per_strip(16).unwrap();
        img.write_data(&ground_truth).unwrap();
    }

    let reader = GeoTiffStreamReader::open(path).unwrap();
    assert!(reader.chunk_info.is_some());
    let info = reader.chunk_info.as_ref().unwrap();
    assert_eq!(info.compression, CompressionMethod::Deflate);

    let mut decoder = reader.open_decoder().unwrap();
    for chunk_idx in 0..reader.chunk_layout.total_chunks {
        let (bounds, decoded) = decoder.read_chunk(chunk_idx).unwrap();
        if let DecodingResult::U8(pixels) = decoded {
            assert_eq!(pixels.len(), (bounds.width * bounds.height) as usize);
            for r in 0..bounds.height {
                for c in 0..bounds.width {
                    let gx = bounds.col_offset + c;
                    let gy = bounds.row_offset + r;
                    let expected = ground_truth[(gy * width + gx) as usize];
                    let actual = pixels[(r * bounds.width + c) as usize];
                    assert_eq!(actual, expected, "Mismatch at ({}, {})", gx, gy);
                }
            }
        } else {
            panic!("Expected U8 result");
        }
    }
}

#[test]
fn test_deflate_striped_f32_parity_and_streaming() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();
    let width = 64u32;
    let height = 48u32;
    let mut ground_truth = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            ground_truth.push((x as f32) * 1.5 + (y as f32) * 2.5);
        }
    }

    {
        let file = File::create(path).unwrap();
        let mut encoder = TiffEncoder::new(BufWriter::new(file)).unwrap();
        let mut img = encoder
            .new_image_with_compression::<colortype::Gray32Float, Deflate>(
                width,
                height,
                Deflate::default(),
            )
            .unwrap();
        img.rows_per_strip(8).unwrap();
        img.write_data(&ground_truth).unwrap();
    }

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut decoder = reader.open_decoder().unwrap();

    let mut all_pixels = vec![0.0f32; (width * height) as usize];
    for chunk_idx in 0..reader.chunk_layout.total_chunks {
        let (bounds, decoded) = decoder.read_chunk(chunk_idx).unwrap();
        if let DecodingResult::F32(pixels) = decoded {
            for r in 0..bounds.height {
                for c in 0..bounds.width {
                    let gx = bounds.col_offset + c;
                    let gy = bounds.row_offset + r;
                    all_pixels[(gy * width + gx) as usize] = pixels[(r * bounds.width + c) as usize];
                }
            }
        } else {
            panic!("Expected F32 result");
        }
    }

    for i in 0..ground_truth.len() {
        assert_eq!(all_pixels[i], ground_truth[i]);
    }
}

#[test]
fn test_deflate_tiled_f32_with_padding_parity() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    // 100x70 image with 32x32 tiles (leaves 4 tiles across with right padding, 3 tiles down with bottom padding)
    let width = 100u32;
    let height = 70u32;
    let tile_w = 32u32;
    let tile_h = 32u32;

    let mut ground_truth = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            ground_truth.push(10.0 + (x as f32) * 0.1 + (y as f32) * 0.2);
        }
    }

    // Write tiled Deflate TIFF with Predictor::None
    write_tiled_deflate_geotiff(path, width, height, tile_w, tile_h, 1, &ground_truth);

    let reader = GeoTiffStreamReader::open(path).unwrap();
    assert!(reader.chunk_info.is_some());
    let info = reader.chunk_info.as_ref().unwrap();
    assert_eq!(info.chunk_dimensions, (32, 32));

    let mut decoder = reader.open_decoder().unwrap();
    let mut reconstructed = vec![0.0f32; (width * height) as usize];

    for chunk_idx in 0..reader.chunk_layout.total_chunks {
        let (bounds, decoded) = decoder.read_chunk(chunk_idx).unwrap();
        if let DecodingResult::F32(pixels) = decoded {
            assert_eq!(pixels.len(), (bounds.width * bounds.height) as usize);
            for r in 0..bounds.height {
                for c in 0..bounds.width {
                    let gx = bounds.col_offset + c;
                    let gy = bounds.row_offset + r;
                    reconstructed[(gy * width + gx) as usize] = pixels[(r * bounds.width + c) as usize];
                }
            }
        } else {
            panic!("Expected F32 result");
        }
    }

    for i in 0..ground_truth.len() {
        assert_eq!(reconstructed[i], ground_truth[i], "Mismatch at index {}", i);
    }
}

#[test]
fn test_deflate_tiled_f32_floating_point_predictor() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    // 96x64 image with 32x32 tiles, Predictor::FloatingPoint
    let width = 96u32;
    let height = 64u32;
    let tile_w = 32u32;
    let tile_h = 32u32;

    let mut ground_truth = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            ground_truth.push(100.5 + (x as f32) * 0.25 - (y as f32) * 0.5);
        }
    }

    write_tiled_deflate_geotiff(path, width, height, tile_w, tile_h, 3, &ground_truth);

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let info = reader.chunk_info.as_ref().unwrap();
    assert_eq!(info.predictor, Predictor::FloatingPoint);

    let mut decoder = reader.open_decoder().unwrap();
    let mut reconstructed = vec![0.0f32; (width * height) as usize];

    for chunk_idx in 0..reader.chunk_layout.total_chunks {
        let (bounds, decoded) = decoder.read_chunk(chunk_idx).unwrap();
        if let DecodingResult::F32(pixels) = decoded {
            assert_eq!(pixels.len(), (bounds.width * bounds.height) as usize);
            for r in 0..bounds.height {
                for c in 0..bounds.width {
                    let gx = bounds.col_offset + c;
                    let gy = bounds.row_offset + r;
                    reconstructed[(gy * width + gx) as usize] = pixels[(r * bounds.width + c) as usize];
                }
            }
        } else {
            panic!("Expected F32 result");
        }
    }

    for i in 0..ground_truth.len() {
        assert_eq!(reconstructed[i], ground_truth[i], "Mismatch at index {}", i);
    }
}

#[test]
fn test_deflate_read_chunk_into_buffer_recycling() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();
    let width = 64u32;
    let height = 64u32;
    let ground_truth: Vec<f32> = (0..width * height).map(|v| v as f32).collect();

    write_tiled_deflate_geotiff(path, width, height, 32, 32, 1, &ground_truth);

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut decoder = reader.open_decoder().unwrap();

    // Pre-allocate a buffer of sufficient capacity
    let mut recycled = DecodingResult::F32(Vec::with_capacity(32 * 32));

    for chunk_idx in 0..reader.chunk_layout.total_chunks {
        let (bounds, decoded) = decoder.read_chunk_into(chunk_idx, recycled).unwrap();
        if let DecodingResult::F32(ref pixels) = decoded {
            assert_eq!(pixels.len(), (bounds.width * bounds.height) as usize);
        }
        recycled = decoded; // pass recycled buffer back
    }
}

/// Helper to write a tiled GeoTIFF with U16 data, Deflate compression, and horizontal predictor.
fn write_tiled_deflate_u16_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    predictor: u16,
    data: &[u16],
) {
    let mut file = BufWriter::new(File::create(path).unwrap());
    file.write_all(b"II\x2a\x00\x08\x00\x00\x00").unwrap();

    let tiles_across = (width + tile_w - 1) / tile_w;
    let tiles_down = (height + tile_h - 1) / tile_h;
    let total_tiles = tiles_across * tiles_down;

    let tile_data_start = 4096u64;
    file.seek(SeekFrom::Start(tile_data_start)).unwrap();

    let mut tile_offsets = Vec::with_capacity(total_tiles as usize);
    let mut tile_byte_counts = Vec::with_capacity(total_tiles as usize);
    let mut compressor = libdeflater::Compressor::new(libdeflater::CompressionLvl::default());

    for tile_idx in 0..total_tiles {
        let tc = tile_idx % tiles_across;
        let tr = tile_idx / tiles_across;
        let x0 = tc * tile_w;
        let y0 = tr * tile_h;

        let mut raw_u16 = vec![0u16; (tile_w * tile_h) as usize];
        for r in 0..tile_h {
            for c in 0..tile_w {
                let img_x = x0 + c;
                let img_y = y0 + r;
                let val = if img_x < width && img_y < height {
                    data[(img_y * width + img_x) as usize]
                } else {
                    0u16
                };
                raw_u16[(r * tile_w + c) as usize] = val;
            }
        }

        if predictor == 2 {
            // Horizontal differencing across rows
            for r in 0..tile_h as usize {
                let row = &mut raw_u16[r * tile_w as usize..(r + 1) * tile_w as usize];
                for i in (1..row.len()).rev() {
                    row[i] = row[i].wrapping_sub(row[i - 1]);
                }
            }
        }

        let mut raw_bytes = vec![0u8; (tile_w * tile_h * 2) as usize];
        for (i, v) in raw_u16.iter().enumerate() {
            raw_bytes[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }

        let mut comp_buf = vec![0u8; raw_bytes.len() + 256];
        let comp_size = compressor.zlib_compress(&raw_bytes, &mut comp_buf).unwrap();

        let current_offset = file.stream_position().unwrap();
        file.write_all(&comp_buf[..comp_size]).unwrap();

        tile_offsets.push(current_offset);
        tile_byte_counts.push(comp_size as u64);
    }

    file.seek(SeekFrom::Start(8)).unwrap();

    let tiepoint_offset = file.stream_position().unwrap();
    let tiepoint: [f64; 6] = [0.0, 0.0, 0.0, -122.45, 37.85, 0.0];
    for v in tiepoint {
        file.write_all(&v.to_le_bytes()).unwrap();
    }

    let scale_offset = file.stream_position().unwrap();
    let scale: [f64; 3] = [0.001, 0.001, 0.0];
    for v in scale {
        file.write_all(&v.to_le_bytes()).unwrap();
    }

    let tile_offsets_pos = file.stream_position().unwrap();
    for off in &tile_offsets {
        file.write_all(&(*off as u32).to_le_bytes()).unwrap();
    }

    let tile_byte_counts_pos = file.stream_position().unwrap();
    for cnt in &tile_byte_counts {
        file.write_all(&(*cnt as u32).to_le_bytes()).unwrap();
    }

    let ifd_offset = file.stream_position().unwrap() as u32;
    file.seek(SeekFrom::Start(4)).unwrap();
    file.write_all(&ifd_offset.to_le_bytes()).unwrap();

    file.seek(SeekFrom::Start(ifd_offset as u64)).unwrap();

    let write_tag = |f: &mut BufWriter<File>, tag: u16, typ: u16, count: u32, val_or_off: u32| {
        f.write_all(&tag.to_le_bytes()).unwrap();
        f.write_all(&typ.to_le_bytes()).unwrap();
        f.write_all(&count.to_le_bytes()).unwrap();
        f.write_all(&val_or_off.to_le_bytes()).unwrap();
    };

    let num_tags = 14u16;
    file.write_all(&num_tags.to_le_bytes()).unwrap();

    write_tag(&mut file, 256, 4, 1, width);
    write_tag(&mut file, 257, 4, 1, height);
    write_tag(&mut file, 258, 3, 1, 16); // BitsPerSample = 16
    write_tag(&mut file, 259, 3, 1, 8);  // Compression = Deflate
    write_tag(&mut file, 262, 3, 1, 1);  // BlackIsZero
    write_tag(&mut file, 277, 3, 1, 1);  // SamplesPerPixel = 1
    write_tag(&mut file, 317, 3, 1, predictor as u32);
    write_tag(&mut file, 322, 4, 1, tile_w);
    write_tag(&mut file, 323, 4, 1, tile_h);
    write_tag(&mut file, 324, 4, total_tiles, tile_offsets_pos as u32);
    write_tag(&mut file, 325, 4, total_tiles, tile_byte_counts_pos as u32);
    write_tag(&mut file, 339, 3, 1, 1);  // SampleFormat = 1 (Uint)
    write_tag(&mut file, 33550, 12, 3, scale_offset as u32);
    write_tag(&mut file, 33922, 12, 6, tiepoint_offset as u32);

    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.flush().unwrap();
}

#[test]
fn test_deflate_tiled_u16_horizontal_predictor() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    let width = 75u32;
    let height = 55u32;
    let tile_w = 32u32;
    let tile_h = 32u32;

    let mut ground_truth = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            ground_truth.push(((x * 17 + y * 31) % 65535) as u16);
        }
    }

    write_tiled_deflate_u16_geotiff(path, width, height, tile_w, tile_h, 2, &ground_truth);

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let info = reader.chunk_info.as_ref().unwrap();
    assert_eq!(info.predictor, Predictor::Horizontal);

    let mut decoder = reader.open_decoder().unwrap();
    let mut reconstructed = vec![0u16; (width * height) as usize];

    for chunk_idx in 0..reader.chunk_layout.total_chunks {
        let (bounds, decoded) = decoder.read_chunk(chunk_idx).unwrap();
        if let DecodingResult::U16(pixels) = decoded {
            assert_eq!(pixels.len(), (bounds.width * bounds.height) as usize);
            for r in 0..bounds.height {
                for c in 0..bounds.width {
                    let gx = bounds.col_offset + c;
                    let gy = bounds.row_offset + r;
                    reconstructed[(gy * width + gx) as usize] = pixels[(r * bounds.width + c) as usize];
                }
            }
        } else {
            panic!("Expected U16 result");
        }
    }

    for i in 0..ground_truth.len() {
        assert_eq!(reconstructed[i], ground_truth[i], "Mismatch at index {}", i);
    }
}

#[test]
fn test_deflate_exact_bitwise_equivalence_with_tiff_decoder() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    let width = 100u32;
    let height = 70u32;
    let tile_w = 32u32;
    let tile_h = 32u32;

    let ground_truth: Vec<f32> = (0..width * height)
        .map(|v| (v as f32) * std::f32::consts::PI)
        .collect();

    write_tiled_deflate_geotiff(path, width, height, tile_w, tile_h, 1, &ground_truth);

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mmap = reader.mmap().unwrap();

    // 1. Decode with standard tiff Decoder (using flate2/miniz_oxide)
    let cursor = std::io::Cursor::new(&mmap[..]);
    let mut standard_decoder = tiff::decoder::Decoder::new(cursor).unwrap();

    // 2. Decode with SIMD libdeflater ChunkDecoder
    let mut simd_decoder = reader.open_decoder().unwrap();

    for chunk_idx in 0..reader.chunk_layout.total_chunks {
        let std_result = standard_decoder.read_chunk(chunk_idx).unwrap();
        let (_bounds, simd_result) = simd_decoder.read_chunk(chunk_idx).unwrap();

        match (std_result, simd_result) {
            (DecodingResult::F32(std_pix), DecodingResult::F32(simd_pix)) => {
                assert_eq!(std_pix.len(), simd_pix.len());
                for (idx, (s, d)) in std_pix.iter().zip(simd_pix.iter()).enumerate() {
                    assert_eq!(
                        s.to_bits(),
                        d.to_bits(),
                        "Bitwise difference at chunk {}, sample {}: std={}, simd={}",
                        chunk_idx,
                        idx,
                        s,
                        d
                    );
                }
            }
            _ => panic!("Expected F32 DecodingResult for both standard and SIMD"),
        }
    }
}

#[test]
fn test_deflate_corrupted_byte_stream_hardening() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    let width = 64u32;
    let height = 64u32;
    let ground_truth: Vec<f32> = (0..width * height).map(|v| v as f32).collect();
    write_tiled_deflate_geotiff(path, width, height, 32, 32, 1, &ground_truth);

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut decoder = reader.open_decoder().unwrap();

    // 1. Empty byte slice
    let res_empty = decoder.decompress_chunk_fast_bytes(0, &[], None);
    assert!(res_empty.is_err(), "Empty byte slice must return Err");

    // 2. Truncated zlib header (only 2 bytes)
    let res_trunc = decoder.decompress_chunk_fast_bytes(0, &[0x78, 0x9c], None);
    assert!(res_trunc.is_err(), "Truncated zlib stream must return Err");

    // 3. Fuzzing: random / invalid byte streams
    let corrupt_cases: Vec<Vec<u8>> = vec![
        vec![0xFF; 64],
        vec![0x00; 128],
        vec![0x78, 0x9c, 0xFF, 0xFF, 0x00, 0x01],
        (0..255).map(|x| (x * 37) as u8).collect(),
        vec![0xAA; 1024],
    ];

    for (i, corrupted) in corrupt_cases.iter().enumerate() {
        let res = decoder.decompress_chunk_fast_bytes(0, corrupted, None);
        assert!(res.is_err(), "Corrupted stream case {} must return Err without panicking", i);

        // Fallback resilience: when corrupted payload fails, read_chunk_with_payload
        // safely falls back to reading from disk without crashing or panicking
        let res_payload = decoder.read_chunk_with_payload(0, Some(corrupted), None);
        assert!(res_payload.is_ok(), "Fallback to file on corrupted payload must succeed gracefully");
    }
}

#[test]
fn test_deflate_buffer_auto_resizing_hardening() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    let width = 64u32;
    let height = 64u32;
    let ground_truth: Vec<f32> = (0..width * height).map(|v| v as f32 * 2.5).collect();
    write_tiled_deflate_geotiff(path, width, height, 32, 32, 1, &ground_truth);

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut decoder = reader.open_decoder().unwrap();

    let (bounds, _read_res) = decoder.read_chunk(0).expect("read_chunk(0) failed");
    assert_eq!(bounds.width, 32);
    assert_eq!(bounds.height, 32);

    // Pass an undersized target buffer: only 4 elements instead of required 1024
    let undersized = DecodingResult::F32(vec![0.0f32; 4]);
    let (_, res_buf) = decoder.read_chunk_into(0, undersized).expect("read_chunk_into failed");
    if let DecodingResult::F32(ref v) = res_buf {
        assert_eq!(v.len(), 1024, "Buffer must be automatically resized to match chunk sample count");
        assert_eq!(v[0], ground_truth[0]);
    } else {
        panic!("Expected F32 DecodingResult");
    }
}

