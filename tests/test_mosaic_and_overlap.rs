use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::raster::mosaic::{
    glob_match, resolve_raster_sources, MosaicReader, OverlapRule,
};
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

fn create_test_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    origin_lon: f64,
    origin_lat: f64,
    pixel_size: f64,
    fill_val: u8,
) {
    let file = File::create(path).expect("failed to create tiff file");
    let mut encoder = TiffEncoder::new(file).expect("failed to create encoder");
    let mut image = encoder
        .new_image::<colortype::Gray8>(width, height)
        .expect("failed to create image");

    image
        .encoder()
        .write_tag(
            Tag::ModelTiepointTag,
            &[0.0_f64, 0.0, 0.0, origin_lon, origin_lat, 0.0][..],
        )
        .expect("write tiepoint tag");
    image
        .encoder()
        .write_tag(
            Tag::ModelPixelScaleTag,
            &[pixel_size, pixel_size, 0.0][..],
        )
        .expect("write pixel scale tag");

    let data = vec![fill_val; (width * height) as usize];
    image.write_data(&data).expect("write data");
}

#[test]
fn test_glob_match_patterns() {
    assert!(glob_match("*.tif", "hawaii_dem.tif"));
    assert!(glob_match("data/*.tif", "data/tile_01.tif"));
    assert!(glob_match("data/*/*.tif", "data/sub/tile_01.tif"));
    assert!(glob_match("tile_??.tif", "tile_01.tif"));
    assert!(glob_match("tile_??.tif", "tile_AB.tif"));
    assert!(!glob_match("tile_??.tif", "tile_001.tif"));
    assert!(!glob_match("*.tif", "hawaii_dem.png"));
    assert!(glob_match("*", "anything"));
    assert!(glob_match("exact_match.tif", "exact_match.tif"));
    assert!(!glob_match("exact_match.tif", "other.tif"));
}

#[test]
fn test_resolve_sources_comma_separated() {
    let temp_dir = std::env::temp_dir().join("raster_h3_test_comma");
    let _ = std::fs::create_dir_all(&temp_dir);

    let f1 = temp_dir.join("t1.tif");
    let f2 = temp_dir.join("t2.tif");
    create_test_geotiff(&f1, 10, 10, -122.45, 37.80, 0.001, 1);
    create_test_geotiff(&f2, 10, 10, -122.40, 37.80, 0.001, 2);

    let input = format!("{},{}", f1.display(), f2.display());
    let resolved = resolve_raster_sources(&input).expect("resolve comma sources");
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0], f1);
    assert_eq!(resolved[1], f2);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_resolve_sources_glob() {
    let temp_dir = std::env::temp_dir().join("raster_h3_test_glob");
    let _ = std::fs::create_dir_all(&temp_dir);

    let f1 = temp_dir.join("alpha_01.tif");
    let f2 = temp_dir.join("alpha_02.tif");
    let f3 = temp_dir.join("beta_01.tif");
    create_test_geotiff(&f1, 10, 10, -122.45, 37.80, 0.001, 1);
    create_test_geotiff(&f2, 10, 10, -122.40, 37.80, 0.001, 2);
    create_test_geotiff(&f3, 10, 10, -122.35, 37.80, 0.001, 3);

    let glob_pat = format!("{}/alpha_*.tif", temp_dir.display());
    let resolved = resolve_raster_sources(&glob_pat).expect("resolve glob");
    assert_eq!(resolved.len(), 2);
    assert!(resolved.iter().all(|p| p.to_string_lossy().contains("alpha_")));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_resolve_sources_vrt_xml() {
    let temp_dir = std::env::temp_dir().join("raster_h3_test_vrt");
    let _ = std::fs::create_dir_all(&temp_dir);

    let f1 = temp_dir.join("part1.tif");
    let f2 = temp_dir.join("part2.tif");
    create_test_geotiff(&f1, 10, 10, -122.45, 37.80, 0.001, 1);
    create_test_geotiff(&f2, 10, 10, -122.40, 37.80, 0.001, 2);

    let vrt_path = temp_dir.join("mosaic.vrt");
    let vrt_content = format!(
        r#"<VRTDataset rasterXSize="20" rasterYSize="10">
  <VRTRasterBand dataType="Byte" band="1">
    <SimpleSource>
      <SourceFilename relativeToVRT="1">part1.tif</SourceFilename>
    </SimpleSource>
    <SimpleSource>
      <SourceFilename relativeToVRT="1">part2.tif</SourceFilename>
    </SimpleSource>
  </VRTRasterBand>
</VRTDataset>"#
    );
    let mut vrt_file = File::create(&vrt_path).expect("create vrt");
    vrt_file.write_all(vrt_content.as_bytes()).expect("write vrt");

    let resolved = resolve_raster_sources(&vrt_path.to_string_lossy()).expect("resolve vrt");
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0], f1);
    assert_eq!(resolved[1], f2);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_mosaic_adjacent_tiles_conservation() {
    let temp_dir = std::env::temp_dir().join("raster_h3_test_adjacent");
    let _ = std::fs::create_dir_all(&temp_dir);

    // Two adjacent 50x50 tiles:
    // Tile 1: [-122.45, 37.85] to [-122.40, 37.80] (dx = 0.001)
    // Tile 2: [-122.40, 37.85] to [-122.35, 37.80] (dx = 0.001)
    let f1 = temp_dir.join("tile_west.tif");
    let f2 = temp_dir.join("tile_east.tif");
    create_test_geotiff(&f1, 50, 50, -122.45, 37.85, 0.001, 42);
    create_test_geotiff(&f2, 50, 50, -122.40, 37.85, 0.001, 42);

    let paths = vec![f1, f2];
    let mosaic = Arc::new(
        MosaicReader::open(&paths, None, None, OverlapRule::Cutline).expect("open mosaic"),
    );

    let config = MultiResolutionConfig::new(vec![8, 9]);
    let mut streamer =
        MultiScanHorizonStreamer::new_mosaic(mosaic, &config).expect("init streamer");

    let records = streamer.fetch_next_batch(100_000);
    assert!(!records.is_empty(), "Should produce records from mosaic");

    // All pixels are 42, so mean must be 42 across all hexagons
    for r in &records {
        assert!((r.accumulator.mean() - 42.0).abs() < 1e-6);
    }

    let total_pixels_res8: f64 = records
        .iter()
        .filter(|r| r.resolution == 8)
        .map(|r| r.accumulator.count)
        .sum();
    // 50*50 + 50*50 = 5000 total pixels
    assert_eq!(total_pixels_res8 as u64, 5000);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_mosaic_overlap_cutline_vs_first_vs_average() {
    let temp_dir = std::env::temp_dir().join("raster_h3_test_overlap");
    let _ = std::fs::create_dir_all(&temp_dir);

    // Two 40x40 tiles overlapping horizontally:
    // Tile 1: origin [-122.45, 37.85], pixel_size 0.001 -> bounds [-122.45, -122.41]
    // Tile 2: origin [-122.43, 37.85], pixel_size 0.001 -> bounds [-122.43, -122.39]
    // Overlap zone is [-122.43, -122.41] (20 pixels wide)
    let f1 = temp_dir.join("ov_west.tif");
    let f2 = temp_dir.join("ov_east.tif");
    create_test_geotiff(&f1, 40, 40, -122.45, 37.85, 0.001, 10);
    create_test_geotiff(&f2, 40, 40, -122.43, 37.85, 0.001, 20);

    let paths = vec![f1, f2];

    // 1. Cutline (Voronoi bisector)
    {
        let mosaic = Arc::new(
            MosaicReader::open(&paths, None, None, OverlapRule::Cutline).expect("open cutline"),
        );
        let mut config = MultiResolutionConfig::new(vec![9]);
        config.overlap_rule = OverlapRule::Cutline;
        let mut streamer =
            MultiScanHorizonStreamer::new_mosaic(mosaic, &config).expect("init cutline");
        let records = streamer.fetch_next_batch(100_000);

        let total_pixels: f64 = records.iter().map(|r| r.accumulator.count).sum();
        // Combined span: [-122.45, -122.39] = 60 pixels wide by 40 tall = 2400 unique ground pixels
        assert_eq!(total_pixels as u64, 2400, "Cutline must not double-count pixels");
    }

    // 2. First (Painter's algorithm: Tile 1 takes precedence in overlap)
    {
        let mosaic = Arc::new(
            MosaicReader::open(&paths, None, None, OverlapRule::First).expect("open first"),
        );
        let mut config = MultiResolutionConfig::new(vec![9]);
        config.overlap_rule = OverlapRule::First;
        let mut streamer =
            MultiScanHorizonStreamer::new_mosaic(mosaic, &config).expect("init first");
        let records = streamer.fetch_next_batch(100_000);

        let total_pixels: f64 = records.iter().map(|r| r.accumulator.count).sum();
        assert_eq!(total_pixels as u64, 2400, "First must not double-count pixels");
    }

    // 3. Average (Accumulate all overlapping observations)
    {
        let mosaic = Arc::new(
            MosaicReader::open(&paths, None, None, OverlapRule::Average).expect("open avg"),
        );
        let mut config = MultiResolutionConfig::new(vec![9]);
        config.overlap_rule = OverlapRule::Average;
        let mut streamer =
            MultiScanHorizonStreamer::new_mosaic(mosaic, &config).expect("init avg");
        let records = streamer.fetch_next_batch(100_000);

        let total_pixels: f64 = records.iter().map(|r| r.accumulator.count).sum();
        // 40*40 + 40*40 = 3200 accumulated observations
        assert_eq!(total_pixels as u64, 3200, "Average must accumulate both observations in overlap");
    }

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_categorical_mosaic_with_overlap() {
    let temp_dir = std::env::temp_dir().join("raster_h3_test_cat_mosaic");
    let _ = std::fs::create_dir_all(&temp_dir);

    let f1 = temp_dir.join("cat_1.tif");
    let f2 = temp_dir.join("cat_2.tif");
    create_test_geotiff(&f1, 30, 30, -122.45, 37.85, 0.001, 1);
    create_test_geotiff(&f2, 30, 30, -122.43, 37.85, 0.001, 2);

    let paths = vec![f1, f2];
    let mosaic = Arc::new(
        MosaicReader::open(&paths, None, None, OverlapRule::Cutline).expect("open cat mosaic"),
    );

    let mut config = MultiResolutionConfig::new(vec![8]);
    config.overlap_rule = OverlapRule::Cutline;
    let mut streamer =
        MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config).expect("init cat streamer");

    let records = streamer.fetch_next_batch(10_000);
    assert!(!records.is_empty(), "Categorical mosaic should yield records");

    let total_count: f64 = records.iter().map(|r| r.accumulator.total_count).sum();
    // 50 x 30 = 1500 unique pixels
    assert_eq!(total_count as u64, 1500);

    let _ = std::fs::remove_dir_all(&temp_dir);
}
