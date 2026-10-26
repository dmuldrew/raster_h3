//! Band calibration and validity: GDAL `SCALE`/`OFFSET` and TIFF alpha.
//!
//! Continuous statistics, quantiles and spectral indices use physical values
//! (`stored * scale + offset`), nodata is matched on stored values, and
//! pixels whose alpha sample is zero carry no observation.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::{ExtraSamples, Tag};

use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
    QuantileTarget, SpectralFormula,
};
use raster_h3_core::aggregator::H3Accumulator;
use raster_h3_core::columnar::{CategoricalBatchReader, CategoricalFormat, ContinuousBatchReader};
use raster_h3_core::error::RasterH3Error;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::mosaic::{MosaicReader, OverlapRule};

/// Optional GDAL/TIFF tags written by [`write_tiff!`].
#[derive(Default)]
struct Tags<'a> {
    gdal_metadata: Option<&'a str>,
    gdal_nodata: Option<&'a str>,
    extra_samples: Option<&'a [u16]>,
}

macro_rules! write_tiff {
    ($colortype:ty, $path:expr, $w:expr, $h:expr, $origin:expr, $data:expr, $tags:expr) => {{
        let (w, h, origin, tags): (u32, u32, (f64, f64), &Tags<'_>) = ($w, $h, $origin, $tags);
        let file = std::fs::File::create($path).unwrap();
        let mut encoder = TiffEncoder::new(std::io::BufWriter::new(file)).unwrap();
        let mut image = encoder.new_image::<$colortype>(w, h).unwrap();
        if let Some(extra) = tags.extra_samples {
            let extra: Vec<ExtraSamples> = extra
                .iter()
                .map(|&k| ExtraSamples::from_u16(k).unwrap())
                .collect();
            image.extra_samples(&extra).unwrap();
        }
        let enc = image.encoder();
        enc.write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, origin.0, origin.1, 0.0][..],
        )
        .unwrap();
        enc.write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();
        let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        enc.write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..])
            .unwrap();
        if let Some(xml) = tags.gdal_metadata {
            enc.write_tag(Tag::Unknown(42112), xml).unwrap();
        }
        if let Some(nd) = tags.gdal_nodata {
            enc.write_tag(Tag::Unknown(42113), nd).unwrap();
        }
        image.write_data($data).unwrap();
    }};
}

fn gdal_metadata(items: &[(&str, usize, f64)]) -> String {
    let body: String = items
        .iter()
        .map(|(role, sample, v)| {
            format!(
                "  <Item name=\"{}\" sample=\"{sample}\" role=\"{role}\">{v}</Item>\n",
                role.to_uppercase()
            )
        })
        .collect();
    format!("<GDALMetadata>\n  <Item name=\"AREA_OR_POINT\">Area</Item>\n{body}</GDALMetadata>\n")
}

fn drain(mut streamer: MultiScanHorizonStreamer) -> HashMap<(u8, u64), H3Accumulator> {
    let mut cells = HashMap::new();
    loop {
        let batch = streamer.fetch_next_batch(4096).unwrap();
        if batch.is_empty() {
            return cells;
        }
        for r in batch {
            assert!(
                cells
                    .insert((r.resolution, r.h3_index), r.accumulator)
                    .is_none(),
                "duplicate cell emitted"
            );
        }
    }
}

fn run(path: &Path, config: &MultiResolutionConfig) -> HashMap<(u8, u64), H3Accumulator> {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    drain(MultiScanHorizonStreamer::new(reader, config).unwrap())
}

fn config(apply_scale_offset: bool) -> MultiResolutionConfig {
    MultiResolutionConfig {
        resolutions: vec![7, 9],
        apply_scale_offset,
        quantiles: vec![QuantileTarget::Percentile(0.5, "p50".into())],
        ..Default::default()
    }
}

/// Stored u16 values with a nodata sentinel (0) on a diagonal pattern.
fn stored_u16(w: usize, h: usize) -> Vec<u16> {
    (0..h)
        .flat_map(|r| {
            (0..w).map(move |c| {
                if (c + 2 * r) % 13 == 0 {
                    0
                } else {
                    100 + ((c * 7 + r * 3) % 900) as u16
                }
            })
        })
        .collect()
}

fn assert_calibrated(scale: f64, offset: f64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scaled.tif");
    let (w, h) = (96u32, 80u32);
    let xml = gdal_metadata(&[("scale", 0, scale), ("offset", 0, offset)]);
    let tags = Tags {
        gdal_metadata: Some(&xml),
        gdal_nodata: Some("0"),
        ..Default::default()
    };
    let data = stored_u16(w as usize, h as usize);
    write_tiff!(
        colortype::Gray16,
        &path,
        w,
        h,
        (-122.45, 37.80),
        &data[..],
        &tags
    );

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    assert_eq!(reader.metadata.band_scaling(1).scale, scale);
    assert_eq!(reader.metadata.band_scaling(1).offset, offset);

    let raw = run(&path, &config(false));
    let physical = run(&path, &config(true));
    assert!(!raw.is_empty());
    assert_eq!(raw.len(), physical.len());
    let expected_valid = data.iter().filter(|&&v| v != 0).count() as f64;
    let total: f64 = physical
        .iter()
        .filter(|(k, _)| k.0 == 9)
        .map(|(_, a)| a.count)
        .sum();
    assert_eq!(total, expected_valid, "nodata is matched on stored values");

    for (key, r) in &raw {
        let p = &physical[key];
        let tol = |x: f64| 1e-9 * x.abs().max(1.0);
        assert_eq!(p.count, r.count);
        assert!((p.mean() - (r.mean() * scale + offset)).abs() < tol(p.mean()));
        assert!((p.sum - (r.sum * scale + offset * r.count)).abs() < tol(p.sum) * r.count);
        let (lo, hi) = (r.min * scale + offset, r.max * scale + offset);
        let (lo, hi) = if scale < 0.0 { (hi, lo) } else { (lo, hi) };
        assert!((p.min - lo).abs() < tol(lo) && (p.max - hi).abs() < tol(hi));
        assert!((p.variance() - r.variance() * scale * scale).abs() < 1e-6 * p.variance().max(1.0));
        // The quantile sketch has bounded relative error; compare on the
        // physical value range rather than bit-for-bit.
        let range = (p.max - p.min).abs().max(1e-12);
        let expected_p50 = r.quantile(0.5) * scale + offset;
        assert!(
            (p.quantile(0.5) - expected_p50).abs() <= 0.03 * range + 0.03 * expected_p50.abs(),
            "p50 {} vs expected {expected_p50}",
            p.quantile(0.5)
        );
    }
}

#[test]
fn scale_and_offset_calibrate_continuous_statistics() {
    assert_calibrated(0.01, 5.0);
}

#[test]
fn negative_scale_swaps_min_and_max() {
    assert_calibrated(-0.5, 1000.0);
}

#[test]
fn spectral_indices_use_per_band_physical_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rgbn.tif");
    let (w, h) = (32u32, 32u32);
    let data: Vec<u8> = (0..w * h).flat_map(|_| [50u8, 100, 150, 200]).collect();
    // NIR (sample 3) stored 200 -> physical 100; red stays 50.
    let xml = gdal_metadata(&[("scale", 3, 0.5)]);
    let tags = Tags {
        gdal_metadata: Some(&xml),
        ..Default::default()
    };
    write_tiff!(
        colortype::RGBA8,
        &path,
        w,
        h,
        (-122.45, 37.80),
        &data[..],
        &tags
    );

    let mut cfg = config(true);
    cfg.quantiles.clear();
    cfg.spectral_formula = Some(SpectralFormula::Ndvi {
        nir_band: 4,
        red_band: 1,
    });
    for acc in run(&path, &cfg).values() {
        assert!((acc.mean() - 50.0 / 150.0).abs() < 1e-12, "{}", acc.mean());
    }
    cfg.apply_scale_offset = false;
    for acc in run(&path, &cfg).values() {
        assert!((acc.mean() - 150.0 / 250.0).abs() < 1e-12, "{}", acc.mean());
    }
}

fn write_alpha_tiff(path: &Path, extra_samples: &[u16]) -> (u32, u32) {
    let (w, h) = (40u32, 30u32);
    // Left half transparent (alpha 0), right half opaque.
    let data: Vec<u8> = (0..h)
        .flat_map(|_| (0..w).flat_map(|c| [7u8, 9, 11, if c < w / 2 { 0 } else { 255 }]))
        .collect();
    let tags = Tags {
        extra_samples: Some(extra_samples),
        ..Default::default()
    };
    // RGB plus one extra sample, as GDAL writes RGBA (PHOTOMETRIC=RGB).
    write_tiff!(
        colortype::RGB8,
        path,
        w,
        h,
        (-122.45, 37.80),
        &data[..],
        &tags
    );
    (w, h)
}

fn categorical_total(path: &Path, cfg: &MultiResolutionConfig) -> f64 {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, cfg).unwrap();
    let mut total = 0.0;
    loop {
        let batch = streamer.fetch_next_batch(4096).unwrap();
        if batch.is_empty() {
            return total;
        }
        total += batch
            .iter()
            .filter(|r| r.resolution == 9)
            .map(|r| r.accumulator.total_count)
            .sum::<f64>();
    }
}

#[test]
fn zero_alpha_pixels_have_no_observation() {
    let dir = tempfile::tempdir().unwrap();
    for kind in [2u16, 1] {
        let path = dir.path().join(format!("alpha_{kind}.tif"));
        let (w, h) = write_alpha_tiff(&path, &[kind]);
        let reader = GeoTiffStreamReader::open(&path).unwrap();
        assert_eq!(reader.metadata.alpha_sample, Some(3));

        let expected = (w * h / 2) as f64;
        let mut cfg = config(true);
        cfg.quantiles.clear();
        cfg.band = 1;
        let continuous: f64 = run(&path, &cfg)
            .iter()
            .filter(|(k, _)| k.0 == 9)
            .map(|(_, a)| a.count)
            .sum();
        assert_eq!(continuous, expected, "continuous, ExtraSamples = {kind}");
        assert_eq!(
            categorical_total(&path, &cfg),
            expected,
            "categorical, ExtraSamples = {kind}"
        );
    }
}

#[test]
fn unspecified_extra_samples_are_data_not_alpha() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rgbn.tif");
    write_alpha_tiff(&path, &[0]);
    let reader = GeoTiffStreamReader::open(&path).unwrap();
    assert_eq!(reader.metadata.samples_per_pixel, 4);
    assert_eq!(reader.metadata.alpha_sample, None);
}

#[test]
fn categorical_classes_are_not_scaled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("classes.tif");
    let data = vec![42u16; 20 * 20];
    let xml = gdal_metadata(&[("scale", 0, 0.1), ("offset", 0, 3.0)]);
    let tags = Tags {
        gdal_metadata: Some(&xml),
        ..Default::default()
    };
    write_tiff!(
        colortype::Gray16,
        &path,
        20,
        20,
        (-122.45, 37.80),
        &data[..],
        &tags
    );
    let reader = GeoTiffStreamReader::open(&path).unwrap();
    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config(true)).unwrap();
    let batch = streamer.fetch_next_batch(4096).unwrap();
    assert!(!batch.is_empty());
    for r in batch {
        assert_eq!(r.accumulator.majority().0, 42);
    }
}

#[test]
fn mosaic_tiles_use_their_own_calibration() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.tif"), dir.path().join("b.tif"));
    let data = vec![10u16; 50 * 50];
    let xml_b = gdal_metadata(&[("scale", 0, 2.0), ("offset", 0, 1.0)]);
    write_tiff!(
        colortype::Gray16,
        &a,
        50,
        50,
        (-122.50, 37.85),
        &data[..],
        &Tags::default()
    );
    let tags_b = Tags {
        gdal_metadata: Some(&xml_b),
        ..Default::default()
    };
    write_tiff!(
        colortype::Gray16,
        &b,
        50,
        50,
        (-122.45, 37.85),
        &data[..],
        &tags_b
    );

    let mosaic = Arc::new(MosaicReader::open(&[a, b], None, None, OverlapRule::Cutline).unwrap());
    let mut cfg = config(true);
    cfg.quantiles.clear();
    cfg.resolutions = vec![9];
    let cells = drain(MultiScanHorizonStreamer::new_mosaic(mosaic, &cfg).unwrap());
    let count: f64 = cells.values().map(|a| a.count).sum();
    let sum: f64 = cells.values().map(|a| a.sum).sum();
    assert_eq!(count, 5000.0);
    assert!(
        (sum - (2500.0 * 10.0 + 2500.0 * 21.0)).abs() < 1e-6,
        "{sum}"
    );
    for acc in cells.values() {
        assert!(acc.min >= 10.0 && acc.max <= 21.0);
    }
}

#[test]
fn malformed_calibration_metadata_is_rejected_in_all_modes() {
    let dir = tempfile::tempdir().unwrap();
    for (name, xml) in [
        ("nan", gdal_metadata(&[("scale", 0, f64::NAN)])),
        ("zero", gdal_metadata(&[("scale", 0, 0.0)])),
        (
            "infinite_offset",
            gdal_metadata(&[("offset", 0, f64::INFINITY)]),
        ),
        (
            "non_numeric_scale",
            "<GDALMetadata><Item sample=\"0\" role=\"scale\">abc</Item></GDALMetadata>".into(),
        ),
        ("band", gdal_metadata(&[("offset", 4, 1.0)])),
    ] {
        let path = dir.path().join(format!("{name}.tif"));
        let tags = Tags {
            gdal_metadata: Some(&xml),
            ..Default::default()
        };
        write_tiff!(
            colortype::Gray16,
            &path,
            4,
            4,
            (-122.45, 37.80),
            &[1u16; 16][..],
            &tags
        );
        let error = GeoTiffStreamReader::open(&path).err().expect(name);
        assert!(matches!(error, RasterH3Error::InvalidMetadata(_)));
        for apply_scale_offset in [true, false] {
            let cfg = config(apply_scale_offset);
            let paths = [path.clone()];
            let continuous = ContinuousBatchReader::open(&paths, &cfg, None, 4096, None);
            let categorical = CategoricalBatchReader::open(
                &paths,
                &cfg,
                CategoricalFormat::Wide,
                None,
                4096,
                None,
            );
            for error in [continuous.err(), categorical.err()] {
                let error = error.unwrap_or_else(|| {
                    panic!("{name} accepted with apply_scale_offset={apply_scale_offset}")
                });
                assert!(
                    matches!(&error, RasterH3Error::InvalidMetadata(message) if message.contains("GDAL_METADATA")),
                    "{name} with apply_scale_offset={apply_scale_offset}: {error}"
                );
            }
        }
    }
}
