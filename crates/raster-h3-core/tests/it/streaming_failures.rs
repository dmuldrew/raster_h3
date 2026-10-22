//! Failures after valid TIFF headers must reach stream consumers and exporters.
use std::fs::{self};

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3_core::pmtiles::tiler::H3PmtilesTiler;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;

use crate::helpers::{corrupt_strip_geotiff as corrupt_payload, TestGeoTiffBuilder};

#[test]
fn corrupt_chunk_errors_are_latched_for_both_streamers() {
    let file = corrupt_payload();
    let config = MultiResolutionConfig::single(8);
    let mut continuous =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(file.path()).unwrap(), &config)
            .unwrap();
    // Completed cells may stream before a later corrupt strip is encountered.
    let mut failure = None;
    for _ in 0..1024 {
        match continuous.fetch_next_batch(2048) {
            Ok(batch) => assert!(!batch.is_empty(), "corruption must not become EOF"),
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        }
    }
    let first = failure.expect("corrupt strip must eventually fail");
    assert!(first.contains("Raster stream failed"));
    assert_eq!(
        continuous.fetch_next_batch(2048).unwrap_err().to_string(),
        first
    );
    let mut emitted = 0;
    assert!(continuous
        .drain_completed_into(2048, |_, _| emitted += 1)
        .is_err());
    assert_eq!(emitted, 0);
    assert!(!continuous.is_finished(), "failure is not successful EOF");

    let mut categorical = MultiCategoricalHorizonStreamer::new(
        GeoTiffStreamReader::open(file.path()).unwrap(),
        &config,
    )
    .unwrap();
    let mut failed = false;
    for _ in 0..1024 {
        match categorical.drain_completed_into(2048, |_, _| emitted += 1) {
            Ok(count) => assert!(count > 0, "corruption must not become EOF"),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    let before = emitted;
    assert!(categorical
        .drain_completed_into(2048, |_, _| emitted += 1)
        .is_err());
    assert_eq!(emitted, before, "no records may be emitted after failure");
    assert!(categorical.fetch_next_batch(2048).is_err());
}

#[test]
fn corrupt_chunk_exports_fail_and_preserve_existing_outputs() {
    let file = corrupt_payload();
    let dir = tempfile::tempdir().unwrap();
    for categorical in [false, true] {
        let parquet = dir.path().join("output.parquet");
        let pmtiles = dir.path().join("output.pmtiles");
        fs::write(&parquet, b"existing parquet").unwrap();
        fs::write(&pmtiles, b"existing pmtiles").unwrap();
        let config = MultiResolutionConfig::single(8);
        let export = ParquetExportConfig {
            is_categorical: categorical,
            row_group_size: 1,
            ..Default::default()
        };
        assert!(H3ParquetWriter::process_raster_source_to_parquet(
            file.path(),
            &parquet,
            config.clone(),
            export
        )
        .is_err());
        let reader = GeoTiffStreamReader::open(file.path()).unwrap();
        let result = if categorical {
            H3PmtilesTiler::generate_from_categorical_streamer(
                MultiCategoricalHorizonStreamer::new(reader, &config).unwrap(),
                &pmtiles,
            )
        } else {
            H3PmtilesTiler::generate_from_continuous_streamer(
                MultiScanHorizonStreamer::new(reader, &config).unwrap(),
                &pmtiles,
            )
        };
        assert!(result.is_err());
        assert_eq!(fs::read(parquet).unwrap(), b"existing parquet");
        assert_eq!(fs::read(pmtiles).unwrap(), b"existing pmtiles");
    }
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        2,
        "no abandoned temporary exports"
    );
}

#[test]
fn parquet_failure_after_flushed_row_groups_is_not_published() {
    use raster_h3_core::aggregator::accumulator::H3Accumulator;
    use raster_h3_core::aggregator::multi_horizon::MultiContinuousRecord;
    use raster_h3_core::error::{RasterH3Error, Result};
    use raster_h3_core::parquet::writer::{
        run_parquet_streaming_pipeline, ContinuousRowGroupBuffer, ParquetStreamer,
    };

    struct FailingStreamer {
        remaining: usize,
    }
    impl ParquetStreamer for FailingStreamer {
        type Record = MultiContinuousRecord;
        fn drain_completed_into<F>(&mut self, n: usize, mut consumer: F) -> Result<usize>
        where
            F: FnMut(usize, Self::Record),
        {
            if self.remaining == 0 {
                return Err(RasterH3Error::StreamFailed("late read failure".into()));
            }
            let n = n.min(self.remaining);
            for i in 0..n {
                consumer(
                    i,
                    MultiContinuousRecord {
                        resolution: 8,
                        h3_index: h3o::LatLng::new(37.8, -122.4)
                            .unwrap()
                            .to_cell(h3o::Resolution::Eight)
                            .into(),
                        accumulator: H3Accumulator::new(42.0),
                    },
                );
            }
            self.remaining -= n;
            Ok(n)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output.parquet");
    fs::write(&output, b"existing dataset").unwrap();
    let config = ParquetExportConfig {
        row_group_size: 1,
        ..Default::default()
    };
    let result = run_parquet_streaming_pipeline::<_, ContinuousRowGroupBuffer, _>(
        FailingStreamer { remaining: 8 },
        &output,
        config,
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("late read failure"));
    assert_eq!(fs::read(output).unwrap(), b"existing dataset");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn premature_prefetch_termination_is_an_error() {
    let file = corrupt_payload();
    let mut streamer = MultiScanHorizonStreamer::new(
        GeoTiffStreamReader::open(file.path()).unwrap(),
        &MultiResolutionConfig::single(8),
    )
    .unwrap();
    streamer.abort_prefetch_for_testing();
    assert!(streamer
        .fetch_next_batch(2048)
        .unwrap_err()
        .to_string()
        .contains("Prefetch ended after 0 of 4 chunks"));
}

/// A low budget over many res-8 cells, so the stream spills within its
/// first few chunks.
fn spilling_config(spill_directory: std::path::PathBuf) -> MultiResolutionConfig {
    let mut config = MultiResolutionConfig::new(vec![6, 8]);
    config.aggregation_budget_bytes = 1 << 20;
    config.aggregation_workers = Some(4);
    config.spill_directory = Some(spill_directory);
    config
}

#[test]
fn unusable_spill_directory_fails_streams_and_latches() {
    let dir = tempfile::tempdir().unwrap();
    let (_file, path) = TestGeoTiffBuilder::new(300, 1200)
        .origin(-120.0, 45.0)
        .pixel_size(0.002)
        .create_f32_tempfile(|c, r| (c + r) as f32);
    let not_a_directory = dir.path().join("file");
    fs::write(&not_a_directory, b"").unwrap();

    for spill_directory in [dir.path().join("missing"), not_a_directory] {
        let config = spilling_config(spill_directory.clone());
        let mut continuous =
            MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config)
                .unwrap();
        let mut failure = None;
        for _ in 0..1024 {
            match continuous.fetch_next_batch(2048) {
                Ok(batch) => assert!(!batch.is_empty(), "spill failure must not become EOF"),
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        let first = failure.unwrap_or_else(|| panic!("{spill_directory:?} must fail"));
        assert!(first.contains("Raster stream failed"), "{first}");
        assert_eq!(
            continuous.fetch_next_batch(2048).unwrap_err().to_string(),
            first
        );
        let mut emitted = 0;
        assert!(continuous
            .drain_completed_into(2048, |_, _| emitted += 1)
            .is_err());
        assert_eq!(emitted, 0);
        assert!(!continuous.is_finished());

        let mut categorical = MultiCategoricalHorizonStreamer::new(
            GeoTiffStreamReader::open(&path).unwrap(),
            &config,
        )
        .unwrap();
        let mut failed = false;
        for _ in 0..1024 {
            match categorical.fetch_next_batch(2048) {
                Ok(batch) => assert!(!batch.is_empty(), "spill failure must not become EOF"),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        assert!(
            failed,
            "{spill_directory:?} must fail the categorical stream"
        );
        assert!(categorical.fetch_next_batch(2048).is_err());
    }
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn spill_runs_leave_no_files_in_the_spill_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (_file, path) = TestGeoTiffBuilder::new(300, 1200)
        .origin(-120.0, 45.0)
        .pixel_size(0.002)
        .create_f32_tempfile(|c, r| (c + r) as f32);
    let entries = || fs::read_dir(dir.path()).unwrap().count();

    let config = spilling_config(dir.path().to_path_buf());
    let mut stream =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config).unwrap();
    let mut checked_mid_stream = false;
    loop {
        let batch = stream.fetch_next_batch(256).unwrap();
        if batch.is_empty() {
            break;
        }
        // Unix unlinks runs on creation, so nothing can leak even if the
        // process dies; Windows deletes them when their handles close.
        if stream.spill_run_count() > 0 && !stream.is_finished() {
            if cfg!(unix) {
                assert_eq!(entries(), 0);
            }
            checked_mid_stream = true;
        }
    }
    assert!(checked_mid_stream && stream.spill_run_count() > 0);
    drop(stream);
    assert_eq!(entries(), 0);
}
