//! Memory benchmark scenarios. Each `run` builds its fixtures, starts a
//! [`Probe`] and returns metrics; each `check` compares variants.
//!
//! Streams pin `decode_workers` and `aggregation_workers` so allocator peaks
//! do not scale with the runner's core count.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use h3o::{LatLng, Resolution};
use raster_h3_core::aggregator::accumulator::H3Accumulator;
use raster_h3_core::aggregator::categorical::CategoricalAccumulator;
use raster_h3_core::aggregator::multi_horizon::sharded_map::ShardedResolutionMap;
use raster_h3_core::aggregator::multi_horizon::spill::{SpillAccumulator, SpillRuns};
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiContinuousRecord, MultiResolutionConfig,
    MultiScanHorizonStreamer, QuantileTarget,
};
use raster_h3_core::parquet::writer::{
    run_parquet_streaming_pipeline, ContinuousRowGroupBuffer, ParquetExportConfig,
    ParquetRowGroupBuffer,
};
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use raster_h3_core::raster::remote_prefetch::{
    coalesce_chunk_ranges, ChunkLocation, RemoteChunkPrefetchQueue, RemotePrefetchConfig,
};
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

use crate::http::RangeServer;
use crate::{Check, Metrics, Probe, Results, Scenario, MIB};

pub fn all() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "spill_sorted_run",
            about: "One resolution's map (117,649 res-11 cells) is taken as a sorted vector and written as a spill run, as the controller does when the map reaches its share of the budget. Checks that the temporary vector, live alongside the not-yet-drained shards, needs no more than the map's own accounted bytes (the controller leaves that headroom), and that the map's memory is released afterwards.",
            variants: &["plain", "quantiles", "categorical"],
            columns: &["cells", "map_estimated_bytes", "spill_transient_bytes", "sorted_vec_bytes", "live_before_bytes", "live_after_bytes"],
            run: spill_sorted_run,
            check: check_spill_sorted_run,
        },
        Scenario {
            name: "spill_stream_quantiles",
            about: "Continuous res-11 stream with p50/p90 sketches over wide-range values, fully in memory (512 MiB budget) and forced to spill (1 MiB budget). The spilled run must reproduce the in-memory output and use less heap.",
            variants: &["in_memory", "spill", "no_quantiles"],
            columns: &["cells", "spill_runs", "peak_active_bytes", "budget_bytes"],
            run: spill_stream_quantiles,
            check: check_spill_stream_quantiles,
        },
        Scenario {
            name: "categorical_high_cardinality",
            about: "Categorical res-9 stream over 3,000 classes (about 46 per cell, past the 16-class inline array, so cells use the heap class map), in memory and forced to spill.",
            variants: &["in_memory", "spill"],
            columns: &["cells", "spill_runs", "peak_active_bytes", "budget_bytes"],
            run: categorical_high_cardinality,
            check: check_categorical_high_cardinality,
        },
        Scenario {
            name: "slow_http_consumer",
            about: "Remote prefetch of 128 strips (32 KiB each, two per 64 KiB range, 8 workers, 2 ms server latency) taken by a consumer that holds each chunk for 1 ms. `budget` is a 256 KiB `max_buffered_bytes`; `unbounded` disables the byte budget, leaving only the 64-chunk `queue_capacity` that bounded the queue before #60.",
            variants: &["budget", "unbounded"],
            columns: &["ranges", "range_requests", "peak_held_bytes", "budget_bytes"],
            run: slow_http_consumer,
            check: check_slow_http_consumer,
        },
        Scenario {
            name: "slow_parquet_writer",
            about: "Continuous stream into the double-buffered Parquet pipeline with 2,048-row groups, with a normal writer and one that sleeps 20 ms per row group. Backpressure must hold the extra heap to a few row-group buffers.",
            variants: &["fast", "slow"],
            columns: &["rows", "row_groups", "row_group_buffer_bytes"],
            run: slow_parquet_writer,
            check: check_slow_parquet_writer,
        },
        Scenario {
            name: "many_tiny_chunks",
            about: "A 512×512 f32 raster in 512 one-row strips (2 KiB each), streamed from a local file and over HTTP. Checks per-chunk allocation overhead and that the strips are coalesced into few range requests.",
            variants: &["local", "remote"],
            columns: &["cells", "chunks", "allocs_per_chunk", "range_requests", "expected_ranges"],
            run: many_tiny_chunks,
            check: check_many_tiny_chunks,
        },
        Scenario {
            name: "concurrent_streams",
            about: "One and four simultaneous continuous streams over the same raster, sharing the dedicated 2-thread aggregation pool. Heap must grow at most linearly with the number of streams.",
            variants: &["1", "4"],
            columns: &["streams", "cells"],
            run: concurrent_streams,
            check: check_concurrent_streams,
        },
    ]
}

fn metric(r: &Results, variant: &str, key: &str) -> f64 {
    r[variant]
        .get(key)
        .copied()
        .unwrap_or_else(|| panic!("{variant} has no metric {key}"))
}

fn set(m: &mut Metrics, key: &str, v: impl Into<f64>) {
    m.insert(key.to_string(), v.into());
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const GEOKEYS_4326: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];

/// A WGS84 GeoTIFF with an explicit strip height, as bytes.
fn tiff_bytes<C, T>(
    (width, height): (u32, u32),
    rows_per_strip: u32,
    (lon, lat, pixel): (f64, f64, f64),
    value: impl Fn(u32, u32) -> T,
) -> Vec<u8>
where
    C: colortype::ColorType<Inner = T>,
    [T]: tiff::encoder::TiffValue,
{
    let mut bytes = Vec::new();
    let mut encoder = TiffEncoder::new(std::io::Cursor::new(&mut bytes)).expect("encoder");
    let mut image = encoder.new_image::<C>(width, height).expect("image");
    image
        .rows_per_strip(rows_per_strip)
        .expect("rows per strip");
    let enc = image.encoder();
    enc.write_tag(Tag::ModelPixelScaleTag, &[pixel, pixel, 0.0][..])
        .expect("scale");
    enc.write_tag(Tag::ModelTiepointTag, &[0.0, 0.0, 0.0, lon, lat, 0.0][..])
        .expect("tiepoint");
    enc.write_tag(Tag::GeoKeyDirectoryTag, &GEOKEYS_4326[..])
        .expect("geokeys");
    let data: Vec<T> = (0..height)
        .flat_map(|r| (0..width).map(move |c| (c, r)))
        .map(|(c, r)| value(c, r))
        .collect();
    image.write_data(&data).expect("write");
    bytes
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// Deterministic hash of a pixel position.
fn mix(c: u32, r: u32) -> u64 {
    let mut z = (u64::from(c) << 32 | u64::from(r)).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn pinned_config(resolution: u8) -> MultiResolutionConfig {
    let mut config = MultiResolutionConfig::single(resolution);
    config.decode_workers = 2;
    config.aggregation_workers = Some(2);
    config
}

/// Cell count and an order-independent digest of every cell's exact output.
#[derive(Default, Clone, Copy, PartialEq)]
struct Digest {
    cells: u64,
    hash: u64,
}

impl Digest {
    /// Fold one cell's values (exact bits) into the digest; a wrapping sum of
    /// per-cell hashes does not depend on output order.
    fn add(&mut self, cell: u64, values: &[f64]) {
        let mut h = mix64(cell);
        for v in values {
            h = mix64(h ^ v.to_bits());
        }
        self.cells += 1;
        self.hash = self.hash.wrapping_add(h);
    }

    fn continuous(&mut self, record: &MultiContinuousRecord, quantiles: bool) {
        // No heap allocation here: the probe counts the harness too.
        let acc = &record.accumulator;
        let mut values = [acc.count, acc.sum, acc.min, acc.max, 0.0, 0.0];
        if quantiles {
            values[4] = acc.quantile(0.5);
            values[5] = acc.quantile(0.9);
        }
        self.add(record.h3_index, &values);
    }

    fn record(&self, m: &mut Metrics) {
        set(m, "cells", self.cells as f64);
        // Keep 52 bits so the JSON number round-trips exactly.
        set(m, "digest", (self.hash >> 12) as f64);
    }
}

fn mix64(x: u64) -> u64 {
    mix((x >> 32) as u32, x as u32)
}

fn drain_continuous(streamer: &mut MultiScanHorizonStreamer, quantiles: bool) -> Digest {
    let mut digest = Digest::default();
    loop {
        let n = streamer
            .drain_completed_into(4096, |_, record| digest.continuous(&record, quantiles))
            .expect("stream");
        if n == 0 {
            return digest;
        }
    }
}

// ---------------------------------------------------------------------------
// spill_sorted_run
// ---------------------------------------------------------------------------

fn spill_cells() -> Vec<u64> {
    let parent = LatLng::new(40.0, -100.0)
        .expect("lat/lng")
        .to_cell(Resolution::Five);
    parent.children(Resolution::Eleven).map(u64::from).collect()
}

fn fill_and_spill<A: SpillAccumulator>(make: impl Fn(u64) -> A) -> Metrics {
    let cells = spill_cells();
    let mut map = ShardedResolutionMap::<A>::new();
    for &cell in &cells {
        map.merge_owned(cell, make(cell));
    }
    // As in the controller, a resolution that spills stops tracking eviction.
    map.set_eviction_enabled(false);
    let dir = tempfile::tempdir().expect("spill dir");
    let mut runs = SpillRuns::<A>::new(usize::MAX, Some(dir.path()));
    let mut m = Metrics::new();
    set(&mut m, "cells", cells.len() as f64);
    set(&mut m, "map_estimated_bytes", map.estimated_bytes() as f64);
    set(
        &mut m,
        "sorted_vec_bytes",
        (cells.len() * size_of::<(u64, A)>()) as f64,
    );
    set(&mut m, "live_before_bytes", crate::alloc::live() as f64);

    let probe = Probe::start();
    let ((), region) = crate::alloc::region(|| {
        let records = map.take_sorted();
        runs.push_sorted(records).expect("spill run");
    });
    probe.finish(&mut m);
    // `region` restarted the peak, so read its own transient figure.
    set(&mut m, "spill_transient_bytes", region.peak_extra as f64);
    set(&mut m, "peak_live_bytes", region.peak_extra as f64);
    set(&mut m, "live_after_bytes", crate::alloc::live() as f64);
    assert_eq!(runs.runs_written, 1);
    drop(map);
    m
}

fn spill_sorted_run(variant: &str) -> Metrics {
    match variant {
        "plain" => fill_and_spill(|cell| H3Accumulator::new((cell % 1000) as f64)),
        "quantiles" => fill_and_spill(|cell| {
            let mut acc = H3Accumulator::with_quantiles();
            for k in 0..8 {
                acc.update(((cell >> k) % 5000) as f64 * 0.37 + 1.0);
            }
            acc
        }),
        "categorical" => fill_and_spill(|cell| {
            let mut acc = CategoricalAccumulator::new();
            // More than 16 classes moves the histogram to the heap map.
            for k in 0..24 {
                acc.update(((cell >> (k % 13)) % 400) as i64 + k);
            }
            acc
        }),
        other => panic!("unknown variant {other}"),
    }
}

fn check_spill_sorted_run(r: &Results) -> Vec<Check> {
    let mut checks = Vec::new();
    for variant in ["plain", "quantiles", "categorical"] {
        let map = metric(r, variant, "map_estimated_bytes");
        checks.push(Check::at_most(
            &format!("{variant}: sorted-run transient within the map's accounted bytes"),
            metric(r, variant, "spill_transient_bytes"),
            map,
        ));
        // Accumulators move into the vector, so only the vector itself (plus
        // the run's write buffer) is new heap: no sketch or class map is cloned.
        checks.push(Check::at_most(
            &format!("{variant}: transient is the sorted vector alone (+64 KiB)"),
            metric(r, variant, "spill_transient_bytes"),
            metric(r, variant, "sorted_vec_bytes") + 65536.0,
        ));
        checks.push(Check::at_most(
            &format!("{variant}: map memory released after the spill (live bytes drop by half its estimate)"),
            metric(r, variant, "live_after_bytes"),
            metric(r, variant, "live_before_bytes") - map / 2.0,
        ));
    }
    checks
}

// ---------------------------------------------------------------------------
// spill_stream_quantiles
// ---------------------------------------------------------------------------

fn spill_stream_quantiles(variant: &str) -> Metrics {
    let dir = tempfile::tempdir().expect("dir");
    let bytes = tiff_bytes::<colortype::Gray32Float, f32>(
        (1536, 384),
        16,
        (-100.0, 40.0, 0.0002),
        // Wide, mixed-sign range so sketches hold many bins.
        |c, r| ((mix(c, r) % 200_000) as f32 - 50_000.0) * 0.013,
    );
    let path = write_file(dir.path(), "values.tif", &bytes);
    let mut config = pinned_config(11);
    config.spill_directory = Some(dir.path().to_path_buf());
    let quantiles = variant != "no_quantiles";
    if quantiles {
        config.quantiles = QuantileTarget::parse_list("p50,p90").expect("quantiles");
    }
    config.aggregation_budget_bytes = match variant {
        "spill" => MIB as usize,
        _ => 512 * MIB as usize,
    };

    let mut m = Metrics::new();
    let probe = Probe::start();
    let reader = GeoTiffStreamReader::open(&path).expect("open");
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).expect("streamer");
    let digest = drain_continuous(&mut streamer, quantiles);
    set(&mut m, "spill_runs", streamer.spill_run_count() as f64);
    set(
        &mut m,
        "peak_active_bytes",
        streamer.peak_active_bytes() as f64,
    );
    drop(streamer);
    probe.finish(&mut m);
    digest.record(&mut m);
    set(
        &mut m,
        "budget_bytes",
        config.aggregation_budget_bytes as f64,
    );
    m
}

fn check_spill_stream_quantiles(r: &Results) -> Vec<Check> {
    let mut checks = vec![
        Check::equal(
            "in-memory run does not spill",
            metric(r, "in_memory", "spill_runs"),
            0.0,
        ),
        Check::at_least(
            "budgeted run spills several runs",
            metric(r, "spill", "spill_runs"),
            2.0,
        ),
    ];
    for key in ["cells", "digest"] {
        checks.push(Check::equal(
            &format!("spilled output matches in memory: {key}"),
            metric(r, "spill", key),
            metric(r, "in_memory", key),
        ));
    }
    checks.push(Check::at_most(
        "spilled active state within the budget",
        metric(r, "spill", "peak_active_bytes"),
        metric(r, "spill", "budget_bytes"),
    ));
    checks.push(Check::at_most(
        "spilling lowers peak heap",
        metric(r, "spill", "peak_live_bytes"),
        metric(r, "in_memory", "peak_live_bytes"),
    ));
    checks.push(Check::at_most(
        "sketches cost heap only when selected",
        metric(r, "no_quantiles", "peak_live_bytes"),
        metric(r, "in_memory", "peak_live_bytes"),
    ));
    checks
}

// ---------------------------------------------------------------------------
// categorical_high_cardinality
// ---------------------------------------------------------------------------

fn categorical_high_cardinality(variant: &str) -> Metrics {
    let dir = tempfile::tempdir().expect("dir");
    let bytes =
        tiff_bytes::<colortype::Gray16, u16>((1024, 512), 16, (-100.0, 40.0, 0.0005), |c, r| {
            (mix(c, r) % 3000) as u16 + 1
        });
    let path = write_file(dir.path(), "classes.tif", &bytes);
    let mut config = pinned_config(9);
    config.spill_directory = Some(dir.path().to_path_buf());
    config.aggregation_budget_bytes = match variant {
        "spill" => 2 * MIB as usize,
        _ => 512 * MIB as usize,
    };

    let mut m = Metrics::new();
    let probe = Probe::start();
    let reader = GeoTiffStreamReader::open(&path).expect("open");
    let mut streamer = MultiCategoricalHorizonStreamer::new(reader, &config).expect("streamer");
    let (mut digest, mut class_entries) = (Digest::default(), 0usize);
    // Reused across cells so the harness adds no per-cell allocations.
    let (mut values, mut classes) = (Vec::new(), Vec::new());
    loop {
        let n = streamer
            .drain_completed_into(4096, |_, record| {
                let acc = &record.accumulator;
                classes.clear();
                acc.for_each_class(|class, count| classes.push((class, count)));
                // Heap maps iterate in hash order; digest in class order.
                classes.sort_by_key(|&(class, _)| class);
                class_entries += classes.len();
                values.clear();
                values.push(acc.total_count);
                values.extend(classes.iter().flat_map(|&(c, n)| [c as f64, n]));
                digest.add(record.h3_index, &values);
            })
            .expect("stream");
        if n == 0 {
            break;
        }
    }
    set(&mut m, "spill_runs", streamer.spill_run_count() as f64);
    set(
        &mut m,
        "peak_active_bytes",
        streamer.peak_active_bytes() as f64,
    );
    drop(streamer);
    probe.finish(&mut m);
    digest.record(&mut m);
    set(&mut m, "class_entries", class_entries as f64);
    set(
        &mut m,
        "budget_bytes",
        config.aggregation_budget_bytes as f64,
    );
    m
}

fn check_categorical_high_cardinality(r: &Results) -> Vec<Check> {
    let mut checks = vec![
        Check::equal(
            "in-memory run does not spill",
            metric(r, "in_memory", "spill_runs"),
            0.0,
        ),
        Check::at_least(
            "budgeted run spills several runs",
            metric(r, "spill", "spill_runs"),
            2.0,
        ),
        Check::at_least(
            "cells use heap class maps (more than 16 classes on average)",
            metric(r, "in_memory", "class_entries") / metric(r, "in_memory", "cells"),
            16.0,
        ),
    ];
    for key in ["cells", "digest"] {
        checks.push(Check::equal(
            &format!("spilled output matches in memory: {key}"),
            metric(r, "spill", key),
            metric(r, "in_memory", key),
        ));
    }
    checks.push(Check::at_most(
        "spilled active state within the budget",
        metric(r, "spill", "peak_active_bytes"),
        metric(r, "spill", "budget_bytes"),
    ));
    checks.push(Check::at_most(
        "spilling lowers peak heap",
        metric(r, "spill", "peak_live_bytes"),
        metric(r, "in_memory", "peak_live_bytes"),
    ));
    checks
}

// ---------------------------------------------------------------------------
// slow_http_consumer
// ---------------------------------------------------------------------------

const STRIP_BYTES: u64 = 32 * 1024;

fn slow_http_consumer(variant: &str) -> Metrics {
    let bytes = tiff_bytes::<colortype::Gray32Float, f32>(
        (1024, 1024),
        8,
        (-100.0, 40.0, 0.0005),
        |c, r| ((c * 7 + r * 13) % 1009) as f32,
    );
    let server = RangeServer::start(bytes, Duration::from_millis(2));
    let reader = GeoTiffStreamReader::open(&server.url).expect("open remote");
    let info = reader.chunk_info.clone().expect("chunk info");
    let n = info.chunk_offsets.len();
    assert!(info.chunk_bytes.iter().all(|&b| b == STRIP_BYTES));
    let budget = 8 * STRIP_BYTES;
    let config = RemotePrefetchConfig {
        num_workers: 8,
        max_range_bytes: 2 * STRIP_BYTES,
        max_buffered_bytes: match variant {
            "budget" => budget,
            _ => u64::MAX,
        },
        ..Default::default()
    };
    let locations: Vec<ChunkLocation> = (0..n)
        .map(|i| ChunkLocation {
            tile_idx: 0,
            chunk_idx: i as u32,
            offset: info.chunk_offsets[i],
            length: info.chunk_bytes[i],
        })
        .collect();
    let ranges = coalesce_chunk_ranges(&locations, config.max_gap_bytes, config.max_range_bytes);
    let before = server.range_requests();

    let mut m = Metrics::new();
    let probe = Probe::start();
    let indices: Vec<u32> = (0..n as u32).collect();
    let queue = RemoteChunkPrefetchQueue::spawn_single(&reader, &indices, Some(config))
        .expect("remote queue");
    let mut checksum = 0u64;
    for i in 0..n as u32 {
        let chunk = queue
            .get_chunk_payload(0, i)
            .expect("payload")
            .expect("scheduled chunk");
        checksum = checksum.wrapping_add(chunk.iter().map(|&b| u64::from(b)).sum::<u64>());
        thread::sleep(Duration::from_millis(1));
    }
    let peak_held = queue.peak_held_bytes();
    drop(queue);
    probe.finish(&mut m);
    set(&mut m, "chunks", n as f64);
    set(&mut m, "ranges", ranges.len() as f64);
    set(
        &mut m,
        "range_requests",
        (server.range_requests() - before) as f64,
    );
    set(&mut m, "peak_held_bytes", peak_held as f64);
    set(&mut m, "budget_bytes", budget as f64);
    set(&mut m, "checksum", checksum as f64);
    m
}

fn check_slow_http_consumer(r: &Results) -> Vec<Check> {
    let budget = metric(r, "budget", "budget_bytes");
    vec![
        Check::at_most(
            "held response bytes within max_buffered_bytes",
            metric(r, "budget", "peak_held_bytes"),
            budget,
        ),
        Check::equal(
            "budget: one request per coalesced range (no refetches)",
            metric(r, "budget", "range_requests"),
            metric(r, "budget", "ranges"),
        ),
        Check::equal(
            "unbounded: one request per coalesced range",
            metric(r, "unbounded", "range_requests"),
            metric(r, "unbounded", "ranges"),
        ),
        Check::equal(
            "same bytes delivered",
            metric(r, "budget", "checksum"),
            metric(r, "unbounded", "checksum"),
        ),
        // Responses dominate the heap, so the budget should hold the peak
        // well below the chunk-count bound alone.
        Check::at_most(
            "budget: peak heap at most twice max_buffered_bytes",
            metric(r, "budget", "peak_live_bytes"),
            2.0 * budget,
        ),
        Check::at_most(
            "budget halves peak heap versus the chunk-count bound alone",
            metric(r, "budget", "peak_live_bytes") * 2.0,
            metric(r, "unbounded", "peak_live_bytes"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// slow_parquet_writer
// ---------------------------------------------------------------------------

const ROW_GROUP: usize = 2048;

/// Delegates to the continuous buffer, optionally sleeping per row group.
struct SlowBuffer<const SLEEP_MS: u64>(ContinuousRowGroupBuffer);

impl<const SLEEP_MS: u64> ParquetRowGroupBuffer for SlowBuffer<SLEEP_MS> {
    type Record = MultiContinuousRecord;

    fn with_capacity_and_options(capacity: usize, compact: bool, geoparquet: bool) -> Self {
        Self(ContinuousRowGroupBuffer::with_capacity_and_options(
            capacity, compact, geoparquet,
        ))
    }
    fn push_record(&mut self, record: Self::Record) {
        self.0.push_record(record);
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn geometry_bounds(&self) -> Option<[f64; 4]> {
        self.0.geometry_bounds()
    }
    fn clear(&mut self) {
        self.0.clear();
    }
    fn sort_by_h3_index(&mut self) {
        self.0.sort_by_h3_index();
    }
    fn flush_to_row_group(
        &self,
        writer: &mut parquet::file::writer::SerializedFileWriter<std::fs::File>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if SLEEP_MS > 0 {
            thread::sleep(Duration::from_millis(SLEEP_MS));
        }
        self.0.flush_to_row_group(writer)
    }
    fn schema_message(compact: bool, geoparquet: bool) -> &'static str {
        ContinuousRowGroupBuffer::schema_message(compact, geoparquet)
    }
}

/// Heap held by one full row-group buffer (`ROW_GROUP` rows).
fn row_group_buffer_bytes(path: &Path) -> usize {
    let reader = GeoTiffStreamReader::open(path).expect("open");
    let mut streamer = MultiScanHorizonStreamer::new(reader, &pinned_config(10)).expect("streamer");
    let records = streamer.fetch_next_batch(ROW_GROUP).expect("records");
    let (buffer, region) = crate::alloc::region(|| {
        let mut buffer =
            ContinuousRowGroupBuffer::with_capacity_and_options(ROW_GROUP, false, true);
        let n = records.len();
        // Pad short batches by repeating records; only the size matters here.
        for i in 0..ROW_GROUP {
            buffer.push_record(records[i % n].clone());
        }
        buffer
    });
    drop(buffer);
    region.retained
}

fn slow_parquet_writer(variant: &str) -> Metrics {
    let dir = tempfile::tempdir().expect("dir");
    let bytes = tiff_bytes::<colortype::Gray32Float, f32>(
        (1024, 1024),
        16,
        (-100.0, 40.0, 0.0005),
        |c, r| ((c * 3 + r * 5) % 251) as f32,
    );
    let path = write_file(dir.path(), "values.tif", &bytes);
    let buffer_bytes = row_group_buffer_bytes(&path);
    let out = dir.path().join("out.parquet");
    let export = ParquetExportConfig {
        row_group_size: ROW_GROUP,
        geoparquet: true,
        ..Default::default()
    };

    let mut m = Metrics::new();
    let probe = Probe::start();
    let reader = GeoTiffStreamReader::open(&path).expect("open");
    let streamer = MultiScanHorizonStreamer::new(reader, &pinned_config(10)).expect("streamer");
    let rows = match variant {
        "slow" => run_parquet_streaming_pipeline::<_, SlowBuffer<20>, _>(streamer, &out, export),
        _ => run_parquet_streaming_pipeline::<_, SlowBuffer<0>, _>(streamer, &out, export),
    }
    .expect("export");
    probe.finish(&mut m);
    set(&mut m, "rows", rows as f64);
    set(&mut m, "row_groups", rows.div_ceil(ROW_GROUP) as f64);
    set(&mut m, "row_group_buffer_bytes", buffer_bytes as f64);
    m
}

fn check_slow_parquet_writer(r: &Results) -> Vec<Check> {
    let buffer = metric(r, "slow", "row_group_buffer_bytes");
    vec![
        Check::equal(
            "same rows written",
            metric(r, "slow", "rows"),
            metric(r, "fast", "rows"),
        ),
        Check::at_least(
            "export spans many row groups",
            metric(r, "slow", "row_groups"),
            8.0,
        ),
        // At most: one being filled, two queued and one being written.
        Check::at_most(
            "slow writer adds at most 3 row-group buffers of heap",
            metric(r, "slow", "peak_live_bytes") - metric(r, "fast", "peak_live_bytes"),
            3.0 * buffer,
        ),
    ]
}

// ---------------------------------------------------------------------------
// many_tiny_chunks
// ---------------------------------------------------------------------------

fn many_tiny_chunks(variant: &str) -> Metrics {
    let dir = tempfile::tempdir().expect("dir");
    let bytes =
        tiff_bytes::<colortype::Gray32Float, f32>((512, 512), 1, (-100.0, 40.0, 0.0005), |c, r| {
            ((c * 11 + r * 3) % 499) as f32
        });
    let server;
    let source = match variant {
        "remote" => {
            server = Some(RangeServer::start(bytes, Duration::ZERO));
            server.as_ref().unwrap().url.clone()
        }
        _ => {
            server = None;
            write_file(dir.path(), "tiny.tif", &bytes)
                .to_string_lossy()
                .into_owned()
        }
    };
    let config = pinned_config(10);

    let mut m = Metrics::new();
    let reader = GeoTiffStreamReader::open(&source).expect("open");
    let info = reader.chunk_info.clone().expect("chunk info");
    let chunks = info.chunk_offsets.len();
    let locations: Vec<ChunkLocation> = (0..chunks)
        .map(|i| ChunkLocation {
            tile_idx: 0,
            chunk_idx: i as u32,
            offset: info.chunk_offsets[i],
            length: info.chunk_bytes[i],
        })
        .collect();
    let defaults = RemotePrefetchConfig::default();
    let expected =
        coalesce_chunk_ranges(&locations, defaults.max_gap_bytes, defaults.max_range_bytes).len();
    let before = server.as_ref().map_or(0, RangeServer::range_requests);

    let probe = Probe::start();
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).expect("streamer");
    let digest = drain_continuous(&mut streamer, false);
    drop(streamer);
    probe.finish(&mut m);
    digest.record(&mut m);
    set(&mut m, "chunks", chunks as f64);
    let per_chunk = m["allocs"] / chunks as f64;
    set(&mut m, "allocs_per_chunk", per_chunk);
    if let Some(server) = &server {
        set(
            &mut m,
            "range_requests",
            (server.range_requests() - before) as f64,
        );
        set(&mut m, "expected_ranges", expected as f64);
        set(&mut m, "bytes_served", server.bytes_served() as f64);
    }
    m
}

fn check_many_tiny_chunks(r: &Results) -> Vec<Check> {
    let mut checks = Vec::new();
    for key in ["cells", "digest"] {
        checks.push(Check::equal(
            &format!("remote output matches local: {key}"),
            metric(r, "remote", key),
            metric(r, "local", key),
        ));
    }
    checks.push(Check::equal(
        "remote strips coalesced: one request per range",
        metric(r, "remote", "range_requests"),
        metric(r, "remote", "expected_ranges"),
    ));
    checks.push(Check::at_most(
        "local: allocations per one-row chunk",
        metric(r, "local", "allocs_per_chunk"),
        LOCAL_ALLOCS_PER_CHUNK,
    ));
    checks
}

/// About 9 measured; a per-chunk regression (one extra buffer per strip) shows.
const LOCAL_ALLOCS_PER_CHUNK: f64 = 16.0;

// ---------------------------------------------------------------------------
// concurrent_streams
// ---------------------------------------------------------------------------

fn concurrent_streams(variant: &str) -> Metrics {
    let streams: usize = variant.parse().expect("stream count");
    let dir = tempfile::tempdir().expect("dir");
    let bytes = tiff_bytes::<colortype::Gray32Float, f32>(
        (1024, 1024),
        16,
        (-100.0, 40.0, 0.0005),
        |c, r| ((c * 5 + r * 9) % 733) as f32,
    );
    let path = Arc::new(write_file(dir.path(), "values.tif", &bytes));
    let config = Arc::new(pinned_config(10));

    let mut m = Metrics::new();
    let probe = Probe::start();
    let handles: Vec<_> = (0..streams)
        .map(|_| {
            let (path, config) = (Arc::clone(&path), Arc::clone(&config));
            thread::spawn(move || {
                let reader = GeoTiffStreamReader::open(&*path).expect("open");
                let mut streamer =
                    MultiScanHorizonStreamer::new(reader, &config).expect("streamer");
                drain_continuous(&mut streamer, false)
            })
        })
        .collect();
    let digests: Vec<Digest> = handles
        .into_iter()
        .map(|h| h.join().expect("stream thread"))
        .collect();
    probe.finish(&mut m);
    assert!(digests.iter().all(|d| *d == digests[0]));
    set(&mut m, "streams", streams as f64);
    digests[0].record(&mut m);
    m
}

fn check_concurrent_streams(r: &Results) -> Vec<Check> {
    vec![
        Check::equal(
            "each stream produces the same output",
            metric(r, "4", "digest"),
            metric(r, "1", "digest"),
        ),
        Check::at_most(
            // Linear, plus 25% for how the streams' peaks happen to line up.
            "4 streams use at most 5× the heap of one",
            metric(r, "4", "peak_live_bytes"),
            5.0 * metric(r, "1", "peak_live_bytes"),
        ),
    ]
}
