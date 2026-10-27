//! Valid samples dropped by failed or out-of-domain coordinate transforms
//! are counted, so missing pixels are visible instead of silent.

use crate::helpers::create_constant_f32_geotiff;
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use std::path::Path;

fn config(sampling: SamplingPattern, bbox: Option<[f64; 4]>) -> MultiResolutionConfig {
    MultiResolutionConfig {
        resolutions: vec![3],
        sampling,
        bbox,
        ..Default::default()
    }
}

/// (transform failures, total sample weight at the configured resolution).
fn continuous(path: &Path, cfg: &MultiResolutionConfig) -> (u64, f64) {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiScanHorizonStreamer::new(reader, cfg).unwrap();
    let mut weight = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(4096).unwrap();
        if batch.is_empty() {
            return (streamer.transform_failures(), weight);
        }
        weight += batch.iter().map(|r| r.accumulator.count).sum::<f64>();
    }
}

fn categorical(path: &Path, cfg: &MultiResolutionConfig) -> u64 {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, cfg).unwrap();
    while !streamer.fetch_next_batch(4096).unwrap().is_empty() {}
    streamer.transform_failures()
}

#[test]
fn samples_beyond_the_pole_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("past_pole.tif");
    // 10 x 20 one-degree pixels from 95N: the top 5 rows (95N..90N) lie
    // beyond the pole and have no valid H3 cell.
    create_constant_f32_geotiff(&path, 10, 20, 0.0, 95.0, 1.0, 3.0);

    let (failures, weight) = continuous(&path, &config(SamplingPattern::center(), None));
    assert_eq!(failures, 50);
    assert_eq!(weight, 150.0);
    assert_eq!(
        categorical(&path, &config(SamplingPattern::center(), None)),
        50
    );

    // Supersampling counts sample points, not pixels.
    let (failures, _) = continuous(&path, &config(SamplingPattern::rgss(), None));
    assert_eq!(failures, 5 * 10 * 4);
}

#[test]
fn valid_rasters_and_bbox_exclusions_report_no_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("valid.tif");
    create_constant_f32_geotiff(&path, 10, 20, 0.0, 60.0, 1.0, 3.0);
    let (failures, weight) = continuous(&path, &config(SamplingPattern::center(), None));
    assert_eq!((failures, weight), (0, 200.0));

    // Pixels outside a bbox are excluded on purpose, not by a failure.
    let bbox = Some([0.0, 45.0, 10.0, 60.0]);
    let (failures, weight) = continuous(&path, &config(SamplingPattern::center(), bbox));
    assert_eq!(failures, 0);
    assert!(weight < 200.0);
}

#[test]
fn failed_inverse_projections_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("laea.tif");
    // Spherical Lambert azimuthal equal-area has no inverse beyond 2R
    // (~12,756 km) from the centre. Pixel centres sit at x = 1, 3, ..., 15 Mm
    // and |y| <= 1.5 Mm, so the columns at 13 and 15 Mm fail.
    create_constant_f32_geotiff(&path, 8, 4, 0.0, 2_000_000.0, 2_000_000.0, 3.0);
    let mut cfg = config(SamplingPattern::center(), None);
    cfg.custom_crs = Some("+proj=laea +lat_0=0 +lon_0=0 +R=6378137 +units=m".into());
    let (failures, weight) = continuous(&path, &cfg);
    assert_eq!(
        failures + weight as u64,
        32,
        "every valid pixel is kept or counted"
    );
    assert_eq!(failures, 2 * 4);
}

#[test]
fn batch_readers_report_transform_failures() {
    use raster_h3_core::columnar::{
        CategoricalBatchReader, CategoricalFormat, ContinuousBatchReader,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("past_pole.tif");
    create_constant_f32_geotiff(&path, 10, 20, 0.0, 95.0, 1.0, 3.0);
    let cfg = config(SamplingPattern::center(), None);
    let paths = [path];

    let mut reader = ContinuousBatchReader::open(&paths, &cfg, None, 64, None).unwrap();
    for batch in &mut reader {
        batch.unwrap();
    }
    assert_eq!(reader.transform_failures(), 50);

    let mut reader =
        CategoricalBatchReader::open(&paths, &cfg, CategoricalFormat::Wide, None, 64, None)
            .unwrap();
    for batch in &mut reader {
        batch.unwrap();
    }
    assert_eq!(reader.transform_failures(), 50);
}

#[test]
fn exporters_report_through_the_shared_counter() {
    use raster_h3_core::parquet::{H3ParquetWriter, ParquetExportConfig};
    use raster_h3_core::pmtiles::H3PmtilesTiler;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("past_pole.tif");
    create_constant_f32_geotiff(&path, 10, 20, 0.0, 95.0, 1.0, 3.0);

    let counter = Arc::new(AtomicU64::new(0));
    let mut cfg = config(SamplingPattern::center(), None);
    cfg.transform_failure_counter = Some(Arc::clone(&counter));
    H3ParquetWriter::process_raster_source_to_parquet(
        &path,
        dir.path().join("out.parquet"),
        cfg.clone(),
        ParquetExportConfig::default(),
    )
    .unwrap();
    assert_eq!(counter.swap(0, Ordering::Relaxed), 50);

    H3PmtilesTiler::process_raster_source_to_pmtiles(&path, dir.path().join("out.pmtiles"), cfg)
        .unwrap();
    assert_eq!(counter.load(Ordering::Relaxed), 50);
}
