//! Cloud-Native Remote COG Streaming via HTTP / HTTPS / S3 Range Requests
//!
//! Enables zero-download streaming of remote Cloud-Optimized GeoTIFFs (COGs).
//! Uses HTTP Range requests (`bytes=start-end`) to read only the initial IFD header
//! and the specific tile chunks intersecting the scanline horizon or spatial bounding box.

use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, RwLock};
use fxhash::FxHashMap;
use ureq::Agent;
use url::Url;

use crate::error::{RasterH3Error, Result};

/// Default block cache size for header and tag reading (128 KB)
pub const DEFAULT_BLOCK_SIZE: usize = 131072;

/// Check if a path or string is a remote URL (http, https, or s3)
pub fn is_remote_url(path: &str) -> bool {
    let p = path.trim().to_lowercase();
    p.starts_with("http://") || p.starts_with("https://") || p.starts_with("s3://")
}

/// Normalize input URL, resolving `s3://bucket/key` into an HTTPS URL with region and endpoint support
pub fn normalize_url(raw_url: &str) -> Result<String> {
    let trimmed = raw_url.trim();
    if trimmed.starts_with("s3://") {
        let after_scheme = &trimmed[5..];
        let mut parts = after_scheme.splitn(2, '/');
        let bucket = parts.next().unwrap_or("");
        let key = parts.next().unwrap_or("");
        if bucket.is_empty() {
            return Err(RasterH3Error::InvalidParameter(format!(
                "Invalid S3 URL (missing bucket): {}",
                raw_url
            )));
        }

        // 1. Check for custom endpoint (AWS_ENDPOINT_URL or AWS_S3_ENDPOINT)
        let custom_endpoint = std::env::var("AWS_ENDPOINT_URL")
            .or_else(|_| std::env::var("AWS_S3_ENDPOINT"))
            .ok();

        if let Some(endpoint) = custom_endpoint {
            let endpoint_trimmed = endpoint.trim_end_matches('/');
            let addressing_style = std::env::var("AWS_S3_ADDRESSING_STYLE")
                .unwrap_or_else(|_| "path".to_string())
                .to_lowercase();

            if addressing_style == "virtual" {
                if let Some(rest) = endpoint_trimmed.strip_prefix("https://") {
                    Ok(format!("https://{}.{}/{}", bucket, rest, key))
                } else if let Some(rest) = endpoint_trimmed.strip_prefix("http://") {
                    Ok(format!("http://{}.{}/{}", bucket, rest, key))
                } else {
                    Ok(format!("https://{}.{}/{}", bucket, endpoint_trimmed, key))
                }
            } else {
                Ok(format!("{}/{}/{}", endpoint_trimmed, bucket, key))
            }
        } else {
            // 2. Check for explicit region (AWS_REGION or AWS_DEFAULT_REGION)
            let region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .ok();

            if let Some(reg) = region {
                let reg_trimmed = reg.trim();
                if !reg_trimmed.is_empty() && reg_trimmed != "us-east-1" {
                    Ok(format!("https://{}.s3.{}.amazonaws.com/{}", bucket, reg_trimmed, key))
                } else {
                    Ok(format!("https://{}.s3.amazonaws.com/{}", bucket, key))
                }
            } else {
                Ok(format!("https://{}.s3.amazonaws.com/{}", bucket, key))
            }
        }
    } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Url::parse(trimmed).map_err(|e| {
            RasterH3Error::InvalidParameter(format!("Invalid remote URL '{}': {}", raw_url, e))
        })?;
        Ok(trimmed.to_string())
    } else {
        Err(RasterH3Error::InvalidParameter(format!(
            "Unsupported remote protocol in '{}': expected http://, https://, or s3://",
            raw_url
        )))
    }
}

/// Retrieve default headers for cloud requests (requester pays, auth tokens)
fn get_default_headers() -> Vec<(String, String)> {
    let mut headers = Vec::new();

    // Requester pays (AWS Open Data Program)
    if let Ok(payer) = std::env::var("AWS_REQUEST_PAYER") {
        if payer.trim().eq_ignore_ascii_case("requester") {
            headers.push(("x-amz-request-payer".to_string(), "requester".to_string()));
        }
    }

    // Bearer token or AWS session token
    if let Ok(token) = std::env::var("RASTER_H3_AUTH_TOKEN") {
        if !token.trim().is_empty() {
            headers.push(("Authorization".to_string(), format!("Bearer {}", token.trim())));
        }
    } else if let Ok(session_token) = std::env::var("AWS_SESSION_TOKEN") {
        if !session_token.trim().is_empty() {
            headers.push(("x-amz-security-token".to_string(), session_token.trim().to_string()));
        }
    }

    headers
}

/// Check if an HTTP error is transient and eligible for retry
fn is_transient_error(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::Status(status, _) => {
            // 429: Too Many Requests / S3 SlowDown
            // 500: Internal Server Error
            // 502: Bad Gateway
            // 503: Service Unavailable (S3 503 SlowDown)
            // 504: Gateway Timeout
            matches!(*status, 429 | 500 | 502 | 503 | 504)
        }
        ureq::Error::Transport(_) => true,
    }
}

/// Execute a request with automatic exponential backoff retry on transient errors
fn execute_request_with_retry<F>(
    url: &str,
    max_retries: usize,
    mut make_request: F,
) -> Result<ureq::Response>
where
    F: FnMut() -> std::result::Result<ureq::Response, ureq::Error>,
{
    let mut attempt = 0;
    let base_delay_ms = 100u64;

    loop {
        match make_request() {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                if attempt >= max_retries || !is_transient_error(&e) {
                    return match e {
                        ureq::Error::Status(404, _) => Err(RasterH3Error::InvalidParameter(format!(
                            "Remote raster not found (HTTP 404): {}",
                            url
                        ))),
                        ureq::Error::Status(status, r) => Err(RasterH3Error::InvalidParameter(format!(
                            "HTTP request failed for '{}': HTTP {} - {}",
                            url,
                            status,
                            r.status_text()
                        ))),
                        ureq::Error::Transport(t) => Err(RasterH3Error::InvalidParameter(format!(
                            "Network transport error connecting to '{}': {}",
                            url, t
                        ))),
                    };
                }

                // Exponential backoff with small jitter
                let jitter = (attempt as u64 * 37) % 50;
                let backoff_ms = (base_delay_ms * (1 << attempt) + jitter).min(2000);
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                attempt += 1;
            }
        }
    }
}

/// Shared, thread-safe connection and block-cache state for a remote GeoTIFF
pub struct RemoteHttpSource {
    pub url: String,
    pub total_size: u64,
    agent: Agent,
    headers: Vec<(String, String)>,
    cache: RwLock<FxHashMap<u64, Arc<Vec<u8>>>>,
    block_size: usize,
}

impl RemoteHttpSource {
    /// Open a remote GeoTIFF, probing byte-range support and fetching the initial header block
    pub fn open(raw_url: &str) -> Result<Self> {
        let url = normalize_url(raw_url)?;

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
            .max_idle_connections(128)
            .max_idle_connections_per_host(32)
            .build();

        let headers = get_default_headers();
        let initial_fetch_len = DEFAULT_BLOCK_SIZE;
        let range_header = format!("bytes=0-{}", initial_fetch_len - 1);

        let resp = execute_request_with_retry(&url, 4, || {
            let mut req = agent.get(&url).set("Range", &range_header);
            for (k, v) in &headers {
                req = req.set(k, v);
            }
            req.call()
        })?;

        let status = resp.status();
        let mut total_size = 0u64;

        let initial_bytes = if status == 206 {
            // Parse Content-Range: bytes 0-131071/total_size
            if let Some(cr) = resp.header("Content-Range") {
                if let Some(slash_idx) = cr.rfind('/') {
                    let total_str = &cr[slash_idx + 1..].trim();
                    if let Ok(ts) = total_str.parse::<u64>() {
                        total_size = ts;
                    }
                }
            }
            let mut reader = resp.into_reader();
            let mut buf = Vec::with_capacity(initial_fetch_len);
            reader.read_to_end(&mut buf).map_err(|e| {
                RasterH3Error::InvalidParameter(format!(
                    "Failed reading initial header bytes from '{}': {}",
                    url, e
                ))
            })?;
            buf
        } else if status == 200 {
            // Server returned entire body (or file is small)
            let cl = resp
                .header("Content-Length")
                .and_then(|h| h.parse::<u64>().ok())
                .unwrap_or(0);
            let mut reader = resp.into_reader();
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).map_err(|e| {
                RasterH3Error::InvalidParameter(format!(
                    "Failed reading response from '{}': {}",
                    url, e
                ))
            })?;
            total_size = if cl > 0 { cl } else { buf.len() as u64 };
            buf
        } else {
            return Err(RasterH3Error::InvalidParameter(format!(
                "Unexpected HTTP status {} from '{}'",
                status, url
            )));
        };

        if total_size == 0 {
            total_size = initial_bytes.len() as u64;
        }

        let mut cache = FxHashMap::default();
        cache.insert(0, Arc::new(initial_bytes));

        Ok(Self {
            url,
            total_size,
            agent,
            headers,
            cache: RwLock::new(cache),
            block_size: initial_fetch_len,
        })
    }

    /// Fetch an arbitrary byte range directly from the remote server with retry
    pub fn fetch_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let range_header = format!("bytes={}-{}", start, end);
        let resp = execute_request_with_retry(&self.url, 4, || {
            let mut req = self.agent.get(&self.url).set("Range", &range_header);
            for (k, v) in &self.headers {
                req = req.set(k, v);
            }
            req.call()
        })?;

        let mut reader = resp.into_reader();
        let mut buf = Vec::with_capacity((end - start + 1) as usize);
        reader.read_to_end(&mut buf).map_err(|e| {
            RasterH3Error::InvalidParameter(format!(
                "Failed reading range {} from '{}': {}",
                range_header, self.url, e
            ))
        })?;

        Ok(buf)
    }

    /// Read bytes starting at `offset` directly into `out`, using the block cache for small reads.
    /// Eliminates intermediate buffer allocations.
    pub fn read_range_into(&self, offset: u64, out: &mut [u8]) -> Result<usize> {
        if offset >= self.total_size || out.is_empty() {
            return Ok(0);
        }

        let actual_len = out.len().min((self.total_size - offset) as usize);

        // For large reads (larger than 1 block), bypass the block cache and fetch exact range
        if actual_len > self.block_size {
            let fetched = self.fetch_range(offset, offset + actual_len as u64 - 1)?;
            let to_copy = fetched.len().min(actual_len);
            out[..to_copy].copy_from_slice(&fetched[..to_copy]);
            return Ok(to_copy);
        }

        let block_idx = offset / self.block_size as u64;
        let block_start = block_idx * self.block_size as u64;
        let offset_in_block = (offset - block_start) as usize;

        // 1. Fast path: check block cache under read lock
        {
            let cache = self.cache.read().map_err(|_| {
                RasterH3Error::InvalidParameter("Remote HTTP cache lock poisoned".to_string())
            })?;
            if let Some(block) = cache.get(&block_idx) {
                if offset_in_block < block.len() {
                    let available = (block.len() - offset_in_block).min(actual_len);
                    out[..available].copy_from_slice(&block[offset_in_block..offset_in_block + available]);
                    return Ok(available);
                }
            }
        }

        // 2. Slow path: fetch block without holding write lock to allow parallel fetches
        let block_end = (block_start + self.block_size as u64).min(self.total_size);
        let fetched_bytes = self.fetch_range(block_start, block_end - 1)?;
        let block_arc = Arc::new(fetched_bytes);

        // Insert into cache under write lock (retaining existing if race occurred)
        let final_block = {
            let mut cache = self.cache.write().map_err(|_| {
                RasterH3Error::InvalidParameter("Remote HTTP cache lock poisoned".to_string())
            })?;
            cache
                .entry(block_idx)
                .or_insert_with(|| Arc::clone(&block_arc))
                .clone()
        };

        let available = (final_block.len().saturating_sub(offset_in_block)).min(actual_len);
        out[..available].copy_from_slice(&final_block[offset_in_block..offset_in_block + available]);
        Ok(available)
    }

    /// Read bytes starting at `offset` up to `len` bytes, using the block cache for small reads
    pub fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = self.read_range_into(offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

/// A streaming reader implementing `Read + Seek` over a remote HTTP GeoTIFF.
/// Used directly by `tiff::decoder::Decoder` to parse headers and fetch chunks on-demand.
#[derive(Clone)]
pub struct HttpRangeReader {
    pub source: Arc<RemoteHttpSource>,
    pub cursor: u64,
}

impl HttpRangeReader {
    /// Create a new reader with its cursor positioned at offset 0
    pub fn new(source: Arc<RemoteHttpSource>) -> Self {
        Self { source, cursor: 0 }
    }
}

impl Read for HttpRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cursor >= self.source.total_size || buf.is_empty() {
            return Ok(0);
        }

        let to_read = buf
            .len()
            .min((self.source.total_size - self.cursor) as usize);
        let bytes_read = self
            .source
            .read_range_into(self.cursor, &mut buf[..to_read])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        self.cursor += bytes_read as u64;
        Ok(bytes_read)
    }
}

impl Seek for HttpRangeReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_cursor = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(delta) => {
                let curr = self.cursor as i64;
                let target = curr + delta;
                if target < 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Cannot seek before start of file",
                    ));
                }
                target as u64
            }
            SeekFrom::End(delta) => {
                let end = self.source.total_size as i64;
                let target = end + delta;
                if target < 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Cannot seek before start of file",
                    ));
                }
                target as u64
            }
        };

        self.cursor = new_cursor;
        Ok(self.cursor)
    }
}
