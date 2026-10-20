//! Failures after valid TIFF headers must reach stream consumers and exporters.
use std::fs::{self};

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3_core::pmtiles::tiler::H3PmtilesTiler;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;

use crate::helpers::corrupt_strip_geotiff as corrupt_payload;

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
