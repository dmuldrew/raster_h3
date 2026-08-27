use std::fs::File;
use std::io::Read;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open("data/CFL_HI_pyramid.pmtiles")?;
    let mut header = [0u8; 127];
    file.read_exact(&mut header)?;

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

    println!("Header info:");
    println!("  root_offset: {}, root_length: {}", root_offset, root_length);
    println!("  json_offset: {}, json_length: {}", json_offset, json_length);
    println!("  leaf_offset: {}, leaf_length: {}", leaf_offset, leaf_length);
    println!("  tile_data_offset: {}, tile_data_length: {}", tile_data_offset, tile_data_length);
    println!("  min_zoom: {}, max_zoom: {}", min_zoom, max_zoom);
    println!("  bounds: [{}, {}, {}, {}]", min_lon, min_lat, max_lon, max_lat);
    println!("  center: ({}, {}), zoom: {}", center_lon, center_lat, center_zoom);
    println!("  num_tile_entries: {}, num_addressed_tiles: {}, num_tile_contents: {}", num_tile_entries, num_addressed_tiles, num_tile_contents);
    println!("  tile_compression: {} (0=none, 1=unknown, 2=gzip)", tile_compression);
    println!("  tile_type: {} (1=mvt)", tile_type);

    // Read metadata JSON
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(json_offset))?;
    let mut json_buf = vec![0u8; json_length as usize];
    file.read_exact(&mut json_buf)?;

    // Decompress if gzip
    let json_str = if internal_compression == 2 {
        use flate2::read::GzDecoder;
        let mut gz = GzDecoder::new(&json_buf[..]);
        let mut s = String::new();
        gz.read_to_string(&mut s)?;
        s
    } else {
        String::from_utf8_lossy(&json_buf).to_string()
    };
    println!("Metadata JSON:\n{}", json_str);

    // Read root directory entries
    file.seek(std::io::SeekFrom::Start(root_offset))?;
    let mut dir_buf = vec![0u8; root_length as usize];
    file.read_exact(&mut dir_buf)?;

    let dir_bytes = if internal_compression == 2 {
        use flate2::read::GzDecoder;
        let mut gz = GzDecoder::new(&dir_buf[..]);
        let mut decompressed = Vec::new();
        gz.read_to_end(&mut decompressed)?;
        decompressed
    } else {
        dir_buf
    };

    println!("Root dir decompressed bytes: {}", dir_bytes.len());

    Ok(())
}
