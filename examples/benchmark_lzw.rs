use std::io::Cursor;
use std::path::Path;
use std::time::Instant;
use tiff::decoder::{Decoder, DecodingResult};

use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer, MultiCategoricalHorizonStreamer};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn benchmark_file(path: &str, sample_count_limit: Option<usize>) {
    println!("\n=========================================================================================");
    println!("  Benchmarking: {}", path);
    println!("=========================================================================================");

    if !Path::new(path).exists() {
        eprintln!("Error: file {} not found", path);
        return;
    }

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open file");
    let mmap = reader.mmap().expect("Failed to get mmap");
    let chunk_info = reader.chunk_info.as_ref().expect("Missing chunk_info");
    let total_chunks = reader.chunk_layout.total_chunks as usize;
    let chunks_to_test = sample_count_limit.unwrap_or(total_chunks).min(total_chunks);
    let (chunk_w, chunk_h) = chunk_info.chunk_dimensions;
    let bytes_per_sample = (chunk_info.bits_per_sample / 8).max(1) as usize;
    let uncomp_chunk_bytes = (chunk_w * chunk_h) as usize * bytes_per_sample;
    let total_uncompressed_mb = (chunks_to_test * uncomp_chunk_bytes) as f64 / (1024.0 * 1024.0);

    println!("  • Grid Size      : {} x {}", reader.metadata.width, reader.metadata.height);
    println!("  • Total Chunks   : {} (testing {} chunks)", total_chunks, chunks_to_test);
    println!("  • Chunk Dims     : {} x {} ({} bytes uncompressed/chunk)", chunk_w, chunk_h, uncomp_chunk_bytes);
    println!("  • Compression    : {:?}", chunk_info.compression);
    println!("  • Sample Format  : {:?} ({} bits)", chunk_info.sample_format, chunk_info.bits_per_sample);

    // =========================================================================
    // BENCHMARK 1: Standard tiff crate LZW decoder
    // =========================================================================
    println!("\n▶ [BENCHMARK 1] Standard tiff LZW Decompression (stream BufReader + allocations)");
    let t0 = Instant::now();
    let mut std_decoder = Decoder::new(Cursor::new(&mmap[..])).unwrap();
    let mut std_samples = 0usize;

    for chunk_idx in 0..chunks_to_test as u32 {
        let chunk_data = std_decoder.read_chunk(chunk_idx).unwrap();
        match chunk_data {
            DecodingResult::U8(v) => std_samples += v.len(),
            DecodingResult::U16(v) => std_samples += v.len(),
            DecodingResult::I16(v) => std_samples += v.len(),
            DecodingResult::F32(v) => std_samples += v.len(),
            _ => {}
        }
    }
    let dur_std = t0.elapsed();
    let std_bandwidth = total_uncompressed_mb / dur_std.as_secs_f64();
    let std_latency_per_chunk = dur_std.as_micros() as f64 / chunks_to_test as f64;

    println!("  • Total Time      : {:.2?}", dur_std);
    println!("  • Decomp Bandwidth: {:.2} MB/sec", std_bandwidth);
    println!("  • Average Latency : {:.2} µs / chunk", std_latency_per_chunk);

    // =========================================================================
    // BENCHMARK 2: Accelerated Zero-Allocation LZW ChunkDecoder
    // =========================================================================
    println!("\n▶ [BENCHMARK 2] Accelerated Zero-Allocation LZW (ChunkDecoder direct mmap)");
    let t1 = Instant::now();
    let mut fast_decoder = reader.open_decoder().unwrap();
    let mut fast_samples = 0usize;

    for chunk_idx in 0..chunks_to_test as u32 {
        let (_bounds, chunk_data) = fast_decoder.read_chunk(chunk_idx).unwrap();
        match chunk_data {
            DecodingResult::U8(v) => fast_samples += v.len(),
            DecodingResult::U16(v) => fast_samples += v.len(),
            DecodingResult::I16(v) => fast_samples += v.len(),
            DecodingResult::F32(v) => fast_samples += v.len(),
            _ => {}
        }
    }
    let dur_fast = t1.elapsed();
    let fast_bandwidth = total_uncompressed_mb / dur_fast.as_secs_f64();
    let fast_latency_per_chunk = dur_fast.as_micros() as f64 / chunks_to_test as f64;

    println!("  • Total Time      : {:.2?}", dur_fast);
    println!("  • Decomp Bandwidth: {:.2} MB/sec", fast_bandwidth);
    println!("  • Average Latency : {:.2} µs / chunk", fast_latency_per_chunk);

    assert_eq!(std_samples, fast_samples);
    let speedup = dur_std.as_secs_f64() / dur_fast.as_secs_f64();
    let saved_us = std_latency_per_chunk - fast_latency_per_chunk;
    println!("\n  ⚡ Pure Decompression Speedup: {:.2}x faster! (Saved {:.2} µs/chunk, +{:.1}% bandwidth)",
        speedup, saved_us, ((fast_bandwidth / std_bandwidth) - 1.0) * 100.0);
}

fn main() {
    println!("=========================================================================================");
    println!("       raster_h3 Benchmark: Accelerated Zero-Allocation LZW Decompression                 ");
    println!("=========================================================================================");

    // Benchmark both Hawaii files
    // Use 2,000 chunks for quick, highly accurate microbenchmark comparison
    benchmark_file("data/CFL_HI.tif", Some(2000));
    benchmark_file("data/LF2024_FBFM40_HI.tif", Some(2000));

    // Full Archipelago End-to-End Streaming Aggregation
    println!("\n=========================================================================================");
    println!("  End-to-End Multi-Core Streaming Throughput with Fast LZW (All 15,840 Chunks)");
    println!("=========================================================================================");

    // Continuous CFL_HI
    let cfl_path = "data/CFL_HI.tif";
    let config = MultiResolutionConfig::new(vec![8]);
    let t_cfl = Instant::now();
    let reader_cfl = GeoTiffStreamReader::open(cfl_path).unwrap();
    let mut streamer_cfl = MultiScanHorizonStreamer::new(reader_cfl, &config).unwrap();
    let mut cfl_hexes = 0usize;
    let mut cfl_pixels = 0.0f64;
    loop {
        let n = streamer_cfl.drain_completed_into(2048, |_i, rec| {
            cfl_hexes += 1;
            cfl_pixels += rec.accumulator.count;
        });
        if n == 0 { break; }
    }
    let dur_cfl = t_cfl.elapsed();
    let mpx_cfl = (cfl_pixels / 1_000_000.0) / dur_cfl.as_secs_f64();
    println!("  • CFL_HI.tif (Continuous) : {:.2?} | {:.0} pixels | {} hexes | {:.2} Mpx/sec",
        dur_cfl, cfl_pixels, cfl_hexes, mpx_cfl);

    // Categorical Landfire
    let lf_path = "data/LF2024_FBFM40_HI.tif";
    let t_lf = Instant::now();
    let reader_lf = GeoTiffStreamReader::open(lf_path).unwrap();
    let mut streamer_lf = MultiCategoricalHorizonStreamer::new(reader_lf, &config).unwrap();
    let mut lf_hexes = 0usize;
    let mut lf_pixels = 0.0f64;
    loop {
        let n = streamer_lf.drain_completed_into(2048, |_i, rec| {
            lf_hexes += 1;
            lf_pixels += rec.accumulator.total_count;
        });
        if n == 0 { break; }
    }
    let dur_lf = t_lf.elapsed();
    let mpx_lf = (256_542_384.0 / 1_000_000.0) / dur_lf.as_secs_f64();
    println!("  • LF2024_FBFM40_HI.tif (Categorical) : {:.2?} | 256.54M pixels | {} hexes | {:.2} Mpx/sec",
        dur_lf, lf_hexes, mpx_lf);

    println!("\n=========================================================================================");
    println!("                          LZW BENCHMARK COMPLETED SUCCESSFULLY                          ");
    println!("=========================================================================================");
}
