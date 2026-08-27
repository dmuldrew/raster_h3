use std::io::Read;
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Fetch header from kpop.pmtiles using curl via command line or std::process::Command
    let output = std::process::Command::new("curl")
        .args(&["-s", "-r", "0-16383", "https://data.source.coop/smartmaps/foil4gr1/kpop.pmtiles"])
        .output()?;

    let bytes = output.stdout;
    println!("Fetched {} bytes from kpop.pmtiles", bytes.len());
    if bytes.len() < 127 {
        println!("Error: response too short");
        return Ok(());
    }

    let header = &bytes[0..127];
    println!("Magic: {:?}", std::str::from_utf8(&header[0..7]));
    println!("Version: {}", header[7]);
    
    let root_offset = u64::from_le_bytes(header[8..16].try_into()?);
    let root_length = u64::from_le_bytes(header[16..24].try_into()?);
    let json_offset = u64::from_le_bytes(header[24..32].try_into()?);
    let json_length = u64::from_le_bytes(header[32..40].try_into()?);
    let leaf_offset = u64::from_le_bytes(header[40..48].try_into()?);
    let leaf_length = u64::from_le_bytes(header[48..56].try_into()?);
    let tile_data_offset = u64::from_le_bytes(header[56..64].try_into()?);
    let tile_data_length = u64::from_le_bytes(header[64..72].try_into()?);
    let num_addressed_tiles = u64::from_le_bytes(header[72..80].try_into()?);
    let num_tile_entries = u64::from_le_bytes(header[80..88].try_into()?);
    let num_tile_contents = u64::from_le_bytes(header[88..96].try_into()?);
    let clustered = header[96] != 0;
    let internal_compression = header[97];
    let tile_compression = header[98];
    let tile_type = header[99];
    let min_zoom = header[100];
    let max_zoom = header[101];

    let min_lon = i32::from_le_bytes(header[102..106].try_into()?) as f64 / 1e7;
    let min_lat = i32::from_le_bytes(header[106..110].try_into()?) as f64 / 1e7;
    let max_lon = i32::from_le_bytes(header[110..114].try_into()?) as f64 / 1e7;
    let max_lat = i32::from_le_bytes(header[114..118].try_into()?) as f64 / 1e7;
    let center_zoom = header[118];
    let center_lon = i32::from_le_bytes(header[119..123].try_into()?) as f64 / 1e7;
    let center_lat = i32::from_le_bytes(header[123..127].try_into()?) as f64 / 1e7;

    println!("kpop.pmtiles Header info:");
    println!("  root_offset: {}, root_length: {}", root_offset, root_length);
    println!("  json_offset: {}, json_length: {}", json_offset, json_length);
    println!("  leaf_offset: {}, leaf_length: {}", leaf_offset, leaf_length);
    println!("  tile_data_offset: {}, tile_data_length: {}", tile_data_offset, tile_data_length);
    println!("  min_zoom: {}, max_zoom: {}", min_zoom, max_zoom);
    println!("  bounds: [{}, {}, {}, {}]", min_lon, min_lat, max_lon, max_lat);
    println!("  center: ({}, {}), zoom: {}", center_lon, center_lat, center_zoom);
    println!("  num_tile_entries: {}, num_addressed_tiles: {}, num_tile_contents: {}", num_tile_entries, num_addressed_tiles, num_tile_contents);
    println!("  clustered: {}", clustered);
    println!("  internal_compression: {}", internal_compression);
    println!("  tile_compression: {}", tile_compression);
    println!("  tile_type: {}", tile_type);

    // Read metadata
    let meta_bytes = &bytes[(json_offset as usize)..((json_offset + json_length) as usize)];
    let meta_str = if internal_compression == 2 {
        let mut gz = GzDecoder::new(meta_bytes);
        let mut s = String::new();
        gz.read_to_string(&mut s)?;
        s
    } else {
        String::from_utf8_lossy(meta_bytes).to_string()
    };
    println!("\nkpop.pmtiles Metadata JSON:\n{}", meta_str);

    // Read root dir
    let dir_bytes = &bytes[(root_offset as usize)..((root_offset + root_length) as usize)];
    let mut gz_dir = GzDecoder::new(dir_bytes);
    let mut decomp_dir = Vec::new();
    gz_dir.read_to_end(&mut decomp_dir)?;

    let mut pos = 0;
    let num_entries = read_varint(&decomp_dir, &mut pos) as usize;
    println!("\nkpop.pmtiles root dir entries: {}", num_entries);

    Ok(())
}
