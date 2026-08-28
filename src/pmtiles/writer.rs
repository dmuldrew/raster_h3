//! PMTiles v3 Single-File Archive Writer
//!
//! Implements the open PMTiles v3 specification for cloud-native single-file
//! vector tile archives with Gzip compression and Hilbert-indexed directory structure.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use flate2::write::GzEncoder;
use flate2::Compression;

/// PMTiles v3 Constants
const PMTILES_HEADER_SIZE: usize = 127;
const COMPRESSION_GZIP: u8 = 2;
const TILE_TYPE_MVT: u8 = 1;

/// Encode a 64-bit unsigned integer as a Protobuf-style varint
#[inline(always)]
fn write_varint(buf: &mut Vec<u8>, mut val: u64) {
    while val >= 0x80 {
        buf.push(((val & 0x7F) | 0x80) as u8);
        val >>= 7;
    }
    buf.push((val & 0x7F) as u8);
}

/// Compute the canonical PMTiles v3 Tile ID for (z, x, y)
pub fn zxy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 {
        return 0;
    }
    let acc = ((1u64 << (2 * z)) - 1) / 3;
    let mut tx = x as u64;
    let mut ty = y as u64;
    let mut d = 0u64;
    let mut s = 1u64 << (z - 1);
    while s > 0 {
        let rx = if (tx & s) > 0 { 1 } else { 0 };
        let ry = if (ty & s) > 0 { 1 } else { 0 };
        d += s * s * ((3 * rx) ^ ry);
        let mut lx = tx & (s - 1);
        let mut ly = ty & (s - 1);
        if ry == 0 {
            if rx == 1 {
                lx = s - 1 - lx;
                ly = s - 1 - ly;
            }
            let t = lx;
            lx = ly;
            ly = t;
        }
        tx = lx;
        ty = ly;
        s >>= 1;
    }
    acc + d
}

fn tile_id_to_z(i: u64) -> u8 {
    let c = 3 * i + 1;
    let leading = c.leading_zeros();
    ((63 - leading) / 2) as u8
}

/// Convert a Hilbert TileID to (z, x, y)
pub fn tile_id_to_zxy(i: u64) -> (u8, u32, u32) {
    if i == 0 {
        return (0, 0, 0);
    }
    let z = tile_id_to_z(i);
    let acc = ((1u64 << (2 * z)) - 1) / 3;
    let mut t = i - acc;
    let mut x = 0u64;
    let mut y = 0u64;
    let mut s = 1u64;
    while s < (1u64 << z) {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            let temp = x;
            x = y;
            y = temp;
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s <<= 1;
    }
    (z, x as u32, y as u32)
}

/// A raw tile payload ready to be written
#[derive(Debug, Clone)]
pub struct TilePayload {
    pub tile_id: u64,
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub data: Vec<u8>, // Gzip-compressed MVT data
}

/// Gzip compress a byte slice
pub fn gzip_compress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

/// PMTiles v3 Directory Entry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

/// Encode a slice of Directory Entries using PMTiles v3 varint delta compression
fn encode_directory(entries: &[Entry]) -> io::Result<Vec<u8>> {
    let mut uncompressed = Vec::new();
    write_varint(&mut uncompressed, entries.len() as u64);

    // 1. Tile ID deltas
    let mut last_id = 0u64;
    for e in entries {
        write_varint(&mut uncompressed, e.tile_id - last_id);
        last_id = e.tile_id;
    }

    // 2. Run lengths
    for e in entries {
        write_varint(&mut uncompressed, e.run_length as u64);
    }

    // 3. Lengths
    for e in entries {
        write_varint(&mut uncompressed, e.length as u64);
    }

    // 4. Offsets: 0 = contiguous with previous, else (offset + 1) to avoid
    //    conflicting with the 0 sentinel. Matches go-pmtiles SerializeEntries.
    for (i, e) in entries.iter().enumerate() {
        if i > 0
            && e.offset
                == entries[i - 1].offset + (entries[i - 1].length as u64)
        {
            write_varint(&mut uncompressed, 0);
        } else {
            write_varint(&mut uncompressed, e.offset + 1);
        }
    }

    gzip_compress(&uncompressed)
}

/// PMTiles v3 Archive Builder
pub struct PmtilesWriter {
    tiles: Vec<TilePayload>,
    min_zoom: u8,
    max_zoom: u8,
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
    metadata_json: String,
}

impl PmtilesWriter {
    /// Create a new PMTiles v3 archive builder
    pub fn new(min_zoom: u8, max_zoom: u8, bbox: [f64; 4], metadata_json: String) -> Self {
        Self {
            tiles: Vec::new(),
            min_zoom,
            max_zoom,
            min_lon: bbox[0],
            min_lat: bbox[1],
            max_lon: bbox[2],
            max_lat: bbox[3],
            metadata_json,
        }
    }

    /// Add a tile payload (automatically gzip compresses uncompressed MVT bytes)
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, uncompressed_mvt: &[u8]) -> io::Result<()> {
        let compressed = gzip_compress(uncompressed_mvt)?;
        let tile_id = zxy_to_tile_id(z, x, y);
        self.tiles.push(TilePayload {
            tile_id,
            z,
            x,
            y,
            data: compressed,
        });
        Ok(())
    }

    /// Finalize and write the complete PMTiles v3 single-file archive to disk
    pub fn finish<P: AsRef<Path>>(mut self, path: P) -> io::Result<()> {
        // Sort tiles by canonical Hilbert Tile ID
        self.tiles.sort_by_key(|t| t.tile_id);

        let mut entries: Vec<Entry> = Vec::with_capacity(self.tiles.len());
        let mut current_offset = 0u64;

        for t in &self.tiles {
            entries.push(Entry {
                tile_id: t.tile_id,
                offset: current_offset,
                length: t.data.len() as u32,
                run_length: 1,
            });
            current_offset += t.data.len() as u64;
        }

        // Compress root directory
        let root_dir_bytes = encode_directory(&entries)?;
        // Compress JSON metadata
        let metadata_bytes = gzip_compress(self.metadata_json.as_bytes())?;

        // Calculate layout offsets
        let root_dir_offset = PMTILES_HEADER_SIZE as u64;
        let root_dir_length = root_dir_bytes.len() as u64;

        let json_metadata_offset = root_dir_offset + root_dir_length;
        let json_metadata_length = metadata_bytes.len() as u64;

        // Even with no leaf directories, the offset must be a valid non-zero
        // position (right after metadata) per the PMTiles v3 spec.
        // See: go-pmtiles/examples/minimal.go
        let leaf_dirs_offset = json_metadata_offset + json_metadata_length;
        let leaf_dirs_length = 0u64;

        let tile_data_offset = leaf_dirs_offset;
        let tile_data_length = current_offset;

        let addressed_tiles_count = self.tiles.len() as u64;
        let tile_entries_count = entries.len() as u64;
        let tile_contents_count = self.tiles.len() as u64;

        let center_zoom = (self.min_zoom + self.max_zoom) / 2;
        let center_lon = (self.min_lon + self.max_lon) / 2.0;
        let center_lat = (self.min_lat + self.max_lat) / 2.0;

        // Build 127-byte PMTiles v3 header
        let mut header = Vec::with_capacity(PMTILES_HEADER_SIZE);
        header.extend_from_slice(b"PMTiles");
        header.push(3); // Version 3

        header.extend_from_slice(&root_dir_offset.to_le_bytes());
        header.extend_from_slice(&root_dir_length.to_le_bytes());
        header.extend_from_slice(&json_metadata_offset.to_le_bytes());
        header.extend_from_slice(&json_metadata_length.to_le_bytes());
        header.extend_from_slice(&leaf_dirs_offset.to_le_bytes());
        header.extend_from_slice(&leaf_dirs_length.to_le_bytes());
        header.extend_from_slice(&tile_data_offset.to_le_bytes());
        header.extend_from_slice(&tile_data_length.to_le_bytes());
        header.extend_from_slice(&addressed_tiles_count.to_le_bytes());
        header.extend_from_slice(&tile_entries_count.to_le_bytes());
        header.extend_from_slice(&tile_contents_count.to_le_bytes());

        header.push(1); // Clustered = true
        header.push(COMPRESSION_GZIP); // Internal compression = Gzip
        header.push(COMPRESSION_GZIP); // Tile compression = Gzip
        header.push(TILE_TYPE_MVT); // Tile type = MVT

        header.push(self.min_zoom);
        header.push(self.max_zoom);

        header.extend_from_slice(&((self.min_lon * 1e7) as i32).to_le_bytes());
        header.extend_from_slice(&((self.min_lat * 1e7) as i32).to_le_bytes());
        header.extend_from_slice(&((self.max_lon * 1e7) as i32).to_le_bytes());
        header.extend_from_slice(&((self.max_lat * 1e7) as i32).to_le_bytes());

        header.push(center_zoom);
        header.extend_from_slice(&((center_lon * 1e7) as i32).to_le_bytes());
        header.extend_from_slice(&((center_lat * 1e7) as i32).to_le_bytes());

        assert_eq!(header.len(), PMTILES_HEADER_SIZE);

        // Write complete archive
        let mut file = File::create(path)?;
        file.write_all(&header)?;
        file.write_all(&root_dir_bytes)?;
        file.write_all(&metadata_bytes)?;

        for t in self.tiles {
            file.write_all(&t.data)?;
        }

        file.flush()?;
        Ok(())
    }
}
