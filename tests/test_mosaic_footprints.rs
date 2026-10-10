//! Mosaic initialization (the path SQL uses even for a single file) must
//! filter tiles with antimeridian-aware bbox logic, and overlap rules must
//! test tile footprints rather than WGS84 bounding rectangles.

mod helpers;

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use helpers::TestGeoTiffBuilder;
use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::raster::mosaic::{MosaicReader, OverlapRule};
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

fn total_count(mut streamer: MultiScanHorizonStreamer) -> f64 {
    let mut total = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(10_000).unwrap();
        if batch.is_empty() {
            return total;
        }
        total += batch.iter().map(|r| r.accumulator.count).sum::<f64>();
    }
}

fn mosaic_count(paths: &[PathBuf], bbox: Option<[f64; 4]>, rule: OverlapRule) -> f64 {
    let mosaic = Arc::new(MosaicReader::open(paths, bbox, None, rule).unwrap());
    let mut config = MultiResolutionConfig::new(vec![9]);
    config.bbox = bbox;
    config.overlap_rule = rule;
    total_count(MultiScanHorizonStreamer::new_mosaic(mosaic, &config).unwrap())
}

#[test]
fn antimeridian_bbox_keeps_tile_through_mosaic_open() {
    let (_tmp, path) = TestGeoTiffBuilder::new(10, 10)
        .origin(178.0, -16.0)
        .pixel_size(0.1)
        .create_constant_tempfile(1.0f32);
    let fiji = [178.0, -20.0, -178.0, -15.0];

    let mut config = MultiResolutionConfig::new(vec![9]);
    config.bbox = Some(fiji);
    let single =
        MultiScanHorizonStreamer::new(GeoTiffStreamReader::open(&path).unwrap(), &config).unwrap();
    assert_eq!(total_count(single), 100.0);

    for rule in [
        OverlapRule::Cutline,
        OverlapRule::First,
        OverlapRule::Average,
    ] {
        assert_eq!(
            mosaic_count(std::slice::from_ref(&path), Some(fiji), rule),
            100.0,
            "{rule:?}"
        );
    }
    // East of the antimeridian only: still rejected.
    assert!(MosaicReader::open(
        &[path],
        Some([-179.0, -20.0, -178.0, -15.0]),
        None,
        OverlapRule::First
    )
    .is_err());
}

/// Write a 10x10 EPSG:4326 Float32 GeoTIFF with an affine
/// ModelTransformationTag (`x = c0 + a*col + b*row`, `y = f0 + d*col + e*row`).
fn write_affine_tiff(path: &Path, gt: [f64; 6]) {
    let [c0, a, b, f0, d, e] = gt;
    let matrix = [
        a, b, 0.0, c0, d, e, 0.0, f0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
    let mut image = encoder.new_image::<colortype::Gray32Float>(10, 10).unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(34264), &matrix[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])
        .unwrap();
    image.write_data(&[1.0f32; 100]).unwrap();
}

#[test]
fn overlap_rules_use_footprints_not_bounding_rectangles() {
    let dir = tempfile::tempdir().unwrap();
    // Diamond with corners (0,1), (1,2), (2,1), (1,0): its bbox [0,2]x[0,2]
    // covers the square below, but its footprint does not.
    let diamond = dir.path().join("diamond.tif");
    write_affine_tiff(&diamond, [0.0, 0.1, 0.1, 1.0, 0.1, -0.1]);
    let square = dir.path().join("square.tif");
    write_affine_tiff(&square, [0.1, 0.01, 0.0, 0.2, 0.0, -0.01]);
    let paths = vec![diamond, square];

    for rule in [
        OverlapRule::First,
        OverlapRule::Cutline,
        OverlapRule::Average,
    ] {
        assert_eq!(mosaic_count(&paths, None, rule), 200.0, "{rule:?}");
    }
}

#[test]
fn first_rule_still_suppresses_true_overlap_inside_rotated_footprint() {
    let dir = tempfile::tempdir().unwrap();
    let diamond = dir.path().join("diamond.tif");
    write_affine_tiff(&diamond, [0.0, 0.1, 0.1, 1.0, 0.1, -0.1]);
    // 10x10 square [0.95,1.05]^2 at the diamond's centre: wholly inside it.
    let inner = dir.path().join("inner.tif");
    write_affine_tiff(&inner, [0.95, 0.01, 0.0, 1.05, 0.0, -0.01]);
    let paths = vec![diamond, inner];

    assert_eq!(mosaic_count(&paths, None, OverlapRule::First), 100.0);
    assert_eq!(mosaic_count(&paths, None, OverlapRule::Average), 200.0);
}
