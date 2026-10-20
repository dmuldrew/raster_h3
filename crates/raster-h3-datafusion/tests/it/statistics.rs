//! `TableProvider::statistics()` cardinality estimates for both providers.

use raster_h3_core::scalar_ops::estimate_raster_cardinality;
use raster_h3_datafusion::{
    CategoricalRasterTableProvider, ContinuousRasterTableProvider, TableProvider,
};

use crate::helpers::TestGeoTiffBuilder;
use crate::rows::plan_table;

#[test]
fn statistics_report_the_core_estimate_as_inexact() {
    let (_tmp, path) = TestGeoTiffBuilder::new(64, 64)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| (c + r) as f32);
    let source = path.to_str().unwrap();

    for res in [vec![7u8], vec![8], vec![7, 8, 9]] {
        let cont = ContinuousRasterTableProvider::try_new(source, res.clone()).unwrap();
        let cat = CategoricalRasterTableProvider::try_new(source, res.clone()).unwrap();
        let expected = estimate_raster_cardinality(std::slice::from_ref(&path), &res);
        for stats in [cont.statistics(), cat.statistics()] {
            assert_eq!(stats.num_rows, expected, "resolutions {res:?}");
            assert!(!stats.is_exact);
        }
    }

    // Finer resolutions and more resolutions both raise the estimate.
    let est = |res: Vec<u8>| {
        ContinuousRasterTableProvider::try_new(source, res)
            .unwrap()
            .statistics()
            .num_rows
    };
    assert!(est(vec![9]) > est(vec![7]));
    assert_eq!(est(vec![7, 9]), est(vec![7]) + est(vec![9]));

    // The estimate is capped by the pixel count and is in the right order of magnitude.
    let p = ContinuousRasterTableProvider::try_new(source, vec![8]).unwrap();
    let actual = plan_table(p.scan(None, &[], None).unwrap()).1.len() as u64;
    let estimate = p.statistics().num_rows;
    assert!(estimate <= 64 * 64);
    assert!(
        estimate * 4 >= actual && estimate <= actual * 4,
        "estimate {estimate} vs actual {actual}"
    );
}

#[test]
fn statistics_scale_with_the_number_of_mosaic_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.tif");
    let b = dir.path().join("b.tif");
    let builder = TestGeoTiffBuilder::new(32, 32).pixel_size(0.001);
    builder.write_f32_constant(&a, 1.0).unwrap();
    builder
        .clone()
        .origin(-122.45 + 0.032, 37.80)
        .write_f32_constant(&b, 2.0)
        .unwrap();

    let single = ContinuousRasterTableProvider::try_new(a.to_str().unwrap(), vec![9]).unwrap();
    let both = CategoricalRasterTableProvider::try_new(
        &format!("{},{}", a.display(), b.display()),
        vec![9],
    )
    .unwrap();
    assert_eq!(both.paths.len(), 2);
    assert_eq!(
        both.statistics().num_rows,
        estimate_raster_cardinality(&both.paths, &[9])
    );
    assert!(both.statistics().num_rows > single.statistics().num_rows);
}

#[test]
fn statistics_fall_back_when_the_raster_cannot_be_opened() {
    let not_a_tiff = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(not_a_tiff.path(), b"not a tiff").unwrap();
    let p = ContinuousRasterTableProvider::try_new(not_a_tiff.path().to_str().unwrap(), vec![7, 8])
        .unwrap();
    assert_eq!(p.statistics().num_rows, 20_000);
    assert!(!p.statistics().is_exact);
    // Scanning it reports the error instead of an empty table.
    assert!(p.scan(None, &[], None).unwrap().execute().is_err());
}
