//! Reading remote COGs end to end: URL handling, header budget, chunk equivalence, mosaics and export.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::Ordering;

use raster_h3_core::error::RasterH3Error;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::http_range::{is_remote_url, normalize_url};
use raster_h3_core::raster::mosaic::{resolve_raster_sources, MosaicReader};
use tiff::decoder::DecodingResult;

use super::{MockHttpServer, ENV_MUTEX};
use crate::helpers;

#[test]
fn test_remote_url_normalization_and_detection() {
    let _env_lock = ENV_MUTEX.lock().unwrap();
    // 1. Detection
    assert!(is_remote_url("http://example.com/raster.tif"));
    assert!(is_remote_url("https://example.com/raster.tif"));
    assert!(is_remote_url("s3://bucket/key/raster.tif"));
    assert!(!is_remote_url("/local/path/to/raster.tif"));
    assert!(!is_remote_url("data/CFL_HI.tif"));
    assert!(!is_remote_url("file:///path/to/file.tif"));
    // Other cloud schemes count as remote (the DuckDB file system serves
    // them); scheme matching ignores case and surrounding whitespace.
    assert!(is_remote_url("gs://bucket/raster.tif"));
    assert!(is_remote_url("az://container/raster.tif"));
    assert!(is_remote_url(
        "abfss://fs@account.dfs.core.windows.net/raster.tif"
    ));
    assert!(is_remote_url("hf://datasets/org/repo/raster.tif"));
    assert!(is_remote_url("HTTPS://example.com/raster.tif"));
    assert!(is_remote_url("  s3://bucket/raster.tif"));
    assert!(!is_remote_url("FILE:///path/to/file.tif"));
    // Single-letter schemes are Windows drive letters, not URIs.
    assert!(!is_remote_url("c://data/raster.tif"));
    assert!(!is_remote_url("C:\\data\\raster.tif"));
    // Malformed schemes are not URIs.
    assert!(!is_remote_url("://bucket/raster.tif"));
    assert!(!is_remote_url("1s3://bucket/raster.tif"));
    assert!(!is_remote_url("my bucket://raster.tif"));
    // The built-in transport still rejects schemes it cannot serve.
    assert!(normalize_url("gs://bucket/raster.tif").is_err());

    // 2. Normalization
    let url_http = normalize_url("http://localhost:8080/data.tif").unwrap();
    assert_eq!(url_http, "http://localhost:8080/data.tif");

    let url_s3 = normalize_url("s3://my-spatial-bucket/wildfire/CFL_2024.tif").unwrap();
    assert_eq!(
        url_s3,
        "https://my-spatial-bucket.s3.amazonaws.com/wildfire/CFL_2024.tif"
    );

    // 3. Custom S3 endpoint override
    std::env::set_var("AWS_S3_ENDPOINT", "http://minio-service:9000");
    let url_minio = normalize_url("s3://geotiff-bucket/elevation.tif").unwrap();
    assert_eq!(
        url_minio,
        "http://minio-service:9000/geotiff-bucket/elevation.tif"
    );
    std::env::remove_var("AWS_S3_ENDPOINT");

    // 4. Invalid protocol error
    let err = normalize_url("ftp://server/file.tif");
    assert!(err.is_err());
}

#[test]
fn test_remote_header_read_budget_efficiency() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        eprintln!("Skipping test: data/CFL_HI.tif not found");
        return;
    }

    let mut file = File::open(&local_path).expect("Failed to open local test GeoTIFF");
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();
    let total_file_size = file_bytes.len();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    // Open remote GeoTIFF reader
    let remote_reader =
        GeoTiffStreamReader::open(&remote_url).expect("Failed to open remote GeoTIFF reader");
    let local_reader =
        GeoTiffStreamReader::open(&local_path).expect("Failed to open local GeoTIFF reader");

    // 1. Verify byte budget: metadata parse must consume < 512 KB (small fraction of 60 MB file)
    let bytes_served = server.bytes_served.load(Ordering::SeqCst);
    let req_count = server.request_count.load(Ordering::SeqCst);
    println!(
        "Total file size: {} bytes, Initial header bytes fetched: {} bytes across {} requests",
        total_file_size, bytes_served, req_count
    );
    assert!(req_count > 0);
    assert!(
        bytes_served <= 524288,
        "Header parsing fetched too much data: {} bytes",
        bytes_served
    );
    assert!(
        bytes_served < total_file_size / 50,
        "Header parse should consume < 2% of the 60MB file"
    );

    // 2. Verify metadata parity
    assert_eq!(remote_reader.metadata.width, local_reader.metadata.width);
    assert_eq!(remote_reader.metadata.height, local_reader.metadata.height);
    assert_eq!(
        remote_reader.metadata.geotransform,
        local_reader.metadata.geotransform
    );
    assert_eq!(remote_reader.metadata.nodata, local_reader.metadata.nodata);
    assert_eq!(remote_reader.metadata.epsg, local_reader.metadata.epsg);
    assert_eq!(
        remote_reader.metadata.samples_per_pixel,
        local_reader.metadata.samples_per_pixel
    );
    assert_eq!(
        remote_reader.chunk_layout.total_chunks,
        local_reader.chunk_layout.total_chunks
    );
}

#[test]
fn test_remote_chunk_exact_numerical_equivalence() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        eprintln!("Skipping test: data/CFL_HI.tif not found");
        return;
    }

    let mut file = File::open(&local_path).expect("Failed to open local test GeoTIFF");
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let remote_reader = GeoTiffStreamReader::open(&remote_url).unwrap();
    let local_reader = GeoTiffStreamReader::open(&local_path).unwrap();

    // Verify chunk 0 decode
    let (remote_bounds, remote_data) = remote_reader.read_chunk(0).unwrap();
    let (local_bounds, local_data) = local_reader.read_chunk(0).unwrap();

    assert_eq!(remote_bounds, local_bounds);

    match (&remote_data, &local_data) {
        (DecodingResult::F32(r_vals), DecodingResult::F32(l_vals)) => {
            assert_eq!(r_vals.len(), l_vals.len());
            assert_eq!(r_vals, l_vals);
        }
        (DecodingResult::U8(r_vals), DecodingResult::U8(l_vals)) => {
            assert_eq!(r_vals.len(), l_vals.len());
            assert_eq!(r_vals, l_vals);
        }
        _ => panic!("Unexpected data type match between remote and local"),
    }

    // Verify persistent decoder and read_chunk_into buffer reuse
    let mut remote_decoder = remote_reader.open_decoder().unwrap();
    let mut local_decoder = local_reader.open_decoder().unwrap();

    let (r_b, r_d) = remote_decoder.read_chunk(1).unwrap();
    let (l_b, l_d) = local_decoder.read_chunk(1).unwrap();
    assert_eq!(r_b, l_b);

    // Reuse buffer
    let (r_b2, r_d2) = remote_decoder.read_chunk_into(1, r_d).unwrap();
    assert_eq!(r_b2, l_b);
    match (&r_d2, &l_d) {
        (DecodingResult::F32(r_vals), DecodingResult::F32(l_vals)) => {
            assert_eq!(r_vals, l_vals);
        }
        (DecodingResult::U8(r_vals), DecodingResult::U8(l_vals)) => {
            assert_eq!(r_vals, l_vals);
        }
        _ => panic!("Unexpected data type match"),
    }
}

#[test]
fn test_remote_spatial_roi_selective_streaming() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        eprintln!("Skipping test: data/CFL_HI.tif not found");
        return;
    }

    let mut file = File::open(&local_path).unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();
    let total_file_size = file_bytes.len();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let remote_reader = GeoTiffStreamReader::open(&remote_url).unwrap();

    // Read only 3 chunks out of 144
    let mut decoder = remote_reader.open_decoder().unwrap();
    let _ = decoder.read_chunk(0).unwrap();
    let _ = decoder.read_chunk(1).unwrap();
    let _ = decoder.read_chunk(2).unwrap();

    let total_bytes_served = server.bytes_served.load(Ordering::SeqCst);
    println!(
        "Full file size: {} bytes, 3-chunk fetch served: {} bytes",
        total_file_size, total_bytes_served
    );

    // Verify that selective streaming downloaded only a small portion (< 2.5 MB out of 60 MB)
    assert!(
        total_bytes_served < 2_500_000,
        "Selective chunk streaming downloaded too much data: {} bytes",
        total_bytes_served
    );
    assert!(
        total_bytes_served < total_file_size / 20,
        "Selective chunk streaming must download a small fraction of the file"
    );
}

#[test]
fn test_remote_error_handling_not_found() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    let server = MockHttpServer::start(vec![0u8; 100]);
    let not_found_url = format!("{}/not_found.tif", server.url_base);

    let res = GeoTiffStreamReader::open(&not_found_url);
    assert!(res.is_err());
    match res.err().unwrap() {
        RasterH3Error::InvalidParameter(msg) => {
            assert!(msg.contains("404") || msg.contains("not found"));
        }
        other => panic!("Expected InvalidParameter error, got {:?}", other),
    }
}

#[test]
fn test_remote_mosaic_source_resolution() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    let server = MockHttpServer::start(vec![0u8; 100]);

    // 1. Single remote URL
    let url1 = format!("{}/tile1.tif", server.url_base);
    let resolved = resolve_raster_sources(&url1).unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].to_str().unwrap(), url1);

    // 2. Comma-separated remote URLs
    let url2 = format!("{}/tile2.tif", server.url_base);
    let multi = format!("{},{}", url1, url2);
    let resolved_multi = resolve_raster_sources(&multi).unwrap();
    assert_eq!(resolved_multi.len(), 2);
    assert_eq!(resolved_multi[0].to_str().unwrap(), url1);
    assert_eq!(resolved_multi[1].to_str().unwrap(), url2);
}

#[test]
fn test_remote_mosaic_reader_integration() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        return;
    }

    let mut file = File::open(&local_path).unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let paths = resolve_raster_sources(&remote_url).unwrap();
    let mosaic = MosaicReader::open(
        &paths,
        None,
        None,
        raster_h3_core::raster::mosaic::OverlapRule::Cutline,
    )
    .unwrap();
    assert_eq!(mosaic.tiles.len(), 1);
    assert_eq!(mosaic.tiles[0].file_path.to_str().unwrap(), remote_url);
    assert!(mosaic.tiles[0].reader.metadata.width > 0);
}

#[test]
fn test_remote_end_to_end_multi_resolution_streamer() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    use raster_h3_core::aggregator::multi_horizon::{
        MultiResolutionConfig, MultiScanHorizonStreamer,
    };

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        return;
    }

    let mut file = File::open(&local_path).unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let remote_reader = GeoTiffStreamReader::open(&remote_url).unwrap();
    let config = MultiResolutionConfig::new(vec![7]);

    let mut streamer = MultiScanHorizonStreamer::new(remote_reader, &config).unwrap();
    let mut records = Vec::new();
    loop {
        let batch = streamer.fetch_next_batch(256).unwrap();
        if batch.is_empty() {
            break;
        }
        records.extend(batch);
    }

    assert!(
        !records.is_empty(),
        "Streamer should yield records from remote COG"
    );
    println!(
        "Successfully aggregated {} continuous records from remote COG stream",
        records.len()
    );
}

#[test]
fn test_remote_cog_to_parquet_streaming_pipeline() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    use parquet::file::reader::{FileReader, SerializedFileReader};
    use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
    use raster_h3_core::parquet::{H3ParquetWriter, ParquetExportConfig};

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        return;
    }

    let mut file = File::open(&local_path).unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let config = MultiResolutionConfig::new(vec![7]);
    let parquet_config = ParquetExportConfig {
        omit_redundant_columns: false,
        compact: false,
        row_group_size: 1000,
        compression: parquet::basic::Compression::SNAPPY,
        is_categorical: false,
        geoparquet: false,
    };

    let temp_parquet_path = "target/test_remote_streaming_pipeline.parquet";
    if std::path::Path::new(temp_parquet_path).exists() {
        let _ = std::fs::remove_file(temp_parquet_path);
    }

    let total_rows = H3ParquetWriter::process_raster_source_to_parquet(
        &remote_url,
        temp_parquet_path,
        config,
        parquet_config,
    )
    .expect("Remote COG to Parquet streaming pipeline failed");

    assert!(total_rows > 0, "Pipeline should write rows to Parquet");

    // Open and verify Parquet file
    let pfile = File::open(temp_parquet_path).expect("Failed to open generated parquet file");
    let reader = SerializedFileReader::new(pfile).expect("Failed to read generated parquet file");
    let metadata = reader.metadata();
    assert_eq!(metadata.file_metadata().num_rows() as usize, total_rows);
    assert!(metadata.num_row_groups() >= 1);

    // Clean up
    let _ = std::fs::remove_file(temp_parquet_path);
}
