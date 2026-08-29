use std::env;
use std::time::{Duration, Instant};
use rayon::prelude::*;
use raster_h3::aggregator::{AggregationConfig, H3Accumulator, is_chunk_all_nodata};
use raster_h3::aggregator::accumulator::FastRunAccumulator;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::crs::transformer::CrsTransformer;
use std::collections::HashMap;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: benchmark_bottleneck <path_to_geotiff>");
        std::process::exit(1);
    }
    let path = &args[1];

    println!("=================================================================");
    println!("  I/O Decompression vs CPU Math Profiler");
    println!("  File: {}", path);
    println!("=================================================================");

    let reader = GeoTiffStreamReader::open(path).expect("Failed to open file");
    let total_chunks = reader.chunk_layout.total_chunks;
    let gt = reader.metadata.geotransform;
    let nodata = reader.metadata.nodata;

    let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build().unwrap();

    let start = Instant::now();
    let (map, total_io_time, total_h3_time, total_pixel_time) = pool.install(|| {
        (0..total_chunks).into_par_iter().map(|chunk_idx| {
            let mut local_reader = reader.clone();
            
            // MEASURE 1: I/O + DECOMPRESSION
            let io_start = Instant::now();
            let chunk_data = local_reader.read_chunk(chunk_idx);
            let io_dur = io_start.elapsed();

            let mut h3_dur = Duration::from_secs(0);
            let mut pixel_dur = Duration::from_secs(0);
            let mut map = HashMap::new();

            if let Ok((chunk, data)) = chunk_data {
                match data {
                    tiff::decoder::DecodingResult::F32(slice) => {
                        let d_lon_step = gt.a;
                        let mut run_cell: u64 = 0;
                        let mut run_acc = FastRunAccumulator::default();
                        let mut current_hex_span = 0;
                        let mut prev_hex_width: usize = 32;
                        let res = h3o::Resolution::try_from(8).unwrap();

                        for r in 0..chunk.height as usize {
                            let row_idx = (chunk.row_offset as usize) + r;
                            let slice_row_start = r * chunk.width as usize;
                            let row_width = (slice.len().saturating_sub(slice_row_start)).min(chunk.width as usize);
                            
                            let (x_start, y_row) = gt.pixel_center_to_coord(chunk.col_offset as usize, row_idx);
                            let mut lon_curr = x_start;
                            let lat_row = y_row;

                            let mut c = 0;
                            while c < row_width {
                                let (lon, lat) = (lon_curr, lat_row);
                                
                                // MEASURE 2: H3 COORDINATE TRANSFORMATION
                                let h3_start = Instant::now();
                                
                                let cache_hit = if let Ok(ll) = h3o::LatLng::new(lat, lon) {
                                    Some(ll.to_cell(res).into())
                                } else { None };
                                
                                if let Some(cell_u64) = cache_hit {
                                    if cell_u64 != run_cell {
                                        if run_cell != 0 && run_acc.count > 0.0 {
                                            let h3_acc = run_acc.into_h3();
                                            map.entry(run_cell).and_modify(|acc: &mut H3Accumulator| acc.merge(&h3_acc)).or_insert(h3_acc);
                                        }
                                        run_cell = cell_u64;
                                        run_acc = FastRunAccumulator::default();
                                        
                                        if current_hex_span > 0 {
                                            prev_hex_width = current_hex_span;
                                        }
                                        current_hex_span = 0;
                                    }

                                    let remaining_guess = prev_hex_width.saturating_sub(current_hex_span).max(1);
                                    let mut guess_c = (c + remaining_guess).min(row_width);
                                    let mut guess_lon = lon_curr + ((guess_c - c) as f64) * d_lon_step;
                                    
                                    while guess_c < row_width {
                                        if let Ok(ll) = h3o::LatLng::new(lat_row, guess_lon) {
                                            let guess_cell: u64 = ll.to_cell(res).into();
                                            if guess_cell == run_cell {
                                                let step = (guess_c - c).max(1);
                                                let new_guess_c = (guess_c + step).min(row_width);
                                                if new_guess_c == guess_c { break; }
                                                guess_c = new_guess_c;
                                                guess_lon = lon_curr + ((guess_c - c) as f64) * d_lon_step;
                                            } else {
                                                break;
                                            }
                                        } else {
                                            break;
                                        }
                                    }

                                    let mut left = c + 1;
                                    let mut right = guess_c;
                                    while left < right {
                                        let mid = left + (right - left) / 2;
                                        let mid_lon = (lon_curr + d_lon_step) + ((mid - (c + 1)) as f64) * d_lon_step;
                                        if let Ok(ll) = h3o::LatLng::new(lat_row, mid_lon) {
                                            let cell: u64 = ll.to_cell(res).into();
                                            if cell == run_cell {
                                                left = mid + 1;
                                            } else {
                                                right = mid;
                                            }
                                        } else {
                                            right = mid;
                                        }
                                    }
                                    let span_end = left;
                                    
                                    h3_dur += h3_start.elapsed();

                                    // MEASURE 3: PIXEL ACCUMULATION MATH
                                    let pixel_start = Instant::now();
                                    for i in c..span_end {
                                        let val_raw = slice[slice_row_start + i];
                                        let val = val_raw as f64;
                                        let mut is_valid = val.is_finite();
                                        if let Some(nd) = nodata {
                                            is_valid &= (val - nd).abs() >= 1e-6;
                                        }

                                        let val_masked = if is_valid { val } else { 0.0 };
                                        let weight = if is_valid { 1.0 } else { 0.0 };

                                        run_acc.sum += val_masked;
                                        run_acc.sum_sq += val_masked * val_masked;
                                        run_acc.count += weight;
                                        run_acc.min = if is_valid { run_acc.min.min(val) } else { run_acc.min };
                                        run_acc.max = if is_valid { run_acc.max.max(val) } else { run_acc.max };
                                    }
                                    pixel_dur += pixel_start.elapsed();

                                    let num_stepped = span_end - c;
                                    current_hex_span += num_stepped;
                                    lon_curr += (num_stepped as f64) * d_lon_step;
                                    c = span_end;
                                } else {
                                    h3_dur += h3_start.elapsed();
                                    c += 1;
                                    lon_curr += d_lon_step;
                                }
                            }
                            if run_cell != 0 && run_acc.count > 0.0 {
                                let h3_acc = run_acc.into_h3();
                                map.entry(run_cell).and_modify(|acc: &mut H3Accumulator| acc.merge(&h3_acc)).or_insert(h3_acc);
                            }
                        }
                    }
                    _ => {} // Ignore for bench
                }
            }

            (map, io_dur, h3_dur, pixel_dur)
        }).reduce(
            || (HashMap::new(), Duration::from_secs(0), Duration::from_secs(0), Duration::from_secs(0)),
            |mut a, b| {
                for (k, v) in b.0 {
                    a.0.entry(k).and_modify(|acc: &mut H3Accumulator| acc.merge(&v)).or_insert(v);
                }
                (a.0, a.1 + b.1, a.2 + b.2, a.3 + b.3)
            }
        )
    });

    let total_dur = start.elapsed();
    
    println!("  Total Elapsed (8 cores)   : {} ms", total_dur.as_millis());
    println!("  Total Thread I/O Time     : {} ms", total_io_time.as_millis());
    println!("  Total Thread H3 Math Time : {} ms", total_h3_time.as_millis());
    println!("  Total Thread Pixel Math   : {} ms", total_pixel_time.as_millis());
    println!("-----------------------------------------------------------------");
    println!("  (Note: Thread time is summed across all 8 cores. Divide by 8");
    println!("   to get the average wall-clock time spent in each phase)");
    println!("=================================================================");
}
