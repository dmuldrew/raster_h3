//! DuckDB bind-side record queue must surface stream failures rather than EOF.

use std::sync::Mutex;

use crate::helpers::corrupt_strip_geotiff as corrupt_payload;
use raster_h3_core::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_duckdb::functions::bind_utils::ConcurrentRecordQueue;

#[test]
fn duckdb_record_queue_propagates_failure_instead_of_eof() {
    let file = corrupt_payload();
    let streamer = Mutex::new(
        MultiScanHorizonStreamer::new(
            GeoTiffStreamReader::open(file.path()).unwrap(),
            &MultiResolutionConfig::single(8),
        )
        .unwrap(),
    );
    let queue = ConcurrentRecordQueue::new();
    let mut failed = false;
    for _ in 0..1024 {
        match queue.pop_or_refill(&streamer, |s, n, f| s.drain_completed_into(n, f)) {
            Ok(batch) => assert!(
                batch.is_some_and(|b| !b.is_empty()),
                "corruption must not become EOF"
            ),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert!(queue
        .pop_or_refill(&streamer, |s, n, f| s.drain_completed_into(n, f))
        .is_err());
    assert!(queue.ready_batches.lock().unwrap().is_empty());
}
