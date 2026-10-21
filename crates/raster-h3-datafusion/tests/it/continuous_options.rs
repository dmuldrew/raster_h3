//! `ContinuousRasterTableProvider` builder options: each scan must equal a core
//! `ContinuousBatchReader` run with the same `MultiResolutionConfig`, and the option must
//! actually change the output.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use raster_h3_core::aggregator::multi_horizon::{
    MultiResolutionConfig, QuantileTarget, SpectralFormula,
};
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::raster::mosaic::OverlapRule;
use raster_h3_datafusion::{ContinuousRasterTableProvider, FilterPredicate, TableProvider};
use tempfile::NamedTempFile;
use tiff::encoder::colortype::RGBA8;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use crate::helpers::TestGeoTiffBuilder;
use crate::rows::{column_f64, core_continuous, plan_table};

/// 32x32 f32 gradient with values 1..=10, 0.001° pixels (several res-8 cells).
fn gradient_tif() -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(32, 32)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| ((c + r) % 10 + 1) as f32)
}

fn provider(path: &Path, res: Vec<u8>) -> ContinuousRasterTableProvider {
    ContinuousRasterTableProvider::try_new(path.to_str().unwrap(), res).unwrap()
}

fn cfg(res: Vec<u8>) -> MultiResolutionConfig {
    MultiResolutionConfig::new(res)
}

fn total(table: &(Vec<String>, Vec<Vec<String>>), col: &str) -> f64 {
    column_f64(table, col).iter().sum()
}

/// 4-band RGBA8 GeoTIFF whose bands vary per pixel: R = c, G = 2r, B = 50, A = 100 + c + r.
fn rgba_tif(width: u32, height: u32) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for r in 0..height {
        for c in 0..width {
            data.extend_from_slice(&[c as u8, (2 * r) as u8, 50, (100 + c + r) as u8]);
        }
    }
    let writer = BufWriter::new(File::create(tmp.path()).unwrap());
    let mut encoder = TiffEncoder::new(writer).unwrap();
    let mut image = encoder.new_image::<RGBA8>(width, height).unwrap();
    image
        .encoder()
        .write_tag(
            Tag::ModelTiepointTag,
            &[0.0f64, 0.0, 0.0, -122.45, 37.80, 0.0][..],
        )
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
        .unwrap();
    image
        .encoder()
        .write_tag(
            Tag::GeoKeyDirectoryTag,
            &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
        )
        .unwrap();
    image.write_data(&data).unwrap();
    tmp
}

#[test]
fn with_nodata_drops_matching_pixels_like_core() {
    let (_tmp, path) = gradient_tif();
    let paths = vec![path.clone()];

    let plain = plan_table(provider(&path, vec![8]).scan(None, &[], None).unwrap());
    let masked = plan_table(
        provider(&path, vec![8])
            .with_nodata(5.0)
            .scan(None, &[], None)
            .unwrap(),
    );
    let mut c = cfg(vec![8]);
    c.custom_nodata = Some(5.0);
    assert_eq!(masked, core_continuous(&paths, &c));

    // Exactly the pixels equal to 5 disappear from the counts.
    let fives = (0..32u32)
        .flat_map(|r| (0..32u32).map(move |c| (c + r) % 10 + 1))
        .filter(|&v| v == 5)
        .count() as f64;
    assert_eq!(total(&plain, "count") - total(&masked, "count"), fives);
    assert_eq!(total(&plain, "sum") - total(&masked, "sum"), 5.0 * fives);
}

#[test]
fn with_sampling_matches_core_and_changes_cell_statistics() {
    let (_tmp, path) = gradient_tif();
    let paths = vec![path.clone()];

    let center = plan_table(provider(&path, vec![9]).scan(None, &[], None).unwrap());
    let rgss = plan_table(
        provider(&path, vec![9])
            .with_sampling(SamplingPattern::rgss())
            .scan(None, &[], None)
            .unwrap(),
    );
    let mut c = cfg(vec![9]);
    c.sampling = SamplingPattern::rgss();
    assert_eq!(rgss, core_continuous(&paths, &c));
    assert_ne!(
        rgss, center,
        "4-point sampling must redistribute pixel mass"
    );
    // Each pixel still contributes a total weight of one.
    assert!((total(&rgss, "count") - total(&center, "count")).abs() < 1e-6);
}

#[test]
fn with_band_selects_the_band_like_core() {
    let tif = rgba_tif(32, 32);
    let path = tif.path().to_path_buf();
    let paths = vec![path.clone()];

    let mut by_band = Vec::new();
    for band in 1..=4 {
        let got = plan_table(
            provider(&path, vec![8])
                .with_band(band)
                .scan(None, &[], None)
                .unwrap(),
        );
        let mut c = cfg(vec![8]);
        c.band = band;
        assert_eq!(got, core_continuous(&paths, &c), "band {band}");
        by_band.push(got);
    }
    // Band 3 is a constant 50; the other bands differ from each other.
    assert!(column_f64(&by_band[2], "mean").iter().all(|&m| m == 50.0));
    assert_ne!(by_band[0], by_band[1]);
    assert_ne!(by_band[1], by_band[3]);

    // Band 0 is clamped to the first band.
    let zero = plan_table(
        provider(&path, vec![8])
            .with_band(0)
            .scan(None, &[], None)
            .unwrap(),
    );
    assert_eq!(zero, by_band[0]);
}

#[test]
fn with_spectral_formula_matches_core() {
    let tif = rgba_tif(32, 32);
    let path = tif.path().to_path_buf();
    let paths = vec![path.clone()];
    let formula = SpectralFormula::Ndvi {
        nir_band: 4,
        red_band: 1,
    };

    let got = plan_table(
        provider(&path, vec![8])
            .with_spectral_formula(formula)
            .scan(None, &[], None)
            .unwrap(),
    );
    let mut c = cfg(vec![8]);
    c.spectral_formula = Some(formula);
    assert_eq!(got, core_continuous(&paths, &c));

    // NDVI = (A - R) / (A + R) with A = 100 + c + r > R = c, so it lies in (0, 1].
    let means = column_f64(&got, "mean");
    assert!(!means.is_empty());
    assert!(means.iter().all(|&m| m > 0.0 && m <= 1.0), "{means:?}");
}

#[test]
fn with_quantiles_adds_columns_matching_core() {
    let (_tmp, path) = gradient_tif();
    let paths = vec![path.clone()];
    let targets = QuantileTarget::parse_list("p25,p50,iqr").unwrap();

    let p = provider(&path, vec![8]).with_quantiles(targets.clone());
    let names: Vec<String> = p.schema().fields.iter().map(|f| f.name.clone()).collect();
    for q in ["p25", "p50", "iqr"] {
        assert!(names.iter().any(|n| n == q), "missing {q} in {names:?}");
    }
    let got = plan_table(p.scan(None, &[], None).unwrap());
    let mut c = cfg(vec![8]);
    c.quantiles = targets;
    assert_eq!(got, core_continuous(&paths, &c));

    let (p25, p50) = (column_f64(&got, "p25"), column_f64(&got, "p50"));
    let (min, max) = (column_f64(&got, "min"), column_f64(&got, "max"));
    for i in 0..p50.len() {
        // DDSketch keeps a 1% relative error.
        assert!(p25[i] <= p50[i] * 1.01);
        assert!(p50[i] >= min[i] * 0.99 && p50[i] <= max[i] * 1.01);
    }

    // Projecting a quantile column by index works like any other column.
    let p50_idx = names.iter().position(|n| n == "p50").unwrap();
    let projected = plan_table(p.scan(Some(&[0, p50_idx]), &[], None).unwrap());
    assert_eq!(projected.0, vec!["h3_index".to_string(), "p50".to_string()]);
    assert_eq!(projected.1.len(), got.1.len());
}

#[test]
fn builder_value_filters_match_core_and_intersect_with_scan_filters() {
    let (_tmp, path) = gradient_tif();
    let paths = vec![path.clone()];

    let unfiltered = plan_table(provider(&path, vec![9]).scan(None, &[], None).unwrap());
    let means = column_f64(&unfiltered, "mean");
    let counts = column_f64(&unfiltered, "count");
    let mut sorted = means.clone();
    sorted.sort_by(f64::total_cmp);
    // Bounds inside the spread of cell means so each filter removes some rows.
    let lo = sorted[sorted.len() / 4];
    let hi = sorted[sorted.len() * 3 / 4];
    let mid_count = {
        let mut c = counts.clone();
        c.sort_by(f64::total_cmp);
        c[c.len() / 2]
    };

    let base = provider(&path, vec![9])
        .with_min_count(mid_count)
        .with_min_mean(lo)
        .with_max_mean(hi);

    let expect = |min_mean: f64, max_mean: f64, min_count: f64| {
        let mut c = cfg(vec![9]);
        c.min_count = Some(min_count);
        c.min_mean = Some(min_mean);
        c.max_mean = Some(max_mean);
        core_continuous(&paths, &c)
    };

    let own = plan_table(base.scan(None, &[], None).unwrap());
    assert_eq!(own, expect(lo, hi, mid_count));
    assert!(!own.1.is_empty() && own.1.len() < unfiltered.1.len());
    for (m, n) in column_f64(&own, "mean")
        .iter()
        .zip(column_f64(&own, "count"))
    {
        assert!(*m >= lo && *m <= hi && n >= mid_count);
    }

    // A pushed MinMean/MaxMean intersects the builder value: the stricter bound wins.
    let tighter_lo = (lo + hi) / 2.0;
    let looser_lo = lo - 100.0;
    let tighter_hi = (lo + hi) / 2.0 + 0.25;
    let looser_hi = hi + 100.0;
    let cases = [
        (
            vec![FilterPredicate::MinMean(tighter_lo)],
            (tighter_lo, hi, mid_count),
        ),
        (
            vec![FilterPredicate::MinMean(looser_lo)],
            (lo, hi, mid_count),
        ),
        (
            vec![FilterPredicate::MaxMean(tighter_hi)],
            (lo, tighter_hi, mid_count),
        ),
        (
            vec![FilterPredicate::MaxMean(looser_hi)],
            (lo, hi, mid_count),
        ),
        (
            vec![FilterPredicate::MinCount(mid_count + 1.0)],
            (lo, hi, mid_count + 1.0),
        ),
        (vec![FilterPredicate::MinCount(1.0)], (lo, hi, mid_count)),
        (
            vec![
                FilterPredicate::MinMean(looser_lo),
                FilterPredicate::MaxMean(tighter_hi),
            ],
            (lo, tighter_hi, mid_count),
        ),
    ];
    for (filters, (min_mean, max_mean, min_count)) in cases {
        let got = plan_table(base.scan(None, &filters, None).unwrap());
        assert_eq!(got, expect(min_mean, max_mean, min_count), "{filters:?}");
    }

    // A pushed range disjoint from the builder's yields nothing.
    let disjoint = plan_table(
        base.scan(None, &[FilterPredicate::MinMean(hi + 1.0)], None)
            .unwrap(),
    );
    assert!(disjoint.1.is_empty());
}

#[test]
fn with_compact_matches_core_and_conserves_mass() {
    let (_tmp, path) = TestGeoTiffBuilder::new(128, 128)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| ((c * 2 + r) % 150 + 20) as f32);
    let paths = vec![path.clone()];

    let plain = plan_table(provider(&path, vec![8]).scan(None, &[], None).unwrap());
    let compact = plan_table(
        provider(&path, vec![8])
            .with_compact(true)
            .scan(None, &[], None)
            .unwrap(),
    );
    let mut c = cfg(vec![8]);
    c.compact = true;
    c.compact_h3_children = true;
    assert_eq!(compact, core_continuous(&paths, &c));

    assert!(compact.1.len() < plain.1.len());
    assert!(column_f64(&compact, "resolution").contains(&7.0));
    assert!((total(&compact, "count") - total(&plain, "count")).abs() < 1e-6);
    assert!((total(&compact, "sum") - total(&plain, "sum")).abs() < 1e-3);

    // `with_compact(false)` restores plain output.
    let off = plan_table(
        provider(&path, vec![8])
            .with_compact(true)
            .with_compact(false)
            .scan(None, &[], None)
            .unwrap(),
    );
    assert_eq!(off, plain);

    // Compacting adjacent resolutions would emit duplicate parents and is rejected.
    let plan = provider(&path, vec![7, 8])
        .with_compact(true)
        .scan(None, &[], None)
        .unwrap();
    assert!(plan.execute().is_err());
}

#[test]
fn with_overlap_rule_on_a_multi_file_source_matches_core() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.tif");
    let b = dir.path().join("b.tif");
    // b starts 16 pixels east of a, so half of each tile overlaps the other.
    TestGeoTiffBuilder::new(32, 32)
        .pixel_size(0.001)
        .write_f32_constant(&a, 10.0)
        .unwrap();
    TestGeoTiffBuilder::new(32, 32)
        .pixel_size(0.001)
        .origin(-122.45 + 16.0 * 0.001, 37.80)
        .write_f32_constant(&b, 30.0)
        .unwrap();
    let source = format!("{},{}", a.display(), b.display());
    let paths = vec![a.clone(), b.clone()];

    let mut by_rule = Vec::new();
    for rule in [
        OverlapRule::Cutline,
        OverlapRule::First,
        OverlapRule::Average,
    ] {
        let p = ContinuousRasterTableProvider::try_new(&source, vec![8])
            .unwrap()
            .with_overlap_rule(rule);
        assert_eq!(p.paths, paths);
        let got = plan_table(p.scan(None, &[], None).unwrap());
        let mut c = cfg(vec![8]);
        c.overlap_rule = rule;
        assert_eq!(got, core_continuous(&paths, &c), "{rule:?}");
        by_rule.push(got);
    }
    let (cutline, first, average) = (&by_rule[0], &by_rule[1], &by_rule[2]);

    // `first` and `cutline` keep one observation per point: 1024 + 512 pixels.
    // `average` keeps both observations of the overlap: 2 * 1024.
    assert_eq!(total(first, "count"), 1536.0);
    assert_eq!(total(cutline, "count"), 1536.0);
    assert_eq!(total(average, "count"), 2048.0);
    // `first` gives the overlap to a, so its sum is 1024 * 10 + 512 * 30.
    assert_eq!(total(first, "sum"), 1024.0 * 10.0 + 512.0 * 30.0);
    assert_ne!(first, cutline);
}
