//! Cloud-Native Remote COG Streaming via HTTP / HTTPS / S3 Range Requests
//!
//! Enables zero-download streaming of remote Cloud-Optimized GeoTIFFs (COGs).
//! Uses HTTP Range requests (`bytes=start-end`) to read only the initial IFD header
//! and the specific tile chunks intersecting the scanline horizon or spatial bounding box.
//!
//! # Architecture & Responsibilities
//! - [`HttpTransport`]: Handles HTTP client creation, authentication headers, transient retries
//!   with exponential backoff and jitter, and HTTP status/header/range length validation.
//! - [`ByteCache`]: Manages in-memory caching of remote bytes, explicitly distinguishing
//!   between full file downloads (complete responses) and paged block caching.
//! - [`RemoteHttpSource`]: High-level orchestrator coordinating transport and caching.
//! - [`HttpRangeReader`]: Standard `Read + Seek` adapter over [`RemoteHttpSource`].
//!
//! # Read Contracts and Invariants
//! - **Short reads at EOF**: [`RemoteHttpSource::read_range_into`] and [`RemoteHttpSource::read_range`]
//!   permit short reads when reading against the file tail (i.e. `offset + requested_len > total_size`).
//!   Available bytes are copied and the number of actual bytes read is returned without error.
//!   If `offset >= total_size`, `Ok(0)` is returned.
//! - **Transport short reads are ERRORS**: If the remote server returns fewer bytes than requested
//!   for an in-bounds byte range, [`HttpTransport`] returns an `Err(RasterH3Error::InvalidParameter)`
//!   rather than returning a silent partial payload that would corrupt decoder state.
//! - **Exact range reads**: [`RemoteHttpSource::read_exact_range`] enforces that the entire requested
//!   byte count is satisfied. If the requested range extends past `total_size` or if fewer bytes are
//!   returned, an explicit error is returned.

use fxhash::FxHashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, RwLock};
use ureq::Agent;
use url::Url;

use crate::error::{RasterH3Error, Result};

/// Default block cache size for header and tag reading (128 KB)
pub const DEFAULT_BLOCK_SIZE: usize = 131072;

/// Parse a standards-compliant `Content-Range` value of the form
/// `bytes start-end/total`.
pub fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim().strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = total.parse().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

pub fn validate_range_response(
    status: u16,
    content_range: Option<&str>,
    start: u64,
    end: u64,
    expected_total: Option<u64>,
    body_len: usize,
) -> Result<u64> {
    let total = validate_range_response_headers(status, content_range, start, end, expected_total)?;
    if body_len != (end - start + 1) as usize {
        return Err(RasterH3Error::InvalidParameter(format!(
            "Remote range response for bytes {}-{} has {} bytes, expected {}",
            start,
            end,
            body_len,
            end - start + 1
        )));
    }
    Ok(total)
}

pub fn validate_range_response_headers(
    status: u16,
    content_range: Option<&str>,
    start: u64,
    end: u64,
    expected_total: Option<u64>,
) -> Result<u64> {
    if status != 206 {
        return Err(RasterH3Error::InvalidParameter(format!(
            "Remote server ignored byte range {}-{} (HTTP {})",
            start, end, status
        )));
    }
    let (actual_start, actual_end, total) =
        content_range.and_then(parse_content_range).ok_or_else(|| {
            RasterH3Error::InvalidParameter(
                "Remote range response lacks a valid Content-Range header".to_string(),
            )
        })?;
    if actual_start != start
        || actual_end != end
        || expected_total.is_some_and(|size| size != total)
    {
        return Err(RasterH3Error::InvalidParameter(format!(
            "Remote range response does not match requested bytes {}-{}",
            start, end
        )));
    }
    Ok(total)
}

/// Check if a path or string is a remote URL (http, https, or s3)
pub fn is_remote_url(path: &str) -> bool {
    let p = path.trim().to_lowercase();
    p.starts_with("http://") || p.starts_with("https://") || p.starts_with("s3://")
}

/// Normalize input URL, resolving `s3://bucket/key` into an HTTPS URL with region and endpoint support
pub fn normalize_url(raw_url: &str) -> Result<String> {
    let trimmed = raw_url.trim();
    if let Some(after_scheme) = trimmed.strip_prefix("s3://") {
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
                    Ok(format!(
                        "https://{}.s3.{}.amazonaws.com/{}",
                        bucket, reg_trimmed, key
                    ))
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
            headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", token.trim()),
            ));
        }
    } else if let Ok(session_token) = std::env::var("AWS_SESSION_TOKEN") {
        if !session_token.trim().is_empty() {
            headers.push((
                "x-amz-security-token".to_string(),
                session_token.trim().to_string(),
            ));
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

/// Result of an initial probe request against a remote raster
#[derive(Debug)]
pub enum ProbeResult {
    /// Server returned the complete file payload (HTTP 200).
    Complete { data: Vec<u8>, total_size: u64 },
    /// Server accepted the range request and returned the requested initial block (HTTP 206).
    Partial { data: Vec<u8>, total_size: u64 },
}

/// Remote HTTP transport handling connections, retries, headers, and range requests.
#[derive(Clone)]
pub struct HttpTransport {
    pub url: String,
    agent: Agent,
    headers: Vec<(String, String)>,
    max_retries: usize,
}

#[allow(clippy::result_large_err)]
impl HttpTransport {
    /// Create a new transport client with configured connection pooling and timeouts
    pub fn new(url: &str) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
            .max_idle_connections(128)
            .max_idle_connections_per_host(32)
            .build();

        let headers = get_default_headers();
        Ok(Self {
            url: url.to_string(),
            agent,
            headers,
            max_retries: 4,
        })
    }

    /// Execute a request with automatic exponential backoff retry on transient errors
    pub fn execute_request<F>(&self, mut make_request: F) -> Result<ureq::Response>
    where
        F: FnMut() -> std::result::Result<ureq::Response, ureq::Error>,
    {
        let mut attempt = 0;
        let base_delay_ms = 100u64;

        loop {
            match make_request() {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if attempt >= self.max_retries || !is_transient_error(&e) {
                        return match e {
                            ureq::Error::Status(404, _) => Err(RasterH3Error::InvalidParameter(
                                format!("Remote raster not found (HTTP 404): {}", self.url),
                            )),
                            ureq::Error::Status(status, r) => {
                                Err(RasterH3Error::InvalidParameter(format!(
                                    "HTTP request failed for '{}': HTTP {} - {}",
                                    self.url,
                                    status,
                                    r.status_text()
                                )))
                            }
                            ureq::Error::Transport(t) => {
                                Err(RasterH3Error::InvalidParameter(format!(
                                    "Network transport error connecting to '{}': {}",
                                    self.url, t
                                )))
                            }
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

    /// Probe remote server to determine byte-range capability and initial header data.
    pub fn probe(&self, initial_fetch_len: usize) -> Result<ProbeResult> {
        let range_header = format!("bytes=0-{}", initial_fetch_len.saturating_sub(1));
        let resp = self.execute_request(|| {
            let mut req = self.agent.get(&self.url).set("Range", &range_header);
            for (k, v) in &self.headers {
                req = req.set(k, v);
            }
            req.call()
        })?;

        let status = resp.status();
        if status == 206 {
            let content_range = resp.header("Content-Range").map(str::to_owned);
            let (_, _, parsed_total) = content_range
                .as_deref()
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    RasterH3Error::InvalidParameter(
                        "Remote range response lacks a valid Content-Range header".to_string(),
                    )
                })?;
            let expected_end = (initial_fetch_len as u64 - 1).min(parsed_total - 1);
            validate_range_response_headers(
                status,
                content_range.as_deref(),
                0,
                expected_end,
                None,
            )?;
            let reader = resp.into_reader();
            let mut buf = Vec::with_capacity(initial_fetch_len);
            reader
                .take(initial_fetch_len as u64 + 1)
                .read_to_end(&mut buf)
                .map_err(|e| {
                    RasterH3Error::InvalidParameter(format!(
                        "Failed reading initial header bytes from '{}': {}",
                        self.url, e
                    ))
                })?;
            let total_size = validate_range_response(
                status,
                content_range.as_deref(),
                0,
                expected_end,
                None,
                buf.len(),
            )?;
            Ok(ProbeResult::Partial {
                data: buf,
                total_size,
            })
        } else if status == 200 {
            // Server returned entire body (or file is small / server ignores ranges)
            let cl = resp
                .header("Content-Length")
                .and_then(|h| h.parse::<u64>().ok())
                .unwrap_or(0);
            let mut reader = resp.into_reader();
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).map_err(|e| {
                RasterH3Error::InvalidParameter(format!(
                    "Failed reading response from '{}': {}",
                    self.url, e
                ))
            })?;
            if cl > 0 && cl != buf.len() as u64 {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Remote response has {} bytes but Content-Length says {}",
                    buf.len(),
                    cl
                )));
            }
            let total_size = buf.len() as u64;
            Ok(ProbeResult::Complete {
                data: buf,
                total_size,
            })
        } else {
            Err(RasterH3Error::InvalidParameter(format!(
                "Unexpected HTTP status {} from '{}'",
                status, self.url
            )))
        }
    }

    /// Fetch an arbitrary byte range directly from the remote server with retry and verification
    pub fn fetch_range(
        &self,
        start: u64,
        end: u64,
        expected_total: Option<u64>,
    ) -> Result<Vec<u8>> {
        if start > end || expected_total.is_some_and(|total| end >= total) {
            let total_msg = expected_total
                .map(|t| format!(" of {} bytes", t))
                .unwrap_or_default();
            return Err(RasterH3Error::InvalidParameter(format!(
                "Requested remote byte range {}-{} lies outside file{}",
                start, end, total_msg
            )));
        }
        let range_header = format!("bytes={}-{}", start, end);
        let resp = self.execute_request(|| {
            let mut req = self.agent.get(&self.url).set("Range", &range_header);
            for (k, v) in &self.headers {
                req = req.set(k, v);
            }
            req.call()
        })?;

        let status = resp.status();
        let content_range = resp.header("Content-Range").map(str::to_owned);

        let expected_len = (end - start + 1) as usize;
        validate_range_response_headers(
            status,
            content_range.as_deref(),
            start,
            end,
            expected_total,
        )?;

        let reader = resp.into_reader();
        let mut buf = Vec::with_capacity(expected_len);
        reader
            .take(expected_len as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| {
                RasterH3Error::InvalidParameter(format!(
                    "Failed reading range {} from '{}': {}",
                    range_header, self.url, e
                ))
            })?;

        validate_range_response(
            status,
            content_range.as_deref(),
            start,
            end,
            expected_total,
            buf.len(),
        )?;

        Ok(buf)
    }
}

/// Thread-safe in-memory cache for remote bytes.
///
/// Explicitly distinguishes between:
/// - A completely downloaded response (e.g. from an HTTP 200 probe or small file),
///   where all byte slices can be served directly from memory without network access.
/// - A block-sparse cache, where chunks are fetched in aligned `block_size` pages.
pub struct ByteCache {
    block_size: usize,
    complete_file: RwLock<Option<Arc<Vec<u8>>>>,
    blocks: RwLock<FxHashMap<u64, Arc<Vec<u8>>>>,
}

impl ByteCache {
    /// Create a cache representing a completely downloaded file.
    pub fn new_with_complete(data: Vec<u8>, block_size: usize) -> Self {
        Self {
            block_size,
            complete_file: RwLock::new(Some(Arc::new(data))),
            blocks: RwLock::new(FxHashMap::default()),
        }
    }

    /// Create a cache initialized with a single block (e.g. block 0 from a partial range probe).
    pub fn new_with_initial_block(data: Vec<u8>, block_size: usize) -> Self {
        let mut blocks = FxHashMap::default();
        blocks.insert(0, Arc::new(data));
        Self {
            block_size,
            complete_file: RwLock::new(None),
            blocks: RwLock::new(blocks),
        }
    }

    /// Returns `true` if the cache holds the complete file payload.
    pub fn is_complete(&self) -> bool {
        self.complete_file
            .read()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Configured block size in bytes.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Attempt to copy bytes from the complete file cache if available.
    ///
    /// Returns `Ok(Some(copied_bytes))` if the complete file is present, or `Ok(None)` otherwise.
    pub fn copy_from_complete(&self, offset: u64, out: &mut [u8]) -> Result<Option<usize>> {
        let guard = self.complete_file.read().map_err(|_| {
            RasterH3Error::InvalidParameter("Remote HTTP cache lock poisoned".to_string())
        })?;
        if let Some(ref full) = *guard {
            let offset = offset as usize;
            if offset >= full.len() || out.is_empty() {
                return Ok(Some(0));
            }
            let actual_len = out.len().min(full.len() - offset);
            out[..actual_len].copy_from_slice(&full[offset..offset + actual_len]);
            return Ok(Some(actual_len));
        }
        Ok(None)
    }

    /// Get a cached block by block index if present.
    pub fn get_block(&self, block_idx: u64) -> Result<Option<Arc<Vec<u8>>>> {
        let guard = self.blocks.read().map_err(|_| {
            RasterH3Error::InvalidParameter("Remote HTTP cache lock poisoned".to_string())
        })?;
        Ok(guard.get(&block_idx).cloned())
    }

    /// Insert a block into the cache under write lock, returning the retained reference
    /// (retaining any existing block if a race occurred).
    pub fn insert_block(&self, block_idx: u64, data: Arc<Vec<u8>>) -> Result<Arc<Vec<u8>>> {
        let mut guard = self.blocks.write().map_err(|_| {
            RasterH3Error::InvalidParameter("Remote HTTP cache lock poisoned".to_string())
        })?;
        Ok(guard.entry(block_idx).or_insert_with(|| data).clone())
    }
}

/// Shared, thread-safe connection and block-cache state for a remote GeoTIFF
pub struct RemoteHttpSource {
    pub url: String,
    pub total_size: u64,
    transport: HttpTransport,
    cache: ByteCache,
}

impl RemoteHttpSource {
    /// Open a remote GeoTIFF, probing byte-range support and fetching the initial header block
    pub fn open(raw_url: &str) -> Result<Self> {
        let url = normalize_url(raw_url)?;
        let transport = HttpTransport::new(&url)?;
        let probe = transport.probe(DEFAULT_BLOCK_SIZE)?;

        let (cache, total_size) = match probe {
            ProbeResult::Complete { data, total_size } => (
                ByteCache::new_with_complete(data, DEFAULT_BLOCK_SIZE),
                total_size,
            ),
            ProbeResult::Partial { data, total_size } => (
                ByteCache::new_with_initial_block(data, DEFAULT_BLOCK_SIZE),
                total_size,
            ),
        };

        Ok(Self {
            url,
            total_size,
            transport,
            cache,
        })
    }

    /// Configured block size in bytes
    pub fn block_size(&self) -> usize {
        self.cache.block_size()
    }

    /// Returns whether this source holds the complete file locally in memory
    pub fn is_complete_file(&self) -> bool {
        self.cache.is_complete()
    }

    /// Fetch an arbitrary byte range directly from the remote server with retry
    pub fn fetch_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        self.transport
            .fetch_range(start, end, Some(self.total_size))
    }

    /// Read bytes starting at `offset` directly into `out`, using the block cache for small reads.
    /// Eliminates intermediate buffer allocations.
    ///
    /// # Short Read Contract
    /// If `offset + out.len() > total_size`, only available bytes up to `total_size` are copied,
    /// and the actual number of copied bytes is returned (`< out.len()`).
    /// If `offset >= total_size`, `Ok(0)` is returned.
    pub fn read_range_into(&self, offset: u64, out: &mut [u8]) -> Result<usize> {
        if offset >= self.total_size || out.is_empty() {
            return Ok(0);
        }

        let actual_len = out.len().min((self.total_size - offset) as usize);

        // 1. Fast path: check explicit complete-file cache
        if let Some(n) = self
            .cache
            .copy_from_complete(offset, &mut out[..actual_len])?
        {
            return Ok(n);
        }

        let block_size = self.cache.block_size();
        let block_idx = offset / block_size as u64;
        let block_start = block_idx * block_size as u64;
        let offset_in_block = (offset - block_start) as usize;

        // 2. Fast path: check block cache
        if actual_len <= block_size - offset_in_block {
            if let Some(block) = self.cache.get_block(block_idx)? {
                if offset_in_block < block.len() {
                    let available = (block.len() - offset_in_block).min(actual_len);
                    out[..available]
                        .copy_from_slice(&block[offset_in_block..offset_in_block + available]);
                    return Ok(available);
                }
            }
        }

        // 3. Multi-block cross-boundary read:
        // A request spanning blocks needs one exact range fetch. Returning the
        // tail of the first block would silently truncate a TIFF chunk payload.
        if actual_len > block_size - offset_in_block {
            let fetched = self.transport.fetch_range(
                offset,
                offset + actual_len as u64 - 1,
                Some(self.total_size),
            )?;
            out[..actual_len].copy_from_slice(&fetched);
            return Ok(actual_len);
        }

        // 4. Slow path: fetch block without holding cache write lock to allow parallel fetches
        let block_end = (block_start + block_size as u64).min(self.total_size);
        let fetched_bytes =
            self.transport
                .fetch_range(block_start, block_end - 1, Some(self.total_size))?;
        let block_arc = Arc::new(fetched_bytes);

        let final_block = self.cache.insert_block(block_idx, block_arc)?;

        let available = (final_block.len().saturating_sub(offset_in_block)).min(actual_len);
        out[..available]
            .copy_from_slice(&final_block[offset_in_block..offset_in_block + available]);
        Ok(available)
    }

    /// Read bytes starting at `offset` up to `len` bytes, using the block cache for small reads.
    /// Truncates the returned buffer if reading across EOF.
    pub fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = self.read_range_into(offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Read exactly `len` bytes starting at `offset`.
    ///
    /// # Strict Read Contract
    /// Unlike [`read_range`], this method guarantees returning exactly `len` bytes.
    /// If `offset + len > total_size` or if the underlying transport fails to read
    /// the full requested range, it returns an error rather than a partial buffer.
    pub fn read_exact_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset > self.total_size {
            return Err(RasterH3Error::InvalidParameter(format!(
                "read_exact_range: offset {} extends beyond file of {} bytes",
                offset, self.total_size
            )));
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        if len as u64 > self.total_size - offset {
            return Err(RasterH3Error::InvalidParameter(format!(
                "read_exact_range: requested {} bytes at offset {} extends beyond file of {} bytes",
                len, offset, self.total_size
            )));
        }
        let buf = self.read_range(offset, len)?;
        if buf.len() != len {
            return Err(RasterH3Error::InvalidParameter(format!(
                "read_exact_range: expected {} bytes at offset {}, got {}",
                len,
                offset,
                buf.len()
            )));
        }
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
            .map_err(|e| std::io::Error::other(e.to_string()))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates_exact_partial_content() {
        assert_eq!(parse_content_range("bytes 10-19/100"), Some((10, 19, 100)));
        assert_eq!(parse_content_range("bytes 19-10/100"), None);
        assert_eq!(parse_content_range("items 10-19/100"), None);
        assert_eq!(
            validate_range_response(206, Some("bytes 10-19/100"), 10, 19, Some(100), 10).unwrap(),
            100
        );
    }

    #[test]
    fn rejects_ignored_or_malformed_range_responses() {
        for (status, header, body_len) in [
            (200, Some("bytes 10-19/100"), 10),
            (206, None, 10),
            (206, Some("bytes 0-9/100"), 10),
            (206, Some("bytes 10-19/99"), 10),
            (206, Some("bytes 10-19/100"), 9),
        ] {
            assert!(validate_range_response(status, header, 10, 19, Some(100), body_len).is_err());
        }
    }

    #[test]
    fn byte_cache_complete_file_direct_serving() {
        let data = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let cache = ByteCache::new_with_complete(data.clone(), 4);
        assert!(cache.is_complete());

        let mut out = [0u8; 4];
        let n = cache.copy_from_complete(2, &mut out).unwrap().unwrap();
        assert_eq!(n, 4);
        assert_eq!(&out, &[3, 4, 5, 6]);

        // Reading past end of file returns available bytes
        let mut out_end = [0u8; 4];
        let n_end = cache.copy_from_complete(8, &mut out_end).unwrap().unwrap();
        assert_eq!(n_end, 2);
        assert_eq!(&out_end[..2], &[9, 10]);

        // Offset at or beyond EOF returns 0
        assert_eq!(cache.copy_from_complete(10, &mut out).unwrap().unwrap(), 0);
    }

    #[test]
    fn byte_cache_block_sparse_serving() {
        let block0 = vec![10, 20, 30, 40];
        let cache = ByteCache::new_with_initial_block(block0.clone(), 4);
        assert!(!cache.is_complete());
        assert_eq!(cache.copy_from_complete(0, &mut [0u8; 4]).unwrap(), None);

        let b0 = cache.get_block(0).unwrap().unwrap();
        assert_eq!(&*b0, &[10, 20, 30, 40]);
        assert!(cache.get_block(1).unwrap().is_none());

        let block1 = Arc::new(vec![50, 60, 70, 80]);
        let inserted = cache.insert_block(1, block1.clone()).unwrap();
        assert_eq!(&*inserted, &[50, 60, 70, 80]);
        assert_eq!(&*cache.get_block(1).unwrap().unwrap(), &[50, 60, 70, 80]);
    }

    #[test]
    fn read_exact_range_bounds_check() {
        let data = vec![1, 2, 3, 4, 5];
        let cache = ByteCache::new_with_complete(data, 128);
        let transport = HttpTransport::new("http://example.com/test.tif").unwrap();
        let source = RemoteHttpSource {
            url: "http://example.com/test.tif".to_string(),
            total_size: 5,
            transport,
            cache,
        };

        assert_eq!(source.read_exact_range(0, 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(source.read_exact_range(2, 3).unwrap(), vec![3, 4, 5]);
        assert_eq!(source.read_exact_range(0, 5).unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(source.read_exact_range(0, 0).unwrap(), Vec::<u8>::new());

        // Past EOF
        assert!(source.read_exact_range(0, 6).is_err());
        assert!(source.read_exact_range(4, 2).is_err());
        assert!(source.read_exact_range(5, 1).is_err());
        assert!(source.read_exact_range(1, usize::MAX).is_err());
        assert!(source.read_exact_range(u64::MAX, 2).is_err());
        assert_eq!(source.read_exact_range(5, 0).unwrap(), Vec::<u8>::new());
    }
}
