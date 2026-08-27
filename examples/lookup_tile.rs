use std::fs::File;
use std::io::Read;
use flate2::read::GzDecoder;
use raster_h3::pmtiles::writer::zxy_to_tile_id;

fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut result = 0u64;
    let mut shift = 0;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 { break; }
        shift += 7;
    }
    result
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open("data/CFL_HI_pyramid.pmtiles")?;
    let mut header = [0u8; 127];
    file.read_exact(&mut header)?;

    let root_length = u64::from_le_bytes(header[16..24].try_into()?);
    let tile_data_offset = u64::from_le_bytes(header[56..64].try_into()?);

    let target_tile_id = zxy_to_tile_id(7, 7, 55);

    let mut dir_compressed = vec![0u8; root_length as usize];
    file.read_exact(&mut dir_compressed)?;
    let mut gz = GzDecoder::new(&dir_compressed[..]);
    let mut dir_bytes = Vec::new();
    gz.read_to_end(&mut dir_bytes)?;

    let mut pos = 0;
    let num_entries = read_varint(&dir_bytes, &mut pos) as usize;

    let mut tile_ids = Vec::with_capacity(num_entries);
    let mut last_id = 0u64;
    for _ in 0..num_entries {
        let val = read_varint(&dir_bytes, &mut pos);
        last_id += val;
        tile_ids.push(last_id);
    }
    let mut lengths = Vec::with_capacity(num_entries);
    for _ in 0..num_entries { lengths.push(0u32); } // skip run_lengths
    for i in 0..num_entries {
        lengths[i] = read_varint(&dir_bytes, &mut pos) as u32;
    }
    for i in 0..num_entries {
        lengths[i] = read_varint(&dir_bytes, &mut pos) as u32;
    }
    let mut offsets = Vec::with_capacity(num_entries);
    let mut last_offset = 0u64;
    for i in 0..num_entries {
        let val = read_varint(&dir_bytes, &mut pos);
        if val == 0 && i > 0 {
            last_offset += lengths[i - 1] as u64;
        } else {
            last_offset = val.saturating_sub(1);
        }
        offsets.push(last_offset);
    }

    let idx = tile_ids.binary_search(&target_tile_id).unwrap();
    let absolute_offset = tile_data_offset + offsets[idx];

    use std::io::Seek;
    use std::io::SeekFrom;
    file.seek(SeekFrom::Start(absolute_offset))?;
    let mut tile_compressed = vec![0u8; lengths[idx] as usize];
    file.read_exact(&mut tile_compressed)?;

    let mut gz = GzDecoder::new(&tile_compressed[..]);
    let mut tile_data = Vec::new();
    gz.read_to_end(&mut tile_data)?;

    let mut p = 0;
    while p < tile_data.len() {
        let tag = read_varint(&tile_data, &mut p);
        let wire = tag & 7;
        if wire == 2 {
            let len = read_varint(&tile_data, &mut p) as usize;
            let layer_bytes = &tile_data[p..p + len];
            p += len;
            decode_layer_properties(layer_bytes);
        }
    }

    Ok(())
}

fn decode_layer_properties(buf: &[u8]) {
    let mut p = 0;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut raw_features = Vec::new();

    while p < buf.len() {
        let tag = read_varint(buf, &mut p);
        let field = tag >> 3;
        match field {
            3 => {
                let len = read_varint(buf, &mut p) as usize;
                keys.push(String::from_utf8_lossy(&buf[p..p + len]).to_string());
                p += len;
            }
            4 => {
                let len = read_varint(buf, &mut p) as usize;
                let val_bytes = &buf[p..p + len];
                p += len;
                values.push(decode_value(val_bytes));
            }
            2 => {
                let len = read_varint(buf, &mut p) as usize;
                raw_features.push(&buf[p..p + len]);
                p += len;
            }
            15 | 5 => {
                let _ = read_varint(buf, &mut p);
            }
            1 => {
                let len = read_varint(buf, &mut p) as usize;
                p += len;
            }
            _ => break,
        }
    }

    println!("Keys: {:?}", keys);
    println!("Values ({}): {:?}", values.len(), values);

    for (i, f_bytes) in raw_features.iter().enumerate() {
        let mut fp = 0;
        let mut f_tags = Vec::new();
        while fp < f_bytes.len() {
            let ftag = read_varint(f_bytes, &mut fp);
            let ffield = ftag >> 3;
            match ffield {
                2 => {
                    let tlen = read_varint(f_bytes, &mut fp) as usize;
                    let tend = fp + tlen;
                    while fp < tend {
                        f_tags.push(read_varint(f_bytes, &mut fp) as usize);
                    }
                }
                1 | 3 => { let _ = read_varint(f_bytes, &mut fp); }
                4 => {
                    let glen = read_varint(f_bytes, &mut fp) as usize;
                    fp += glen;
                }
                _ => break,
            }
        }
        println!("\nFeature #{}:", i + 1);
        for pair in f_tags.chunks(2) {
            if pair.len() == 2 {
                let k = &keys[pair[0]];
                let v = &values[pair[1]];
                println!("  {}: {}", k, v);
            }
        }
    }
}

fn decode_value(buf: &[u8]) -> String {
    let mut p = 0;
    while p < buf.len() {
        let tag = read_varint(buf, &mut p);
        let field = tag >> 3;
        match field {
            1 => {
                let len = read_varint(buf, &mut p) as usize;
                return format!("\"{}\"", String::from_utf8_lossy(&buf[p..p + len]));
            }
            2 => {
                let f = f32::from_le_bytes(buf[p..p + 4].try_into().unwrap());
                p += 4;
                return format!("{}", f);
            }
            3 => {
                let d = f64::from_le_bytes(buf[p..p + 8].try_into().unwrap());
                p += 8;
                return format!("{}", d);
            }
            4 => {
                let i = read_varint(buf, &mut p) as i64;
                return format!("{}", i);
            }
            5 => {
                let u = read_varint(buf, &mut p);
                return format!("{}", u);
            }
            7 => {
                let b = read_varint(buf, &mut p) != 0;
                return format!("{}", b);
            }
            _ => break,
        }
    }
    "null".to_string()
}
