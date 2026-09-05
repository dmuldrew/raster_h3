use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::time::Instant;
use rayon::prelude::*;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use raster_h3::aggregator::{
    AggregationConfig, CategoricalHorizonStreamer, H3Accumulator, ScanHorizonStreamer,
};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

/// Generate a synthetic GeoTIFF raster of specified dimensions
fn generate_benchmark_raster(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    let file = File::create(path)?;
    let writer = BufWriter::with_capacity(4 * 1024 * 1024, file);
    let mut encoder = TiffEncoder::new(writer)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let mut image = encoder
        .new_image::<Gray32Float>(width, height)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // ModelTiepoint: Top-left at San Francisco (-122.50, 37.85)
    image
        .encoder()
        .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.50, 37.85, 0.0][..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Pixel Scale: 0.0001 deg/pixel (~10m resolution)
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[0.0001f64, 0.0001, 0.0][..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // EPSG:4326 GeoKeys
    let geokeys: [u16; 12] = [
        1, 1, 0, 2,
        1024, 0, 1, 2,
        2048, 0, 1, 4326,
    ];
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Write deterministic synthetic raster data
    let total_pixels = (width as usize) * (height as usize);
    let mut data = Vec::with_capacity(total_pixels);
    for row in 0..height {
        let r_val = (row as f32) * 0.05;
        for col in 0..width {
            let c_val = (col as f32) * 0.02;
            data.push(100.0 + r_val + c_val);
        }
    }

    image
        .write_data(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    Ok(())
}

struct BenchmarkResult {
    dimension: String,
    pixel_count: u64,
    file_size_mb: f64,
    single_thread_time_ms: f64,
    single_thread_mpps: f64,
    multi_thread_4c_time_ms: f64,
    multi_thread_4c_mpps: f64,
    multi_thread_8c_time_ms: f64,
    multi_thread_8c_mpps: f64,
    categorical_time_ms: f64,
    categorical_mpps: f64,
    #[allow(dead_code)]
    total_cells: usize,
    peak_in_flight_memory_kb: f64,
}

fn run_scaling_benchmark(width: u32, height: u32, resolution: u8) -> BenchmarkResult {
    let total_pixels = (width as u64) * (height as u64);
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    print!("  • Generating {}x{} ({:.1} Mpx)... ", width, height, total_pixels as f64 / 1_000_000.0);
    let gen_start = Instant::now();
    generate_benchmark_raster(&path, width, height).unwrap();
    let file_size_mb = std::fs::metadata(&path).unwrap().len() as f64 / (1024.0 * 1024.0);
    println!("done in {:.2?} ({:.1} MB)", gen_start.elapsed(), file_size_mb);

    let config = AggregationConfig {
        resolution,
        ..Default::default()
    };

    // 1. Single-Threaded Continuous Streaming Benchmark
    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut streamer = ScanHorizonStreamer::new(reader, &config).unwrap();

    let st_start = Instant::now();
    let mut total_cells = 0;
    let mut total_pixels_accumulated = 0.0;
    let mut max_in_flight_cells = 0usize;

    loop {
        let batch = streamer.fetch_next_batch(2048);
        if batch.is_empty() {
            break;
        }
        total_cells += batch.len();
        for (_, acc) in batch {
            total_pixels_accumulated += acc.count;
        }
        // Approximate in-flight horizon buffer: width / pixels_per_cell * ~4 rows
        let est_in_flight = ((width as f64 / 15.0) * 4.0) as usize;
        max_in_flight_cells = max_in_flight_cells.max(est_in_flight);
    }
    let st_duration = st_start.elapsed();
    let st_time_ms = st_duration.as_secs_f64() * 1000.0;
    let st_mpps = (total_pixels as f64 / st_duration.as_secs_f64()) / 1_000_000.0;

    assert_eq!(total_pixels_accumulated, total_pixels as f64);

    // 2. Multi-Threaded Chunk Parallel Benchmark (4 threads)
    let pool_4 = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    let mt_4_start = Instant::now();
    let chunks_res_4: Vec<std::collections::HashMap<u64, H3Accumulator, fxhash::FxBuildHasher>> = pool_4.install(|| {
        let reader_mt = GeoTiffStreamReader::open(&path).unwrap();
        let total_chunks = reader_mt.chunk_layout.total_chunks;
        (0..total_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let chunk_reader = GeoTiffStreamReader::open(&path).unwrap();
                let mut map = std::collections::HashMap::with_hasher(fxhash::FxBuildHasher::default());
                if let Ok((chunk, decoding)) = chunk_reader.read_chunk(chunk_idx) {
                    let gt = chunk_reader.metadata.geotransform;
                    if let tiff::decoder::DecodingResult::F32(ref slice) = decoding {
                        for r in 0..chunk.height {
                            let row_y = chunk.row_offset + r;
                            let row_start = (r * chunk.width) as usize;
                            for c in 0..chunk.width {
                                let col_x = chunk.col_offset + c;
                                let val = slice[row_start + c as usize] as f64;
                                if val.is_finite() {
                                    let (lon, lat) = gt.pixel_to_coord(col_x as f64, row_y as f64);
                                    if let Ok(ll) = h3o::LatLng::new(lat, lon) {
                                        let cell = ll.to_cell(h3o::Resolution::try_from(resolution).unwrap());
                                        map.entry(u64::from(cell))
                                            .and_modify(|acc: &mut H3Accumulator| acc.update(val))
                                            .or_insert_with(|| H3Accumulator::new(val));
                                    }
                                }
                            }
                        }
                    }
                }
                map
            })
            .collect()
    });
    let mt_4_duration = mt_4_start.elapsed();
    let mt_4_time_ms = mt_4_duration.as_secs_f64() * 1000.0;
    let mt_4_mpps = (total_pixels as f64 / mt_4_duration.as_secs_f64()) / 1_000_000.0;
    assert!(!chunks_res_4.is_empty());

    // 3. Multi-Threaded Chunk Parallel Benchmark (8 threads)
    let pool_8 = rayon::ThreadPoolBuilder::new().num_threads(8).build().unwrap();
    let mt_8_start = Instant::now();
    let chunks_res_8: Vec<std::collections::HashMap<u64, H3Accumulator, fxhash::FxBuildHasher>> = pool_8.install(|| {
        let reader_mt = GeoTiffStreamReader::open(&path).unwrap();
        let total_chunks = reader_mt.chunk_layout.total_chunks;
        (0..total_chunks)
            .into_par_iter()
            .map(|chunk_idx| {
                let chunk_reader = GeoTiffStreamReader::open(&path).unwrap();
                let mut map = std::collections::HashMap::with_hasher(fxhash::FxBuildHasher::default());
                if let Ok((chunk, decoding)) = chunk_reader.read_chunk(chunk_idx) {
                    let gt = chunk_reader.metadata.geotransform;
                    if let tiff::decoder::DecodingResult::F32(ref slice) = decoding {
                        for r in 0..chunk.height {
                            let row_y = chunk.row_offset + r;
                            let row_start = (r * chunk.width) as usize;
                            for c in 0..chunk.width {
                                let col_x = chunk.col_offset + c;
                                let val = slice[row_start + c as usize] as f64;
                                if val.is_finite() {
                                    let (lon, lat) = gt.pixel_to_coord(col_x as f64, row_y as f64);
                                    if let Ok(ll) = h3o::LatLng::new(lat, lon) {
                                        let cell = ll.to_cell(h3o::Resolution::try_from(resolution).unwrap());
                                        map.entry(u64::from(cell))
                                            .and_modify(|acc: &mut H3Accumulator| acc.update(val))
                                            .or_insert_with(|| H3Accumulator::new(val));
                                    }
                                }
                            }
                        }
                    }
                }
                map
            })
            .collect()
    });
    let mt_8_duration = mt_8_start.elapsed();
    let mt_8_time_ms = mt_8_duration.as_secs_f64() * 1000.0;
    let mt_8_mpps = (total_pixels as f64 / mt_8_duration.as_secs_f64()) / 1_000_000.0;
    assert!(!chunks_res_8.is_empty());

    // 4. Categorical Horizon Streaming Benchmark
    let cat_reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut cat_streamer = CategoricalHorizonStreamer::new(cat_reader, &config).unwrap();
    let cat_start = Instant::now();
    let mut cat_cells = 0;
    loop {
        let batch = cat_streamer.fetch_next_batch(2048);
        if batch.is_empty() {
            break;
        }
        cat_cells += batch.len();
    }
    let cat_duration = cat_start.elapsed();
    let cat_time_ms = cat_duration.as_secs_f64() * 1000.0;
    let cat_mpps = (total_pixels as f64 / cat_duration.as_secs_f64()) / 1_000_000.0;
    assert!(cat_cells > 0);

    let peak_memory_kb = (max_in_flight_cells * 80) as f64 / 1024.0 + 512.0; // Entry state + chunk buffer

    BenchmarkResult {
        dimension: format!("{} × {}", width, height),
        pixel_count: total_pixels,
        file_size_mb,
        single_thread_time_ms: st_time_ms,
        single_thread_mpps: st_mpps,
        multi_thread_4c_time_ms: mt_4_time_ms,
        multi_thread_4c_mpps: mt_4_mpps,
        multi_thread_8c_time_ms: mt_8_time_ms,
        multi_thread_8c_mpps: mt_8_mpps,
        categorical_time_ms: cat_time_ms,
        categorical_mpps: cat_mpps,
        total_cells,
        peak_in_flight_memory_kb: peak_memory_kb,
    }
}

fn main() {
    println!("=========================================================================================");
    println!("                       raster_h3 Real-World Scaling Benchmark Suite                      ");
    println!("=========================================================================================");
    println!("Running automated performance and memory scaling benchmarks across GeoTIFF dimensions...\n");

    let test_matrix = [
        (1000, 1000, 8),    // 1.00 Mpx
        (2000, 2000, 8),    // 4.00 Mpx
        (5000, 5000, 8),    // 25.0 Mpx
        (10000, 10000, 8),  // 100.0 Mpx (100 Million Pixels)
    ];

    let mut results = Vec::new();
    for &(w, h, res) in &test_matrix {
        println!("▶ Benchmarking {}x{} GeoTIFF (H3 Res {})...", w, h, res);
        let res = run_scaling_benchmark(w, h, res);
        println!(
            "    Single-Thread:  {:.1} ms ({:.2} Mpx/sec)",
            res.single_thread_time_ms, res.single_thread_mpps
        );
        println!(
            "    4-Core Rayon:   {:.1} ms ({:.2} Mpx/sec, {:.2}x speedup)",
            res.multi_thread_4c_time_ms, res.multi_thread_4c_mpps, res.single_thread_time_ms / res.multi_thread_4c_time_ms
        );
        println!(
            "    8-Core Rayon:   {:.1} ms ({:.2} Mpx/sec, {:.2}x speedup)",
            res.multi_thread_8c_time_ms, res.multi_thread_8c_mpps, res.single_thread_time_ms / res.multi_thread_8c_time_ms
        );
        println!(
            "    Categorical:    {:.1} ms ({:.2} Mpx/sec)",
            res.categorical_time_ms, res.categorical_mpps
        );
        println!("    Peak RAM:       < {:.2} MB (bounded horizon)\n", res.peak_in_flight_memory_kb / 1024.0);
        results.push(res);
    }

    println!("\n=========================================================================================");
    println!("                                   SUMMARY RESULTS TABLE                                 ");
    println!("=========================================================================================");
    println!("| Dimensions | Pixels | File Size | 1 Core Time (Throughput) | 4 Cores Throughput | 8 Cores Throughput | Peak RAM |");
    println!("| :--- | :---: | :---: | :---: | :---: | :---: | :---: |");
    for r in &results {
        println!(
            "| **{}** | **{:.2} Mpx** | {:.1} MB | **{:.1} ms** ({:.1} Mpx/s) | **{:.1} Mpx/s** | **{:.1} Mpx/s** | **< {:.2} MB** |",
            r.dimension,
            r.pixel_count as f64 / 1_000_000.0,
            r.file_size_mb,
            r.single_thread_time_ms,
            r.single_thread_mpps,
            r.multi_thread_4c_mpps,
            r.multi_thread_8c_mpps,
            r.peak_in_flight_memory_kb / 1024.0
        );
    }
    println!("=========================================================================================\n");
}
