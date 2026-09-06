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

/// Default block cache size for header and tag reading (64 KB)
pub const DEFAULT_BLOCK_SIZE: usize = 65536;

/// Check if a path or string is a remote URL (http, https, or s3)
pub fn is_remote_url(path: &str) -> bool {
    let p = path.trim().to_lowercase();
    p.starts_with("http://") || p.starts_with("https://") || p.starts_with("s3://")
}

/// Normalize input URL, resolving `s3://bucket/key` into an HTTPS URL
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
        if let Ok(endpoint) = std::env::var("AWS_S3_ENDPOINT") {
            let endpoint_trimmed = endpoint.trim_end_matches('/');
            Ok(format!("{}/{}/{}", endpoint_trimmed, bucket, key))
        } else {
            Ok(format!("https://{}.s3.amazonaws.com/{}", bucket, key))
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

/// Shared, thread-safe connection and block-cache state for a remote GeoTIFF
pub struct RemoteHttpSource {
    pub url: String,
    pub total_size: u64,
    agent: Agent,
    cache: RwLock<FxHashMap<u64, Arc<Vec<u8>>>>,
    block_size: usize,
}

impl RemoteHttpSource {
    /// Open a remote GeoTIFF, probing byte-range support and fetching the initial 64 KB header block
    pub fn open(raw_url: &str) -> Result<Self> {
        let url = normalize_url(raw_url)?;

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
            .build();

        let initial_fetch_len = DEFAULT_BLOCK_SIZE;
        let range_header = format!("bytes=0-{}", initial_fetch_len - 1);

        let resp = match agent.get(&url).set("Range", &range_header).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(404, _)) => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Remote raster not found (HTTP 404): {}",
                    url
                )));
            }
            Err(ureq::Error::Status(status, r)) => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Failed to fetch remote raster '{}': HTTP {} - {}",
                    url,
                    status,
                    r.status_text()
                )));
            }
            Err(e) => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "Network error connecting to remote raster '{}': {}",
                    url, e
                )));
            }
        };

        let status = resp.status();
        let mut total_size = 0u64;

        let initial_bytes = if status == 206 {
            // Parse Content-Range: bytes 0-65535/total_size
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
            cache: RwLock::new(cache),
            block_size: initial_fetch_len,
        })
    }

    /// Fetch an arbitrary byte range directly from the remote server
    pub fn fetch_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let range_header = format!("bytes={}-{}", start, end);
        let resp = match self
            .agent
            .get(&self.url)
            .set("Range", &range_header)
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::Status(status, r)) => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "HTTP Range request {} failed for '{}': HTTP {} - {}",
                    range_header,
                    self.url,
                    status,
                    r.status_text()
                )));
            }
            Err(e) => {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "HTTP Range request {} network error for '{}': {}",
                    range_header, self.url, e
                )));
            }
        };

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

    /// Read bytes starting at `offset` up to `len` bytes, using the block cache for small reads
    pub fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset >= self.total_size || len == 0 {
            return Ok(Vec::new());
        }

        let actual_len = len.min((self.total_size - offset) as usize);

        // For large reads (larger than 1 block), bypass the block cache and fetch exact range
        if actual_len > self.block_size {
            return self.fetch_range(offset, offset + actual_len as u64 - 1);
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
                    if available == actual_len {
                        return Ok(block[offset_in_block..offset_in_block + actual_len].to_vec());
                    }
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
        Ok(final_block[offset_in_block..offset_in_block + available].to_vec())
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
        let bytes = self
            .source
            .read_range(self.cursor, to_read)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        buf[..bytes.len()].copy_from_slice(&bytes);
        self.cursor += bytes.len() as u64;
        Ok(bytes.len())
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
