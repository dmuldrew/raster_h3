//! Tests remote Cloud Optimized GeoTIFF (COG) streaming over HTTP/S3.
//!
//! Validates HTTP byte-range requests via a mock server, prefetch range coalescing, retry and recovery,
//! spatial ROI chunk filtering, header budget optimization, and streaming Parquet pipeline integration.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use raster_h3_core::error::RasterH3Error;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::http_range::{is_remote_url, normalize_url, RemoteHttpSource};
use raster_h3_core::raster::mosaic::{resolve_raster_sources, MosaicReader};
use raster_h3_core::raster::remote_prefetch::{
    coalesce_chunk_ranges, ChunkLocation, RemoteChunkPrefetchQueue, RemotePrefetchConfig,
};
use rustc_hash::FxHashMap;
use tiff::decoder::DecodingResult;

static ENV_MUTEX: Mutex<()> = Mutex::new(());

use crate::helpers;

#[test]
fn remote_chunk_payload_rejects_truncated_range() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let (file, path) = helpers::TestGeoTiffBuilder::new(2, 2).create_f32_tempfile(|_, _| 1.0);
    let bytes = std::fs::read(file.path()).unwrap();
    let size = bytes.len() as u64;
    let server = MockHttpServer::start(bytes);
    let remote = Arc::new(RemoteHttpSource::open(&format!("{}/ranged", server.url_base)).unwrap());
    let local = GeoTiffStreamReader::open(&path).unwrap();
    let mut info = local.chunk_info.as_ref().unwrap().as_ref().clone();
    info.chunk_offsets = vec![size - 1].into();
    info.chunk_bytes = vec![2].into();
    let source = raster_h3_core::raster::geotiff::DecoderSource::Remote(remote);
    let error = match source.get_chunk_payload(0, &info) {
        Ok(_) => panic!("truncated payload must be rejected before decoding"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("extends beyond file"));
}

/// Lightweight mock HTTP server supporting HTTP Range requests (`bytes=start-end`)
struct MockHttpServer {
    #[allow(dead_code)]
    port: u16,
    url_base: String,
    bytes_served: Arc<AtomicUsize>,
    request_count: Arc<AtomicUsize>,
    #[allow(dead_code)]
    transient_failures: Arc<AtomicUsize>,
    recorded_headers: Arc<Mutex<Vec<String>>>,
    gate: Arc<RequestGate>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct RequestGateState {
    enabled: bool,
    fail: Option<bool>,
    requests: FxHashMap<usize, usize>,
}

#[derive(Default)]
struct RequestGate {
    state: Mutex<RequestGateState>,
    condvar: Condvar,
}

impl RequestGate {
    fn hold(&self) {
        self.state.lock().unwrap().enabled = true;
    }

    fn release(&self, fail: bool) {
        self.state.lock().unwrap().fail = Some(fail);
        self.condvar.notify_all();
    }

    fn block_request(&self, start: usize) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.enabled {
            return false;
        }
        *state.requests.entry(start).or_default() += 1;
        self.condvar.notify_all();
        while state.fail.is_none() {
            state = self.condvar.wait(state).unwrap();
        }
        state.fail.unwrap()
    }

    fn wait_for_requests(&self, count: usize) {
        let (mut state, _) = self
            .condvar
            .wait_timeout_while(
                self.state.lock().unwrap(),
                Duration::from_secs(10),
                |state| state.requests.values().sum::<usize>() < count,
            )
            .unwrap();
        let received = state.requests.values().sum::<usize>();
        if received < count {
            state.fail = Some(false);
            self.condvar.notify_all();
        }
        drop(state);
        assert_eq!(received, count, "prefetch ranges were not all requested");
    }
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
        Self::start_with_failures(file_bytes, 0)
    }

    fn start_with_failures(file_bytes: Vec<u8>, failure_count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let bytes_served = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::new(AtomicUsize::new(0));
        let transient_failures = Arc::new(AtomicUsize::new(failure_count));
        let recorded_headers = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(RequestGate::default());
        let gate_clone = Arc::clone(&gate);

        let shutdown_clone = Arc::clone(&shutdown);
        let bytes_served_clone = Arc::clone(&bytes_served);
        let request_count_clone = Arc::clone(&request_count);
        let failures_clone = Arc::clone(&transient_failures);
        let headers_clone = Arc::clone(&recorded_headers);
        let file_bytes_arc = Arc::new(file_bytes);

        let handle = thread::spawn(move || {
            let _ = listener.set_nonblocking(true);

            while !shutdown_clone.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };

                if shutdown_clone.load(Ordering::SeqCst) {
                    break;
                }

                let file_data = Arc::clone(&file_bytes_arc);
                let served = Arc::clone(&bytes_served_clone);
                let reqs = Arc::clone(&request_count_clone);
                let fails = Arc::clone(&failures_clone);
                let hdrs = Arc::clone(&headers_clone);
                let gate = Arc::clone(&gate_clone);

                thread::spawn(move || {
                    // Accepted sockets can inherit the listener's nonblocking mode on macOS.
                    stream
                        .set_nonblocking(false)
                        .expect("Failed to make accepted socket blocking");
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    Self::handle_connection(stream, &file_data, served, reqs, fails, hdrs, gate);
                });
            }
        });

        Self {
            port,
            url_base: format!("http://127.0.0.1:{}", port),
            bytes_served,
            request_count,
            transient_failures,
            recorded_headers,
            gate,
            shutdown,
            handle: Some(handle),
        }
    }

    fn recorded_headers(&self) -> Vec<String> {
        self.recorded_headers.lock().unwrap().clone()
    }

    fn handle_connection(
        mut stream: TcpStream,
        file_bytes: &[u8],
        bytes_served: Arc<AtomicUsize>,
        request_count: Arc<AtomicUsize>,
        transient_failures: Arc<AtomicUsize>,
        recorded_headers: Arc<Mutex<Vec<String>>>,
        gate: Arc<RequestGate>,
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
            if let Ok(mut h) = recorded_headers.lock() {
                h.push(line.clone());
            }
            if line.to_lowercase().starts_with("range:") {
                let range_val = line["range:".len()..].trim().to_string();
                range_header = Some(range_val);
            }
        }

        if method != "GET" && method != "HEAD" {
            let resp =
                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        if path.contains("not_found") {
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        request_count.fetch_add(1, Ordering::SeqCst);

        // Check for simulated transient failure (HTTP 503 Slow Down)
        // `try_update` replaces this on Rust 1.95+, but rust-version is 1.94.
        #[allow(deprecated)]
        let fail_hit =
            transient_failures.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                if count > 0 {
                    Some(count - 1)
                } else {
                    None
                }
            });
        if fail_hit.is_ok() {
            let resp = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        let total_size = file_bytes.len();

        // Exercise servers that ignore Range or lie about the returned offset.
        // The initial header request remains valid so the failure occurs in fetch_range.
        if path.contains("full_body")
            || (path.contains("ignore_range") && range_header.as_deref() != Some("bytes=0-131071"))
        {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                total_size
            );
            if stream.write_all(header.as_bytes()).is_ok() && method == "GET" {
                let _ = stream.write_all(file_bytes);
            }
            return;
        }

        if path.contains("wrong_range") && range_header.as_deref() != Some("bytes=0-131071") {
            let body = &file_bytes[..10];
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/{}\r\nContent-Length: 10\r\nConnection: close\r\n\r\n",
                total_size
            );
            if stream.write_all(header.as_bytes()).is_ok() && method == "GET" {
                let _ = stream.write_all(body);
            }
            return;
        }

        if let Some(range_str) = range_header {
            if let Some((start, end)) = parse_byte_range(&range_str, total_size) {
                if gate.block_request(start) {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
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
        self.gate.release(false);
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

type PrefetchResult = Result<Option<Vec<u8>>, String>;

struct GatedPrefetch {
    server: MockHttpServer,
    reader: GeoTiffStreamReader,
    queue: Arc<RemoteChunkPrefetchQueue>,
    file_bytes: Vec<u8>,
}

impl GatedPrefetch {
    fn new(chunk_indices: &[u32]) -> Self {
        let (file, _) = helpers::TestGeoTiffBuilder::new(512, 512)
            .create_f32_tempfile(|col, row| (col + row * 512) as f32);
        let file_bytes = std::fs::read(file.path()).unwrap();
        let server = MockHttpServer::start(file_bytes.clone());
        let reader = GeoTiffStreamReader::open(format!("{}/test.tif", server.url_base)).unwrap();
        server.gate.hold();
        let queue = Arc::new(
            RemoteChunkPrefetchQueue::spawn_single(
                &reader,
                chunk_indices,
                Some(RemotePrefetchConfig {
                    num_workers: 2,
                    max_range_bytes: 1, // Keep each strip in its own HTTP request.
                    ..Default::default()
                }),
            )
            .unwrap(),
        );
        let fixture = Self {
            server,
            reader,
            queue,
            file_bytes,
        };
        assert_eq!(fixture.queue.total_jobs(), chunk_indices.len());
        fixture.server.gate.wait_for_requests(chunk_indices.len());
        fixture
    }

    fn request_chunk(
        &self,
        chunk_idx: u32,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<PrefetchResult>) {
        let queue = Arc::clone(&self.queue);
        let (tx, rx) = mpsc::channel();
        let consumer = thread::spawn(move || {
            let result = queue
                .get_chunk_payload(0, chunk_idx)
                .map(|payload| payload.map(|bytes| bytes.to_vec()))
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        (consumer, rx)
    }
}

impl Drop for GatedPrefetch {
    fn drop(&mut self) {
        self.server.gate.release(false);
        self.queue.cancel();
    }
}

#[test]
fn test_waits_for_in_flight_ranges_after_all_jobs_claimed() {
    let fixture = GatedPrefetch::new(&[0, 1]);
    let consumers: Vec<_> = (0..2).map(|chunk| fixture.request_chunk(chunk)).collect();
    let early: Vec<_> = consumers
        .iter()
        .map(|(_, rx)| rx.recv_timeout(Duration::from_millis(200)))
        .collect();
    fixture.server.gate.release(false);

    for (chunk_idx, ((consumer, rx), early)) in consumers.into_iter().zip(early).enumerate() {
        let waited = matches!(early, Err(mpsc::RecvTimeoutError::Timeout));
        let result = if waited {
            rx.recv_timeout(Duration::from_secs(10))
        } else {
            early
        };
        if result.is_err() {
            fixture.queue.cancel();
        }
        consumer.join().unwrap();
        assert!(waited, "consumer returned before its HTTP response");
        let info = fixture.reader.chunk_info.as_ref().unwrap();
        let start = info.chunk_offsets[chunk_idx] as usize;
        let len = info.chunk_bytes[chunk_idx] as usize;
        assert_eq!(
            result.unwrap().unwrap().as_deref(),
            Some(&fixture.file_bytes[start..start + len])
        );
        assert_eq!(
            fixture.server.gate.state.lock().unwrap().requests[&start],
            1
        );
    }
}

#[test]
fn test_final_prefetch_error_wakes_all_consumers() {
    let fixture = GatedPrefetch::new(&[0]);
    let consumers: Vec<_> = (0..2).map(|_| fixture.request_chunk(0)).collect();
    let early: Vec<_> = consumers
        .iter()
        .map(|(_, rx)| rx.recv_timeout(Duration::from_millis(200)))
        .collect();
    fixture.server.gate.release(true);

    let results: Vec<_> = consumers
        .iter()
        .map(|(_, rx)| rx.recv_timeout(Duration::from_secs(10)))
        .collect();
    fixture.queue.cancel();
    for (((consumer, _), early), result) in consumers.into_iter().zip(early).zip(results) {
        consumer.join().unwrap();
        assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
        let error = result
            .expect("consumer missed the final fetch error")
            .unwrap_err();
        assert!(error.contains("HTTP 404"), "unexpected error: {error}");
    }
}

#[test]
fn test_prefetch_cancellation_wakes_all_consumers() {
    let fixture = GatedPrefetch::new(&[0]);
    let consumers: Vec<_> = (0..2).map(|_| fixture.request_chunk(0)).collect();
    let early: Vec<_> = consumers
        .iter()
        .map(|(_, rx)| rx.recv_timeout(Duration::from_millis(200)))
        .collect();
    fixture.queue.cancel();

    for ((consumer, rx), early) in consumers.into_iter().zip(early) {
        let result = rx.recv_timeout(Duration::from_secs(10));
        if result.is_err() {
            fixture.queue.cancel();
        }
        consumer.join().unwrap();
        assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(result.expect("consumer missed cancellation").unwrap(), None);
    }
}

/// A 512×512 f32 remote GeoTIFF in 64 strips of 8 rows (16 KiB each), whose
/// range requests are counted per start offset (the gate records without
/// blocking).
struct RemoteStrips {
    server: MockHttpServer,
    reader: GeoTiffStreamReader,
    file_bytes: Vec<u8>,
}

impl RemoteStrips {
    fn new() -> Self {
        use tiff::encoder::{colortype, TiffEncoder};
        use tiff::tags::Tag;
        const SIDE: u32 = 512;
        let mut file_bytes = Vec::new();
        {
            let mut encoder = TiffEncoder::new(std::io::Cursor::new(&mut file_bytes)).unwrap();
            let mut image = encoder
                .new_image::<colortype::Gray32Float>(SIDE, SIDE)
                .unwrap();
            image.rows_per_strip(8).unwrap();
            let enc = image.encoder();
            enc.write_tag(Tag::ModelPixelScaleTag, &[0.001, 0.001, 0.0][..])
                .unwrap();
            enc.write_tag(
                Tag::ModelTiepointTag,
                &[0.0, 0.0, 0.0, -122.45, 37.8, 0.0][..],
            )
            .unwrap();
            let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
            enc.write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..])
                .unwrap();
            let data: Vec<f32> = (0..SIDE * SIDE).map(|i| (i % 7919) as f32).collect();
            image.write_data(&data).unwrap();
        }
        let server = MockHttpServer::start(file_bytes.clone());
        let reader = GeoTiffStreamReader::open(format!("{}/test.tif", server.url_base)).unwrap();
        server.gate.hold();
        server.gate.release(false);
        Self {
            server,
            reader,
            file_bytes,
        }
    }

    fn chunks(&self) -> (Vec<u64>, Vec<u64>) {
        let info = self.reader.chunk_info.as_ref().unwrap();
        (info.chunk_offsets.to_vec(), info.chunk_bytes.to_vec())
    }

    fn expected(&self, chunk_idx: usize) -> &[u8] {
        let (offsets, lens) = self.chunks();
        let start = offsets[chunk_idx] as usize;
        &self.file_bytes[start..start + lens[chunk_idx] as usize]
    }

    /// Largest coalesced range the queue will request under `config`.
    fn largest_range(&self, config: &RemotePrefetchConfig) -> u64 {
        let (offsets, lens) = self.chunks();
        let locations: Vec<_> = (0..offsets.len())
            .map(|i| ChunkLocation {
                tile_idx: 0,
                chunk_idx: i as u32,
                offset: offsets[i],
                length: lens[i],
            })
            .collect();
        coalesce_chunk_ranges(&locations, config.max_gap_bytes, config.max_range_bytes)
            .iter()
            .map(|r| r.end_offset - r.start_offset + 1)
            .max()
            .unwrap()
    }

    fn requests(&self) -> FxHashMap<usize, usize> {
        self.server.gate.state.lock().unwrap().requests.clone()
    }

    fn spawn(&self, config: RemotePrefetchConfig) -> RemoteChunkPrefetchQueue {
        let n = self.chunks().0.len() as u32;
        let indices: Vec<u32> = (0..n).collect();
        RemoteChunkPrefetchQueue::spawn_single(&self.reader, &indices, Some(config)).unwrap()
    }

    fn take_with_timeout(&self, queue: &Arc<RemoteChunkPrefetchQueue>, chunk_idx: u32) {
        let worker_queue = Arc::clone(queue);
        let (tx, rx) = mpsc::channel();
        let consumer = thread::spawn(move || {
            let result = worker_queue
                .get_chunk_payload(0, chunk_idx)
                .map(|payload| payload.map(|bytes| bytes.to_vec()));
            let _ = tx.send(result);
        });
        let result = rx.recv_timeout(Duration::from_secs(10));
        if result.is_err() {
            queue.cancel();
        }
        consumer.join().unwrap();
        assert_eq!(
            result.expect("requested chunk stalled").unwrap().as_deref(),
            Some(self.expected(chunk_idx as usize)),
            "chunk {chunk_idx}"
        );
    }

    /// Take every chunk in order, checking bytes and holding each for `hold`.
    fn consume(&self, queue: &RemoteChunkPrefetchQueue, hold: Duration) {
        for i in 0..self.chunks().0.len() {
            let chunk = queue.get_chunk_payload(0, i as u32).unwrap().unwrap();
            assert_eq!(&chunk[..], self.expected(i), "chunk {i}");
            thread::sleep(hold);
        }
    }
}

#[test]
fn prefetch_out_of_order_requests_refetch_evicted_ranges() {
    for workers in [1, 4] {
        for oversized in [false, true] {
            let fixture = RemoteStrips::new();
            let (offsets, lens) = fixture.chunks();
            let strip = lens[0];
            let queue = Arc::new(
                RemoteChunkPrefetchQueue::spawn_single(
                    &fixture.reader,
                    &[0, 63],
                    Some(RemotePrefetchConfig {
                        num_workers: workers,
                        max_range_bytes: 1,
                        max_buffered_bytes: if oversized { 0 } else { strip },
                        ..Default::default()
                    }),
                )
                .unwrap(),
            );
            fixture.server.gate.wait_for_requests(1);
            fixture.take_with_timeout(&queue, 63);
            fixture.take_with_timeout(&queue, 0);
            assert_eq!(queue.peak_held_bytes(), strip);
            assert_eq!(queue.held_bytes(), 0);
            let requests = fixture.requests();
            assert_eq!(requests[&(offsets[0] as usize)], 2);
            assert_eq!(requests[&(offsets[63] as usize)], 1);
        }
    }
}

#[test]
fn prefetch_out_of_order_coalesced_siblings_remain_available() {
    let fixture = RemoteStrips::new();
    let (_, lens) = fixture.chunks();
    let budget = lens[0] * 2;
    let queue = Arc::new(
        RemoteChunkPrefetchQueue::spawn_single(
            &fixture.reader,
            &[0, 1, 2, 3],
            Some(RemotePrefetchConfig {
                num_workers: 4,
                max_range_bytes: budget,
                max_buffered_bytes: budget,
                ..Default::default()
            }),
        )
        .unwrap(),
    );
    assert_eq!(queue.total_jobs(), 2);
    for chunk in [0, 3, 1, 2] {
        fixture.take_with_timeout(&queue, chunk);
    }
    assert_eq!(
        queue.held_bytes(),
        0,
        "consumed siblings must not be republished"
    );
    assert!(queue.peak_held_bytes() <= budget);
    assert!(queue.get_chunk_payload(0, 0).unwrap().is_none());
    assert!(queue.try_get_chunk_payload(0, 3).is_none());
}

#[test]
fn prefetch_concurrent_reverse_requests_stay_within_budget() {
    let fixture = RemoteStrips::new();
    let strip = fixture.chunks().1[0];
    let queue = Arc::new(fixture.spawn(RemotePrefetchConfig {
        num_workers: 4,
        max_range_bytes: 1,
        max_buffered_bytes: strip,
        ..Default::default()
    }));
    fixture.server.gate.wait_for_requests(1);
    let (tx, rx) = mpsc::channel();
    let consumers: Vec<_> = (0..64)
        .rev()
        .map(|chunk| {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            thread::spawn(move || {
                let result = queue
                    .get_chunk_payload(0, chunk)
                    .map(|payload| payload.map(|bytes| bytes.to_vec()));
                let _ = tx.send((chunk, result));
            })
        })
        .collect();
    drop(tx);
    let mut received = Vec::new();
    for _ in 0..64 {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(item) => received.push(item),
            Err(_) => break,
        }
    }
    queue.cancel();
    for consumer in consumers {
        consumer.join().unwrap();
    }
    assert_eq!(received.len(), 64, "concurrent requests stalled");
    for (chunk, result) in received {
        assert_eq!(
            result.unwrap().as_deref(),
            Some(fixture.expected(chunk as usize))
        );
    }
    assert_eq!(queue.held_bytes(), 0);
    assert_eq!(queue.peak_held_bytes(), strip);
}

#[test]
fn prefetch_out_of_order_requests_bypass_full_chunk_capacity() {
    let fixture = RemoteStrips::new();
    let budget = fixture.chunks().1.iter().sum();
    let queue = Arc::new(fixture.spawn(RemotePrefetchConfig {
        num_workers: 1,
        max_range_bytes: 1,
        max_buffered_bytes: budget,
        queue_capacity: 16,
        ..Default::default()
    }));
    fixture.server.gate.wait_for_requests(16);
    fixture.take_with_timeout(&queue, 63);
    fixture.take_with_timeout(&queue, 0);
    assert!(queue.peak_held_bytes() <= budget);
}

#[test]
fn prefetch_caller_clones_retain_budget_until_dropped() {
    let fixture = RemoteStrips::new();
    let strip = fixture.chunks().1[0];
    let queue = Arc::new(
        RemoteChunkPrefetchQueue::spawn_single(
            &fixture.reader,
            &[0, 1],
            Some(RemotePrefetchConfig {
                num_workers: 2,
                max_range_bytes: 1,
                max_buffered_bytes: strip,
                ..Default::default()
            }),
        )
        .unwrap(),
    );
    let payload = queue.get_chunk_payload(0, 0).unwrap().unwrap();
    let clone = payload.clone();
    drop(payload);
    let worker_queue = Arc::clone(&queue);
    let (tx, rx) = mpsc::channel();
    let consumer = thread::spawn(move || {
        let result = worker_queue
            .get_chunk_payload(0, 1)
            .map(|payload| payload.map(|bytes| bytes.to_vec()));
        let _ = tx.send(result);
    });
    let early = rx.recv_timeout(Duration::from_millis(100));
    let held = queue.held_bytes();
    drop(clone);
    let waited = matches!(early, Err(mpsc::RecvTimeoutError::Timeout));
    let result = if waited {
        rx.recv_timeout(Duration::from_secs(10))
    } else {
        early
    };
    if result.is_err() {
        queue.cancel();
    }
    consumer.join().unwrap();
    assert!(
        waited,
        "caller-held bytes must not be evicted or bypass the budget"
    );
    assert_eq!(held, strip);
    assert_eq!(
        result.unwrap().unwrap().as_deref(),
        Some(fixture.expected(1))
    );
    assert_eq!(queue.peak_held_bytes(), strip);
    assert_eq!(queue.held_bytes(), 0);
}

#[test]
fn prefetch_payload_remains_valid_after_queue_drop() {
    let fixture = RemoteStrips::new();
    let queue = RemoteChunkPrefetchQueue::spawn_single(&fixture.reader, &[0], None).unwrap();
    let payload = queue.get_chunk_payload(0, 0).unwrap().unwrap();
    drop(queue);
    assert_eq!(&payload[..], fixture.expected(0));
    assert_eq!(payload.response_len(), payload.len());
}

#[test]
fn prefetch_byte_budget_bounds_held_bytes_with_slow_consumer() {
    let fixture = RemoteStrips::new();
    let (offsets, lens) = fixture.chunks();
    let strip = *lens.iter().max().unwrap();
    let budget = 4 * strip;
    let queue = fixture.spawn(RemotePrefetchConfig {
        num_workers: 6,
        max_range_bytes: 1, // one strip per request
        max_buffered_bytes: budget,
        ..Default::default()
    });
    fixture.consume(&queue, Duration::from_millis(1));

    let peak = queue.peak_held_bytes();
    assert!(
        (strip..=budget).contains(&peak),
        "peak {peak} outside [{strip}, {budget}]"
    );
    assert_eq!(queue.held_bytes(), 0, "every response released");
    let requests = fixture.requests();
    assert_eq!(requests.len(), offsets.len());
    assert!(requests.values().all(|&n| n == 1), "duplicate requests");
}

#[test]
fn prefetch_shared_response_stays_charged_until_last_slice_drops() {
    let fixture = RemoteStrips::new();
    let (_, lens) = fixture.chunks();
    let indices = [0, 1, 2, 3];
    let queue = RemoteChunkPrefetchQueue::spawn_single(
        &fixture.reader,
        &indices,
        Some(RemotePrefetchConfig {
            num_workers: 1,
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(queue.total_jobs(), 1, "adjacent strips coalesce");

    let mut chunks: Vec<_> = indices
        .iter()
        .map(|&i| queue.get_chunk_payload(0, i).unwrap().unwrap())
        .collect();
    let response = chunks[0].response_len() as u64;
    assert!(response >= lens[..4].iter().sum::<u64>());
    for (i, chunk) in chunks.iter().enumerate() {
        assert_eq!(&chunk[..], fixture.expected(i));
        assert_eq!(chunk.response_len() as u64, response, "one shared buffer");
    }
    assert_eq!(queue.held_bytes(), response);

    let last = chunks.pop().unwrap();
    drop(chunks);
    assert_eq!(queue.held_bytes(), response, "pinned by the last slice");
    drop(last);
    assert_eq!(queue.held_bytes(), 0);
}

#[test]
fn prefetch_oversized_ranges_run_one_at_a_time() {
    let fixture = RemoteStrips::new();
    let config = RemotePrefetchConfig {
        num_workers: 4,
        max_range_bytes: 64 * 1024,
        max_buffered_bytes: 1, // every range exceeds the budget
        ..Default::default()
    };
    let largest = fixture.largest_range(&config);
    let queue = fixture.spawn(config);
    assert!(queue.total_jobs() > 2);
    fixture.consume(&queue, Duration::ZERO);
    assert!(
        queue.peak_held_bytes() <= largest,
        "peak {} exceeds one range ({largest})",
        queue.peak_held_bytes()
    );
    assert_eq!(queue.held_bytes(), 0);
}

#[test]
fn prefetch_cancel_wakes_workers_waiting_for_budget() {
    let fixture = RemoteStrips::new();
    let queue = fixture.spawn(RemotePrefetchConfig {
        num_workers: 4,
        max_range_bytes: 1,
        max_buffered_bytes: 1, // admit a single range, never consumed
        ..Default::default()
    });
    let started = std::time::Instant::now();
    while queue.held_bytes() == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no range fetched"
        );
        thread::sleep(Duration::from_millis(1));
    }
    // Give the other workers time to block on the budget.
    thread::sleep(Duration::from_millis(50));

    let (tx, rx) = mpsc::channel();
    let dropper = thread::spawn(move || {
        drop(queue); // cancels, then joins every worker
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("workers waiting for budget were not woken by cancel");
    dropper.join().unwrap();
    assert_eq!(fixture.requests().len(), 1, "budget admitted extra ranges");
}

#[test]
fn remote_range_reader_rejects_ignored_and_wrong_ranges() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let server = MockHttpServer::start(vec![0u8; 200_000]);
    for path in ["ignore_range", "wrong_range"] {
        let source = RemoteHttpSource::open(&format!("{}/{}", server.url_base, path)).unwrap();
        let err = source.fetch_range(100, 109).unwrap_err();
        assert!(
            err.to_string().contains("range"),
            "unexpected error for {path}: {err}"
        );
    }
}

#[test]
fn initial_full_body_response_serves_later_blocks_from_cache() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    let source = RemoteHttpSource::open(&format!("{}/full_body", server.url_base)).unwrap();
    let mut actual = [0u8; 20];
    assert_eq!(source.read_range_into(150_000, &mut actual).unwrap(), 20);
    assert_eq!(&actual, &data[150_000..150_020]);
    assert_eq!(source.read_range(0, 150_000).unwrap(), data[..150_000]);
    assert_eq!(server.request_count.load(Ordering::SeqCst), 1);
}

#[test]
fn remote_read_crossing_cache_blocks_returns_all_bytes() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    let source = RemoteHttpSource::open(&format!("{}/ranged", server.url_base)).unwrap();

    let mut across_boundary = [0u8; 10];
    assert_eq!(
        source
            .read_range_into(131_070, &mut across_boundary)
            .unwrap(),
        10
    );
    assert_eq!(&across_boundary, &data[131_070..131_080]);
    assert_eq!(
        source.read_range(131_070, 10).unwrap(),
        data[131_070..131_080]
    );
    assert_eq!(source.read_range(299_996, 10).unwrap(), data[299_996..]);
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
    let _env_lock = ENV_MUTEX.lock().unwrap();
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
fn test_remote_prefetch_queue_and_request_coalescing() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    use raster_h3_core::raster::prefetch::PrefetchedChunkReader;

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
    let local_reader = GeoTiffStreamReader::open(&local_path).unwrap();

    let initial_reqs = server.request_count.load(Ordering::SeqCst);

    // Request 16 consecutive chunks across scanlines
    let chunk_indices: Vec<u32> = (0..16).collect();
    let prefetcher =
        PrefetchedChunkReader::spawn_with_workers(remote_reader, chunk_indices.clone(), 32, 4);

    let mut drained = Vec::new();
    while let Some(item) = prefetcher.next_chunk() {
        drained.push(item.unwrap());
        if drained.len() == 16 {
            break;
        }
    }

    assert_eq!(drained.len(), 16);

    // Verify bitwise/numerical parity against local decoder for all 16 chunks
    let mut local_decoder = local_reader.open_decoder().unwrap();
    for &(chunk_idx, ref bounds, ref data) in &drained {
        let (loc_bounds, loc_data) = local_decoder.read_chunk(chunk_idx).unwrap();
        assert_eq!(bounds, &loc_bounds);
        match (data, &loc_data) {
            (DecodingResult::F32(r_vals), DecodingResult::F32(l_vals)) => {
                assert_eq!(r_vals, l_vals);
            }
            _ => panic!("Expected F32 sample format match"),
        }
    }

    let total_reqs = server.request_count.load(Ordering::SeqCst) - initial_reqs;
    println!(
        "Drained 16 chunks in {} HTTP requests (coalescing efficiency: {:.1}x reduction)",
        total_reqs,
        16.0 / total_reqs.max(1) as f64
    );

    // Without coalescing, 16 individual chunks would require 16 separate range requests.
    // With coalescing, adjacent tiles on rows are merged, requiring <= 8 requests.
    assert!(
        total_reqs <= 8,
        "Expected range coalescing to reduce requests to <= 8, got {}",
        total_reqs
    );
}

#[test]
fn test_remote_coalesce_chunk_ranges_algorithm() {
    use raster_h3_core::raster::remote_prefetch::{coalesce_chunk_ranges, ChunkLocation};

    let chunks = vec![
        ChunkLocation {
            tile_idx: 0,
            chunk_idx: 0,
            offset: 1000,
            length: 2000,
        },
        ChunkLocation {
            tile_idx: 0,
            chunk_idx: 1,
            offset: 3000,
            length: 2000,
        },
        ChunkLocation {
            tile_idx: 0,
            chunk_idx: 2,
            offset: 5000,
            length: 2000,
        },
        // Large gap (50,000 bytes > 32KB max gap)
        ChunkLocation {
            tile_idx: 0,
            chunk_idx: 3,
            offset: 57000,
            length: 3000,
        },
        ChunkLocation {
            tile_idx: 0,
            chunk_idx: 4,
            offset: 60000,
            length: 3000,
        },
    ];

    let coalesced = coalesce_chunk_ranges(&chunks, 32768, 1024 * 1024);
    assert_eq!(coalesced.len(), 2);

    // Range 1: Chunks 0, 1, 2
    assert_eq!(coalesced[0].tile_idx, 0);
    assert_eq!(coalesced[0].start_offset, 1000);
    assert_eq!(coalesced[0].end_offset, 6999);
    assert_eq!(coalesced[0].chunk_slices.len(), 3);
    assert_eq!(coalesced[0].chunk_slices[0], (0, 0, 2000));
    assert_eq!(coalesced[0].chunk_slices[1], (1, 2000, 2000));
    assert_eq!(coalesced[0].chunk_slices[2], (2, 4000, 2000));

    // Range 2: Chunks 3, 4
    assert_eq!(coalesced[1].tile_idx, 0);
    assert_eq!(coalesced[1].start_offset, 57000);
    assert_eq!(coalesced[1].end_offset, 62999);
    assert_eq!(coalesced[1].chunk_slices.len(), 2);
    assert_eq!(coalesced[1].chunk_slices[0], (3, 0, 3000));
    assert_eq!(coalesced[1].chunk_slices[1], (4, 3000, 3000));
}

#[test]
fn test_unified_chunk_byte_pathway() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    use raster_h3_core::raster::geotiff::ChunkPayload;

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        return;
    }

    let mut file = File::open(&local_path).unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let local_reader = GeoTiffStreamReader::open(&local_path).unwrap();
    let remote_reader = GeoTiffStreamReader::open(&remote_url).unwrap();

    // 1. Verify get_chunk_payload across local (zero-copy borrowed) and remote (range-fetched owned)
    for chunk_idx in [0, 5, 10, 15] {
        let local_payload = local_reader.get_chunk_payload(chunk_idx).unwrap();
        let remote_payload = remote_reader.get_chunk_payload(chunk_idx).unwrap();

        // Local must be zero-copy borrowed slice from mmap
        assert!(matches!(local_payload, ChunkPayload::Borrowed(_)));
        // Remote must be owned bytes fetched from remote range
        assert!(matches!(remote_payload, ChunkPayload::Owned(_)));

        // Both must be 100% byte-for-byte identical
        assert_eq!(local_payload.as_ref(), remote_payload.as_ref());
    }

    // 2. Verify standalone read_chunk and read_chunk_into on remote decoders use SIMD/LZW directly
    let mut local_decoder = local_reader.open_decoder().unwrap();
    let mut remote_decoder = remote_reader.open_decoder().unwrap();

    fn assert_decoding_result_eq(a: &DecodingResult, b: &DecodingResult) {
        match (a, b) {
            (DecodingResult::U8(v1), DecodingResult::U8(v2)) => assert_eq!(v1, v2),
            (DecodingResult::U16(v1), DecodingResult::U16(v2)) => assert_eq!(v1, v2),
            (DecodingResult::U32(v1), DecodingResult::U32(v2)) => assert_eq!(v1, v2),
            (DecodingResult::U64(v1), DecodingResult::U64(v2)) => assert_eq!(v1, v2),
            (DecodingResult::F32(v1), DecodingResult::F32(v2)) => assert_eq!(v1, v2),
            (DecodingResult::F64(v1), DecodingResult::F64(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I8(v1), DecodingResult::I8(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I16(v1), DecodingResult::I16(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I32(v1), DecodingResult::I32(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I64(v1), DecodingResult::I64(v2)) => assert_eq!(v1, v2),
            _ => panic!("Mismatched DecodingResult types"),
        }
    }

    for chunk_idx in [0, 1, 2, 7, 12] {
        // Test read_chunk
        let (loc_bounds, loc_data) = local_decoder.read_chunk(chunk_idx).unwrap();
        let (rem_bounds, rem_data) = remote_decoder.read_chunk(chunk_idx).unwrap();
        assert_eq!(loc_bounds, rem_bounds);
        assert_decoding_result_eq(&loc_data, &rem_data);

        // Test read_chunk_into buffer recycling
        let (loc_b2, loc_d2) = local_decoder.read_chunk_into(chunk_idx, loc_data).unwrap();
        let (rem_b2, rem_d2) = remote_decoder.read_chunk_into(chunk_idx, rem_data).unwrap();
        assert_eq!(loc_b2, rem_b2);
        assert_decoding_result_eq(&loc_d2, &rem_d2);

        // Test read_chunk_with_payload with externally provided bytes
        let raw_payload = local_decoder.read_chunk_payload(chunk_idx).unwrap();
        let (pay_b, pay_d) = remote_decoder
            .read_chunk_with_payload(chunk_idx, Some(raw_payload.as_ref()), None)
            .unwrap();
        assert_eq!(loc_b2, pay_b);
        assert_decoding_result_eq(&loc_d2, &pay_d);
    }
}

#[test]
fn test_s3_url_regional_and_custom_endpoints() {
    let _env_lock = ENV_MUTEX.lock().unwrap();
    // 1. Default S3 URL
    let url_default = normalize_url("s3://test-bucket/prefix/cog.tif").unwrap();
    assert_eq!(
        url_default,
        "https://test-bucket.s3.amazonaws.com/prefix/cog.tif"
    );

    // 2. Explicit AWS_REGION
    std::env::set_var("AWS_REGION", "us-west-2");
    let url_west = normalize_url("s3://test-bucket/prefix/cog.tif").unwrap();
    assert_eq!(
        url_west,
        "https://test-bucket.s3.us-west-2.amazonaws.com/prefix/cog.tif"
    );
    std::env::remove_var("AWS_REGION");

    // 3. Fallback AWS_DEFAULT_REGION
    std::env::set_var("AWS_DEFAULT_REGION", "eu-central-1");
    let url_eu = normalize_url("s3://test-bucket/prefix/cog.tif").unwrap();
    assert_eq!(
        url_eu,
        "https://test-bucket.s3.eu-central-1.amazonaws.com/prefix/cog.tif"
    );
    std::env::remove_var("AWS_DEFAULT_REGION");

    // 4. Custom endpoint path-style (MinIO / LocalStack)
    std::env::set_var("AWS_ENDPOINT_URL", "http://localhost:9000");
    let url_minio = normalize_url("s3://my-bucket/data/cog.tif").unwrap();
    assert_eq!(url_minio, "http://localhost:9000/my-bucket/data/cog.tif");

    // 5. Custom endpoint virtual-hosted style
    std::env::set_var("AWS_S3_ADDRESSING_STYLE", "virtual");
    let url_minio_virtual = normalize_url("s3://my-bucket/data/cog.tif").unwrap();
    assert_eq!(
        url_minio_virtual,
        "http://my-bucket.localhost:9000/data/cog.tif"
    );
    std::env::remove_var("AWS_ENDPOINT_URL");
    std::env::remove_var("AWS_S3_ADDRESSING_STYLE");
}

#[test]
fn test_remote_transient_retry_and_recovery() {
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

    // Start mock server configured to return HTTP 503 on the first 2 requests
    let server = MockHttpServer::start_with_failures(file_bytes, 2);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let local_reader = GeoTiffStreamReader::open(&local_path).unwrap();

    // Open should automatically retry through exponential backoff and succeed on 3rd attempt
    let reader = GeoTiffStreamReader::open(&remote_url)
        .expect("Reader open should recover from transient 503 errors");

    assert_eq!(reader.metadata.width, local_reader.metadata.width);
    assert_eq!(reader.metadata.height, local_reader.metadata.height);

    let total_reqs = server.request_count.load(Ordering::SeqCst);
    assert!(
        total_reqs >= 3,
        "Expected at least 3 requests (2 failures + 1 success), got {}",
        total_reqs
    );
}

#[test]
fn test_remote_request_headers_injection() {
    let _env_lock = ENV_MUTEX.lock().unwrap();
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

    std::env::set_var("AWS_REQUEST_PAYER", "requester");
    std::env::set_var("RASTER_H3_AUTH_TOKEN", "test-secret-token-xyz");

    let _reader = GeoTiffStreamReader::open(&remote_url).expect("Reader open with custom headers");

    std::env::remove_var("AWS_REQUEST_PAYER");
    std::env::remove_var("RASTER_H3_AUTH_TOKEN");

    let headers = server.recorded_headers();
    let found_payer = headers
        .iter()
        .any(|h| h.to_lowercase().contains("x-amz-request-payer: requester"));
    let found_auth = headers.iter().any(|h| {
        h.to_lowercase()
            .contains("authorization: bearer test-secret-token-xyz")
    });

    assert!(
        found_payer,
        "Expected x-amz-request-payer header in HTTP request"
    );
    assert!(found_auth, "Expected authorization header in HTTP request");
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
