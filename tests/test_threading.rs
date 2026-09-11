use std::collections::HashMap;
use tempfile::tempdir;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::aggregator::accumulator::H3Accumulator;
use raster_h3::error::Result;
mod helpers;
use tiff::tags::CompressionMethod;

fn drain_streamer_to_map(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> HashMap<u64, H3Accumulator> {
    let mut streamer = MultiScanHorizonStreamer::new(reader, config).unwrap();
    let mut map = HashMap::new();
    while !streamer.is_finished() {
        for record in streamer.fetch_next_batch(128) {
            map.entry(record.h3_index)
                .and_modify(|acc: &mut H3Accumulator| acc.merge(&record.accumulator))
                .or_insert(record.accumulator);
        }
    }
    map
}

#[test]
fn test_threaded_aggregation_consistency() -> Result<()> {
    // Create a small temporary GeoTIFF
    let dir = tempdir()?;
    let path = dir.path().join("thread_test.tif");
    helpers::create_temp_geotiff(&path, 128, 128, CompressionMethod::None)?;

    let config = MultiResolutionConfig::new(vec![8]);

    // Parallel aggregation (default Rayon pool)
    let reader_par = GeoTiffStreamReader::open(&path)?;
    let map_parallel = drain_streamer_to_map(reader_par, &config);

    // Sequential aggregation by forcing single thread
    let reader_seq = GeoTiffStreamReader::open(&path)?;
    let sequential_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let map_sequential = sequential_pool.install(|| drain_streamer_to_map(reader_seq, &config));

    // Compare maps
    assert_eq!(map_parallel.len(), map_sequential.len());
    for (k, v) in map_parallel {
        let v_seq = map_sequential.get(&k).expect("key missing in sequential map");
        assert!((v.mean() - v_seq.mean()).abs() < 1e-12);
        assert!((v.count - v_seq.count).abs() < 1e-12);
    }
    Ok(())
}

#[test]
fn test_multithreaded_pool_scaling() -> Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("thread_scale_test.tif");
    helpers::create_temp_geotiff(&path, 64, 64, CompressionMethod::None)?;

    let config = MultiResolutionConfig::new(vec![8]);

    // Baseline with 1 thread
    let reader_1 = GeoTiffStreamReader::open(&path)?;
    let pool_1 = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let baseline_map = pool_1.install(|| drain_streamer_to_map(reader_1, &config));

    // Compare against 2, 4, and 8 worker threads
    for num_threads in [2, 4] {
        let reader_n = GeoTiffStreamReader::open(&path)?;
        let pool = rayon::ThreadPoolBuilder::new().num_threads(num_threads).build().unwrap();
        let map = pool.install(|| drain_streamer_to_map(reader_n, &config));

        assert_eq!(map.len(), baseline_map.len(), "Thread count {} produced different map size", num_threads);
        for (k, v) in &baseline_map {
            let v_other = map.get(k).unwrap_or_else(|| panic!("Key {} missing for thread count {}", k, num_threads));
            assert!((v.mean() - v_other.mean()).abs() < 1e-12);
            assert!((v.count - v_other.count).abs() < 1e-12);
        }
    }
    Ok(())
}

#[test]
fn test_multithreaded_streamer_high_contention_stress() -> Result<()> {
    use std::sync::{Arc, Mutex};
    use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer, MultiContinuousRecord};
    use rayon::prelude::*;

    let dir = tempdir()?;
    let path = dir.path().join("contention_test.tif");
    helpers::create_temp_geotiff(&path, 128, 128, CompressionMethod::None)?;

    let reader = GeoTiffStreamReader::open(&path)?;
    let config = MultiResolutionConfig::new(vec![7, 8]);
    let streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let streamer_shared = Arc::new(Mutex::new(streamer));

    // Spawn 16 worker threads concurrently draining the streamer in small batches
    let pool = rayon::ThreadPoolBuilder::new().num_threads(16).build().unwrap();
    let total_records: Vec<Vec<MultiContinuousRecord>> = pool.install(|| {
        (0..16)
            .into_par_iter()
            .map(|_| {
                let mut local_batch = Vec::new();
                loop {
                    let batch = {
                        let mut guard = match streamer_shared.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        guard.fetch_next_batch(128)
                    };
                    if batch.is_empty() {
                        break;
                    }
                    local_batch.extend(batch);
                }
                local_batch
            })
            .collect()
    });

    let all_records: Vec<MultiContinuousRecord> = total_records.into_iter().flatten().collect();
    assert!(!all_records.is_empty(), "Should have received streamed multi-resolution records");

    // Total pixel mass for 128x128 = 16,384 pixels across 2 resolutions (7 and 8) = 32,768 pixel units
    let res7_pixels: f64 = all_records.iter().filter(|r| r.resolution == 7).map(|r| r.accumulator.count).sum();
    let res8_pixels: f64 = all_records.iter().filter(|r| r.resolution == 8).map(|r| r.accumulator.count).sum();

    assert_eq!(res7_pixels, 16384.0, "Resolution 7 must strictly conserve total pixel mass under high thread contention");
    assert_eq!(res8_pixels, 16384.0, "Resolution 8 must strictly conserve total pixel mass under high thread contention");

    Ok(())
}

