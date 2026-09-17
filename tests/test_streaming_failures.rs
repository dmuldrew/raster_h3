//! Failures after valid TIFF headers must reach stream consumers and exporters.
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::sync::Mutex;

use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::functions::bind_utils::ConcurrentRecordQueue;
use raster_h3::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use tempfile::NamedTempFile;
use tiff::encoder::{colortype, compression::Deflate, TiffEncoder};
use tiff::tags::Tag;

fn corrupt_payload() -> NamedTempFile {
    let file = NamedTempFile::new().unwrap();
    {
        let mut encoder = TiffEncoder::new(File::create(file.path()).unwrap()).unwrap();
        let mut image = encoder
            .new_image_with_compression::<colortype::Gray32Float, Deflate>(
                32,
                32,
                Deflate::default(),
            )
            .unwrap();
        image.rows_per_strip(8).unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::ModelTiepointTag,
                &[0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::GeoKeyDirectoryTag,
                &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
            )
            .unwrap();
        image.write_data(&vec![42.0; 32 * 32]).unwrap();
    }
    let (offset, length) = {
        let reader = GeoTiffStreamReader::open(file.path()).unwrap();
        let info = reader.chunk_info.as_ref().unwrap();
        (info.chunk_offsets[2], info.chunk_bytes[2])
    };
    let mut writer = OpenOptions::new().write(true).open(file.path()).unwrap();
    writer.seek(SeekFrom::Start(offset)).unwrap();
    writer.write_all(&vec![0xff; length as usize]).unwrap();
    // Metadata remains readable; only a later strip has a broken compressed payload.
    let reader = GeoTiffStreamReader::open(file.path()).unwrap();
    let mut decoder = reader.open_decoder().unwrap();
    assert!(decoder.read_chunk_with_payload(0, None, None).is_ok());
    assert!(decoder.read_chunk_with_payload(2, None, None).is_err());
    file
}

#[test]
fn corrupt_chunk_errors_are_latched_for_both_streamers() {
    let file = corrupt_payload();
    let config = MultiResolutionConfig::single(8);
    let mut continuous =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(file.path()).unwrap(), &config)
            .unwrap();
    let first = continuous.fetch_next_batch(2048).unwrap_err().to_string();
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
    assert!(categorical
        .drain_completed_into(2048, |_, _| emitted += 1)
        .is_err());
    assert!(categorical.fetch_next_batch(2048).is_err());
    assert_eq!(emitted, 0);
}

#[test]
fn duckdb_record_queue_propagates_failure_instead_of_eof() {
    let file = corrupt_payload();
    let streamer = Mutex::new(
        MultiScanHorizonStreamer::new(
            GeoTiffStreamReader::open(file.path()).unwrap(),
            &MultiResolutionConfig::single(8),
        )
        .unwrap(),
    );
    let queue = ConcurrentRecordQueue::new();
    for _ in 0..2 {
        assert!(queue
            .pop_or_refill(&streamer, |s, n, f| s.drain_completed_into(n, f))
            .is_err());
    }
    assert!(queue.ready_batches.lock().unwrap().is_empty());
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
        let mut export = ParquetExportConfig::default();
        export.is_categorical = categorical;
        export.row_group_size = 1;
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
    use raster_h3::aggregator::accumulator::H3Accumulator;
    use raster_h3::aggregator::multi_horizon::MultiContinuousRecord;
    use raster_h3::error::{RasterH3Error, Result};
    use raster_h3::parquet::writer::{
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
    let mut config = ParquetExportConfig::default();
    config.row_group_size = 1;
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
    streamer.prefetcher.take();
    assert!(streamer
        .fetch_next_batch(2048)
        .unwrap_err()
        .to_string()
        .contains("Prefetch ended after 0 of 4 chunks"));
}
