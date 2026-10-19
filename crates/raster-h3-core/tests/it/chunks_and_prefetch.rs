//! Chunk layout, NoData chunk detection, bbox chunk pruning, and prefetched chunk readers.

use crate::helpers::{create_constant_f32_geotiff, TestGeoTiffBuilder};
use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::aggregator::{chunk_intersects_bbox, is_chunk_all_nodata};
use raster_h3_core::crs::CrsTransformer;
use raster_h3_core::raster::{
    ChunkLayout, GeoTiffStreamReader, GeoTransform, PrefetchedChunkReader,
};
use tempfile::NamedTempFile;

#[test]
fn test_is_chunk_all_nodata() {
    let empty_slice = vec![-9999.0f32; 100];
    assert!(is_chunk_all_nodata(&empty_slice, Some(-9999.0), |x| x as f64));

    let mut mixed_slice = vec![-9999.0f32; 100];
    mixed_slice[50] = 12.0;
    assert!(!is_chunk_all_nodata(&mixed_slice, Some(-9999.0), |x| x as f64));
}

#[test]
fn test_prefetched_chunk_reader() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(20, 20).create_constant_tempfile(1.0f32);

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let total_chunks = reader.chunk_layout.total_chunks;
    let indices = (0..total_chunks).collect();

    let prefetcher = PrefetchedChunkReader::spawn(reader, indices, 2);
    let mut count = 0;
    while let Some(res) = prefetcher.next_chunk() {
        assert!(res.is_ok());
        count += 1;
    }
    assert_eq!(count, total_chunks);
}

#[test]
fn test_prefetched_chunk_reader_multi_worker_ordering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi_worker_test.tif");
    TestGeoTiffBuilder::new(64, 64)
        .write_fn(&path, |col, row| (row * 64 + col) as f32)
        .unwrap();

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let total_chunks = reader.chunk_layout.total_chunks;
    let indices: Vec<u32> = (0..total_chunks).collect();

    // Verify ordering and correctness across 1, 2, and 4 worker threads
    for num_workers in [1, 2, 4] {
        let prefetcher = PrefetchedChunkReader::spawn_with_workers(
            reader.clone(),
            indices.clone(),
            4,
            num_workers,
        );

        let mut expected_chunk_idx = 0;
        while let Some(res) = prefetcher.next_chunk() {
            let (idx, bounds, _data) = res.expect("chunk decoding should succeed");
            assert_eq!(
                idx, expected_chunk_idx,
                "Chunk out of order for num_workers={}",
                num_workers
            );
            let expected_bounds = reader.chunk_layout.get_chunk_bounds(
                idx,
                reader.metadata.width,
                reader.metadata.height,
            );
            assert_eq!(bounds.col_offset, expected_bounds.col_offset);
            assert_eq!(bounds.row_offset, expected_bounds.row_offset);
            expected_chunk_idx += 1;
        }
        assert_eq!(expected_chunk_idx, total_chunks);
    }
}

#[test]
fn test_prefetched_chunk_reader_early_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("early_drop_test.tif");
    TestGeoTiffBuilder::new(64, 64)
        .write_fn(&path, |col, row| (row * 64 + col) as f32)
        .unwrap();

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let total_chunks = reader.chunk_layout.total_chunks;
    let indices: Vec<u32> = (0..total_chunks).collect();

    // Drop prefetcher after consuming only 1 chunk with 4 workers running
    let prefetcher = PrefetchedChunkReader::spawn_with_workers(reader, indices, 2, 4);
    assert!(prefetcher.next_chunk().is_some());
    drop(prefetcher); // should gracefully exit all threads without deadlock
}

#[test]
fn test_tiled_chunk_layout_calculation() {
    // 512x512 image tiled into 256x256 tiles -> 2 cols x 2 rows = 4 tiles total
    let layout = ChunkLayout::new_tiled(512, 512, 256, 256);
    assert_eq!(layout.total_chunks, 4);
    assert_eq!(layout.chunks_across, 2);
    assert_eq!(layout.chunks_down, 2);

    // Tile (0, 0)
    let chunk0 = layout.get_chunk_bounds(0, 512, 512);
    assert_eq!(chunk0.col_offset, 0);
    assert_eq!(chunk0.row_offset, 0);
    assert_eq!(chunk0.width, 256);
    assert_eq!(chunk0.height, 256);

    // Tile (1, 0)
    let chunk1 = layout.get_chunk_bounds(1, 512, 512);
    assert_eq!(chunk1.col_offset, 256);
    assert_eq!(chunk1.row_offset, 0);
    assert_eq!(chunk1.width, 256);
    assert_eq!(chunk1.height, 256);

    // Tile (0, 1)
    let chunk2 = layout.get_chunk_bounds(2, 512, 512);
    assert_eq!(chunk2.col_offset, 0);
    assert_eq!(chunk2.row_offset, 256);
    assert_eq!(chunk2.width, 256);
    assert_eq!(chunk2.height, 256);

    // Tile (1, 1)
    let chunk3 = layout.get_chunk_bounds(3, 512, 512);
    assert_eq!(chunk3.col_offset, 256);
    assert_eq!(chunk3.row_offset, 256);
    assert_eq!(chunk3.width, 256);
    assert_eq!(chunk3.height, 256);
}

#[test]
fn test_chunk_intersects_bbox_tile_filtering() {
    let layout = ChunkLayout::new_tiled(200, 200, 100, 100);
    let gt = GeoTransform {
        c0: 0.0,
        a: 1.0,
        b: 0.0,
        f0: 200.0,
        d: 0.0,
        e: -1.0,
    };
    let transformer = CrsTransformer::Wgs84Identity;

    // Tile 0 covers cols [0..100], rows [0..100] -> x: [0..100], y: [100..200]
    let chunk0 = layout.get_chunk_bounds(0, 200, 200);
    // Tile 3 covers cols [100..200], rows [100..200] -> x: [100..200], y: [0..100]
    let chunk3 = layout.get_chunk_bounds(3, 200, 200);

    // Query bbox in upper-left: [10, 150, 50, 180]
    let bbox_upper_left = [10.0, 150.0, 50.0, 180.0];
    assert!(chunk_intersects_bbox(
        &chunk0,
        &gt,
        &transformer,
        &bbox_upper_left
    ));
    assert!(!chunk_intersects_bbox(
        &chunk3,
        &gt,
        &transformer,
        &bbox_upper_left
    ));

    // Query bbox in lower-right: [120, 10, 180, 50]
    let bbox_lower_right = [120.0, 10.0, 180.0, 50.0];
    assert!(!chunk_intersects_bbox(
        &chunk0,
        &gt,
        &transformer,
        &bbox_lower_right
    ));
    assert!(chunk_intersects_bbox(
        &chunk3,
        &gt,
        &transformer,
        &bbox_lower_right
    ));
}

#[test]
fn test_tiled_uneven_chunk_layout_and_bounds() {
    // 500x300 image tiled into 256x256 tiles -> 2 cols x 2 rows = 4 tiles total
    // Uneven edges: right column width = 244, bottom row height = 44
    let layout = ChunkLayout::new_tiled(500, 300, 256, 256);
    assert_eq!(layout.total_chunks, 4);
    assert_eq!(layout.chunks_across, 2);
    assert_eq!(layout.chunks_down, 2);

    // Tile (0, 0): top-left full tile
    let chunk0 = layout.get_chunk_bounds(0, 500, 300);
    assert_eq!(chunk0.col_offset, 0);
    assert_eq!(chunk0.row_offset, 0);
    assert_eq!(chunk0.width, 256);
    assert_eq!(chunk0.height, 256);

    // Tile (1, 0): top-right partial tile (width = 500 - 256 = 244)
    let chunk1 = layout.get_chunk_bounds(1, 500, 300);
    assert_eq!(chunk1.col_offset, 256);
    assert_eq!(chunk1.row_offset, 0);
    assert_eq!(chunk1.width, 244);
    assert_eq!(chunk1.height, 256);

    // Tile (0, 1): bottom-left partial tile (height = 300 - 256 = 44)
    let chunk2 = layout.get_chunk_bounds(2, 500, 300);
    assert_eq!(chunk2.col_offset, 0);
    assert_eq!(chunk2.row_offset, 256);
    assert_eq!(chunk2.width, 256);
    assert_eq!(chunk2.height, 44);

    // Tile (1, 1): bottom-right corner partial tile (width = 244, height = 44)
    let chunk3 = layout.get_chunk_bounds(3, 500, 300);
    assert_eq!(chunk3.col_offset, 256);
    assert_eq!(chunk3.row_offset, 256);
    assert_eq!(chunk3.width, 244);
    assert_eq!(chunk3.height, 44);
}

#[test]
fn test_bounding_box_pruning() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    create_constant_f32_geotiff(&path, 100, 100, -122.50, 37.80, 0.001, 50.0);

    let reader = GeoTiffStreamReader::open(&path).unwrap();

    // Query only a small sub-rectangle: lon [-122.48, -122.46], lat [37.72, 37.74]
    let mut config = MultiResolutionConfig::single(9);
    config.bbox = Some([-122.48, 37.72, -122.46, 37.74]);

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let mut filtered_pixels: f64 = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(16).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            filtered_pixels += rec.accumulator.count;
        }
    }

    // Should only cover the sub-rectangle (approx 20x20 = 400 pixels out of 10,000)
    assert!(filtered_pixels > 0.0);
    assert!(filtered_pixels < 10000.0);
}

#[test]
fn test_spatial_filter_pushdown_chunk_skipping() {
    let (_temp_file, path) = TestGeoTiffBuilder::new(200, 200)
        .origin(-122.50, 37.80)
        .pixel_size(0.001)
        .create_constant_tempfile(100.0f32);

    // 1. Full Scan (No Pushdown)
    let reader_full = GeoTiffStreamReader::open(&path).unwrap();
    let config_full = MultiResolutionConfig::single(8);
    let mut streamer_full = MultiScanHorizonStreamer::new(reader_full, &config_full).unwrap();
    let mut full_pixels = 0.0f64;
    loop {
        let batch = streamer_full.fetch_next_batch(100).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            full_pixels += rec.accumulator.count;
        }
    }
    assert_eq!(full_pixels, 40000.0);

    // 2. Filter Pushdown with Targeted Spatial Bounding Box
    // Query quarter of the raster: lon [-122.45, -122.35], lat [37.65, 37.75]
    let reader_filtered = GeoTiffStreamReader::open(&path).unwrap();
    let target_bbox = [-122.45, 37.65, -122.35, 37.75];
    let mut config_filtered = MultiResolutionConfig::single(8);
    config_filtered.bbox = Some(target_bbox);

    let mut streamer_filtered =
        MultiScanHorizonStreamer::new(reader_filtered, &config_filtered).unwrap();
    let mut filtered_pixels = 0.0f64;
    loop {
        let batch = streamer_filtered.fetch_next_batch(100).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            filtered_pixels += rec.accumulator.count;
        }
    }

    // Filtered pixel mass must strictly match the spatial subset (~10,000 pixels)
    assert!(filtered_pixels > 0.0);
    assert!(filtered_pixels < full_pixels);
    assert!((filtered_pixels - 10000.0).abs() < 500.0);
}
