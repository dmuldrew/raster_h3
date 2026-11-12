//! Remote chunk prefetch queue: wake-ups, byte budget, out-of-order requests and range coalescing.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::remote_prefetch::{
    coalesce_chunk_ranges, ChunkLocation, RemoteChunkPrefetchQueue, RemotePrefetchConfig,
};
use rustc_hash::FxHashMap;
use tiff::decoder::DecodingResult;

use super::MockHttpServer;
use crate::helpers;

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
