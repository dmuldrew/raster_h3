//! ETag pinning: conditional range requests, replacement detection and retargeting.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use raster_h3_core::raster::http_range::{HttpTransport, RemoteHttpSource};

use super::MockHttpServer;

fn versioned_source(mode: &str) -> (MockHttpServer, Arc<RemoteHttpSource>) {
    let server = MockHttpServer::start(vec![7; 400_000]);
    let source = Arc::new(RemoteHttpSource::open(&format!("{}/{mode}", server.url_base)).unwrap());
    (server, source)
}

fn conditional_requests(server: &MockHttpServer) -> Vec<String> {
    server
        .recorded_headers()
        .into_iter()
        .filter(|h| h.to_ascii_lowercase().starts_with("if-match:"))
        .collect()
}

#[test]
fn strong_etag_pins_cached_direct_and_concurrent_ranges() {
    let (server, source) = versioned_source("etag_stable");
    assert_eq!(source.read_exact_range(0, 10).unwrap(), vec![7; 10]);
    assert_eq!(server.request_count.load(Ordering::SeqCst), 1);
    let workers: Vec<_> = (0..4)
        .map(|i| {
            let source = Arc::clone(&source);
            thread::spawn(move || {
                source
                    .fetch_range(150_000 + i * 100, 150_009 + i * 100)
                    .unwrap()
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), vec![7; 10]);
    }
    // Both the single-block cache fill and cross-block direct path are conditional.
    assert_eq!(source.read_exact_range(140_000, 10).unwrap(), vec![7; 10]);
    assert_eq!(source.read_exact_range(131_070, 10).unwrap(), vec![7; 10]);
    let conditions = conditional_requests(&server);
    assert_eq!(conditions.len(), 6);
    assert!(conditions.iter().all(|h| h.trim() == "If-Match: \"v1\""));
    assert_eq!(
        server
            .recorded_headers()
            .iter()
            .filter(|h| h.trim().eq_ignore_ascii_case("Accept-Encoding: identity"))
            .count(),
        7
    );
}

#[test]
fn same_size_replacement_fails_without_retrying_or_caching_new_bytes() {
    let (server, source) = versioned_source("etag_changed");
    for expected_requests in [2, 3] {
        let err = source.read_exact_range(140_000, 10).unwrap_err();
        assert!(err.to_string().contains("HTTP 412"), "{err}");
        assert_eq!(
            server.request_count.load(Ordering::SeqCst),
            expected_requests
        );
    }
    assert_eq!(source.read_exact_range(0, 10).unwrap(), vec![7; 10]);
    assert_eq!(conditional_requests(&server).len(), 2);
}

#[test]
fn changed_missing_or_weak_response_etag_is_rejected_even_if_server_ignores_condition() {
    for mode in ["etag_ignored", "etag_missing", "etag_downgraded"] {
        let (server, source) = versioned_source(mode);
        let err = source.read_exact_range(140_000, 10).unwrap_err();
        assert!(
            err.to_string().contains("ETag changed or disappeared"),
            "{mode}: {err}"
        );
        let err = source.fetch_range(150_000, 150_009).unwrap_err();
        assert!(
            err.to_string().contains("ETag changed or disappeared"),
            "{mode}: {err}"
        );
        assert_eq!(server.request_count.load(Ordering::SeqCst), 3);
        assert_eq!(conditional_requests(&server).len(), 2);
    }
}

#[test]
fn absent_or_weak_initial_etags_do_not_send_invalid_preconditions() {
    for mode in ["ranged", "etag_weak"] {
        let (server, source) = versioned_source(mode);
        assert_eq!(source.fetch_range(150_000, 150_009).unwrap(), vec![7; 10]);
        assert!(conditional_requests(&server).is_empty());
    }
}

#[test]
fn transport_clones_and_transient_retries_keep_the_original_etag() {
    let server = MockHttpServer::start(vec![7; 400_000]);
    let transport = HttpTransport::new(&format!("{}/etag_stable", server.url_base)).unwrap();
    let cloned = transport.clone(); // Clone before the probe pins the version.
    transport.probe(131_072).unwrap();
    server.transient_failures.store(1, Ordering::SeqCst);
    assert_eq!(
        cloned.fetch_range(150_000, 150_009, Some(400_000)).unwrap(),
        vec![7; 10]
    );
    assert_eq!(server.request_count.load(Ordering::SeqCst), 3);
    let conditions = conditional_requests(&server);
    assert_eq!(conditions.len(), 2);
    assert!(conditions.iter().all(|h| h.trim() == "If-Match: \"v1\""));

    let changing = MockHttpServer::start(vec![7; 400_000]);
    let transport = HttpTransport::new(&format!("{}/etag_ignored", changing.url_base)).unwrap();
    transport.probe(131_072).unwrap();
    assert!(transport
        .clone()
        .probe(131_072)
        .unwrap_err()
        .to_string()
        .contains("ETag changed"));
}

#[test]
fn retargeted_transport_uses_url_scoped_etags_for_probes_and_ranges() {
    for probe_first in [false, true] {
        let server = MockHttpServer::start(vec![7; 400_000]);
        let original_url = format!("{}/etag_stable", server.url_base);
        let mut transport = HttpTransport::new(&original_url).unwrap();
        transport.probe(131_072).unwrap();
        let original = transport.clone();

        transport.url = format!("{}/etag_other", server.url_base);
        if probe_first {
            transport.probe(131_072).unwrap();
        } else {
            assert_eq!(
                transport
                    .fetch_range(150_000, 150_009, Some(400_000))
                    .unwrap(),
                vec![7; 10]
            );
        }
        transport
            .fetch_range(150_000, 150_009, Some(400_000))
            .unwrap();
        original
            .fetch_range(150_000, 150_009, Some(400_000))
            .unwrap();
        transport.url = original_url;
        transport.probe(131_072).unwrap();

        let conditions = conditional_requests(&server);
        let conditions: Vec<_> = conditions.iter().map(|h| h.trim()).collect();
        assert_eq!(
            conditions,
            [
                "If-Match: \"other\"",
                "If-Match: \"v1\"",
                "If-Match: \"v1\""
            ]
        );
    }
}

#[test]
fn retargeted_clones_pin_different_urls_concurrently() {
    let server = MockHttpServer::start(vec![7; 400_000]);
    let original_url = format!("{}/etag_stable", server.url_base);
    let other_url = format!("{}/etag_other", server.url_base);
    let transport = HttpTransport::new(&original_url).unwrap();
    let mut other = transport.clone();
    other.url = other_url.clone();
    let workers: Vec<_> = [transport.clone(), other]
        .into_iter()
        .map(|transport| {
            thread::spawn(move || {
                transport.probe(131_072).unwrap();
                transport
                    .fetch_range(150_000, 150_009, Some(400_000))
                    .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let mut retargeted = transport.clone();
    retargeted.url = other_url;
    retargeted.probe(131_072).unwrap();
    transport.probe(131_072).unwrap();

    let conditions = conditional_requests(&server);
    assert_eq!(conditions.len(), 4);
    for tag in ["v1", "other"] {
        assert_eq!(
            conditions
                .iter()
                .filter(|h| h.trim() == format!("If-Match: \"{tag}\""))
                .count(),
            2
        );
    }
}

#[test]
fn retargeting_after_an_unversioned_probe_can_pin_a_strong_etag() {
    for mode in ["ranged", "etag_weak"] {
        let server = MockHttpServer::start(vec![7; 400_000]);
        let mut transport = HttpTransport::new(&format!("{}/{mode}", server.url_base)).unwrap();
        transport.probe(131_072).unwrap();
        transport.url = format!("{}/etag_stable", server.url_base);
        transport.probe(131_072).unwrap();
        assert_eq!(
            transport
                .fetch_range(150_000, 150_009, Some(400_000))
                .unwrap(),
            vec![7; 10]
        );
        transport.url = format!("{}/{mode}", server.url_base);
        transport.probe(131_072).unwrap();
        let conditions = conditional_requests(&server);
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].trim(), "If-Match: \"v1\"");
    }
}
