//! Integration tests verifying prefetch pipeline resilience:
//! 1. Mixed local/remote mosaics do not deadlock or stall on local tiles.
//! 2. Unscheduled / sparse chunks return Ok(None) immediately without waiting for HTTP jobs.
//! 3. Decode worker panics are caught and latched into OrderedPrefetchQueue instead of hanging queries.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use raster_h3::raster::mosaic::{MosaicReader, OverlapRule};
use raster_h3::raster::prefetch::{OrderedPrefetchQueue, PrefetchedMosaicReader};
use raster_h3::raster::remote_prefetch::RemoteChunkPrefetchQueue;

/// Minimal mock HTTP server supporting HTTP Range requests
struct MockHttpServer {
    url_base: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockHttpServer {
    fn is_networking_supported() -> bool {
        if let Ok(listener) = TcpListener::bind("127.0.0.1:0") {
            if let Ok(addr) = listener.local_addr() {
                if let Ok(_stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(50)) {
                    return true;
                }
            }
        }
        false
    }

    fn start(file_bytes: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let file_bytes_arc = Arc::new(file_bytes);

        let handle = thread::spawn(move || {
            let _ = listener.set_nonblocking(true);
            while !shutdown_clone.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };

                let file_data = Arc::clone(&file_bytes_arc);
                thread::spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut reader = BufReader::new(&stream);
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).is_err() || req_line.is_empty() {
                        return;
                    }
                    let mut range_header = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                            break;
                        }
                        if line.to_lowercase().starts_with("range:") {
                            range_header = Some(line["range:".len()..].trim().to_string());
                        }
                    }

                    let total_size = file_data.len();
                    if let Some(range_str) = range_header {
                        if let Some((start, end)) = parse_byte_range(&range_str, total_size) {
                            let slice = &file_data[start..=end];
                            let header = format!(
                                "HTTP/1.1 206 Partial Content\r\n\
                                Accept-Ranges: bytes\r\n\
                                Content-Type: image/tiff\r\n\
                                Content-Range: bytes {}-{}/{}\r\n\
                                Content-Length: {}\r\n\
                                Connection: close\r\n\r\n",
                                start,
                                end,
                                total_size,
                                slice.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(slice);
                            let _ = stream.flush();
                            let _ = stream.shutdown(std::net::Shutdown::Write);
                            return;
                        }
                    }

                    let header = format!(
                        "HTTP/1.1 200 OK\r\n\
                        Accept-Ranges: bytes\r\n\
                        Content-Type: image/tiff\r\n\
                        Content-Length: {}\r\n\
                        Connection: close\r\n\r\n",
                        total_size
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&file_data);
                    let _ = stream.flush();
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                    let mut drain = [0u8; 1024];
                    while let Ok(n) = stream.read(&mut drain) {
                        if n == 0 {
                            break;
                        }
                    }
                });
            }
        });

        Self {
            url_base: format!("http://127.0.0.1:{}", port),
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
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
fn test_mixed_local_remote_mosaic_prefetching_completes_without_deadlock() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    let tile0_path = PathBuf::from("data/burn_probability_mosaic/tile_r00_c00.tif");
    let tile1_path = PathBuf::from("data/burn_probability_mosaic/tile_r00_c01.tif");
    if !tile0_path.exists() || !tile1_path.exists() {
        eprintln!("Skipping test: mosaic test tiles not found");
        return;
    }

    // Serve tile1 as a remote URL via mock server
    let mut file1 = File::open(&tile1_path).unwrap();
    let mut file1_bytes = Vec::new();
    file1.read_to_end(&mut file1_bytes).unwrap();
    let server = MockHttpServer::start(file1_bytes);
    let remote_url = format!("{}/tile_r00_c01.tif", server.url_base);

    // Build mixed local/remote mosaic: tile 0 is local, tile 1 is remote
    let sources = vec![tile0_path, PathBuf::from(remote_url)];
    let mosaic = MosaicReader::open(&sources, None, None, OverlapRule::Cutline).unwrap();
    assert_eq!(mosaic.tiles.len(), 2);
    assert!(
        mosaic.tiles[0].reader.remote_source().is_none(),
        "tile 0 must be local"
    );
    assert!(
        mosaic.tiles[1].reader.remote_source().is_some(),
        "tile 1 must be remote"
    );

    // Check remote prefetch queue scheduled chunk tracking
    let remote_queue = RemoteChunkPrefetchQueue::spawn_mosaic(&mosaic, None).unwrap();
    for chunk_ref in &mosaic.chunk_refs {
        if chunk_ref.tile_idx == 0 {
            // Local tile chunks must NOT be scheduled in remote queue
            assert!(!remote_queue.is_chunk_scheduled(0, chunk_ref.chunk_idx));
            // get_chunk_payload must return Ok(None) immediately without blocking
            assert_eq!(
                remote_queue
                    .get_chunk_payload(0, chunk_ref.chunk_idx)
                    .unwrap(),
                None
            );
        } else {
            // Remote tile chunks must be scheduled in remote queue
            assert!(remote_queue.is_chunk_scheduled(1, chunk_ref.chunk_idx));
        }
    }

    // Spawn PrefetchedMosaicReader with 4 workers and small capacity to stress backpressure
    let prefetcher = PrefetchedMosaicReader::spawn(Arc::new(mosaic), 16);
    let mut total_drained = 0;
    let mut tile0_chunks = 0;
    let mut tile1_chunks = 0;
    let mut batch = Vec::new();

    loop {
        batch.clear();
        let drained = prefetcher.drain_chunk_batch_into(&mut batch, 1, 8);
        if drained == 0 {
            break;
        }
        total_drained += drained;
        for item in &batch {
            let (tile_idx, _chunk_idx, _bounds, _data, _overlap) = item.as_ref().unwrap();
            if *tile_idx == 0 {
                tile0_chunks += 1;
            } else {
                tile1_chunks += 1;
            }
        }
    }

    assert!(
        total_drained > 0,
        "Prefetched mosaic reader must yield chunks"
    );
    assert!(tile0_chunks > 0, "Must decompress local chunks");
    assert!(tile1_chunks > 0, "Must decompress remote chunks");
    println!(
        "Mixed mosaic test passed: drained {total_drained} chunks ({tile0_chunks} local, {tile1_chunks} remote) without deadlock"
    );
}

#[test]
fn test_ordered_prefetch_queue_worker_panic_latches_error() {
    let capacity = 16;
    let total_jobs = 100;
    let queue: Arc<OrderedPrefetchQueue<raster_h3::error::Result<usize>>> =
        Arc::new(OrderedPrefetchQueue::new(capacity, total_jobs));

    let q = Arc::clone(&queue);
    let worker = thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(q.push(0, Ok(0)));
            assert!(q.push(1, Ok(1)));
            panic!("unexpected decoder decompression failure");
        }));

        if let Err(payload) = res {
            let msg = raster_h3::ffi::panic_payload_to_string(payload);
            q.fail(Err(raster_h3::error::RasterH3Error::InvalidParameter(
                format!("Decode worker panicked: {msg}"),
            )));
        }
    });

    let _ = worker.join();

    // Consumer drains under backpressure: must get terminal Err without hanging
    let mut batch = Vec::new();
    let count = queue.drain_into(&mut batch, 1, 10);
    assert_eq!(count, 1, "Terminal error must be delivered immediately");
    assert!(
        batch[0].is_err(),
        "First delivered item on failure must be the latched error"
    );
    let err_str = batch[0].as_ref().unwrap_err().to_string();
    assert!(
        err_str.contains("unexpected decoder decompression failure"),
        "Error message must preserve panic details: {err_str}"
    );
}

#[test]
fn test_prefetched_chunk_reader_corrupt_chunk_unblocks_without_hanging() {
    use raster_h3::raster::geotiff::GeoTiffStreamReader;
    use raster_h3::raster::prefetch::PrefetchedChunkReader;
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom};
    use tempfile::NamedTempFile;
    use tiff::encoder::{colortype, compression::Deflate, TiffEncoder};
    use tiff::tags::Tag;

    let file = NamedTempFile::new().unwrap();
    {
        let mut encoder = TiffEncoder::new(File::create(file.path()).unwrap()).unwrap();
        let mut image = encoder
            .new_image_with_compression::<colortype::Gray32Float, Deflate>(
                32,
                32,
                Deflate::default(),
            )
            .unwrap();
        image.rows_per_strip(8).unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::ModelTiepointTag,
                &[0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::GeoKeyDirectoryTag,
                &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
            )
            .unwrap();
        image.write_data(&vec![42.0; 32 * 32]).unwrap();
    }
    let (offset, length) = {
        let reader = GeoTiffStreamReader::open(file.path()).unwrap();
        let info = reader.chunk_info.as_ref().unwrap();
        (info.chunk_offsets[2], info.chunk_bytes[2])
    };
    let mut writer = OpenOptions::new().write(true).open(file.path()).unwrap();
    writer.seek(SeekFrom::Start(offset)).unwrap();
    writer.write_all(&vec![0xff; length as usize]).unwrap();

    let reader = GeoTiffStreamReader::open(file.path()).unwrap();
    let total_chunks = reader.chunk_layout.total_chunks;
    assert_eq!(total_chunks, 4);

    let chunk_indices = (0..total_chunks).collect();
    let prefetcher = PrefetchedChunkReader::spawn_with_workers(reader, chunk_indices, 16, 2);

    let mut had_error = false;
    let mut chunks_read = 0;
    while let Some(item) = prefetcher.next_chunk() {
        match item {
            Ok(_) => chunks_read += 1,
            Err(_) => {
                had_error = true;
                break;
            }
        }
    }

    assert!(
        had_error,
        "PrefetchedChunkReader must report corruption error"
    );
    assert_eq!(
        chunks_read, 2,
        "Chunks 0 and 1 must be read before chunk 2 failure"
    );
}
