use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use flate2::read::GzDecoder;

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
    let tile_data_offset = u64::from_le_bytes(header[56..64].try_into()?);

    // Read Tile 0 at offset 0, len 237
    file.seek(SeekFrom::Start(tile_data_offset))?;
    let mut tile_compressed = vec![0u8; 237];
    file.read_exact(&mut tile_compressed)?;

    let mut gz = GzDecoder::new(&tile_compressed[..]);
    let mut decompressed = Vec::new();
    gz.read_to_end(&mut decompressed)?;

    println!("Tile decompressed bytes: {}", decompressed.len());
    
    // Parse tile layer
    let mut pos = 0;
    while pos < decompressed.len() {
        let tag_wire = read_varint(&decompressed, &mut pos);
        let field = tag_wire >> 3;
        let wire = tag_wire & 0x7;
        println!("Tile Tag: field={}, wire={}", field, wire);
        if field == 3 && wire == 2 {
            let layer_len = read_varint(&decompressed, &mut pos) as usize;
            let layer_end = pos + layer_len;
            println!("Layer length: {}", layer_len);

            let mut keys = Vec::new();
            let mut features = Vec::new();
            while pos < layer_end {
                let l_tag_wire = read_varint(&decompressed, &mut pos);
                let l_field = l_tag_wire >> 3;
                let l_wire = l_tag_wire & 0x7;
                match (l_field, l_wire) {
                    (15, 0) => {
                        let ver = read_varint(&decompressed, &mut pos);
                        println!("  Layer version: {}", ver);
                    }
                    (1, 2) => {
                        let name_len = read_varint(&decompressed, &mut pos) as usize;
                        let name = String::from_utf8_lossy(&decompressed[pos..pos+name_len]).to_string();
                        pos += name_len;
                        println!("  Layer name: {}", name);
                    }
                    (2, 2) => {
                        let feat_len = read_varint(&decompressed, &mut pos) as usize;
                        let feat_bytes = &decompressed[pos..pos+feat_len];
                        pos += feat_len;
                        features.push(feat_bytes);
                    }
                    (3, 2) => {
                        let k_len = read_varint(&decompressed, &mut pos) as usize;
                        let k = String::from_utf8_lossy(&decompressed[pos..pos+k_len]).to_string();
                        pos += k_len;
                        keys.push(k);
                    }
                    (4, 2) => {
                        let v_len = read_varint(&decompressed, &mut pos) as usize;
                        pos += v_len;
                    }
                    (5, 0) => {
                        let extent = read_varint(&decompressed, &mut pos);
                        println!("  Layer extent: {}", extent);
                    }
                    _ => {
                        println!("  Unhandled layer field: {}, wire: {}", l_field, l_wire);
                        break;
                    }
                }
            }

            println!("  Keys found: {:?}", keys);
            println!("  Total features: {}", features.len());

            for (f_idx, feat_bytes) in features.iter().enumerate() {
                println!("\n  --- Feature #{} ({} bytes) ---", f_idx, feat_bytes.len());
                let mut f_pos = 0;
                while f_pos < feat_bytes.len() {
                    let f_tag = read_varint(feat_bytes, &mut f_pos);
                    let f_f = f_tag >> 3;
                    let f_w = f_tag & 0x7;
                    match (f_f, f_w) {
                        (1, 0) => {
                            let id = read_varint(feat_bytes, &mut f_pos);
                            println!("    ID: 0x{:x}", id);
                        }
                        (2, 2) => {
                            let tag_len = read_varint(feat_bytes, &mut f_pos) as usize;
                            let tag_end = f_pos + tag_len;
                            let mut tags = Vec::new();
                            while f_pos < tag_end {
                                tags.push(read_varint(feat_bytes, &mut f_pos));
                            }
                            println!("    Tags (k/v pairs count): {}", tags.len() / 2);
                        }
                        (3, 0) => {
                            let geom_type = read_varint(feat_bytes, &mut f_pos);
                            println!("    Geom Type: {} (1=point, 2=line, 3=polygon)", geom_type);
                        }
                        (4, 2) => {
                            let g_len = read_varint(feat_bytes, &mut f_pos) as usize;
                            let g_end = f_pos + g_len;
                            println!("    Geometry command stream ({} bytes):", g_len);
                            let mut cur_x = 0;
                            let mut cur_y = 0;
                            while f_pos < g_end {
                                let cmd_int = read_varint(feat_bytes, &mut f_pos) as u32;
                                let cmd = cmd_int & 0x7;
                                let count = cmd_int >> 3;
                                match cmd {
                                    1 => {
                                        let dx = zigzag_decode(read_varint(feat_bytes, &mut f_pos) as u32);
                                        let dy = zigzag_decode(read_varint(feat_bytes, &mut f_pos) as u32);
                                        cur_x += dx;
                                        cur_y += dy;
                                        println!("      MoveTo(count={}): relative=({}, {}), absolute=({}, {})", count, dx, dy, cur_x, cur_y);
                                    }
                                    2 => {
                                        println!("      LineTo(count={}):", count);
                                        for _ in 0..count {
                                            let dx = zigzag_decode(read_varint(feat_bytes, &mut f_pos) as u32);
                                            let dy = zigzag_decode(read_varint(feat_bytes, &mut f_pos) as u32);
                                            cur_x += dx;
                                            cur_y += dy;
                                            println!("        -> relative=({}, {}), absolute=({}, {})", dx, dy, cur_x, cur_y);
                                        }
                                    }
                                    7 => {
                                        println!("      ClosePath(count={})", count);
                                    }
                                    _ => {
                                        println!("      UNKNOWN CMD: {}", cmd);
                                    }
                                }
                            }
                        }
                        _ => {
                            println!("    Unknown feature field {}, wire {}", f_f, f_w);
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
