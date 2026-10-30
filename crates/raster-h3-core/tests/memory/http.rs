//! Minimal HTTP/1.1 server for remote-COG scenarios: serves one file with
//! `Range: bytes=a-b` support, optional per-request latency, and counts
//! requests and bytes served.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Default)]
pub struct Counters {
    /// Range requests answered with 206.
    pub range_requests: AtomicU64,
    /// Every request, including header reads.
    pub requests: AtomicU64,
    pub bytes_served: AtomicU64,
}

pub struct RangeServer {
    pub url: String,
    pub counters: Arc<Counters>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl RangeServer {
    pub fn start(file: Vec<u8>, latency: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let file = Arc::new(file);
        let counters = Arc::new(Counters::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = {
            let (counters, shutdown) = (Arc::clone(&counters), Arc::clone(&shutdown));
            thread::spawn(move || {
                while !shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (file, counters) = (Arc::clone(&file), Arc::clone(&counters));
                            thread::spawn(move || serve(stream, &file, &counters, latency));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            url: format!("http://127.0.0.1:{port}/raster.tif"),
            counters,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn range_requests(&self) -> u64 {
        self.counters.range_requests.load(Ordering::SeqCst)
    }

    pub fn bytes_served(&self) -> u64 {
        self.counters.bytes_served.load(Ordering::SeqCst)
    }
}

impl Drop for RangeServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(stream: TcpStream, file: &[u8], counters: &Counters, latency: Duration) {
    // Accepted sockets can inherit the listener's nonblocking mode on macOS.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let method = request_line.split_whitespace().next().unwrap_or("");
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some(value) = line
            .to_ascii_lowercase()
            .strip_prefix("range:")
            .and_then(|v| v.trim().strip_prefix("bytes="))
        {
            range = value.split_once('-').and_then(|(a, b)| {
                let start: usize = a.trim().parse().ok()?;
                let end: usize = b.trim().parse::<usize>().ok()?.min(file.len() - 1);
                (start <= end).then_some((start, end))
            });
        }
    }
    counters.requests.fetch_add(1, Ordering::SeqCst);
    if !latency.is_zero() {
        thread::sleep(latency);
    }
    let mut stream = &stream;
    let (status, body) = match range {
        Some((start, end)) => {
            counters.range_requests.fetch_add(1, Ordering::SeqCst);
            (
                format!(
                    "206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}",
                    file.len()
                ),
                &file[start..=end],
            )
        }
        None => ("200 OK".to_string(), file),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(header.as_bytes()).is_ok() && method == "GET" {
        counters
            .bytes_served
            .fetch_add(body.len() as u64, Ordering::SeqCst);
        let _ = stream.write_all(body);
    }
}
