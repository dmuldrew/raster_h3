use std::fs::File;
use std::io::{BufWriter, Cursor, Seek, SeekFrom, Write};
use std::path::Path;
use tempfile::NamedTempFile;
use tiff::decoder::{Decoder, DecodingResult};
use weezl::encode::Encoder as LZWEncoder;
use weezl::BitOrder;

use raster_h3::raster::geotiff::GeoTiffStreamReader;

#[test]
fn test_lzw_hawaii_cfl_exact_bitwise_parity() {
    let path = "data/CFL_HI.tif";
    if !Path::new(path).exists() {
        eprintln!("Skipping test: {} does not exist", path);
        return;
    }

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open CFL_HI.tif");
    let mmap = reader.mmap().expect("Failed to get mmap");
    let mut std_decoder = Decoder::new(Cursor::new(&mmap[..])).unwrap();
    let mut fast_decoder = reader.open_decoder().unwrap();

    let total_chunks = reader.chunk_layout.total_chunks;
    // Test a representative sample of chunks across beginning, middle, and end of the raster
    let test_indices = [
        0, 1, 2, 50, 100, 500, 1000, 2500, 5000, 7500, 10000, 12500, 15000, total_chunks - 1,
    ];

    for &chunk_idx in &test_indices {
        if chunk_idx >= total_chunks {
            continue;
        }
        let std_chunk = std_decoder.read_chunk(chunk_idx).unwrap();
        let (_bounds, fast_chunk) = fast_decoder.read_chunk(chunk_idx).unwrap();

        match (std_chunk, fast_chunk) {
            (DecodingResult::F32(v_std), DecodingResult::F32(v_fast)) => {
                assert_eq!(
                    v_std.len(),
                    v_fast.len(),
                    "Sample count mismatch on chunk {}",
                    chunk_idx
                );
                for (i, (s, f)) in v_std.iter().zip(v_fast.iter()).enumerate() {
                    if s.is_nan() && f.is_nan() {
                        continue;
                    }
                    assert_eq!(
                        s.to_bits(),
                        f.to_bits(),
                        "Bitwise mismatch on chunk {} sample {}: std={} vs fast={}",
                        chunk_idx,
                        i,
                        s,
                        f
                    );
                }
            }
            _ => panic!("Expected F32 DecodingResult for CFL_HI.tif"),
        }
    }
}

#[test]
fn test_lzw_hawaii_landfire_exact_bitwise_parity() {
    let path = "data/LF2024_FBFM40_HI.tif";
    if !Path::new(path).exists() {
        eprintln!("Skipping test: {} does not exist", path);
        return;
    }

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open LF2024_FBFM40_HI.tif");
    let mmap = reader.mmap().expect("Failed to get mmap");
    let mut std_decoder = Decoder::new(Cursor::new(&mmap[..])).unwrap();
    let mut fast_decoder = reader.open_decoder().unwrap();

    let total_chunks = reader.chunk_layout.total_chunks;
    let test_indices = [
        0, 1, 2, 50, 100, 500, 1000, 2500, 5000, 7500, 10000, 12500, 15000, total_chunks - 1,
    ];

    for &chunk_idx in &test_indices {
        if chunk_idx >= total_chunks {
            continue;
        }
        let std_chunk = std_decoder.read_chunk(chunk_idx).unwrap();
        let (_bounds, fast_chunk) = fast_decoder.read_chunk(chunk_idx).unwrap();

        match (std_chunk, fast_chunk) {
            (DecodingResult::I16(v_std), DecodingResult::I16(v_fast)) => {
                assert_eq!(
                    v_std.len(),
                    v_fast.len(),
                    "Sample count mismatch on chunk {}",
                    chunk_idx
                );
                assert_eq!(
                    v_std, v_fast,
                    "Pixel mismatch on chunk {}: std != fast",
                    chunk_idx
                );
            }
            _ => panic!("Expected I16 DecodingResult for LF2024_FBFM40_HI.tif"),
        }
    }
}

#[test]
fn test_lzw_read_chunk_into_buffer_recycling() {
    let path = "data/LF2024_FBFM40_HI.tif";
    if !Path::new(path).exists() {
        eprintln!("Skipping test: {} does not exist", path);
        return;
    }

    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut fast_decoder = reader.open_decoder().unwrap();

    // Read first chunk to establish baseline and recycled buffer
    let (_bounds0, buf) = fast_decoder.read_chunk(500).unwrap();

    // Verify read_chunk_into populates the recycled buffer
    let (_bounds1, buf_recycled) = fast_decoder.read_chunk_into(501, buf).unwrap();
    let (_bounds_direct, buf_direct) = fast_decoder.read_chunk(501).unwrap();

    match (buf_recycled, buf_direct) {
        (DecodingResult::I16(v_rec), DecodingResult::I16(v_dir)) => {
            assert_eq!(v_rec, v_dir);
        }
        _ => panic!("Expected I16"),
    }
}

/// Helper to write a custom synthetic LZW GeoTIFF with optional horizontal predictor
fn write_synthetic_lzw_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    predictor: u16,
) {
    let mut file = BufWriter::new(File::create(path).unwrap());
    file.write_all(b"II\x2a\x00\x08\x00\x00\x00").unwrap();

    let tiles_across = (width + tile_w - 1) / tile_w;
    let tiles_down = (height + tile_h - 1) / tile_h;
    let total_tiles = tiles_across * tiles_down;

    let tile_data_start = 65536u64;
    file.seek(SeekFrom::Start(tile_data_start)).unwrap();

    let mut tile_offsets = Vec::with_capacity(total_tiles as usize);
    let mut tile_byte_counts = Vec::with_capacity(total_tiles as usize);

    for tile_idx in 0..total_tiles {
        let tc = tile_idx % tiles_across;
        let tr = tile_idx / tiles_across;
        let x0 = tc * tile_w;
        let y0 = tr * tile_h;

        let mut raw_u16: Vec<u16> = Vec::with_capacity((tile_w * tile_h) as usize);
        for r in 0..tile_h {
            for c in 0..tile_w {
                let img_x = x0 + c;
                let img_y = y0 + r;
                let val = ((img_x * 7 + img_y * 11) % 5000) as u16;
                raw_u16.push(val);
            }
        }

        // Apply horizontal differencing if predictor == 2
        let mut processed_u16 = raw_u16.clone();
        if predictor == 2 {
            for r in 0..tile_h as usize {
                let row_start = r * tile_w as usize;
                for col in (1..tile_w as usize).rev() {
                    processed_u16[row_start + col] =
                        processed_u16[row_start + col].wrapping_sub(processed_u16[row_start + col - 1]);
                }
            }
        }

        // Convert to little endian bytes
        let mut raw_bytes = Vec::with_capacity(processed_u16.len() * 2);
        for &v in &processed_u16 {
            raw_bytes.extend_from_slice(&v.to_le_bytes());
        }

        // Compress with LZW (with TIFF size switch, MSB bit order)
        let mut encoder = LZWEncoder::with_tiff_size_switch(BitOrder::Msb, 8);
        let comp_buf = encoder.encode(&raw_bytes).unwrap();

        let current_offset = file.stream_position().unwrap();
        file.write_all(&comp_buf).unwrap();

        tile_offsets.push(current_offset);
        tile_byte_counts.push(comp_buf.len() as u64);
    }

    file.seek(SeekFrom::Start(8)).unwrap();

    let tiepoint_offset = file.stream_position().unwrap();
    let tiepoint: [f64; 6] = [0.0, 0.0, 0.0, -122.0, 38.0, 0.0];
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
    write_tag(&mut file, 259, 3, 1, 5);  // Compression = 5 (LZW)
    write_tag(&mut file, 262, 3, 1, 1);
    write_tag(&mut file, 277, 3, 1, 1);
    write_tag(&mut file, 317, 3, 1, predictor as u32); // Predictor
    write_tag(&mut file, 322, 4, 1, tile_w);
    write_tag(&mut file, 323, 4, 1, tile_h);
    write_tag(&mut file, 324, 4, total_tiles, tile_offsets_pos as u32);
    write_tag(&mut file, 325, 4, total_tiles, tile_byte_counts_pos as u32);
    write_tag(&mut file, 339, 3, 1, 1); // Uint
    write_tag(&mut file, 33550, 12, 3, scale_offset as u32);
    write_tag(&mut file, 33922, 12, 6, tiepoint_offset as u32);

    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.flush().unwrap();
}

#[test]
fn test_lzw_tiled_u16_horizontal_predictor_parity() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    let width = 64u32;
    let height = 64u32;
    let tile_w = 32u32;
    let tile_h = 32u32;
    // Predictor = 2 (Horizontal differencing)
    write_synthetic_lzw_geotiff(path, width, height, tile_w, tile_h, 2);

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open synthetic raster");
    let mut std_decoder = Decoder::new(Cursor::new(&reader.mmap().unwrap()[..])).unwrap();
    let mut fast_decoder = reader.open_decoder().unwrap();

    let total_chunks = reader.chunk_layout.total_chunks;
    assert_eq!(total_chunks, 4);

    for c in 0..total_chunks {
        let std_chunk = std_decoder.read_chunk(c).unwrap();
        let (_bounds, fast_chunk) = fast_decoder.read_chunk(c).unwrap();

        match (std_chunk, fast_chunk) {
            (DecodingResult::U16(v_std), DecodingResult::U16(v_fast)) => {
                assert_eq!(v_std, v_fast, "Mismatch on chunk {}", c);
            }
            _ => panic!("Expected U16 DecodingResult"),
        }
    }
}

#[test]
fn test_lzw_tiled_with_edge_padding_parity() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    // 70x50 with 32x32 tiles requires right-edge and bottom-edge tile padding
    let width = 70u32;
    let height = 50u32;
    let tile_w = 32u32;
    let tile_h = 32u32;
    write_synthetic_lzw_geotiff(path, width, height, tile_w, tile_h, 1);

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open synthetic raster");
    let mut std_decoder = Decoder::new(Cursor::new(&reader.mmap().unwrap()[..])).unwrap();
    let mut fast_decoder = reader.open_decoder().unwrap();

    let total_chunks = reader.chunk_layout.total_chunks;
    assert_eq!(total_chunks, 6); // 3 across x 2 down

    for c in 0..total_chunks {
        let std_chunk = std_decoder.read_chunk(c).unwrap();
        let (_bounds, fast_chunk) = fast_decoder.read_chunk(c).unwrap();

        match (std_chunk, fast_chunk) {
            (DecodingResult::U16(v_std), DecodingResult::U16(v_fast)) => {
                assert_eq!(v_std, v_fast, "Mismatch on edge-padded chunk {}", c);
            }
            _ => panic!("Expected U16 DecodingResult"),
        }
    }
}
