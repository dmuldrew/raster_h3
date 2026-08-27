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
