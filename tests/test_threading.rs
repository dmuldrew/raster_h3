// use std::collections::HashMap; // unused in this test
use tempfile::tempdir;
use raster_h3::aggregator::h3_map::aggregate_raster_stream;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::aggregator::horizon_streamer::AggregationConfig;
use raster_h3::error::Result;
use raster_h3::aggregator::h3_map::H3HashMap;
mod helpers;
use tiff::tags::CompressionMethod;

#[test]
fn test_threaded_aggregation_consistency() -> Result<()> {
    // Create a small temporary GeoTIFF
    let dir = tempdir()?;
    let path = dir.path().join("thread_test.tif");
    helpers::create_temp_geotiff(&path, 128, 128, CompressionMethod::None)?;

    // Open reader
    let reader = GeoTiffStreamReader::open(&path)?;
    let config = AggregationConfig::default();

    // Parallel aggregation (default Rayon pool)
    let map_parallel: H3HashMap = aggregate_raster_stream(&reader, &config)?;

    // Sequential aggregation by forcing single thread
    let sequential_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let map_sequential: H3HashMap = sequential_pool.install(|| aggregate_raster_stream(&reader, &config))?;


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

    let reader = GeoTiffStreamReader::open(&path)?;
    let config = AggregationConfig::default();

    // Baseline with 1 thread
    let pool_1 = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let baseline_map: H3HashMap = pool_1.install(|| aggregate_raster_stream(&reader, &config))?;

    // Compare against 2, 4, and 8 worker threads
    for num_threads in [2, 4, 8] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(num_threads).build().unwrap();
        let map: H3HashMap = pool.install(|| aggregate_raster_stream(&reader, &config))?;

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

