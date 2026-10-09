//! Bounding boxes that cross the antimeridian, and rasters whose longitudes
//! extend past 180 degrees, must keep every sample that falls inside the bbox.

mod helpers;

use std::fs::File;
use std::io::BufWriter;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::RGBA8;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use helpers::TestGeoTiffBuilder;
use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::aggregator::sampling::SamplingPattern;
use raster_h3::raster::geotiff::GeoTiffStreamReader;

const FIJI: [f64; 4] = [178.0, -20.0, -178.0, -15.0];

struct Grid {
    width: u32,
    height: u32,
    origin_lon: f64,
    origin_lat: f64,
    pixel: f64,
}

impl Grid {
    fn builder(&self) -> TestGeoTiffBuilder {
        TestGeoTiffBuilder::new(self.width, self.height)
            .origin(self.origin_lon, self.origin_lat)
            .pixel_size(self.pixel)
    }

    /// Independent reference: total sample weight whose position lies in `bbox`.
    fn expected_weight(&self, sampling: &SamplingPattern, bbox: [f64; 4]) -> f64 {
        let [min_lon, min_lat, max_lon, max_lat] = bbox;
        let mut total = 0.0;
        for row in 0..self.height {
            for col in 0..self.width {
                for sp in &sampling.points {
                    let lon = self.origin_lon + (col as f64 + sp.dx) * self.pixel;
                    let lat = self.origin_lat - (row as f64 + sp.dy) * self.pixel;
                    let lon = (lon + 180.0).rem_euclid(360.0) - 180.0;
                    let lon_in = if min_lon <= max_lon {
                        lon >= min_lon && lon <= max_lon
                    } else {
                        lon >= min_lon || lon <= max_lon
                    };
                    if lon_in && lat >= min_lat && lat <= max_lat {
                        total += sp.weight;
                    }
                }
            }
        }
        total
    }
}

fn config(bbox: [f64; 4], sampling: SamplingPattern) -> MultiResolutionConfig {
    let mut config = MultiResolutionConfig::single(5);
    config.resolutions = vec![4, 6];
    config.bbox = Some(bbox);
    config.sampling = sampling;
    config
}

fn continuous_weights(path: &std::path::Path, config: &MultiResolutionConfig) -> Vec<f64> {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiScanHorizonStreamer::new(reader, config).unwrap();
    let mut totals = vec![0.0; config.resolutions.len()];
    loop {
        let batch = streamer.fetch_next_batch(256).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let i = config
                .resolutions
                .iter()
                .position(|&r| r == rec.resolution)
                .unwrap();
            totals[i] += rec.accumulator.count;
        }
    }
    totals
}

fn categorical_weights(path: &std::path::Path, config: &MultiResolutionConfig) -> Vec<f64> {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, config).unwrap();
    let mut totals = vec![0.0; config.resolutions.len()];
    loop {
        let batch = streamer.fetch_next_batch(256).unwrap();
        if batch.is_empty() {
            break;
        }
        for rec in batch {
            let i = config
                .resolutions
                .iter()
                .position(|&r| r == rec.resolution)
                .unwrap();
            totals[i] += rec.accumulator.total_count;
        }
    }
    totals
}

fn assert_weights(actual: &[f64], expected: f64, label: &str) {
    assert!(expected > 0.0, "{label}: test grid must intersect the bbox");
    for (i, total) in actual.iter().enumerate() {
        assert!(
            (total - expected).abs() < 1e-6,
            "{label}: resolution #{i} kept weight {total}, expected {expected}"
        );
    }
}

fn check_grid(grid: &Grid, bbox: [f64; 4], label: &str) {
    let tmp = NamedTempFile::new().unwrap();
    grid.builder()
        .write_f32_fn(tmp.path(), |c, r| (c + r) as f32)
        .unwrap();
    let gray = NamedTempFile::new().unwrap();
    grid.builder()
        .write_gray8_fn(gray.path(), |c, _| (c % 7) as u8 + 1)
        .unwrap();
    for (name, sampling) in [
        ("center", SamplingPattern::center()),
        ("rgss", SamplingPattern::rgss()),
        ("9point", SamplingPattern::nine_point()),
    ] {
        let expected = grid.expected_weight(&sampling, bbox);
        let config = config(bbox, sampling);
        assert_weights(
            &continuous_weights(tmp.path(), &config),
            expected,
            &format!("{label}/continuous/{name}"),
        );
        assert_weights(
            &categorical_weights(gray.path(), &config),
            expected,
            &format!("{label}/categorical/{name}"),
        );
    }
}

#[test]
fn antimeridian_bbox_on_grid_ending_at_180() {
    let grid = Grid {
        width: 200,
        height: 20,
        origin_lon: 160.0,
        origin_lat: -16.0,
        pixel: 0.1,
    };
    check_grid(&grid, FIJI, "160..180");
}

#[test]
fn antimeridian_bbox_on_grid_crossing_180() {
    let grid = Grid {
        width: 200,
        height: 20,
        origin_lon: 170.0,
        origin_lat: -16.0,
        pixel: 0.1,
    };
    check_grid(&grid, FIJI, "170..190");
}

#[test]
fn western_bbox_on_grid_crossing_180() {
    let grid = Grid {
        width: 200,
        height: 20,
        origin_lon: 170.0,
        origin_lat: -16.0,
        pixel: 0.1,
    };
    check_grid(&grid, [-179.5, -20.0, -175.0, -15.0], "170..190/west");
}

#[test]
fn bbox_on_0_to_360_grid() {
    // Longitudes 100..300: the bbox below is 260..280 in this grid's convention.
    let grid = Grid {
        width: 200,
        height: 4,
        origin_lon: 100.0,
        origin_lat: 2.0,
        pixel: 1.0,
    };
    check_grid(&grid, [-100.0, -1.5, -80.0, 1.5], "0..360");
}

fn create_rgba_geotiff(grid: &Grid) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let mut data = Vec::with_capacity((grid.width * grid.height * 4) as usize);
    for _ in 0..grid.width * grid.height {
        data.extend_from_slice(&[10, 20, 30, 40]);
    }
    let writer = BufWriter::new(File::create(tmp.path()).unwrap());
    let mut encoder = TiffEncoder::new(writer).unwrap();
    let mut image = encoder.new_image::<RGBA8>(grid.width, grid.height).unwrap();
    let tiepoint = [0.0, 0.0, 0.0, grid.origin_lon, grid.origin_lat, 0.0];
    image
        .encoder()
        .write_tag(Tag::Unknown(33922), &tiepoint[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[grid.pixel, grid.pixel, 0.0][..])
        .unwrap();
    let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])
        .unwrap();
    image.write_data(&data).unwrap();
    tmp
}

#[test]
fn antimeridian_bbox_on_multiband_raster() {
    let grid = Grid {
        width: 200,
        height: 20,
        origin_lon: 160.0,
        origin_lat: -16.0,
        pixel: 0.1,
    };
    let tmp = create_rgba_geotiff(&grid);
    for band in [1, 2] {
        for sampling in [SamplingPattern::center(), SamplingPattern::rgss()] {
            let expected = grid.expected_weight(&sampling, FIJI);
            let mut config = config(FIJI, sampling);
            config.band = band;
            assert_weights(
                &continuous_weights(tmp.path(), &config),
                expected,
                &format!("multiband/continuous/band{band}"),
            );
            assert_weights(
                &categorical_weights(tmp.path(), &config),
                expected,
                &format!("multiband/categorical/band{band}"),
            );
        }
    }
}
