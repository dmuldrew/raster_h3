use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use raster_h3::error::RasterH3Error;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::raster::http_range::{is_remote_url, normalize_url};
use raster_h3::raster::mosaic::{resolve_raster_sources, MosaicReader};
use tiff::decoder::DecodingResult;

/// Lightweight mock HTTP server supporting HTTP Range requests (`bytes=start-end`)
struct MockHttpServer {
    port: u16,
    url_base: String,
    bytes_served: Arc<AtomicUsize>,
    request_count: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockHttpServer {
    fn start(file_bytes: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let bytes_served = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::new(AtomicUsize::new(0));

        let shutdown_clone = Arc::clone(&shutdown);
        let bytes_served_clone = Arc::clone(&bytes_served);
        let request_count_clone = Arc::clone(&request_count);
        let file_bytes_arc = Arc::new(file_bytes);

        let handle = thread::spawn(move || {
            listener
                .set_nonblocking(false)
                .expect("Cannot set blocking");

            while !shutdown_clone.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => break,
                };

                if shutdown_clone.load(Ordering::SeqCst) {
                    break;
                }

                let file_data = Arc::clone(&file_bytes_arc);
                let served = Arc::clone(&bytes_served_clone);
                let reqs = Arc::clone(&request_count_clone);

                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    Self::handle_connection(stream, &file_data, served, reqs);
                });
            }
        });

        Self {
            port,
            url_base: format!("http://127.0.0.1:{}", port),
            bytes_served,
            request_count,
            shutdown,
            handle: Some(handle),
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        file_bytes: &[u8],
        bytes_served: Arc<AtomicUsize>,
        request_count: Arc<AtomicUsize>,
    ) {
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
            return;
        }

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }
        let method = parts[0];
        let path = parts[1];

        // Read headers
        let mut range_header: Option<String> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                break;
            }
            if line.to_lowercase().starts_with("range:") {
                let range_val = line["range:".len()..].trim().to_string();
                range_header = Some(range_val);
            }
        }

        if method != "GET" && method != "HEAD" {
            let resp = "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        if path.contains("not_found") {
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        request_count.fetch_add(1, Ordering::SeqCst);
        let total_size = file_bytes.len();

        if let Some(range_str) = range_header {
            if let Some((start, end)) = parse_byte_range(&range_str, total_size) {
                let slice = &file_bytes[start..=end];
                let content_len = slice.len();
                bytes_served.fetch_add(content_len, Ordering::SeqCst);

                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                    Accept-Ranges: bytes\r\n\
                    Content-Type: image/tiff\r\n\
                    Content-Range: bytes {}-{}/{}\r\n\
                    Content-Length: {}\r\n\
                    Connection: close\r\n\r\n",
                    start, end, total_size, content_len
                );

                if stream.write_all(header.as_bytes()).is_ok() && method == "GET" {
                    let _ = stream.write_all(slice);
                }
                return;
            }
        }

        // Full content response
        bytes_served.fetch_add(total_size, Ordering::SeqCst);
        let header = format!(
            "HTTP/1.1 200 OK\r\n\
            Accept-Ranges: bytes\r\n\
            Content-Type: image/tiff\r\n\
            Content-Length: {}\r\n\
            Connection: close\r\n\r\n",
            total_size
        );
        if stream.write_all(header.as_bytes()).is_ok() && method == "GET" {
            let _ = stream.write_all(file_bytes);
        }
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Trigger a dummy connection to unblock listener.accept()
        let _ = TcpStream::connect(format!("127.0.0.1:{}", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn parse_byte_range(range_str: &str, total_size: usize) -> Option<(usize, usize)> {
    let s = range_str.trim();
    if !s.starts_with("bytes=") {
        return None;
    }
    let val = &s["bytes=".len()..];
    let parts: Vec<&str> = val.split('-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start: usize = parts[0].trim().parse().ok()?;
    let end: usize = if parts[1].trim().is_empty() {
        total_size.saturating_sub(1)
    } else {
        parts[1].trim().parse().ok()?
    };

    let end = end.min(total_size.saturating_sub(1));
    if start <= end {
        Some((start, end))
    } else {
        None
    }
}

#[test]
fn test_remote_url_normalization_and_detection() {
    // 1. Detection
    assert!(is_remote_url("http://example.com/raster.tif"));
    assert!(is_remote_url("https://example.com/raster.tif"));
    assert!(is_remote_url("s3://bucket/key/raster.tif"));
    assert!(!is_remote_url("/local/path/to/raster.tif"));
    assert!(!is_remote_url("data/CFL_HI.tif"));
    assert!(!is_remote_url("file:///path/to/file.tif"));

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
    let local_path = PathBuf::from("data/CFL_HI.tif");
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
    let remote_reader = GeoTiffStreamReader::open(&remote_url)
        .expect("Failed to open remote GeoTIFF reader");
    let local_reader = GeoTiffStreamReader::open(&local_path)
        .expect("Failed to open local GeoTIFF reader");

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
    assert_eq!(remote_reader.metadata.geotransform, local_reader.metadata.geotransform);
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
    let local_path = PathBuf::from("data/CFL_HI.tif");
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
    let local_path = PathBuf::from("data/CFL_HI.tif");
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
    let local_path = PathBuf::from("data/CFL_HI.tif");
    if !local_path.exists() {
        return;
    }

    let mut file = File::open(&local_path).unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let paths = resolve_raster_sources(&remote_url).unwrap();
    let mosaic = MosaicReader::open(&paths, None, None, raster_h3::raster::mosaic::OverlapRule::Cutline).unwrap();
    assert_eq!(mosaic.tiles.len(), 1);
    assert_eq!(mosaic.tiles[0].file_path.to_str().unwrap(), remote_url);
    assert!(mosaic.tiles[0].reader.metadata.width > 0);
}

#[test]
fn test_remote_end_to_end_multi_resolution_streamer() {
    use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};

    let local_path = PathBuf::from("data/CFL_HI.tif");
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
        let batch = streamer.fetch_next_batch(256);
        if batch.is_empty() {
            break;
        }
        records.extend(batch);
    }

    assert!(!records.is_empty(), "Streamer should yield records from remote COG");
    println!(
        "Successfully aggregated {} continuous records from remote COG stream",
        records.len()
    );
}
