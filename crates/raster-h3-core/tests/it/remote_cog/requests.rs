//! What goes on the wire: S3 endpoints, transient retries, request headers and auth-token scoping.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::Ordering;

use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::http_range::{normalize_url, HttpTransport, RemoteHttpSource};

use super::{MockHttpServer, ENV_MUTEX};
use crate::helpers;

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
    std::env::set_var("RASTER_H3_AUTH_HOSTS", "127.0.0.1");

    let _reader = GeoTiffStreamReader::open(&remote_url).expect("Reader open with custom headers");

    std::env::remove_var("AWS_REQUEST_PAYER");
    std::env::remove_var("RASTER_H3_AUTH_TOKEN");
    std::env::remove_var("RASTER_H3_AUTH_HOSTS");

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

/// Lower-cased request header lines the mock server has seen.
fn recorded_lowercase(server: &MockHttpServer) -> Vec<String> {
    server
        .recorded_headers()
        .iter()
        .map(|h| h.to_lowercase())
        .collect()
}

// The bearer token belongs to the process, the URL often to a caller (a SQL
// argument). It goes only to hosts the process owner listed, and a transport
// pointed at another host does not take it along.
#[test]
fn auth_token_is_sent_only_to_listed_hosts() {
    let _env_lock = ENV_MUTEX.lock().unwrap();
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let sent = |headers: &[String]| {
        headers
            .iter()
            .any(|h| h.contains("authorization: bearer scoped-token"))
    };

    std::env::set_var("RASTER_H3_AUTH_TOKEN", "scoped-token");

    // No host list: the token stays home, and public data still reads.
    std::env::remove_var("RASTER_H3_AUTH_HOSTS");
    let server = MockHttpServer::start(data.clone());
    let source = RemoteHttpSource::open(&format!("{}/test.tif", server.url_base)).unwrap();
    assert_eq!(source.fetch_range(0, 9).unwrap(), data[..10]);
    let unlisted = recorded_lowercase(&server);

    // Another host is listed.
    std::env::set_var("RASTER_H3_AUTH_HOSTS", "data.example.com, .example.org");
    let server = MockHttpServer::start(data.clone());
    RemoteHttpSource::open(&format!("{}/test.tif", server.url_base)).unwrap();
    let other_host = recorded_lowercase(&server);

    // This host is listed. The same server is then reached under a name that
    // is not, through the transport that was created for the listed one.
    std::env::set_var("RASTER_H3_AUTH_HOSTS", "data.example.com,127.0.0.1");
    let server = MockHttpServer::start(data.clone());
    let mut transport = HttpTransport::new(&format!("{}/test.tif", server.url_base)).unwrap();
    assert_eq!(transport.fetch_range(0, 9, None).unwrap(), data[..10]);
    let listed = recorded_lowercase(&server);
    transport.url = transport.url.replace("127.0.0.1", "localhost");
    let retargeted = match transport.fetch_range(0, 9, None) {
        // `localhost` may not resolve to the IPv4 listener everywhere.
        Ok(_) => Some(recorded_lowercase(&server)[listed.len()..].to_vec()),
        Err(_) => None,
    };

    std::env::remove_var("RASTER_H3_AUTH_TOKEN");
    std::env::remove_var("RASTER_H3_AUTH_HOSTS");

    assert!(!unlisted.is_empty() && !sent(&unlisted), "{unlisted:?}");
    assert!(
        !other_host.is_empty() && !sent(&other_host),
        "{other_host:?}"
    );
    assert!(sent(&listed), "{listed:?}");
    if let Some(retargeted) = retargeted {
        assert!(
            !retargeted.is_empty() && !sent(&retargeted),
            "{retargeted:?}"
        );
    }
}

fn count_lines(headers: &[String], line: &str) -> usize {
    headers.iter().filter(|h| h.trim() == line).count()
}

// The transport follows redirects itself, keeping `Range` and the pinned
// validator on every hop.
#[test]
fn redirects_are_followed_with_range_and_validator() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    let url = format!("{}/redirect_hops_2_etag_stable.tif", server.url_base);
    let source = RemoteHttpSource::open(&url).unwrap();
    assert_eq!(
        source.fetch_range(150_000, 150_009).unwrap(),
        data[150_000..150_010]
    );
    let headers = recorded_lowercase(&server);
    // Probe and range fetch, each two redirects plus the final request.
    let ranges = headers.iter().filter(|h| h.starts_with("range:")).count();
    assert_eq!(ranges, 6, "{headers:?}");
    assert_eq!(count_lines(&headers, "range: bytes=0-131071"), 3);
    // The validator pinned by the probe goes with every hop of the fetch.
    assert_eq!(count_lines(&headers, "if-match: \"v1\""), 3, "{headers:?}");
}

#[test]
fn redirect_limits_and_missing_locations_fail_clearly() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let server = MockHttpServer::start(vec![3u8; 4096]);
    let open = |path: &str| RemoteHttpSource::open(&format!("{}/{path}", server.url_base));
    assert!(
        open("redirect_hops_5.tif").is_ok(),
        "five redirects are followed"
    );
    let too_many = open("redirect_hops_6.tif").err().unwrap().to_string();
    assert!(
        too_many.contains("redirected more than 5 times"),
        "{too_many}"
    );
    let missing = open("redirect_without_location.tif")
        .err()
        .unwrap()
        .to_string();
    assert!(missing.contains("no usable Location header"), "{missing}");
}

// Credential headers are resolved for each hop's own URL: a redirect away
// from the host a credential is scoped to does not carry it, and a redirect to
// that host does. ureq alone would carry `x-amz-security-token` to any host.
// The bearer and session tokens are exclusive, so each gets its own round.
#[test]
fn credentials_do_not_follow_a_redirect_to_another_host() {
    let _env_lock = ENV_MUTEX.lock().unwrap();
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let Some(other) = MockHttpServer::start_on_second_loopback(data.clone()) else {
        eprintln!("Skipping test: 127.0.0.2 is not a usable loopback address here");
        return;
    };
    let listed = MockHttpServer::start(data.clone());
    let vars = [
        "RASTER_H3_AUTH_TOKEN",
        "RASTER_H3_AUTH_HOSTS",
        "AWS_SESSION_TOKEN",
        "AWS_ENDPOINT_URL",
    ];
    let rounds = [
        (
            vec![
                ("RASTER_H3_AUTH_TOKEN", "scoped-token".to_string()),
                ("RASTER_H3_AUTH_HOSTS", "127.0.0.1".to_string()),
            ],
            "authorization: bearer scoped-token",
        ),
        (
            vec![
                ("AWS_SESSION_TOKEN", "session-token".to_string()),
                ("AWS_ENDPOINT_URL", listed.url_base.clone()),
            ],
            "x-amz-security-token: session-token",
        ),
    ];
    let fetch = |url: String| HttpTransport::new(&url).and_then(|t| t.fetch_range(0, 9, None));

    for (env, credential) in rounds {
        for (name, value) in &env {
            std::env::set_var(name, value);
        }
        let (listed_before, other_before) = (
            recorded_lowercase(&listed).len(),
            recorded_lowercase(&other).len(),
        );
        let away = fetch(format!(
            "{}/redirect_to_127.0.0.2_{}/away.tif",
            listed.url_base, other.port
        ));
        let (listed_mid, other_mid) = (
            recorded_lowercase(&listed).len(),
            recorded_lowercase(&other).len(),
        );
        let back = fetch(format!(
            "{}/redirect_to_127.0.0.1_{}/back.tif",
            other.url_base, listed.port
        ));
        for name in vars {
            std::env::remove_var(name);
        }

        assert_eq!(away.unwrap(), data[..10], "{credential}");
        assert_eq!(back.unwrap(), data[..10], "{credential}");
        let listed_all = recorded_lowercase(&listed);
        let other_all = recorded_lowercase(&other);
        for (headers, sent) in [
            (&listed_all[listed_before..listed_mid], true),
            (&other_all[other_before..other_mid], false),
            (&other_all[other_mid..], false),
            (&listed_all[listed_mid..], true),
        ] {
            assert!(!headers.is_empty(), "{credential}");
            assert_eq!(
                count_lines(headers, credential),
                usize::from(sent),
                "{credential}: {headers:?}"
            );
        }
    }
}

// The HTTP agent's per-connection buffers are fixed in size: a request head
// must fit the output buffer and a response head the input buffer. Typical
// large heads (a 6 KiB bearer token, 12 KiB of response headers) still work.
// The token is process-wide while set, so it must stay small enough that
// concurrent loopback tests that do not take ENV_MUTEX still fit their heads;
// the oversized case is the `check_request_head` unit test instead.
#[test]
fn large_request_and_response_heads_fit_the_agent_buffers() {
    let _env_lock = ENV_MUTEX.lock().unwrap();
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let token = "t".repeat(6 * 1024);
    std::env::set_var("RASTER_H3_AUTH_TOKEN", &token);
    std::env::set_var("RASTER_H3_AUTH_HOSTS", "127.0.0.1");
    let server = MockHttpServer::start(data.clone());
    let transport = HttpTransport::new(&format!("{}/padded_head_12288.tif", server.url_base));
    let result = transport.and_then(|t| t.fetch_range(0, 9, None));
    std::env::remove_var("RASTER_H3_AUTH_TOKEN");
    std::env::remove_var("RASTER_H3_AUTH_HOSTS");

    assert_eq!(result.unwrap(), data[..10]);
    let expected = format!("authorization: bearer {token}");
    assert!(recorded_lowercase(&server)
        .iter()
        .any(|h| h.trim() == expected));
}

// A response head larger than the agent's input buffer fails the request
// with a clear transport error instead of hanging or being misread.
#[test]
fn oversized_response_head_is_a_transport_error() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let server = MockHttpServer::start(vec![0u8; 4096]);
    let url = format!("{}/padded_head_20480.tif", server.url_base);
    let error = match RemoteHttpSource::open(&url) {
        Ok(_) => panic!("a 20 KiB response head must be rejected"),
        Err(err) => err.to_string(),
    };
    assert!(error.contains("Network transport error"), "{error}");
    assert!(error.contains("response header is too big"), "{error}");
}

// A server that refuses the request is the only sign the caller gets that a
// configured token was not sent, so the error says so.
#[test]
fn refused_request_reports_a_withheld_auth_token() {
    let _env_lock = ENV_MUTEX.lock().unwrap();
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let server = MockHttpServer::start(vec![0u8; 1024]);
    let url = format!("{}/forbidden.tif", server.url_base);
    let open_error = || match RemoteHttpSource::open(&url) {
        Ok(_) => panic!("a 403 response must fail the open"),
        Err(err) => err.to_string(),
    };

    std::env::remove_var("RASTER_H3_AUTH_TOKEN");
    std::env::remove_var("RASTER_H3_AUTH_HOSTS");
    let without_token = open_error();

    std::env::set_var("RASTER_H3_AUTH_TOKEN", "scoped-token");
    let withheld = open_error();

    std::env::set_var("RASTER_H3_AUTH_HOSTS", "127.0.0.1");
    let sent = open_error();

    std::env::remove_var("RASTER_H3_AUTH_TOKEN");
    std::env::remove_var("RASTER_H3_AUTH_HOSTS");

    assert!(without_token.contains("HTTP 403"), "{without_token}");
    assert!(!without_token.contains("RASTER_H3_AUTH"), "{without_token}");
    assert!(
        withheld.contains("RASTER_H3_AUTH_TOKEN was not sent")
            && withheld.contains("RASTER_H3_AUTH_HOSTS"),
        "{withheld}"
    );
    assert!(!sent.contains("was not sent"), "{sent}");
    for message in [&without_token, &withheld, &sent] {
        assert!(!message.contains("scoped-token"), "{message}");
    }
}
