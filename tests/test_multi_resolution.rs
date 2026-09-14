mod helpers;

use std::collections::HashMap;
use std::fs::File;
use tempfile::NamedTempFile;

use helpers::create_wave_test_geotiff as create_test_geotiff;
use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiContinuousRecord, MultiResolutionConfig,
    MultiScanHorizonStreamer,
};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::RowAccessor;

#[test]
fn test_multi_resolution_direct_ground_truth_exact_match() {
    let width = 128;
    let height = 128;
    let total_pixels = (width * height) as f64;
    let temp_raster = create_test_geotiff(width, height);
    let raster_path = temp_raster.path();

    // 1. Run single-pass multi-resolution streaming on [7, 8, 9]
    let multi_config = MultiResolutionConfig {
        resolutions: vec![7, 8, 9],
        ..Default::default()
    };
    let reader = GeoTiffStreamReader::open(raster_path).unwrap();
    let mut multi_streamer = MultiScanHorizonStreamer::new(reader, &multi_config).unwrap();

    let mut multi_res_map: HashMap<u8, HashMap<u64, MultiContinuousRecord>> = HashMap::new();
    multi_res_map.insert(7, HashMap::new());
    multi_res_map.insert(8, HashMap::new());
    multi_res_map.insert(9, HashMap::new());

    loop {
        let batch = multi_streamer.fetch_next_batch(32);
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let res = rec.resolution;
            multi_res_map.get_mut(&res).unwrap().insert(rec.h3_index, rec);
        }
    }

    // 2. Run standalone single-resolution streaming for 7, 8, and 9
    for &target_res in &[7, 8, 9] {
        let single_config = MultiResolutionConfig::single(target_res);
        let single_reader = GeoTiffStreamReader::open(raster_path).unwrap();
        let mut single_streamer = MultiScanHorizonStreamer::new(single_reader, &single_config).unwrap();

        let mut single_cells = HashMap::new();
        let mut total_single_mass = 0.0;

        loop {
            let batch = single_streamer.fetch_next_batch(32);
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                total_single_mass += rec.accumulator.count;
                single_cells.insert(rec.h3_index, rec.accumulator);
            }
        }

        assert_eq!(total_single_mass, total_pixels);

        let multi_cells = multi_res_map.get(&target_res).unwrap();
        assert_eq!(
            multi_cells.len(),
            single_cells.len(),
            "Cell count mismatch at res {}",
            target_res
        );

        let mut total_multi_mass = 0.0;
        for (cell_u64, single_acc) in &single_cells {
            let multi_rec = multi_cells.get(cell_u64).unwrap_or_else(|| {
                panic!("Cell {:x} missing at res {}", cell_u64, target_res);
            });
            total_multi_mass += multi_rec.accumulator.count;

            assert_eq!(
                multi_rec.accumulator.count, single_acc.count,
                "Pixel count mismatch at cell {:x} (res {})",
                cell_u64, target_res
            );
            assert!(
                (multi_rec.accumulator.mean() - single_acc.mean()).abs() < 1e-9,
                "Mean mismatch at cell {:x} (res {})",
                cell_u64, target_res
            );
            assert!(
                (multi_rec.accumulator.variance() - single_acc.variance()).abs() < 1e-7,
                "Variance mismatch at cell {:x} (res {})",
                cell_u64, target_res
            );
            assert_eq!(multi_rec.accumulator.min, single_acc.min);
            assert_eq!(multi_rec.accumulator.max, single_acc.max);
        }

        assert_eq!(total_multi_mass, total_pixels);
    }
}

#[test]
fn test_multi_resolution_categorical_direct_ground_truth_exact_match() {
    let width = 64;
    let height = 64;
    let total_pixels = (width * height) as f64;
    let temp_raster = create_test_geotiff(width, height);
    let raster_path = temp_raster.path();

    let multi_config = MultiResolutionConfig {
        resolutions: vec![7, 8],
        ..Default::default()
    };
    let reader = GeoTiffStreamReader::open(raster_path).unwrap();
    let mut multi_cat_streamer = MultiCategoricalHorizonStreamer::new(reader, &multi_config).unwrap();

    let mut multi_cat_map: HashMap<u8, HashMap<u64, _>> = HashMap::new();
    multi_cat_map.insert(7, HashMap::new());
    multi_cat_map.insert(8, HashMap::new());

    loop {
        let batch = multi_cat_streamer.fetch_next_batch(32);
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let res = rec.resolution;
            multi_cat_map.get_mut(&res).unwrap().insert(rec.h3_index, rec);
        }
    }

    for &target_res in &[7, 8] {
        let single_config = MultiResolutionConfig::single(target_res);
        let single_reader = GeoTiffStreamReader::open(raster_path).unwrap();
        let mut single_streamer = MultiCategoricalHorizonStreamer::new(single_reader, &single_config).unwrap();

        let mut single_cells = HashMap::new();
        let mut total_single_mass = 0.0;

        loop {
            let batch = single_streamer.fetch_next_batch(32);
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                total_single_mass += rec.accumulator.total_count;
                single_cells.insert(rec.h3_index, rec.accumulator);
            }
        }

        assert_eq!(total_single_mass, total_pixels);
        let multi_cells = multi_cat_map.get(&target_res).unwrap();
        assert_eq!(multi_cells.len(), single_cells.len());

        for (cell_u64, single_acc) in &single_cells {
            let multi_rec = multi_cells.get(cell_u64).unwrap();
            assert_eq!(multi_rec.accumulator.total_count, single_acc.total_count);
            assert_eq!(multi_rec.accumulator.majority(), single_acc.majority());
            assert_eq!(multi_rec.accumulator.unique_classes(), single_acc.unique_classes());
            single_acc.for_each_class(|cat, cnt| {
                assert_eq!(multi_rec.accumulator.get_class_count(cat), cnt);
            });
        }
    }
}

#[test]
fn test_multi_resolution_empty_error_handling() {
    let temp_raster = create_test_geotiff(16, 16);
    let reader = GeoTiffStreamReader::open(temp_raster.path()).unwrap();
    let empty_config = MultiResolutionConfig {
        resolutions: vec![],
        ..Default::default()
    };
    let result = MultiScanHorizonStreamer::new(reader, &empty_config);
    assert!(result.is_err());
}

#[test]
fn test_prefetch_drain_chunk_batch_into() {
    use raster_h3::raster::prefetch::PrefetchedChunkReader;

    let temp_raster = create_test_geotiff(64, 64);
    let reader = GeoTiffStreamReader::open(temp_raster.path()).unwrap();
    let num_chunks = reader.chunk_layout.total_chunks as u32;

    let indices: Vec<u32> = (0..num_chunks).collect();
    let prefetcher = PrefetchedChunkReader::spawn_with_workers(reader, indices, num_chunks as usize, 2);

    let mut batch = Vec::new();
    let min_b = (num_chunks as usize / 2).max(1);
    let fetched = prefetcher.drain_chunk_batch_into(&mut batch, min_b, num_chunks as usize);
    assert!(fetched >= min_b);
    assert_eq!(batch.len(), fetched);

    // Verify chunk order
    for (i, item) in batch.iter().enumerate() {
        let (chunk_idx, _, _) = item.as_ref().unwrap();
        assert_eq!(*chunk_idx, i as u32);
    }

    // Drain remainder if any
    let fetched_rem = prefetcher.drain_chunk_batch_into(&mut batch, 1, num_chunks as usize);
    assert_eq!(batch.len(), num_chunks as usize);
    assert_eq!(fetched + fetched_rem, num_chunks as usize);
}

#[test]
fn test_multi_resolution_fusion_supersampling_exact_match() {
    let width = 64;
    let height = 64;
    let total_pixels = (width * height) as f64;
    let temp_raster = create_test_geotiff(width, height);
    let raster_path = temp_raster.path();

    // 1. Run single-pass multi-resolution streaming on [8, 9] with 5-point super-sampling
    let multi_config = MultiResolutionConfig {
        resolutions: vec![8, 9],
        sampling: SamplingPattern::five_point(),
        ..Default::default()
    };
    let reader = GeoTiffStreamReader::open(raster_path).unwrap();
    let mut multi_streamer = MultiScanHorizonStreamer::new(reader, &multi_config).unwrap();

    let mut multi_res_map: HashMap<u8, HashMap<u64, MultiContinuousRecord>> = HashMap::new();
    multi_res_map.insert(8, HashMap::new());
    multi_res_map.insert(9, HashMap::new());

    loop {
        let batch = multi_streamer.fetch_next_batch(32);
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let res = rec.resolution;
            multi_res_map.get_mut(&res).unwrap().insert(rec.h3_index, rec);
        }
    }

    // 2. Run standalone single-resolution streaming for 8 and 9 with 5-point super-sampling
    for &target_res in &[8, 9] {
        let mut single_config = MultiResolutionConfig::single(target_res);
        single_config.sampling = SamplingPattern::five_point();
        let single_reader = GeoTiffStreamReader::open(raster_path).unwrap();
        let mut single_streamer = MultiScanHorizonStreamer::new(single_reader, &single_config).unwrap();

        let mut single_cells = HashMap::new();
        let mut total_single_mass = 0.0;

        loop {
            let batch = single_streamer.fetch_next_batch(32);
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                total_single_mass += rec.accumulator.count;
                single_cells.insert(rec.h3_index, rec.accumulator);
            }
        }

        assert!((total_single_mass - total_pixels).abs() < 1e-6);

        let multi_cells = multi_res_map.get(&target_res).unwrap();
        assert_eq!(
            multi_cells.len(),
            single_cells.len(),
            "Cell count mismatch at res {}",
            target_res
        );

        let mut total_multi_mass = 0.0;
        for (cell_u64, single_acc) in &single_cells {
            let multi_rec = multi_cells.get(cell_u64).unwrap_or_else(|| {
                panic!("Cell {:x} missing at res {}", cell_u64, target_res);
            });
            total_multi_mass += multi_rec.accumulator.count;

            assert!(
                (multi_rec.accumulator.count - single_acc.count).abs() < 1e-6,
                "Count mismatch at cell {:x} (res {}): multi = {}, single = {}",
                cell_u64, target_res, multi_rec.accumulator.count, single_acc.count
            );
            assert!(
                (multi_rec.accumulator.sum - single_acc.sum).abs() < 1e-5,
                "Sum mismatch at cell {:x} (res {}): multi = {}, single = {}",
                cell_u64, target_res, multi_rec.accumulator.sum, single_acc.sum
            );
            assert!(
                (multi_rec.accumulator.mean() - single_acc.mean()).abs() < 1e-5,
                "Mean mismatch at cell {:x} (res {})",
                cell_u64, target_res
            );
        }

        assert!((total_multi_mass - total_pixels).abs() < 1e-6);
        assert!((total_multi_mass - total_single_mass).abs() < 1e-6);
    }
}

#[test]
fn test_multi_resolution_categorical_supersampling_exact_match() {
    let width = 64;
    let height = 64;
    let total_pixels = (width * height) as f64;
    let temp_raster = create_test_geotiff(width, height);
    let raster_path = temp_raster.path();

    let multi_config = MultiResolutionConfig {
        resolutions: vec![8, 9],
        sampling: SamplingPattern::five_point(),
        ..Default::default()
    };
    let reader = GeoTiffStreamReader::open(raster_path).unwrap();
    let mut multi_cat_streamer = MultiCategoricalHorizonStreamer::new(reader, &multi_config).unwrap();

    let mut multi_cat_map: HashMap<u8, HashMap<u64, _>> = HashMap::new();
    multi_cat_map.insert(8, HashMap::new());
    multi_cat_map.insert(9, HashMap::new());

    loop {
        let batch = multi_cat_streamer.fetch_next_batch(32);
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let res = rec.resolution;
            multi_cat_map.get_mut(&res).unwrap().insert(rec.h3_index, rec);
        }
    }

    for &target_res in &[8, 9] {
        let mut single_config = MultiResolutionConfig::single(target_res);
        single_config.sampling = SamplingPattern::five_point();
        let single_reader = GeoTiffStreamReader::open(raster_path).unwrap();
        let mut single_streamer = MultiCategoricalHorizonStreamer::new(single_reader, &single_config).unwrap();

        let mut single_cells = HashMap::new();
        let mut total_single_mass = 0.0;

        loop {
            let batch = single_streamer.fetch_next_batch(32);
            if batch.is_empty() {
                break;
            }
            for rec in batch {
                total_single_mass += rec.accumulator.total_count;
                single_cells.insert(rec.h3_index, rec.accumulator);
            }
        }

        assert!((total_single_mass - total_pixels).abs() < 1e-6);
        let multi_cells = multi_cat_map.get(&target_res).unwrap();
        assert_eq!(multi_cells.len(), single_cells.len());

        for (cell_u64, single_acc) in &single_cells {
            let multi_rec = multi_cells.get(cell_u64).unwrap();
            assert!(
                (multi_rec.accumulator.total_count - single_acc.total_count).abs() < 1e-6,
                "Total count mismatch at cell {:x}", cell_u64
            );
            assert_eq!(multi_rec.accumulator.majority(), single_acc.majority());
            single_acc.for_each_class(|cat, cnt| {
                assert!(
                    (multi_rec.accumulator.get_class_count(cat) - cnt).abs() < 1e-6,
                    "Class count mismatch for cat {} at cell {:x}", cat, cell_u64
                );
            });
        }
    }
}

#[test]
fn test_parquet_continuous_streaming_export_and_sorting() {
    let tiff_file = create_test_geotiff(128, 128);
    let tiff_path = tiff_file.path().to_str().unwrap();

    let parquet_file = NamedTempFile::new().unwrap();
    let parquet_path = parquet_file.path().to_path_buf();

    let config = MultiResolutionConfig::new(vec![8, 9]);
    let reader = GeoTiffStreamReader::open(tiff_path).unwrap();
    let streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let parquet_config = ParquetExportConfig {
        compact: false,
        row_group_size: 50,
        ..Default::default()
    };

    let total_written = H3ParquetWriter::write_continuous_streamer_to_parquet(
        streamer,
        &parquet_path,
        parquet_config,
    )
    .unwrap();

    assert!(total_written > 0, "Should write hexagons to Parquet");

    let file = File::open(&parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let metadata = reader.metadata();

    assert!(metadata.num_row_groups() > 1, "Should produce multiple row groups with row_group_size=50");

    let mut row_count = 0;
    for i in 0..metadata.num_row_groups() {
        let rg_reader = reader.get_row_group(i).unwrap();
        let num_rg_rows = rg_reader.metadata().num_rows() as usize;
        assert!(num_rg_rows <= 50);
        row_count += num_rg_rows;
    }
    assert_eq!(row_count, total_written);

    // Verify sorting within row groups
    let iter = reader.get_row_iter(None).unwrap();
    let mut prev_index = i64::MIN;
    let mut current_rg_rows = 0;
    for row in iter {
        let row = row.unwrap();
        let h3_idx = row.get_long(0).unwrap();
        if current_rg_rows % 50 == 0 {
            // New row group started, reset monotonic check
            prev_index = h3_idx;
        } else {
            assert!(h3_idx >= prev_index, "h3_index must be sorted within row group: {} >= {}", h3_idx, prev_index);
            prev_index = h3_idx;
        }
        current_rg_rows += 1;
    }
}

#[test]
fn test_parquet_categorical_streaming_export_and_sorting() {
    let tiff_file = create_test_geotiff(128, 128);
    let tiff_path = tiff_file.path().to_str().unwrap();

    let parquet_file = NamedTempFile::new().unwrap();
    let parquet_path = parquet_file.path().to_path_buf();

    let config = MultiResolutionConfig::new(vec![8, 9]);
    let reader = GeoTiffStreamReader::open(tiff_path).unwrap();
    let streamer = MultiCategoricalHorizonStreamer::new(reader, &config).unwrap();

    let parquet_config = ParquetExportConfig {
        compact: true,
        row_group_size: 50,
        is_categorical: true,
        ..Default::default()
    };

    let total_written = H3ParquetWriter::write_categorical_streamer_to_parquet(
        streamer,
        &parquet_path,
        parquet_config,
    )
    .unwrap();

    assert!(total_written > 0, "Should write categorical hexagons to Parquet");

    let file = File::open(&parquet_path).unwrap();
    let reader = SerializedFileReader::new(file).unwrap();
    let metadata = reader.metadata();

    assert!(metadata.num_row_groups() > 1);

    let mut row_count = 0;
    for i in 0..metadata.num_row_groups() {
        let rg_reader = reader.get_row_group(i).unwrap();
        row_count += rg_reader.metadata().num_rows() as usize;
    }
    assert_eq!(row_count, total_written);

    // Verify sorting within row groups
    let iter = reader.get_row_iter(None).unwrap();
    let mut prev_index = i64::MIN;
    let mut current_rg_rows = 0;
    for row in iter {
        let row = row.unwrap();
        let h3_idx = row.get_long(0).unwrap();
        if current_rg_rows % 50 == 0 {
            prev_index = h3_idx;
        } else {
            assert!(h3_idx >= prev_index, "h3_index must be sorted within row group");
            prev_index = h3_idx;
        }
        current_rg_rows += 1;
    }
}


