//! PMTiles v3 Single-File Archive Writer
//!
//! Implements the open PMTiles v3 specification for cloud-native single-file
//! vector tile archives with Gzip compression and Hilbert-indexed directory structure.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use libdeflater::{CompressionLvl, Compressor};

thread_local! {
    static THREAD_COMPRESSOR: RefCell<Compressor> = RefCell::new(
        Compressor::new(CompressionLvl::new(3).unwrap())
    );
}

/// PMTiles v3 Constants
const PMTILES_HEADER_SIZE: usize = 127;
const COMPRESSION_GZIP: u8 = 2;
const TILE_TYPE_MVT: u8 = 1;

/// Encode a 64-bit unsigned integer as a Protobuf-style varint
#[inline(always)]
fn write_varint(buf: &mut Vec<u8>, mut val: u64) {
    if val < 0x80 {
        buf.push(val as u8);
        return;
    }
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

/// Gzip compress a byte slice with fast level 3 compression using a thread-local persistent libdeflater compressor
pub fn gzip_compress(data: &[u8]) -> io::Result<Vec<u8>> {
    THREAD_COMPRESSOR.with(|compressor_cell| {
        let mut compressor = compressor_cell.borrow_mut();
        let max_len = compressor.gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; max_len];
        let actual_size = compressor
            .gzip_compress(data, &mut compressed)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("gzip compression error: {:?}", e)))?;
        compressed.truncate(actual_size);
        Ok(compressed)
    })
}

/// PMTiles v3 Directory Entry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

/// Encode a slice of Directory Entries using PMTiles v3 varint delta compression
fn encode_directory(entries: &[Entry]) -> io::Result<Vec<u8>> {
    let mut uncompressed = Vec::with_capacity(entries.len() * 8);
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

/// Partition directory entries into PMTiles v3 Leaf Directories and construct root pointers
pub fn build_leaf_directories(entries: &[Entry], leaf_size: usize) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut root_pointers = Vec::new();
    let mut all_leaf_bytes = Vec::new();
    let mut current_leaf_offset = 0u64;

    for chunk in entries.chunks(leaf_size) {
        let leaf_bytes = encode_directory(chunk)?;
        let leaf_len = leaf_bytes.len() as u32;
        let first_tile_id = chunk[0].tile_id;

        root_pointers.push(Entry {
            tile_id: first_tile_id,
            offset: current_leaf_offset,
            length: leaf_len,
            run_length: 0, // 0 signifies a pointer to a Leaf Directory in PMTiles v3
        });

        current_leaf_offset += leaf_len as u64;
        all_leaf_bytes.extend_from_slice(&leaf_bytes);
    }

    let root_bytes = encode_directory(&root_pointers)?;
    Ok((root_bytes, all_leaf_bytes))
}

#[derive(Debug, Clone, Copy)]
struct SpillTileEntry {
    tile_id: u64,
    spill_offset: u64,
    length: u32,
}

/// PMTiles v3 Streaming Archive Builder with Disk-Spill Storage
pub struct PmtilesWriter {
    spill_file: File,
    entries: Vec<SpillTileEntry>,
    current_spill_offset: u64,
    min_zoom: u8,
    max_zoom: u8,
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
    metadata_json: String,
}

impl PmtilesWriter {
    /// Create a new PMTiles v3 streaming archive builder with disk spill storage
    pub fn new(min_zoom: u8, max_zoom: u8, bbox: [f64; 4], metadata_json: String) -> io::Result<Self> {
        let spill_file = tempfile::tempfile()?;
        Ok(Self {
            spill_file,
            entries: Vec::new(),
            current_spill_offset: 0,
            min_zoom,
            max_zoom,
            min_lon: bbox[0],
            min_lat: bbox[1],
            max_lon: bbox[2],
            max_lat: bbox[3],
            metadata_json,
        })
    }

    /// Update bounding box and metadata JSON before finalizing
    pub fn set_metadata(&mut self, bbox: [f64; 4], metadata_json: String) {
        self.min_lon = bbox[0];
        self.min_lat = bbox[1];
        self.max_lon = bbox[2];
        self.max_lat = bbox[3];
        self.metadata_json = metadata_json;
    }

    /// Add a pre-compressed tile payload directly into the spill file
    pub fn add_compressed_tile(&mut self, z: u8, x: u32, y: u32, compressed_data: &[u8]) -> io::Result<()> {
        let len = compressed_data.len() as u32;
        let tile_id = zxy_to_tile_id(z, x, y);
        self.spill_file.write_all(compressed_data)?;
        self.entries.push(SpillTileEntry {
            tile_id,
            spill_offset: self.current_spill_offset,
            length: len,
        });
        self.current_spill_offset += len as u64;
        Ok(())
    }

    /// Add a tile payload (automatically gzip compresses uncompressed MVT bytes and spills to disk)
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, uncompressed_mvt: &[u8]) -> io::Result<()> {
        let compressed = gzip_compress(uncompressed_mvt)?;
        self.add_compressed_tile(z, x, y, &compressed)
    }

    /// Return number of tiles currently written to disk spill storage
    pub fn tile_count(&self) -> usize {
        self.entries.len()
    }

    /// Finalize and write the complete PMTiles v3 single-file archive to disk
    pub fn finish<P: AsRef<Path>>(mut self, path: P) -> io::Result<()> {
        self.spill_file.flush()?;

        // Sort entries by canonical Hilbert Tile ID
        self.entries.sort_by_key(|e| e.tile_id);

        let mut directory_entries: Vec<Entry> = Vec::with_capacity(self.entries.len());
        let mut final_tile_offset = 0u64;

        for e in &self.entries {
            directory_entries.push(Entry {
                tile_id: e.tile_id,
                offset: final_tile_offset,
                length: e.length,
                run_length: 1,
            });
            final_tile_offset += e.length as u64;
        }

        // PMTiles v3 Specification requires the root directory to fit in the initial 16KB fetch
        // (16,384 bytes - 127 bytes header = 16,257 bytes max root).
        // For larger archives, split directory into leaf directories of 4,096 entries.
        const MAX_ENTRIES_PER_LEAF: usize = 4096;

        let (root_dir_bytes, leaf_dirs_bytes) = if directory_entries.len() <= MAX_ENTRIES_PER_LEAF {
            let root_bytes = encode_directory(&directory_entries)?;
            if root_bytes.len() <= 16257 {
                (root_bytes, Vec::new())
            } else {
                build_leaf_directories(&directory_entries, MAX_ENTRIES_PER_LEAF)?
            }
        } else {
            build_leaf_directories(&directory_entries, MAX_ENTRIES_PER_LEAF)?
        };

        // Compress JSON metadata
        let metadata_bytes = gzip_compress(self.metadata_json.as_bytes())?;

        // Calculate layout offsets
        let root_dir_offset = PMTILES_HEADER_SIZE as u64;
        let root_dir_length = root_dir_bytes.len() as u64;

        let json_metadata_offset = root_dir_offset + root_dir_length;
        let json_metadata_length = metadata_bytes.len() as u64;

        let leaf_dirs_offset = json_metadata_offset + json_metadata_length;
        let leaf_dirs_length = leaf_dirs_bytes.len() as u64;

        let tile_data_offset = leaf_dirs_offset + leaf_dirs_length;
        let tile_data_length = final_tile_offset;

        let addressed_tiles_count = self.entries.len() as u64;
        let tile_entries_count = directory_entries.len() as u64;
        let tile_contents_count = self.entries.len() as u64;

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

        // Write complete archive
        let file = File::create(path)?;
        let mut writer = io::BufWriter::with_capacity(1024 * 1024, file);
        writer.write_all(&header)?;
        writer.write_all(&root_dir_bytes)?;
        writer.write_all(&metadata_bytes)?;
        writer.write_all(&leaf_dirs_bytes)?;

        // Stream tile data from spill_file to out_file in sorted Hilbert order via zero-copy memory map
        self.spill_file.flush()?;
        if self.current_spill_offset > 0 {
            let mmap = unsafe { memmap2::Mmap::map(&self.spill_file)? };
            for e in &self.entries {
                let start = e.spill_offset as usize;
                let end = start + e.length as usize;
                if end > mmap.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "Spill file truncated: tile offset {}+{} exceeds spill file size {}",
                            start, e.length, mmap.len()
                        ),
                    ));
                }
                writer.write_all(&mmap[start..end])?;
            }
        }

        writer.flush()?;
        Ok(())
    }
}
