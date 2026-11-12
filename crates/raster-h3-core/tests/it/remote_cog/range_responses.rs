//! Range and full-body response handling: validation, caching, size limits and retries.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::http_range::{HttpTransport, RemoteHttpSource};
use raster_h3_core::raster::remote_prefetch::RemoteChunkPrefetchQueue;

use super::MockHttpServer;
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
fn full_body_response_serves_fetch_range_from_cache() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    let source = RemoteHttpSource::open(&format!("{}/full_body", server.url_base)).unwrap();

    // The probe was the only request: a non-Range server answers every
    // subsequent request with HTTP 200, which fetch_range would reject.
    assert_eq!(source.fetch_range(100, 199).unwrap(), data[100..200]);
    assert_eq!(
        source.fetch_range(199_990, 199_999).unwrap(),
        data[199_990..]
    );
    assert_eq!(server.request_count.load(Ordering::SeqCst), 1);
}

#[test]
fn full_body_response_over_limit_is_rejected() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data);
    let url = format!("{}/full_body", server.url_base);
    let transport = HttpTransport::new(&url)
        .unwrap()
        .with_max_full_body_bytes(1024);
    match RemoteHttpSource::from_transport(url, Box::new(transport)) {
        Err(err) => assert!(
            err.to_string().contains("RASTER_H3_MAX_FULL_BODY_BYTES"),
            "unexpected error: {err}"
        ),
        Ok(_) => panic!("full body over the limit must be rejected"),
    }
}

#[test]
fn full_body_response_within_limit_is_accepted() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    let url = format!("{}/full_body", server.url_base);
    let transport = HttpTransport::new(&url)
        .unwrap()
        .with_max_full_body_bytes(data.len() as u64);
    let source = RemoteHttpSource::from_transport(url, Box::new(transport))
        .expect("full body within the limit must be accepted");
    assert!(source.is_complete_file());
}

#[test]
fn truncated_range_body_is_retried() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..400_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    let source = RemoteHttpSource::open(&format!("{}/test.tif", server.url_base)).unwrap();
    server.body_truncations.store(1, Ordering::SeqCst);
    let fetched = source.fetch_range(200_000, 200_099).unwrap();
    assert_eq!(fetched, data[200_000..=200_099]);
    assert_eq!(server.request_count.load(Ordering::SeqCst), 3);
}

/// A numeric `Retry-After` longer than the backoff (at most 100 ms for the
/// first retry) sets the wait.
#[test]
fn retry_after_seconds_delay_the_retry() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..400_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    let source = RemoteHttpSource::open(&format!("{}/retry_after.tif", server.url_base)).unwrap();
    server.transient_failures.store(1, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let fetched = source.fetch_range(200_000, 200_099).unwrap();
    assert_eq!(fetched, data[200_000..=200_099]);
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "retried after {:?}",
        started.elapsed()
    );
}

#[test]
fn persistently_truncated_range_body_fails_after_retries() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..400_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data);
    let source = RemoteHttpSource::open(&format!("{}/test.tif", server.url_base)).unwrap();
    server.body_truncations.store(100, Ordering::SeqCst);
    assert!(source.fetch_range(200_000, 200_099).is_err());
    assert_eq!(server.request_count.load(Ordering::SeqCst), 1 + 5);
}

#[test]
fn truncated_probe_body_is_retried() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..400_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    server.body_truncations.store(1, Ordering::SeqCst);
    let source = RemoteHttpSource::open(&format!("{}/truncate_any.tif", server.url_base)).unwrap();
    assert_eq!(server.request_count.load(Ordering::SeqCst), 2);
    assert_eq!(source.fetch_range(0, 99).unwrap(), data[0..100]);
}

#[test]
fn truncated_full_body_is_retried() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let server = MockHttpServer::start(data.clone());
    server.body_truncations.store(1, Ordering::SeqCst);
    let source =
        RemoteHttpSource::open(&format!("{}/full_body_truncate_any", server.url_base)).unwrap();
    assert!(source.is_complete_file());
    let last = data.len() as u64 - 1;
    assert_eq!(source.fetch_range(0, last).unwrap(), data);
    assert_eq!(server.request_count.load(Ordering::SeqCst), 2);
}

#[test]
fn full_body_reader_skips_remote_prefetch() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    let (file, _path) = helpers::TestGeoTiffBuilder::new(64, 64).create_f32_tempfile(|_, _| 1.0);
    let bytes = std::fs::read(file.path()).unwrap();
    let server = MockHttpServer::start(bytes);
    let reader = GeoTiffStreamReader::open(format!("{}/full_body", server.url_base)).unwrap();
    let info = reader.chunk_info.as_ref().unwrap();
    let indices: Vec<u32> = (0..info.chunk_offsets.len() as u32).collect();
    assert!(
        RemoteChunkPrefetchQueue::spawn_single(&reader, &indices, None).is_none(),
        "complete-file sources must not spawn remote prefetch workers"
    );
    // Chunk reads still succeed through the cache-backed path.
    let source = reader.remote_source().unwrap();
    let payload = source
        .read_exact_range(info.chunk_offsets[0], info.chunk_bytes[0] as usize)
        .unwrap();
    assert_eq!(payload.len(), info.chunk_bytes[0] as usize);
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
