//! Tests remote Cloud Optimized GeoTIFF (COG) streaming over HTTP/S3.
//!
//! Validates HTTP byte-range requests via a mock server, prefetch range coalescing, retry and recovery,
//! spatial ROI chunk filtering, header budget optimization, and streaming Parquet pipeline integration.

mod chunk_decode;
mod etag;
mod prefetch;
mod range_responses;
mod requests;
mod streaming;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use rustc_hash::FxHashMap;

static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Lightweight mock HTTP server supporting HTTP Range requests (`bytes=start-end`)
struct MockHttpServer {
    #[allow(dead_code)]
    port: u16,
    url_base: String,
    bytes_served: Arc<AtomicUsize>,
    request_count: Arc<AtomicUsize>,
    #[allow(dead_code)]
    transient_failures: Arc<AtomicUsize>,
    body_truncations: Arc<AtomicUsize>,
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
        Self::try_start_on("127.0.0.1", file_bytes, failure_count)
            .expect("Failed to bind ephemeral port")
    }

    /// A server on another loopback address (`127.0.0.2`), so a redirect can
    /// cross hosts; `None` where that address is not configured (macOS by default).
    fn start_on_second_loopback(file_bytes: Vec<u8>) -> Option<Self> {
        let server = Self::try_start_on("127.0.0.2", file_bytes, 0)?;
        let addr = format!("127.0.0.2:{}", server.port).parse().ok()?;
        TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
        Some(server)
    }

    fn try_start_on(ip: &str, file_bytes: Vec<u8>, failure_count: usize) -> Option<Self> {
        let listener = TcpListener::bind((ip, 0)).ok()?;
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let bytes_served = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::new(AtomicUsize::new(0));
        let transient_failures = Arc::new(AtomicUsize::new(failure_count));
        let body_truncations = Arc::new(AtomicUsize::new(0));
        let recorded_headers = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(RequestGate::default());
        let gate_clone = Arc::clone(&gate);

        let shutdown_clone = Arc::clone(&shutdown);
        let bytes_served_clone = Arc::clone(&bytes_served);
        let request_count_clone = Arc::clone(&request_count);
        let failures_clone = Arc::clone(&transient_failures);
        let truncations_clone = Arc::clone(&body_truncations);
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
                let truncs = Arc::clone(&truncations_clone);
                let hdrs = Arc::clone(&headers_clone);
                let gate = Arc::clone(&gate_clone);

                thread::spawn(move || {
                    // Accepted sockets can inherit the listener's nonblocking mode on macOS.
                    stream
                        .set_nonblocking(false)
                        .expect("Failed to make accepted socket blocking");
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    Self::handle_connection(
                        stream, &file_data, served, reqs, fails, truncs, hdrs, gate,
                    );
                });
            }
        });

        Some(Self {
            port,
            url_base: format!("http://{ip}:{port}"),
            bytes_served,
            request_count,
            transient_failures,
            body_truncations,
            recorded_headers,
            gate,
            shutdown,
            handle: Some(handle),
        })
    }

    fn recorded_headers(&self) -> Vec<String> {
        self.recorded_headers.lock().unwrap().clone()
    }

    /// For a redirect fixture, `Some(Location)` (`Some(None)` for a redirect
    /// without one):
    /// - `redirect_hops_<k>`: `k` relative hops to the same path with `k - 1`;
    /// - `redirect_without_location`: a 302 with no `Location`;
    /// - `redirect_to_<ip>_<port>/<rest>`: an absolute redirect to `http://<ip>:<port>/<rest>`.
    #[allow(clippy::option_option)]
    fn redirect_location(path: &str) -> Option<Option<String>> {
        if path.contains("redirect_without_location") {
            return Some(None);
        }
        if let Some((_, rest)) = path.split_once("redirect_to_") {
            let (target, tail) = rest.split_once('/')?;
            let (ip, port) = target.rsplit_once('_')?;
            return Some(Some(format!("http://{ip}:{port}/{tail}")));
        }
        let (_, rest) = path.split_once("redirect_hops_")?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let hops: usize = digits.parse().ok()?;
        (hops > 0).then(|| {
            Some(path.replacen(
                &format!("redirect_hops_{digits}"),
                &format!("redirect_hops_{}", hops - 1),
                1,
            ))
        })
    }

    /// Atomic decrement-if-positive: whether this connection may serve a
    /// truncated body once.
    // `try_update` replaces this on Rust 1.95+, but rust-version is 1.94.
    #[allow(deprecated)]
    fn try_truncate(body_truncations: &AtomicUsize) -> bool {
        body_truncations
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                if count > 0 {
                    Some(count - 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_connection(
        mut stream: TcpStream,
        file_bytes: &[u8],
        bytes_served: Arc<AtomicUsize>,
        request_count: Arc<AtomicUsize>,
        transient_failures: Arc<AtomicUsize>,
        body_truncations: Arc<AtomicUsize>,
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
        let mut if_match: Option<String> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                break;
            }
            if let Ok(mut h) = recorded_headers.lock() {
                h.push(line.clone());
            }
            if line.to_lowercase().starts_with("if-match:") {
                if_match = Some(line["if-match:".len()..].trim().to_string());
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

        // Redirect fixtures answer before the request is counted, so the
        // version fixtures below see only the final hop.
        if let Some(location) = Self::redirect_location(path) {
            let location = location
                .map(|l| format!("Location: {l}\r\n"))
                .unwrap_or_default();
            let resp = format!(
                "HTTP/1.1 302 Found\r\n{location}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        if path.contains("forbidden") {
            let resp = "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        if path.contains("not_found") {
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        let request_number = request_count.fetch_add(1, Ordering::SeqCst);

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
            // Paths containing `retry_after` ask the client to wait one second.
            let retry_after = if path.contains("retry_after") {
                "Retry-After: 1\r\n"
            } else {
                ""
            };
            let resp = format!("HTTP/1.1 503 Service Unavailable\r\n{retry_after}Content-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            return;
        }

        // Version fixtures replace bytes without changing length after the probe.
        let replaced = request_number > 0
            && [
                "etag_changed",
                "etag_ignored",
                "etag_missing",
                "etag_downgraded",
            ]
            .iter()
            .any(|mode| path.contains(mode));
        let etag = if path.contains("etag_weak") {
            Some("W/\"v1\"")
        } else if replaced && path.contains("etag_missing") {
            None
        } else if replaced && path.contains("etag_downgraded") {
            Some("W/\"v1\"")
        } else if replaced {
            Some("\"v2\"")
        } else if path.contains("etag_other") {
            Some("\"other\"")
        } else if path.contains("etag_") {
            Some("\"v1\"")
        } else {
            None
        };
        if path.contains("etag_changed") && if_match.as_deref().is_some_and(|tag| Some(tag) != etag)
        {
            let _ = stream.write_all(b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let mut version_header = etag
            .map(|tag| format!("ETag: {tag}\r\n"))
            .unwrap_or_default();
        // `padded_head_<N>` adds an N-byte header value to every response head.
        if let Some(n) = path
            .split("padded_head_")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse::<usize>().ok())
        {
            version_header.push_str(&format!("X-Padding: {}\r\n", "p".repeat(n)));
        }
        let replacement;
        let file_bytes = if replaced {
            replacement = file_bytes
                .iter()
                .map(|byte| byte ^ 0xff)
                .collect::<Vec<_>>();
            replacement.as_slice()
        } else {
            file_bytes
        };

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
            let body = if path.contains("truncate_any") && Self::try_truncate(&body_truncations) {
                &file_bytes[..file_bytes.len() / 2]
            } else {
                file_bytes
            };
            if stream.write_all(header.as_bytes()).is_ok() && method == "GET" {
                let _ = stream.write_all(body);
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

                // Simulate a mid-body connection reset: announce the full
                // Content-Length but close after writing only half the body.
                let truncate_hit = (range_str != "bytes=0-131071" || path.contains("truncate_any"))
                    && Self::try_truncate(&body_truncations);
                if truncate_hit {
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\n\
                        {version_header}Accept-Ranges: bytes\r\n\
                        Content-Type: image/tiff\r\n\
                        Content-Range: bytes {}-{}/{}\r\n\
                        Content-Length: {}\r\n\
                        Connection: close\r\n\r\n",
                        start, end, total_size, content_len
                    );
                    if stream.write_all(header.as_bytes()).is_ok() && method == "GET" {
                        let _ = stream.write_all(&slice[..slice.len() / 2]);
                    }
                    return;
                }

                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\n\
                    {version_header}Accept-Ranges: bytes\r\n\
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
