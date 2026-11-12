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
//! - **Object consistency**: The built-in HTTP transport pins the first successful
//!   response's strong ETag per URL and sends `If-Match` on subsequent requests. Changed or
//!   missing response ETags and HTTP 412 fail before bytes reach caches or decoders.
//!   Servers without a strong ETag retain size/range validation only; use immutable
//!   versioned URLs when reading such sources. Host transports own their consistency policy.
//! - **Exact range reads**: [`RemoteHttpSource::read_exact_range`] enforces that the entire requested
//!   byte count is satisfied. If the requested range extends past `total_size` or if fewer bytes are
//!   returned, an explicit error is returned.

use rustc_hash::FxHashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use ureq::Agent;
use url::Url;

use crate::error::{RasterH3Error, Result};

/// Default block cache size for header and tag reading (128 KB)
pub const DEFAULT_BLOCK_SIZE: usize = 131072;

/// Most blocks a remote source keeps cached: 512 blocks of [`DEFAULT_BLOCK_SIZE`] are
/// 64 MiB. Past it the least recently used block is dropped.
pub const DEFAULT_MAX_CACHED_BLOCKS: usize = 512;

/// Largest body (1 GiB) accepted when a server ignores Range and returns the
/// whole file, since that body is held in memory; override with env
/// `RASTER_H3_MAX_FULL_BODY_BYTES`.
pub const DEFAULT_MAX_FULL_BODY_BYTES: u64 = 1 << 30;

fn parse_max_full_body_bytes(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_MAX_FULL_BODY_BYTES)
}

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

/// Check if a path or string is a remote URI: `http(s)://`, `s3://`, or any
/// other `scheme://` cloud URI (gs, gcs, azure, abfs, r2, oss, hf, ftp, ...).
/// Non-http(s) schemes need a remote source factory that understands them
/// (e.g. the DuckDB file system); the built-in HTTP transport only supports
/// http/https and s3 (which it normalizes to https).
pub fn is_remote_url(path: &str) -> bool {
    let p = path.trim().to_lowercase();
    let Some(pos) = p.find("://") else {
        return false;
    };
    let scheme = &p[..pos];
    // `file://` URIs name local paths and take the local (mmap) path.
    if scheme == "file" {
        return false;
    }
    // A URI scheme is [A-Za-z][A-Za-z0-9+._-]*; require length >= 2 so
    // single-letter Windows drive paths like "c://x" are not remote.
    scheme.len() >= 2
        && scheme
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'_' | b'-'))
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

/// Comma-separated hosts that may receive `RASTER_H3_AUTH_TOKEN`. An entry is
/// an exact host (`data.example.com`) or, with a leading `.` or `*.`, a
/// domain and all of its subdomains (`.example.com`).
pub const AUTH_HOSTS_ENV: &str = "RASTER_H3_AUTH_HOSTS";

/// Headers sent with every request to one URL, and what was held back.
#[derive(Debug, Default, PartialEq, Eq)]
struct RequestHeaders {
    headers: Vec<(String, String)>,
    /// Why `RASTER_H3_AUTH_TOKEN` is set but not sent to this URL.
    withheld_token: Option<&'static str>,
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether `host` is `domain` or one of its subdomains.
fn host_in_domain(host: &str, domain: &str) -> bool {
    let (host, domain) = (host.to_ascii_lowercase(), domain.to_ascii_lowercase());
    !domain.is_empty()
        && (host == domain
            || host
                .strip_suffix(&domain)
                .is_some_and(|rest| rest.ends_with('.')))
}

/// Whether `host` matches an entry of an [`AUTH_HOSTS_ENV`] list.
fn auth_host_allowed(host: &str, allowed: &str) -> bool {
    allowed.split(',').map(str::trim).any(|entry| {
        match entry.strip_prefix("*.").or_else(|| entry.strip_prefix('.')) {
            Some(domain) => host_in_domain(host, domain),
            None => !entry.is_empty() && host.eq_ignore_ascii_case(entry),
        }
    })
}

/// Whether `host` is an S3 endpoint: AWS itself, or the endpoint configured
/// through `AWS_ENDPOINT_URL` / `AWS_S3_ENDPOINT` (including its
/// virtual-hosted bucket subdomains).
fn is_s3_host(host: &str, custom_endpoint: Option<&str>) -> bool {
    if ["amazonaws.com", "amazonaws.com.cn"]
        .iter()
        .any(|domain| host_in_domain(host, domain))
    {
        return true;
    }
    custom_endpoint
        .map(str::trim)
        .and_then(|endpoint| {
            Url::parse(endpoint)
                .or_else(|_| Url::parse(&format!("https://{endpoint}")))
                .ok()
        })
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|endpoint_host| host_in_domain(host, &endpoint_host))
}

/// Default headers for requests to `url` (requester pays, auth tokens), with
/// `env` standing in for the process environment.
///
/// Credentials are bound to where they may go, because the URL is often
/// caller-supplied (a SQL argument) while the environment belongs to the
/// process:
///
/// - Nothing secret is sent over plain `http://`, except to a loopback host.
/// - `RASTER_H3_AUTH_TOKEN` is sent as a bearer token only to hosts listed in
///   [`AUTH_HOSTS_ENV`].
/// - `AWS_SESSION_TOKEN` is sent only to S3 endpoints.
fn default_headers_for(url: &str, env: &dyn Fn(&str) -> Option<String>) -> RequestHeaders {
    let mut out = RequestHeaders::default();
    let var = |name: &str| {
        env(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };

    // Requester pays (AWS Open Data Program). Not a credential.
    if var("AWS_REQUEST_PAYER").is_some_and(|payer| payer.eq_ignore_ascii_case("requester")) {
        out.headers
            .push(("x-amz-request-payer".to_string(), "requester".to_string()));
    }

    let parsed = Url::parse(url.trim()).ok();
    let host = parsed.as_ref().and_then(Url::host_str).unwrap_or("");
    let confidential = !host.is_empty()
        && (parsed.as_ref().is_some_and(|u| u.scheme() == "https") || is_loopback_host(host));

    // Bearer token or AWS session token
    if env("RASTER_H3_AUTH_TOKEN").is_some() {
        if let Some(token) = var("RASTER_H3_AUTH_TOKEN") {
            if !confidential {
                out.withheld_token = Some("the URL is not https");
            } else if !var(AUTH_HOSTS_ENV).is_some_and(|allowed| auth_host_allowed(host, &allowed))
            {
                out.withheld_token = Some("its host is not listed in RASTER_H3_AUTH_HOSTS");
            } else {
                out.headers
                    .push(("Authorization".to_string(), format!("Bearer {token}")));
            }
        }
    } else if let Some(session_token) = var("AWS_SESSION_TOKEN") {
        let endpoint = var("AWS_ENDPOINT_URL").or_else(|| var("AWS_S3_ENDPOINT"));
        if confidential && is_s3_host(host, endpoint.as_deref()) {
            out.headers
                .push(("x-amz-security-token".to_string(), session_token));
        }
    }

    out
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

/// Byte-range backend for a RemoteHttpSource: the built-in HTTP range
/// client or a host-provided file-system adapter (e.g. DuckDB's
/// duckdb_file_system). Implementations must be safe to call concurrently
/// from prefetch and decode worker threads.
pub trait RemoteTransport: Send + Sync {
    /// Fetch the inclusive byte range [start, end] and return exactly the
    /// requested bytes. Returning fewer bytes is treated as an error by the
    /// caller.
    fn fetch_range(&self, start: u64, end: u64, expected_total: Option<u64>) -> Result<Vec<u8>>;

    /// Discover the source's total size and prefetch its first bytes.
    /// Transports that already know the file size after opening should
    /// override this and may report Complete for small files.
    fn probe(&self, initial_fetch_len: usize) -> Result<ProbeResult>;
}

type PinnedEtag = Arc<OnceLock<Option<String>>>;

/// Outcome of a single response attempt: `Done` carries the validated result;
/// `Retry` carries the error to return if retries are exhausted.
enum BodyAttempt<T> {
    Done(T),
    Retry(RasterH3Error),
}

/// Body expectation for `HttpTransport::read_body`: a 206 partial range
/// (`Range`, `read_cap` is the `.take` limit, always `expected + 1`) or a 200
/// whole-file response (`Full`, `content_length` 0 = none announced).
enum ExpectedBody {
    Range { start: u64, end: u64, read_cap: u64 },
    Full { content_length: u64 },
}

const MAX_RETRY_AFTER_MS: u64 = 30_000;

/// Jittered exponential backoff: uniform in [ceiling/2, ceiling] so concurrent
/// workers do not retry in lockstep. A numeric `Retry-After` (seconds) can
/// only lengthen the wait, capped at 30 s; the HTTP-date form is ignored.
fn retry_delay(attempt: usize, entropy: u64, retry_after: Option<&str>) -> std::time::Duration {
    let ceiling = (100u64 << attempt.min(16)).min(2000);
    let jittered = ceiling / 2 + entropy % (ceiling / 2 + 1);
    let delay = match retry_after.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(secs) => jittered.max(secs.saturating_mul(1000).min(MAX_RETRY_AFTER_MS)),
        None => jittered,
    };
    std::time::Duration::from_millis(delay)
}

fn retry_entropy() -> u64 {
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// Remote HTTP transport handling connections, retries, headers, and range requests.
#[derive(Clone)]
pub struct HttpTransport {
    /// Request URL. Clones share a pinned validator only for the same URL.
    pub url: String,
    agent: Agent,
    max_retries: usize,
    max_full_body_bytes: u64,
    // Clones share validators per URL, including URLs with no usable strong ETag.
    etags: Arc<RwLock<FxHashMap<String, PinnedEtag>>>,
}

#[allow(clippy::result_large_err)]
impl RemoteTransport for HttpTransport {
    fn fetch_range(&self, start: u64, end: u64, expected_total: Option<u64>) -> Result<Vec<u8>> {
        HttpTransport::fetch_range(self, start, end, expected_total)
    }

    fn probe(&self, initial_fetch_len: usize) -> Result<ProbeResult> {
        HttpTransport::probe(self, initial_fetch_len)
    }
}

impl HttpTransport {
    /// Create a new transport client with configured connection pooling and timeouts
    pub fn new(url: &str) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(60))
            .max_idle_connections(128)
            .max_idle_connections_per_host(32)
            .build();

        Ok(Self {
            url: url.to_string(),
            agent,
            max_retries: 4,
            max_full_body_bytes: parse_max_full_body_bytes(
                std::env::var("RASTER_H3_MAX_FULL_BODY_BYTES")
                    .ok()
                    .as_deref(),
            ),
            etags: Arc::new(RwLock::new(FxHashMap::default())),
        })
    }

    /// Cap the body size accepted when a server ignores Range and returns the
    /// whole file; see [`DEFAULT_MAX_FULL_BODY_BYTES`].
    pub fn with_max_full_body_bytes(mut self, max: u64) -> Self {
        self.max_full_body_bytes = max;
        self
    }

    fn full_body_limit_error(&self) -> RasterH3Error {
        RasterH3Error::InvalidParameter(format!(
            "Remote server at '{}' ignored the Range request and the file exceeds the {}-byte full-download limit; raise RASTER_H3_MAX_FULL_BODY_BYTES to allow larger full downloads",
            self.url, self.max_full_body_bytes
        ))
    }

    /// Retry a response attempt whose body read failed transiently: an io
    /// error mid-body or a body shorter than announced. `once` returns
    /// `Err` for fatal failures and `Ok(Retry(e))` for retryable ones.
    fn retry_body<T>(&self, mut once: impl FnMut() -> Result<BodyAttempt<T>>) -> Result<T> {
        let mut attempt = 0;
        loop {
            match once()? {
                BodyAttempt::Done(v) => return Ok(v),
                BodyAttempt::Retry(e) => {
                    if attempt >= self.max_retries {
                        return Err(e);
                    }
                    std::thread::sleep(retry_delay(attempt, retry_entropy(), None));
                    attempt += 1;
                }
            }
        }
    }

    fn pinned_etag(&self) -> Result<PinnedEtag> {
        let mut etags = self.etags.write().map_err(|_| {
            RasterH3Error::InvalidParameter("Remote HTTP validator lock poisoned".into())
        })?;
        Ok(Arc::clone(
            etags
                .entry(self.url.clone())
                .or_insert_with(|| Arc::new(OnceLock::new())),
        ))
    }

    /// Default headers for the current `url`. Resolved per request, not at
    /// construction: `url` is public, and a credential scoped to one host must
    /// not follow the transport to another.
    fn request_headers(&self) -> RequestHeaders {
        default_headers_for(&self.url, &|name| std::env::var(name).ok())
    }

    fn range_request(&self, range: &str, etag: &OnceLock<Option<String>>) -> ureq::Request {
        let mut req = self
            .agent
            .get(&self.url)
            .set("Range", range)
            .set("Accept-Encoding", "identity");
        for (k, v) in &self.request_headers().headers {
            req = req.set(k, v);
        }
        if let Some(Some(etag)) = etag.get() {
            req = req.set("If-Match", etag);
        }
        req
    }

    /// Pin only a strong ETag: weak tags do not guarantee byte-for-byte identity.
    /// Even if a server ignores If-Match, never return bytes with a different validator.
    /// Initialization is atomic so concurrent first reads cannot pin different versions.
    fn validate_etag(etag: &OnceLock<Option<String>>, response_etag: Option<&str>) -> Result<()> {
        let pinned = etag.get_or_init(|| {
            response_etag
                .filter(|tag| {
                    let bytes = tag.as_bytes();
                    bytes.len() >= 2
                        && bytes[0] == b'"'
                        && bytes[bytes.len() - 1] == b'"'
                        && bytes[1..bytes.len() - 1]
                            .iter()
                            .all(|&b| b == 0x21 || (0x23..=0x7e).contains(&b) || b >= 0x80)
                })
                .map(str::to_owned)
        });
        if let Some(expected) = pinned {
            if response_etag != Some(expected.as_str()) {
                return Err(RasterH3Error::InvalidParameter(
                    "Remote raster changed during scan: ETag changed or disappeared".into(),
                ));
            }
        }
        Ok(())
    }

    /// Read and validate a response body: `Retry` when the read fails or the
    /// body is shorter than announced, `Err` when longer; the ETag check runs
    /// only on a complete body. `what` completes each site's read-error
    /// message: "Failed reading {what} from '{url}': {e}".
    #[allow(clippy::result_large_err)]
    fn read_body(
        &self,
        resp: ureq::Response,
        expected: ExpectedBody,
        etag: &OnceLock<Option<String>>,
        what: &str,
    ) -> Result<BodyAttempt<Vec<u8>>> {
        let response_etag = resp.header("ETag").map(str::to_owned);
        let (take_limit, capacity) = match &expected {
            ExpectedBody::Range {
                start,
                end,
                read_cap,
            } => (*read_cap, (end - start + 1) as usize),
            ExpectedBody::Full { content_length } => {
                if *content_length > self.max_full_body_bytes {
                    return Err(self.full_body_limit_error());
                }
                (self.max_full_body_bytes.saturating_add(1), 0)
            }
        };
        let reader = resp.into_reader();
        let mut buf = Vec::with_capacity(capacity);
        if let Err(e) = reader.take(take_limit).read_to_end(&mut buf) {
            return Ok(BodyAttempt::Retry(RasterH3Error::InvalidParameter(
                format!("Failed reading {} from '{}': {}", what, self.url, e),
            )));
        }
        match expected {
            ExpectedBody::Range { start, end, .. } => {
                let expected_len = (end - start + 1) as usize;
                if buf.len() != expected_len {
                    let err = RasterH3Error::InvalidParameter(format!(
                        "Remote range response for bytes {}-{} has {} bytes, expected {}",
                        start,
                        end,
                        buf.len(),
                        end - start + 1
                    ));
                    if buf.len() < expected_len {
                        return Ok(BodyAttempt::Retry(err));
                    }
                    return Err(err);
                }
            }
            ExpectedBody::Full { content_length } => {
                if buf.len() as u64 > self.max_full_body_bytes {
                    return Err(self.full_body_limit_error());
                }
                if content_length > 0 && content_length != buf.len() as u64 {
                    let err = RasterH3Error::InvalidParameter(format!(
                        "Remote response has {} bytes but Content-Length says {}",
                        buf.len(),
                        content_length
                    ));
                    if (buf.len() as u64) < content_length {
                        return Ok(BodyAttempt::Retry(err));
                    }
                    return Err(err);
                }
            }
        }
        Self::validate_etag(etag, response_etag.as_deref())?;
        Ok(BodyAttempt::Done(buf))
    }

    /// Execute a request with automatic exponential backoff retry on transient errors
    pub fn execute_request<F>(&self, mut make_request: F) -> Result<ureq::Response>
    where
        F: FnMut() -> std::result::Result<ureq::Response, ureq::Error>,
    {
        let mut attempt = 0;

        loop {
            match make_request() {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let retry_after = match &e {
                        ureq::Error::Status(_, resp) => {
                            resp.header("Retry-After").map(str::to_owned)
                        }
                        _ => None,
                    };
                    if attempt >= self.max_retries || !is_transient_error(&e) {
                        return match e {
                            ureq::Error::Status(412, _) => Err(RasterH3Error::InvalidParameter(
                                "Remote raster changed during scan (HTTP 412 Precondition Failed)"
                                    .into(),
                            )),
                            ureq::Error::Status(404, _) => Err(RasterH3Error::InvalidParameter(
                                format!("Remote raster not found (HTTP 404): {}", self.url),
                            )),
                            ureq::Error::Status(status, r) => {
                                // A refusal is the only sign the caller gets
                                // that a configured token stayed home.
                                let hint = match self.request_headers().withheld_token {
                                    Some(why) if matches!(status, 401 | 403) => {
                                        format!(" (RASTER_H3_AUTH_TOKEN was not sent: {why})")
                                    }
                                    _ => String::new(),
                                };
                                Err(RasterH3Error::InvalidParameter(format!(
                                    "HTTP request failed for '{}': HTTP {} - {}{}",
                                    self.url,
                                    status,
                                    r.status_text(),
                                    hint
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

                    // Jittered exponential backoff, honouring Retry-After
                    std::thread::sleep(retry_delay(
                        attempt,
                        retry_entropy(),
                        retry_after.as_deref(),
                    ));
                    attempt += 1;
                }
            }
        }
    }

    /// Probe remote server to determine byte-range capability and initial header data.
    #[allow(clippy::result_large_err)]
    pub fn probe(&self, initial_fetch_len: usize) -> Result<ProbeResult> {
        let etag = self.pinned_etag()?;
        self.retry_body(|| self.probe_once(initial_fetch_len, &etag))
    }

    #[allow(clippy::result_large_err)]
    fn probe_once(
        &self,
        initial_fetch_len: usize,
        etag: &OnceLock<Option<String>>,
    ) -> Result<BodyAttempt<ProbeResult>> {
        let range_header = format!("bytes=0-{}", initial_fetch_len.saturating_sub(1));
        let resp = self.execute_request(|| self.range_request(&range_header, etag).call())?;

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
            let buf = match self.read_body(
                resp,
                ExpectedBody::Range {
                    start: 0,
                    end: expected_end,
                    read_cap: initial_fetch_len as u64 + 1,
                },
                etag,
                "initial header bytes",
            )? {
                BodyAttempt::Done(buf) => buf,
                BodyAttempt::Retry(e) => return Ok(BodyAttempt::Retry(e)),
            };
            Ok(BodyAttempt::Done(ProbeResult::Partial {
                data: buf,
                total_size: parsed_total,
            }))
        } else if status == 200 {
            // Server returned entire body (or file is small / server ignores ranges)
            let cl = resp
                .header("Content-Length")
                .and_then(|h| h.parse::<u64>().ok())
                .unwrap_or(0);
            let buf = match self.read_body(
                resp,
                ExpectedBody::Full { content_length: cl },
                etag,
                "response",
            )? {
                BodyAttempt::Done(buf) => buf,
                BodyAttempt::Retry(e) => return Ok(BodyAttempt::Retry(e)),
            };
            let total_size = buf.len() as u64;
            Ok(BodyAttempt::Done(ProbeResult::Complete {
                data: buf,
                total_size,
            }))
        } else {
            Err(RasterH3Error::InvalidParameter(format!(
                "Unexpected HTTP status {} from '{}'",
                status, self.url
            )))
        }
    }

    /// Fetch an arbitrary byte range directly from the remote server with retry and verification
    #[allow(clippy::result_large_err)]
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
        let etag = self.pinned_etag()?;
        self.retry_body(|| self.fetch_range_once(start, end, expected_total, &etag))
    }

    #[allow(clippy::result_large_err)]
    fn fetch_range_once(
        &self,
        start: u64,
        end: u64,
        expected_total: Option<u64>,
        etag: &OnceLock<Option<String>>,
    ) -> Result<BodyAttempt<Vec<u8>>> {
        let range_header = format!("bytes={}-{}", start, end);
        let resp = self.execute_request(|| self.range_request(&range_header, etag).call())?;

        let status = resp.status();
        let content_range = resp.header("Content-Range").map(str::to_owned);

        validate_range_response_headers(
            status,
            content_range.as_deref(),
            start,
            end,
            expected_total,
        )?;

        self.read_body(
            resp,
            ExpectedBody::Range {
                start,
                end,
                read_cap: end - start + 2,
            },
            etag,
            &format!("range {}", range_header),
        )
    }
}

/// Thread-safe in-memory cache for remote bytes.
///
/// Explicitly distinguishes between:
/// - A completely downloaded response (e.g. from an HTTP 200 probe or small file),
///   where all byte slices can be served directly from memory without network access.
/// - A block-sparse cache, where chunks are fetched in aligned `block_size` pages. It holds
///   at most `max_blocks` blocks ([`DEFAULT_MAX_CACHED_BLOCKS`] unless changed with
///   [`Self::with_max_blocks`]) and drops the least recently used one past that, so reading
///   a large file through it does not keep the whole file in memory.
pub struct ByteCache {
    block_size: usize,
    max_blocks: usize,
    complete_file: RwLock<Option<Arc<Vec<u8>>>>,
    blocks: RwLock<FxHashMap<u64, CachedBlock>>,
    /// Use counter; a block's `last_used` is the counter value of its latest lookup.
    clock: AtomicU64,
}

struct CachedBlock {
    data: Arc<Vec<u8>>,
    last_used: AtomicU64,
}

impl ByteCache {
    /// Create a cache representing a completely downloaded file.
    pub fn new_with_complete(data: Vec<u8>, block_size: usize) -> Self {
        Self {
            block_size,
            max_blocks: DEFAULT_MAX_CACHED_BLOCKS,
            complete_file: RwLock::new(Some(Arc::new(data))),
            blocks: RwLock::new(FxHashMap::default()),
            clock: AtomicU64::new(0),
        }
    }

    /// Create a cache initialized with a single block (e.g. block 0 from a partial range probe).
    pub fn new_with_initial_block(data: Vec<u8>, block_size: usize) -> Self {
        let mut blocks = FxHashMap::default();
        blocks.insert(
            0,
            CachedBlock {
                data: Arc::new(data),
                last_used: AtomicU64::new(0),
            },
        );
        Self {
            block_size,
            max_blocks: DEFAULT_MAX_CACHED_BLOCKS,
            complete_file: RwLock::new(None),
            blocks: RwLock::new(blocks),
            clock: AtomicU64::new(0),
        }
    }

    /// Keep at most `max_blocks` blocks (at least one), dropping the least recently used.
    pub fn with_max_blocks(mut self, max_blocks: usize) -> Self {
        self.max_blocks = max_blocks.max(1);
        self
    }

    /// Number of blocks currently cached.
    pub fn cached_blocks(&self) -> usize {
        self.blocks.read().map_or(0, |guard| guard.len())
    }

    #[inline]
    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed) + 1
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

    /// Get a cached block by block index if present, marking it as recently used.
    pub fn get_block(&self, block_idx: u64) -> Result<Option<Arc<Vec<u8>>>> {
        let guard = self.blocks.read().map_err(|_| {
            RasterH3Error::InvalidParameter("Remote HTTP cache lock poisoned".to_string())
        })?;
        Ok(guard.get(&block_idx).map(|block| {
            block.last_used.store(self.tick(), Ordering::Relaxed);
            Arc::clone(&block.data)
        }))
    }

    /// Insert a block into the cache under write lock, returning the retained reference
    /// (retaining any existing block if a race occurred). Past `max_blocks`, the least
    /// recently used other blocks are dropped; readers holding them keep their `Arc`.
    pub fn insert_block(&self, block_idx: u64, data: Arc<Vec<u8>>) -> Result<Arc<Vec<u8>>> {
        let mut guard = self.blocks.write().map_err(|_| {
            RasterH3Error::InvalidParameter("Remote HTTP cache lock poisoned".to_string())
        })?;
        let now = self.tick();
        let kept = match guard.entry(block_idx) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                entry.get().last_used.store(now, Ordering::Relaxed);
                Arc::clone(&entry.get().data)
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let block = entry.insert(CachedBlock {
                    data,
                    last_used: AtomicU64::new(now),
                });
                Arc::clone(&block.data)
            }
        };
        while guard.len() > self.max_blocks {
            // A linear scan: at most `max_blocks + 1` entries, once per network fetch.
            let oldest = guard
                .iter()
                .filter(|(&idx, _)| idx != block_idx)
                .min_by_key(|(_, block)| block.last_used.load(Ordering::Relaxed))
                .map(|(&idx, _)| idx);
            match oldest {
                Some(idx) => guard.remove(&idx),
                None => break,
            };
        }
        Ok(kept)
    }
}

/// Shared, thread-safe connection and block-cache state for a remote GeoTIFF
pub struct RemoteHttpSource {
    pub url: String,
    pub total_size: u64,
    transport: Box<dyn RemoteTransport>,
    cache: ByteCache,
}

/// Factory opening a [`RemoteHttpSource`] for a path. Engine adapters (e.g.
/// the DuckDB file-system layer) supply one to route raster reads through
/// their own credentials and remote file handling; the built-in HTTP/S3
/// client is used when no factory is supplied.
type RemoteSourceOpener = dyn Fn(&str) -> Result<Arc<RemoteHttpSource>> + Send + Sync;

#[derive(Clone)]
pub struct RemoteSourceFactory(Arc<RemoteSourceOpener>);

impl RemoteSourceFactory {
    /// Wrap a source-opening closure.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&str) -> Result<Arc<RemoteHttpSource>> + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }

    /// Open a remote byte source for path.
    pub fn open(&self, path: &str) -> Result<Arc<RemoteHttpSource>> {
        (self.0)(path)
    }
}

impl std::fmt::Debug for RemoteSourceFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteSourceFactory")
    }
}

impl RemoteHttpSource {
    /// Open a remote GeoTIFF, probing byte-range support and fetching the initial header block
    pub fn open(raw_url: &str) -> Result<Self> {
        let url = normalize_url(raw_url)?;
        let transport = HttpTransport::new(&url)?;
        Self::from_transport(url, Box::new(transport))
    }

    /// Open a source over a caller-provided transport, probing its size and
    /// fetching the initial header block.
    pub fn from_transport(url: String, transport: Box<dyn RemoteTransport>) -> Result<Self> {
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
    #[allow(clippy::result_large_err)]
    pub fn fetch_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        // Servers that ignore Range headers hand us the complete file at probe
        // time; serve ranged requests from that cache — the server could not
        // answer them over HTTP anyway.
        if self.cache.is_complete() && start <= end && end < self.total_size {
            let mut buf = vec![0u8; (end - start + 1) as usize];
            if let Some(n) = self.cache.copy_from_complete(start, &mut buf)? {
                // A custom transport may report a total_size beyond the body it
                // delivered; never hand back a silently short range.
                if n != buf.len() {
                    return Err(RasterH3Error::InvalidParameter(format!(
                        "Cached remote file holds {n} of {} requested bytes at offset {start}",
                        buf.len()
                    )));
                }
                return Ok(buf);
            }
        }
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
    /// Unlike [`read_range`](Self::read_range), this method guarantees returning exactly `len` bytes.
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
    overlay: Option<Arc<ReadOverlay>>,
}

/// Bytes of one range of a remote file that are already in memory. An [`HttpRangeReader`]
/// built with [`HttpRangeReader::with_overlay`] serves reads inside the staged range from
/// it instead of fetching them again.
#[derive(Debug, Default)]
pub struct ReadOverlay {
    staged: Mutex<Option<(u64, Vec<u8>)>>,
}

impl ReadOverlay {
    /// Serve `bytes` for the file range starting at `offset` until [`Self::clear`].
    pub fn stage(&self, offset: u64, bytes: &[u8]) {
        if let Ok(mut staged) = self.staged.lock() {
            let mut buf = staged.take().map(|(_, buf)| buf).unwrap_or_default();
            buf.clear();
            buf.extend_from_slice(bytes);
            *staged = Some((offset, buf));
        }
    }

    /// Stop serving the staged bytes (the allocation is kept for the next stage).
    pub fn clear(&self) {
        if let Ok(mut staged) = self.staged.lock() {
            if let Some((offset, buf)) = staged.as_mut() {
                *offset = 0;
                buf.clear();
            }
        }
    }

    /// Copy staged bytes at `cursor` into `out`. `Ok(n)` with `n > 0` when `cursor` is inside
    /// the staged range; otherwise `Ok(0)`, with `out_len` cut to stop at the range's start.
    fn read_at(&self, cursor: u64, out: &mut [u8], out_len: &mut usize) -> std::io::Result<usize> {
        let staged = self
            .staged
            .lock()
            .map_err(|_| std::io::Error::other("remote read overlay lock poisoned"))?;
        let Some((start, bytes)) = staged.as_ref() else {
            return Ok(0);
        };
        let end = start + bytes.len() as u64;
        if cursor >= *start && cursor < end {
            let from = (cursor - start) as usize;
            let n = (*out_len).min(bytes.len() - from);
            out[..n].copy_from_slice(&bytes[from..from + n]);
            return Ok(n);
        }
        if cursor < *start && cursor + *out_len as u64 > *start {
            // Read up to the staged range; the next read starts inside it.
            *out_len = (*start - cursor) as usize;
        }
        Ok(0)
    }
}

impl HttpRangeReader {
    /// Create a new reader with its cursor positioned at offset 0
    pub fn new(source: Arc<RemoteHttpSource>) -> Self {
        Self {
            source,
            cursor: 0,
            overlay: None,
        }
    }

    /// A reader that serves reads inside `overlay`'s staged range from memory.
    pub fn with_overlay(source: Arc<RemoteHttpSource>, overlay: Arc<ReadOverlay>) -> Self {
        Self {
            source,
            cursor: 0,
            overlay: Some(overlay),
        }
    }
}

impl Read for HttpRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cursor >= self.source.total_size || buf.is_empty() {
            return Ok(0);
        }

        let mut to_read = buf
            .len()
            .min((self.source.total_size - self.cursor) as usize);
        if let Some(overlay) = &self.overlay {
            let n = overlay.read_at(self.cursor, buf, &mut to_read)?;
            if n > 0 {
                self.cursor += n as u64;
                return Ok(n);
            }
        }
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

    /// Headers for `url` under a fixed environment.
    fn headers_with(url: &str, env: &[(&str, &str)]) -> RequestHeaders {
        default_headers_for(url, &|name| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    fn header<'a>(headers: &'a RequestHeaders, name: &str) -> Option<&'a str> {
        headers
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn auth_token_requires_a_listed_host_and_a_confidential_channel() {
        let token = ("RASTER_H3_AUTH_TOKEN", " secret ");
        let hosts = (
            AUTH_HOSTS_ENV,
            "data.example.com, .example.org ,*.example.net,localhost",
        );
        for (url, expected) in [
            ("https://data.example.com/a.tif", true),
            ("https://DATA.EXAMPLE.COM:8443/a.tif", true),
            ("https://example.org/a.tif", true),
            ("https://tiles.cdn.example.org/a.tif", true),
            ("https://cdn.example.net/a.tif", true),
            ("http://localhost:9000/a.tif", true),
            // Listed, but plain http to a remote host.
            ("http://data.example.com/a.tif", false),
            // Not listed, including look-alike and parent domains.
            ("https://other.example.com/a.tif", false),
            ("https://example.com/a.tif", false),
            ("https://evilexample.org/a.tif", false),
            ("https://data.example.com.evil.test/a.tif", false),
            ("https://evil.test/data.example.com/a.tif", false),
            ("https://data.example.com@evil.test/a.tif", false),
            ("http://127.0.0.1:9000/a.tif", false),
            ("not a url", false),
        ] {
            let headers = headers_with(url, &[token, hosts]);
            assert_eq!(
                header(&headers, "Authorization"),
                expected.then_some("Bearer secret"),
                "{url}"
            );
            assert_eq!(headers.withheld_token.is_some(), !expected, "{url}");
        }

        // No list, or an empty one, sends the token nowhere.
        for env in [vec![token], vec![token, (AUTH_HOSTS_ENV, " , ")]] {
            let headers = headers_with("https://data.example.com/a.tif", &env);
            assert_eq!(header(&headers, "Authorization"), None);
            assert!(headers
                .withheld_token
                .is_some_and(|why| why.contains(AUTH_HOSTS_ENV)));
        }
        // Without a token nothing is withheld.
        assert_eq!(
            headers_with("https://data.example.com/a.tif", &[hosts]),
            RequestHeaders::default()
        );
    }

    #[test]
    fn session_token_goes_only_to_s3_endpoints() {
        let session = ("AWS_SESSION_TOKEN", "session");
        let sent = |url: &str, env: &[(&str, &str)]| {
            header(&headers_with(url, env), "x-amz-security-token").is_some()
        };
        for (url, expected) in [
            ("https://bucket.s3.amazonaws.com/a.tif", true),
            ("https://bucket.s3.eu-west-1.amazonaws.com/a.tif", true),
            ("https://bucket.s3.cn-north-1.amazonaws.com.cn/a.tif", true),
            ("http://bucket.s3.amazonaws.com/a.tif", false),
            ("https://amazonaws.com.evil.test/a.tif", false),
            ("https://evil.test/a.tif", false),
        ] {
            assert_eq!(sent(url, &[session]), expected, "{url}");
        }

        // A configured endpoint counts, with path or virtual-hosted buckets.
        let minio = [session, ("AWS_ENDPOINT_URL", "http://localhost:9000/")];
        assert!(sent("http://localhost:9000/bucket/a.tif", &minio));
        assert!(!sent("https://evil.test/bucket/a.tif", &minio));
        let hosted = [session, ("AWS_S3_ENDPOINT", "storage.example.com")];
        assert!(sent("https://storage.example.com/bucket/a.tif", &hosted));
        assert!(sent("https://bucket.storage.example.com/a.tif", &hosted));
        assert!(!sent("http://storage.example.com/bucket/a.tif", &hosted));

        // As before, a bearer token variable takes the session token's place.
        let both = [session, ("RASTER_H3_AUTH_TOKEN", "secret")];
        assert!(!sent("https://bucket.s3.amazonaws.com/a.tif", &both));
    }

    #[test]
    fn requester_pays_is_not_scoped() {
        let env = [("AWS_REQUEST_PAYER", " Requester ")];
        let headers = headers_with("http://example.com/a.tif", &env);
        assert_eq!(header(&headers, "x-amz-request-payer"), Some("requester"));
    }

    #[test]
    fn parses_max_full_body_bytes_override() {
        for value in [None, Some(""), Some("abc"), Some("0")] {
            assert_eq!(
                parse_max_full_body_bytes(value),
                DEFAULT_MAX_FULL_BODY_BYTES
            );
        }
        assert_eq!(parse_max_full_body_bytes(Some(" 4096 ")), 4096);
    }

    #[test]
    fn retry_delay_is_jittered_within_bounds() {
        for attempt in 0..8 {
            let ceiling = (100u64 << attempt).min(2000);
            for entropy in [0u64, 1, 12345, u64::MAX] {
                let ms = retry_delay(attempt, entropy, None).as_millis() as u64;
                assert!(
                    (ceiling / 2..=ceiling).contains(&ms),
                    "attempt {attempt} entropy {entropy}: {ms}ms outside [{}, {ceiling}]",
                    ceiling / 2
                );
            }
        }
        assert_ne!(
            retry_delay(3, 0, None),
            retry_delay(3, 7, None),
            "different entropy must jitter differently"
        );
    }

    #[test]
    fn retry_delay_honours_retry_after_seconds() {
        assert_eq!(
            retry_delay(0, 0, Some("3")),
            std::time::Duration::from_millis(3000)
        );
        assert_eq!(
            retry_delay(0, 0, Some(" 3 ")),
            std::time::Duration::from_millis(3000)
        );
        assert_eq!(
            retry_delay(0, 0, Some("9999")),
            std::time::Duration::from_millis(MAX_RETRY_AFTER_MS)
        );
        for invalid in ["abc", "Wed, 21 Oct 2015 07:28:00 GMT"] {
            assert_eq!(retry_delay(0, 0, Some(invalid)), retry_delay(0, 0, None));
        }
        // Retry-After never shortens the backoff
        assert_eq!(retry_delay(4, 0, Some("0")), retry_delay(4, 0, None));
    }

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

    /// Serves a fixed body and counts range requests (overlay tests).
    struct OverlayTestTransport {
        data: Vec<u8>,
        fetches: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RemoteTransport for OverlayTestTransport {
        fn fetch_range(&self, start: u64, end: u64, _: Option<u64>) -> Result<Vec<u8>> {
            self.fetches
                .fetch_add(1, Ordering::SeqCst);
            Ok(self.data[start as usize..=end as usize].to_vec())
        }

        fn probe(&self, _: usize) -> Result<ProbeResult> {
            unreachable!("the test builds the source directly")
        }
    }

    #[test]
    fn overlay_serves_the_staged_range_from_memory() {
        use std::sync::atomic::Ordering;
        let data: Vec<u8> = (0..=255).collect();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = Arc::new(RemoteHttpSource {
            url: "http://example.com/overlay.tif".to_string(),
            total_size: data.len() as u64,
            transport: Box::new(OverlayTestTransport {
                data: data.clone(),
                fetches: Arc::clone(&fetches),
            }),
            cache: ByteCache::new_with_initial_block(data[..16].to_vec(), 16),
        });
        let overlay = Arc::new(ReadOverlay::default());
        let mut reader = HttpRangeReader::with_overlay(Arc::clone(&source), Arc::clone(&overlay));
        // Staged bytes deliberately differ from the file, to show where reads come from.
        overlay.stage(100, &[9; 40]);

        // Inside the range: from memory, cut at its end, no fetch.
        let mut buf = [0u8; 64];
        reader.seek(SeekFrom::Start(120)).unwrap();
        assert_eq!(reader.read(&mut buf).unwrap(), 20);
        assert_eq!(buf[..20], [9; 20]);
        assert_eq!(fetches.load(Ordering::SeqCst), 0);

        // A read running into the range stops at its start; the next read is from memory.
        reader.seek(SeekFrom::Start(90)).unwrap();
        let mut buf = [0u8; 20];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 10);
        assert_eq!(buf[..10], data[90..100]);
        assert_eq!(reader.read(&mut buf).unwrap(), 20);
        assert_eq!(buf, [9; 20]);

        // Cleared: the same offsets come from the source again.
        overlay.clear();
        reader.seek(SeekFrom::Start(120)).unwrap();
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[120..128]);

        // A reader without an overlay ignores staged bytes.
        overlay.stage(0, &[7; 8]);
        let mut plain = HttpRangeReader::new(source);
        plain.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[..8]);
    }

    #[test]
    fn byte_cache_drops_the_least_recently_used_block() {
        let cache = ByteCache::new_with_initial_block(vec![0; 4], 4).with_max_blocks(3);
        cache.insert_block(1, Arc::new(vec![1; 4])).unwrap();
        cache.insert_block(2, Arc::new(vec![2; 4])).unwrap();
        // Touch block 0, so block 1 is now the least recently used.
        cache.get_block(0).unwrap().unwrap();
        let held = cache.get_block(1).unwrap().unwrap();
        cache.get_block(2).unwrap().unwrap();
        cache.get_block(0).unwrap().unwrap();

        cache.insert_block(3, Arc::new(vec![3; 4])).unwrap();
        assert_eq!(cache.cached_blocks(), 3);
        assert!(
            cache.get_block(1).unwrap().is_none(),
            "block 1 was least recently used"
        );
        for idx in [0, 2, 3] {
            assert!(
                cache.get_block(idx).unwrap().is_some(),
                "block {idx} must stay"
            );
        }
        // A reader still holding an evicted block keeps its bytes.
        assert_eq!(&*held, &[1; 4]);

        // Re-inserting a cached block neither duplicates nor evicts anything.
        cache.insert_block(3, Arc::new(vec![9; 4])).unwrap();
        assert_eq!(cache.cached_blocks(), 3);
        assert_eq!(&*cache.get_block(3).unwrap().unwrap(), &[3; 4]);

        // A limit of zero still keeps the block just inserted.
        let tiny = ByteCache::new_with_initial_block(vec![0; 4], 4).with_max_blocks(0);
        tiny.insert_block(1, Arc::new(vec![1; 4])).unwrap();
        assert_eq!(tiny.cached_blocks(), 1);
        assert!(tiny.get_block(1).unwrap().is_some());
    }

    /// Serves a fixed body and counts range requests.
    struct CountingTransport {
        data: Vec<u8>,
        fetches: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RemoteTransport for CountingTransport {
        fn fetch_range(&self, start: u64, end: u64, _: Option<u64>) -> Result<Vec<u8>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(self.data[start as usize..=end as usize].to_vec())
        }

        fn probe(&self, _: usize) -> Result<ProbeResult> {
            unreachable!("the test builds the source directly")
        }
    }

    #[test]
    fn reading_a_large_file_keeps_the_block_cache_bounded() {
        let block = 64;
        let data: Vec<u8> = (0..block * 100).map(|i| (i % 251) as u8).collect();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = RemoteHttpSource {
            url: "http://example.com/large.tif".to_string(),
            total_size: data.len() as u64,
            transport: Box::new(CountingTransport {
                data: data.clone(),
                fetches: Arc::clone(&fetches),
            }),
            cache: ByteCache::new_with_initial_block(data[..block].to_vec(), block)
                .with_max_blocks(8),
        };
        // One small read from each of the 100 blocks, as chunk reads would make.
        for b in 0..100u64 {
            let offset = b * block as u64 + 3;
            assert_eq!(
                source.read_exact_range(offset, 10).unwrap(),
                data[offset as usize..offset as usize + 10]
            );
            let cached = source.cache.cached_blocks();
            assert!(cached <= 8, "cache grew to {cached} blocks");
        }
        // Block 0 came from the probe; the other 99 were fetched once each.
        assert_eq!(fetches.load(Ordering::SeqCst), 99);
        // The most recent block is still served from memory; block 0 was evicted.
        source.read_exact_range(99 * block as u64, 10).unwrap();
        assert_eq!(fetches.load(Ordering::SeqCst), 99);
        source.read_exact_range(0, 10).unwrap();
        assert_eq!(fetches.load(Ordering::SeqCst), 100);
    }

    #[test]
    fn read_exact_range_bounds_check() {
        let data = vec![1, 2, 3, 4, 5];
        let cache = ByteCache::new_with_complete(data, 128);
        let transport = HttpTransport::new("http://example.com/test.tif").unwrap();
        let source = RemoteHttpSource {
            url: "http://example.com/test.tif".to_string(),
            total_size: 5,
            transport: Box::new(transport),
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

    #[test]
    fn fetch_range_serves_complete_file_from_cache() {
        let data: Vec<u8> = (0..100).collect();
        let cache = ByteCache::new_with_complete(data.clone(), 16);
        let transport = HttpTransport::new("http://example.com/test.tif").unwrap();
        let source = RemoteHttpSource {
            url: "http://example.com/test.tif".to_string(),
            total_size: data.len() as u64,
            transport: Box::new(transport),
            cache,
        };

        // Servers that ignore Range headers hand us the complete file at probe
        // time; fetch_range must serve from that cache rather than issuing an
        // HTTP request the server cannot honor anyway.
        assert_eq!(source.fetch_range(0, 9).unwrap(), data[..10]);
        assert_eq!(source.fetch_range(90, 99).unwrap(), data[90..]);
        assert_eq!(source.fetch_range(0, 99).unwrap(), data);

        // Out-of-bounds and inverted ranges are still rejected by transport
        // validation before any request.
        assert!(source.fetch_range(99, 100).is_err());
        assert!(source.fetch_range(50, 40).is_err());
    }

    #[test]
    fn fetch_range_rejects_short_complete_cache() {
        // total_size claims more bytes than the cached body holds.
        let cache = ByteCache::new_with_complete((0..50).collect(), 16);
        let source = RemoteHttpSource {
            url: "http://example.com/test.tif".to_string(),
            total_size: 100,
            transport: Box::new(HttpTransport::new("http://example.com/test.tif").unwrap()),
            cache,
        };
        assert_eq!(
            source.fetch_range(0, 9).unwrap(),
            (0..10).collect::<Vec<u8>>()
        );
        assert!(source.fetch_range(40, 59).is_err());
    }
}
