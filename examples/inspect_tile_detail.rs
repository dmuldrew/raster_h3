use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use flate2::read::GzDecoder;
use raster_h3::pmtiles::writer::tile_id_to_zxy;

fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            break;
        }
        shift += 7;
    }
    result
}

fn zigzag_decode(val: u32) -> i32 {
    ((val >> 1) as i32) ^ (-((val & 1) as i32))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open("data/CFL_HI_pyramid.pmtiles")?;
    let mut header = [0u8; 127];
    file.read_exact(&mut header)?;

    let root_offset = u64::from_le_bytes(header[8..16].try_into()?);
    let root_length = u64::from_le_bytes(header[16..24].try_into()?);
    let tile_data_offset = u64::from_le_bytes(header[56..64].try_into()?);
    let num_tile_entries = u64::from_le_bytes(header[80..88].try_into()?);

    println!("Total tile entries: {}", num_tile_entries);

    // Read and decompress root directory
    file.seek(SeekFrom::Start(root_offset))?;
    let mut dir_buf = vec![0u8; root_length as usize];
    file.read_exact(&mut dir_buf)?;

    let mut gz = GzDecoder::new(&dir_buf[..]);
    let mut dir_decompressed = Vec::new();
    gz.read_to_end(&mut dir_decompressed)?;

    // Parse directory
    let mut pos = 0;
    let num_entries = read_varint(&dir_decompressed, &mut pos) as usize;
    println!("Directory entries count: {}", num_entries);

    // Tile IDs
    let mut tile_ids = Vec::with_capacity(num_entries);
    let mut last_id = 0u64;
    for _ in 0..num_entries {
        let delta = read_varint(&dir_decompressed, &mut pos);
        let id = last_id + delta;
        tile_ids.push(id);
        last_id = id;
    }

    // Run lengths
    let mut run_lengths = Vec::with_capacity(num_entries);
    for _ in 0..num_entries {
        run_lengths.push(read_varint(&dir_decompressed, &mut pos) as u32);
    }

    // Lengths
    let mut lengths = Vec::with_capacity(num_entries);
    for _ in 0..num_entries {
        lengths.push(read_varint(&dir_decompressed, &mut pos) as u32);
    }

    // Offsets
    let mut offsets = Vec::with_capacity(num_entries);
    let mut last_offset = 0u64;
    for (i, &rl) in run_lengths.iter().enumerate() {
        if i == 0 || rl == 0 {
            let off = read_varint(&dir_decompressed, &mut pos);
            offsets.push(off);
            last_offset = off + (lengths[i] as u64);
        } else {
            let delta = read_varint(&dir_decompressed, &mut pos);
            let off = if delta == 0 {
                last_offset
            } else {
                last_offset + delta - 1
            };
            offsets.push(off);
            last_offset = off + (lengths[i] as u64);
        }
    }

    println!("\nSample of tiles in directory:");
    for i in 0..num_entries.min(15) {
        let tid = tile_ids[i];
        let (z, x, y) = tile_id_to_zxy(tid);
        println!("  Entry #{:3}: TileID={:<8} -> (z={:<2}, x={:<5}, y={:<5}), offset={}, length={}",
            i, tid, z, x, y, offsets[i], lengths[i]);
    }

    // Now inspect a tile at zoom 5 or 7
    let target_idx = tile_ids.iter().position(|&tid| {
        let (z, _, _) = tile_id_to_zxy(tid);
        z == 5 || z == 7
    }).unwrap_or(0);

    let (tz, tx, ty) = tile_id_to_zxy(tile_ids[target_idx]);
    let toff = offsets[target_idx];
    let tlen = lengths[target_idx];
    println!("\nInspecting Tile: (z={}, x={}, y={}) at offset {}, len {}", tz, tx, ty, toff, tlen);

    file.seek(SeekFrom::Start(tile_data_offset + toff))?;
    let mut tile_compressed = vec![0u8; tlen as usize];
    file.read_exact(&mut tile_compressed)?;

    let mut gz_tile = GzDecoder::new(&tile_compressed[..]);
    let mut mvt_decompressed = Vec::new();
    let decompress_res = gz_tile.read_to_end(&mut mvt_decompressed);
    match decompress_res {
        Ok(sz) => println!("MVT decompressed size: {} bytes", sz),
        Err(e) => {
            println!("MVT decompression failed: {:?} (raw bytes len: {})", e, tile_compressed.len());
            return Ok(());
        }
    }

    // Inspect protobuf bytes in MVT
    println!("MVT header bytes (first 32): {:02x?}", &mvt_decompressed[..32.min(mvt_decompressed.len())]);

    Ok(())
}
