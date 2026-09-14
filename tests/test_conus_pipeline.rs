use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use parquet::file::reader::{FileReader, SerializedFileReader};
use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::parquet::{H3ParquetWriter, ParquetExportConfig};
use raster_h3::pmtiles::H3PmtilesTiler;
use raster_h3::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};
use tempfile::TempDir;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

fn create_test_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    origin_lon: f64,
    origin_lat: f64,
    pixel_size: f64,
    fill_val: f32,
) {
    let file = File::create(path).expect("failed to create tiff file");
    let mut encoder = TiffEncoder::new(file).expect("failed to create encoder");
    let mut image = encoder
        .new_image::<colortype::Gray32Float>(width, height)
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

    let geokeys: [u16; 12] = [
        1, 1, 0, 2,
        1024, 0, 1, 2,
        2048, 0, 1, 4326,
    ];
    image.encoder().write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();

    let data = vec![fill_val; (width * height) as usize];
    image.write_data(&data).expect("write data");
}

fn is_valid_tiff(path: &Path) -> bool {
    if let Ok(mut f) = File::open(path) {
        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_ok() {
            return (magic == [0x49, 0x49, 0x2A, 0x00]) || (magic == [0x4D, 0x4D, 0x00, 0x2A]);
        }
    }
    false
}

#[test]
fn test_conus_batch_and_evict_pipeline_simulation() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let base_path = temp_dir.path();
    let raw_staging_dir = base_path.join("raw_band_0");
    let output_dir = base_path.join("output");
    fs::create_dir_all(&raw_staging_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();

    // 1. Create a 2x2 grid of synthetic tiles simulating a latitudinal band
    let tile_00 = raw_staging_dir.join("tile_r00_c00.tif");
    let tile_01 = raw_staging_dir.join("tile_r00_c01.tif");
    let tile_10 = raw_staging_dir.join("tile_r01_c00.tif");
    let tile_11 = raw_staging_dir.join("tile_r01_c01.tif");

    create_test_geotiff(&tile_00, 32, 32, -120.00, 38.00, 0.002, 0.25);
    create_test_geotiff(&tile_01, 32, 32, -119.94, 38.00, 0.002, 0.35);
    create_test_geotiff(&tile_10, 32, 32, -120.00, 37.94, 0.002, 0.45);
    create_test_geotiff(&tile_11, 32, 32, -119.94, 37.94, 0.002, 0.55);

    // Verify all tiles pass TIFF magic validation
    assert!(is_valid_tiff(&tile_00));
    assert!(is_valid_tiff(&tile_01));
    assert!(is_valid_tiff(&tile_10));
    assert!(is_valid_tiff(&tile_11));

    let pattern = format!("{}/tile_*.tif", raw_staging_dir.display());
    let tile_paths = resolve_raster_sources(&pattern).expect("resolve sources");
    assert_eq!(tile_paths.len(), 4);

    // 2. Open MosaicReader across the tiles
    let mosaic = Arc::new(MosaicReader::open(&tile_paths, None, None, OverlapRule::Cutline).unwrap());
    assert_eq!(mosaic.chunk_refs.len(), 4);

    // 3. Configure multi-resolution streaming for H3 resolutions 8 and 9
    let resolutions = vec![8u8, 9u8];
    let mut config = MultiResolutionConfig::new(resolutions.clone());
    config.overlap_rule = OverlapRule::Cutline;

    // 4. Test Parquet output with progress callback
    let out_parquet = output_dir.join("conus_bp_band_0.parquet");
    let streamer_parquet = MultiScanHorizonStreamer::new_mosaic(Arc::clone(&mosaic), &config).unwrap();

    let mut progress_invocations = 0usize;
    let mut last_progress_count = 0usize;
    let parquet_config = ParquetExportConfig {
        compact: true,
        compression: parquet::basic::Compression::SNAPPY,
        row_group_size: 64,
        is_categorical: false,
    };

    let total_written = H3ParquetWriter::write_continuous_streamer_to_parquet_with_progress(
        streamer_parquet,
        &out_parquet,
        parquet_config,
        |count| {
            progress_invocations += 1;
            last_progress_count = count;
        },
    ).expect("write parquet");

    assert!(total_written > 0, "Should write hexagons to Parquet");
    assert!(progress_invocations > 0, "Progress callback should be invoked");
    assert_eq!(last_progress_count, total_written);

    // Verify Parquet file contents
    let parquet_file = File::open(&out_parquet).expect("open parquet file");
    let reader = SerializedFileReader::new(parquet_file).expect("create parquet reader");
    let meta = reader.metadata();
    assert_eq!(meta.file_metadata().num_rows() as usize, total_written);

    // 5. Test PMTiles output
    let out_pmtiles = output_dir.join("conus_bp_band_0.pmtiles");
    let streamer_pmtiles = MultiScanHorizonStreamer::new_mosaic(Arc::clone(&mosaic), &config).unwrap();
    let total_pmtiles_hex = H3PmtilesTiler::generate_from_continuous_streamer(
        streamer_pmtiles,
        &out_pmtiles,
    ).expect("generate pmtiles");

    assert_eq!(total_pmtiles_hex, total_written);
    assert!(out_pmtiles.exists());
    let mut pmtiles_bytes = [0u8; 7];
    let mut f = File::open(&out_pmtiles).unwrap();
    f.read_exact(&mut pmtiles_bytes).unwrap();
    assert_eq!(&pmtiles_bytes, b"PMTiles", "PMTiles archive must start with PMTiles magic bytes");

    // 6. Test Eviction of raw GeoTIFF files
    assert!(raw_staging_dir.exists());
    fs::remove_dir_all(&raw_staging_dir).expect("purge raw staging dir");
    assert!(!raw_staging_dir.exists(), "Raw staging directory must be purged");

    // Outputs must still exist intact after raw eviction
    assert!(out_parquet.exists(), "Parquet output must persist after eviction");
    assert!(out_pmtiles.exists(), "PMTiles output must persist after eviction");
}

#[test]
fn test_tiff_magic_validation_edge_cases() {
    let temp_dir = TempDir::new().unwrap();

    // 1. Valid Little-Endian TIFF (II*\0)
    let valid_le = temp_dir.path().join("valid_le.tif");
    fs::write(&valid_le, [0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]).unwrap();
    assert!(is_valid_tiff(&valid_le));

    // 2. Valid Big-Endian TIFF (MM\0*)
    let valid_be = temp_dir.path().join("valid_be.tif");
    fs::write(&valid_be, [0x4D, 0x4D, 0x00, 0x2A, 0x00, 0x00, 0x00, 0x08]).unwrap();
    assert!(is_valid_tiff(&valid_be));

    // 3. ArcGIS Server JSON error payload disguised as TIFF
    let json_err = temp_dir.path().join("error.json");
    fs::write(&json_err, b"{\"error\":{\"code\":500,\"message\":\"Internal server error\"}}").unwrap();
    assert!(!is_valid_tiff(&json_err));

    // 4. HTML error page
    let html_err = temp_dir.path().join("error.html");
    fs::write(&html_err, b"<html><head><title>504 Gateway Timeout</title></head></html>").unwrap();
    assert!(!is_valid_tiff(&html_err));

    // 5. Truncated 2-byte file
    let truncated = temp_dir.path().join("truncated.tif");
    fs::write(&truncated, [0x49, 0x49]).unwrap();
    assert!(!is_valid_tiff(&truncated));
}
