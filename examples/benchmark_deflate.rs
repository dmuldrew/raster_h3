//! Benchmarks SIMD-accelerated `libdeflater` versus standard `flate2` decompression on synthetic tiled GeoTIFFs.
//!
//! Evaluates chunk decompression throughput and latency between scalar and SIMD paths alongside H3 aggregation.
//!
//! Run with: `cargo run --example benchmark_deflate`

use std::fs::File;
use std::io::{BufWriter, Cursor, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;
use tempfile::NamedTempFile;
use tiff::decoder::{Decoder, DecodingResult};

use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn write_benchmark_deflate_geotiff(path: &Path, width: u32, height: u32, tile_w: u32, tile_h: u32) {
    let mut file = BufWriter::new(File::create(path).unwrap());
    file.write_all(b"II\x2a\x00\x08\x00\x00\x00").unwrap();

    let tiles_across = (width + tile_w - 1) / tile_w;
    let tiles_down = (height + tile_h - 1) / tile_h;
    let total_tiles = tiles_across * tiles_down;

    let tile_data_start = 65536u64;
    file.seek(SeekFrom::Start(tile_data_start)).unwrap();

    let mut tile_offsets = Vec::with_capacity(total_tiles as usize);
    let mut tile_byte_counts = Vec::with_capacity(total_tiles as usize);
    let mut compressor = libdeflater::Compressor::new(libdeflater::CompressionLvl::default());

    for tile_idx in 0..total_tiles {
        let tc = tile_idx % tiles_across;
        let tr = tile_idx / tiles_across;
        let x0 = tc * tile_w;
        let y0 = tr * tile_h;

        let mut raw_bytes = vec![0u8; (tile_w * tile_h * 4) as usize];
        for r in 0..tile_h {
            for c in 0..tile_w {
                let img_x = x0 + c;
                let img_y = y0 + r;
                let val = ((img_x * 13 + img_y * 29) % 1000) as f32 * 0.1;
                let offset = ((r * tile_w + c) * 4) as usize;
                raw_bytes[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
            }
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
    let scale: [f64; 3] = [0.0001, 0.0001, 0.0];
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
    write_tag(&mut file, 258, 3, 1, 32);
    write_tag(&mut file, 259, 3, 1, 8); // Deflate
    write_tag(&mut file, 262, 3, 1, 1);
    write_tag(&mut file, 277, 3, 1, 1);
    write_tag(&mut file, 317, 3, 1, 1); // Predictor::None
    write_tag(&mut file, 322, 4, 1, tile_w);
    write_tag(&mut file, 323, 4, 1, tile_h);
    write_tag(&mut file, 324, 4, total_tiles, tile_offsets_pos as u32);
    write_tag(&mut file, 325, 4, total_tiles, tile_byte_counts_pos as u32);
    write_tag(&mut file, 339, 3, 1, 3); // IEEEFP
    write_tag(&mut file, 33550, 12, 3, scale_offset as u32);
    write_tag(&mut file, 33922, 12, 6, tiepoint_offset as u32);

    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.flush().unwrap();
}

fn main() {
    println!(
        "========================================================================================="
    );
    println!(
        "       raster_h3 Benchmark: SIMD-Accelerated Chunk Decompression (libdeflater)           "
    );
    println!(
        "========================================================================================="
    );

    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    let width = 4096u32;
    let height = 4096u32;
    let tile_w = 128u32;
    let tile_h = 128u32;
    let total_pixels = (width * height) as f64;

    println!("\n▶ Generating synthetic tiled Deflate GeoTIFF...");
    println!(
        "  • Raster Size     : {} x {} ({:.2} million pixels)",
        width,
        height,
        total_pixels / 1_000_000.0
    );
    println!(
        "  • Tile Dimensions : {} x {} ({} tiles)",
        tile_w,
        tile_h,
        (width / tile_w) * (height / tile_h)
    );
    println!("  • Compression     : Deflate / zlib");
    println!("  • Data Type       : Float32 (4 bytes/sample)");

    let t_gen = Instant::now();
    write_benchmark_deflate_geotiff(path, width, height, tile_w, tile_h);
    println!("  • Generation Done : {:.2?}", t_gen.elapsed());

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open test raster");
    let total_chunks = reader.chunk_layout.total_chunks;
    let mmap = reader.mmap().expect("Failed to get mmap");

    let uncompressed_tile_bytes = (tile_w * tile_h * 4) as usize;
    let total_uncompressed_mb =
        (total_chunks as usize * uncompressed_tile_bytes) as f64 / (1024.0 * 1024.0);

    // =========================================================================
    // BENCHMARK 1: Standard tiff crate decoding (flate2 / miniz_oxide)
    // =========================================================================
    println!("\n▶ [BENCHMARK 1] Standard tiff Decompression (flate2 / miniz_oxide scalar)");
    let t0 = Instant::now();
    let mut std_decoder = Decoder::new(Cursor::new(&mmap[..])).unwrap();
    let mut std_samples_read = 0usize;

    for chunk_idx in 0..total_chunks {
        let chunk_data = std_decoder.read_chunk(chunk_idx).unwrap();
        if let DecodingResult::F32(v) = chunk_data {
            std_samples_read += v.len();
        }
    }
    let dur_std = t0.elapsed();
    let std_bandwidth = total_uncompressed_mb / dur_std.as_secs_f64();
    let std_latency_per_tile = dur_std.as_micros() as f64 / total_chunks as f64;

    println!("  • Total Time      : {:.2?}", dur_std);
    println!("  • Decomp Bandwidth: {:.2} MB/sec", std_bandwidth);
    println!(
        "  • Average Latency : {:.2} µs / tile",
        std_latency_per_tile
    );

    // =========================================================================
    // BENCHMARK 2: SIMD libdeflater Chunk Decompression
    // =========================================================================
    println!("\n▶ [BENCHMARK 2] SIMD-Accelerated Decompression (libdeflater AVX2/NEON)");
    let t1 = Instant::now();
    let mut simd_decoder = reader.open_decoder().unwrap();
    let mut simd_samples_read = 0usize;

    for chunk_idx in 0..total_chunks {
        let (_bounds, chunk_data) = simd_decoder.read_chunk(chunk_idx).unwrap();
        if let DecodingResult::F32(v) = chunk_data {
            simd_samples_read += v.len();
        }
    }
    let dur_simd = t1.elapsed();
    let simd_bandwidth = total_uncompressed_mb / dur_simd.as_secs_f64();
    let simd_latency_per_tile = dur_simd.as_micros() as f64 / total_chunks as f64;

    println!("  • Total Time      : {:.2?}", dur_simd);
    println!("  • Decomp Bandwidth: {:.2} MB/sec", simd_bandwidth);
    println!(
        "  • Average Latency : {:.2} µs / tile",
        simd_latency_per_tile
    );

    assert_eq!(std_samples_read, simd_samples_read);
    let speedup = dur_std.as_secs_f64() / dur_simd.as_secs_f64();
    println!(
        "\n  ⚡ Pure Decompression Speedup: {:.2}x faster with libdeflater!",
        speedup
    );

    // =========================================================================
    // BENCHMARK 3: End-to-End Multi-Core Rayon Streaming Aggregation
    // =========================================================================
    println!("\n▶ [BENCHMARK 3] End-to-End Multi-Core Rayon Streaming Pipeline (H3 Res 9)");
    let config = MultiResolutionConfig::new(vec![9]);
    let t_stream = Instant::now();
    let reader_full = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiScanHorizonStreamer::new(reader_full, &config).unwrap();
    let mut total_hexes = 0usize;
    let mut total_pixels_streamed = 0.0f64;

    loop {
        let n = streamer.drain_completed_into(2048, |_i, rec| {
            total_hexes += 1;
            total_pixels_streamed += rec.accumulator.count;
        });
        if n == 0 {
            break;
        }
    }
    let dur_stream = t_stream.elapsed();
    let mpx_sec = (total_pixels_streamed / 1_000_000.0) / dur_stream.as_secs_f64();

    println!("  • Hexagons Produced : {} cells", total_hexes);
    println!(
        "  • Pixels Ingested   : {:.0} pixels",
        total_pixels_streamed
    );
    println!("  • Streaming Time    : {:.2?}", dur_stream);
    println!("  • Sustained Stream  : {:.2} Mpx/sec", mpx_sec);

    println!("\n=========================================================================================");
    println!(
        "                          DEFLATE BENCHMARK COMPLETED SUCCESSFULLY                       "
    );
    println!(
        "========================================================================================="
    );
}
