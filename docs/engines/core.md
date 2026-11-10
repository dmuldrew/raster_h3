# Core Engine Deep Dive: `raster-h3-core`

[← Engines Overview](README.md) · [← Back to README](../../README.md)

`raster-h3-core` ([`crates/raster-h3-core`](../../crates/raster-h3-core)) is the
engine-neutral library every adapter builds on. It has no dependency on DuckDB,
PostgreSQL, Python or DataFusion. It holds the modules that used to live in the root
`src/` (raster I/O, CRS, aggregator, encoding, Parquet, PMTiles, transcode), plus two
modules added for the multi-engine split:

- [`columnar`](../../crates/raster-h3-core/src/columnar/mod.rs): an Arrow-shaped
  `RecordBatch` model, streaming batch readers, the Arrow C Data / C Stream interface,
  and the `RH3IPC01` / NDJSON serializers.
- [`scalar_ops`](../../crates/raster-h3-core/src/scalar_ops.rs): the H3 scalar helpers
  and parameter parsers that all adapters share.

See [Module Architecture](../module-architecture.md) for a file-by-file map of the
aggregation internals and [Engineering Architecture](../engineering.md) for streaming,
eviction and memory budgets.

```toml
[dependencies]
raster-h3-core = { path = "crates/raster-h3-core" }
```

## Pipeline

The sample-unpacking boundary checks TIFF chunk dimensions, arithmetic overflow, source
length and destination capacity. Invalid or truncated decoded chunks return `InvalidMetadata`
before indexing/copying, including the floating-point predictor path.

```mermaid
flowchart LR
    SRC["resolve_raster_sources(path | glob | list | VRT)"] --> MOSAIC["MosaicReader::open(paths, bbox, crs, overlap_rule)"]
    MOSAIC --> STREAMER["MultiScanHorizonStreamer / MultiCategoricalHorizonStreamer"]
    CFG["MultiResolutionConfig"] --> STREAMER
    STREAMER --> READER["ContinuousBatchReader / CategoricalBatchReader<br/>(Iterator&lt;Item = Result&lt;RecordBatch&gt;&gt;)"]
    READER --> ARROW["ArrowArrayStream (C Stream Interface)"]
    READER --> IPC["write_ipc_stream (RH3IPC01)"]
    READER --> JSONL["write_jsonl_stream (NDJSON)"]
    STREAMER --> PQ["H3ParquetWriter"]
    CFG --> PMT["H3PmtilesTiler"]
```

Every adapter follows the same steps:

1. Build a `MultiResolutionConfig` from its own parameters (`resolutions`, `custom_crs`,
   `custom_nodata`, `apply_scale_offset`, `bbox`, `overlap_rule`, `sampling`, `quantiles`,
   filters, …). See [band calibration and validity](../sql-api-reference.md#band-calibration-and-validity).
2. Open a mosaic and a horizon streamer.
3. Drain completed cells with `drain_completed_into(max, |resolution, record| …)` until
   it returns `Ok(0)` (end of stream) or an error. Errors latch: a failed stream keeps
   returning the error instead of reporting a clean EOF.

### Spill diagnostics

Both streamers report spilling after or during a stream: `spill_run_count()` is the
number of partial sorted runs written to disk, `spill_run_counts()` the same per
configured resolution (in `resolutions` order), and `peak_active_bytes()` the peak
active-map allocation estimate. The budget is split evenly between
resolutions, so a fine resolution can spill while a coarse one stays in memory. Once any
resolution has spilled, the eviction horizon stops advancing: cells of in-memory
resolutions south of it are held until EOF and then emitted from memory, never from runs.

A horizon map holds at least one batch of cells, and a batch is never smaller than one
chunk, so fine resolutions over wide strips or large tiles can need more than the
64 MiB default. See [budget sizing](../engineering.md#1-southernmost-scan-line-horizon-eviction)
for an estimate.

### Transform-failure diagnostics

`transform_failures()` on both streamers and both batch readers counts valid sample points
(pixels, or sub-pixel points when super-sampling) that were dropped because the coordinate
transform failed, returned a non-finite coordinate or a latitude outside [-90, 90], or H3
rejected the point. Pixels outside `bbox` or owned by another mosaic tile are excluded on
purpose and are not counted. A non-zero count means valid data is missing from the output,
usually because of a wrong or out-of-domain source CRS. It is counted in every build, not
only with `stream-profile`, and is also available as `metrics().worker.transform_failures`.

For pipelines that own the streamer (`H3ParquetWriter`, `H3PmtilesTiler`), set
`MultiResolutionConfig::transform_failure_counter` to a shared `Arc<AtomicU64>`; every
stream built from that configuration adds its failures to it.

`MultiResolutionConfig::strict_transforms` (default `false`) turns the count into an error:
the stream fails with `RasterH3Error::TransformFailures(n)` after the first kernel wave that
drops a sample, and later reads return the latched `StreamFailed` error. `n` covers the work
done so far, not the whole raster. Records already emitted before that wave stay emitted,
so a strict consumer must treat the error as fatal to the whole result.

The engines surface the count, and strict mode, as follows:

| Engine | Where the count appears | Strict mode |
| :--- | :--- | :--- |
| DuckDB | `transform_failures` column of the `h3_raster_to_parquet` / `h3_raster_to_pmtiles` summary row. The streaming aggregate functions have no warnings channel and report nothing. | `strict_transforms := true` on every table function |
| PostgreSQL | One `WARNING` at the end of `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate`. | — |
| Python | `RuntimeWarning` from `raster_to_parquet` / `raster_to_pmtiles`, and from `scan_raster_h3` / `RasterH3ArrowStream.iter_batches()` / `.to_dict()` once the scan completes. `to_polars()` / `read_raster_h3()` / `__arrow_c_stream__` hand the stream to Arrow and report nothing. | `strict_transforms=True` on `scan_raster_h3`, `read_raster_h3` and `RasterH3ArrowStream` |
| CLI | Warning on stderr from every subcommand. | — |
| DataFusion | `with_transform_failure_counter(Arc<AtomicU64>)` on either provider. | `with_strict_transforms(true)` |

`custom_crs` (every adapter's CRS override) is resolved by `CrsTransformer::from_epsg_code`
or `from_proj_string`. Besides geographic, Web Mercator, Albers, UTM and polar codes, it
accepts European national grids with Helmert datum shifts (`EPSG:27700`, `28992`,
`31466`–`31469`, `23028`–`23038`) and any PROJ string with `+towgs84`. Grid shifts
(`+nadgrids`, `+datum=NAD27`) are rejected. See [Datum Shifts](../crs-and-projection.md#datum-shifts).

Albers inverse constants are prepared once per walker under an immutable borrow;
public `AlbersConicFast` source fields retain their original struct-literal API.
Standalone inverse calls derive constants from current source fields, not stale caches.
`AlbersConicFast::row_yp` supplies shared row-invariant inverse terms. WGS84, Mercator,
and Albers hoist row terms whenever affine `d == 0`, including horizontal shear.
`stream-profile` retains these optimized paths and counts per-sample projection work
(including cache use, excluding row setup).

Bounds from `CrsTransformer::transform_rect_bounds` are certified only when
`bounds_certified()` is true: analytic for the fast paths and built-in EPSG:3413/3031 polar definitions, a Lipschitz enclosure
for transverse Mercator, a sampled estimate otherwise. Code that rejects data
from bounds (bbox pruning, overlap filtering) must pass them through
`rejection_bounds()`, which returns the whole globe for uncertified bounds.
TM certification follows proj4rs scale precedence (`k0`, then `k`) and fails
open for duplicate parameters or syntax it cannot interpret unambiguously. See
[Bounding boxes and pruning](../crs-and-projection.md#bounding-boxes-and-pruning).

### Profiling and wave dispatch

The streamers' `metrics()` method returns a `StreamProfile`. Dispatch counters are
collected even without `stream-profile`:

| Counter | Meaning |
| :--- | :--- |
| `waves` | Non-empty kernel dispatches, each containing up to `worker_tasks` window jobs (`jobs` counts the individual jobs). |
| `bridged_waves` | Waves sent to a dedicated aggregation pool via a newly spawned helper OS thread because the caller is a worker of another Rayon pool. |
| `serial_waves` | Waves executed serially on the caller: it is already a worker of the dedicated pool, or is a Rayon worker with no dedicated pool. |
| `eviction_key_fallbacks` | New horizon-map cells whose eviction key the serial merge computed itself because the workers had not precomputed it. Normally 0. |

Direct non-Rayon callers dispatch in parallel using the dedicated pool when
configured, otherwise the ambient global pool; neither subtype counter increases.
The subtype counters are disjoint subsets of `waves`. Serial execution and the
helper-thread bridge avoid Rayon work-stealing deadlocks when callers hold locks.
`stream-profile` additionally enables hot-path per-sample worker counters and timers.

Prefer local profiling on stable hardware for performance comparisons. Run the
hotspot harness with:

```bash
cargo run --locked --release -p raster-h3-core --example profile_hotspots -- --quick --json profile.json
```

Omit `--quick` for the full profile. Record the commit SHA, hardware, thread count,
and configuration alongside results, and keep those conditions consistent when
comparing changes. For an optional hosted run, use **Actions → Hotspot Profile →
Run workflow** and select the branch and optional `quick` input. The workflow is
manual-only; hosted timings are informational. Normal CI runs the harness's
ownership-sampling test without running the profiling experiments.

It reports mosaic planning, ownership calls sampled equally over each tile's full
pixel footprint (including non-overlap samples), end-to-end overlap rules, and the
four direct/nested × dedicated/ambient dispatch combinations. Allocation columns
for mosaic opening include file opening **and** planning. Allocator peaks are
approximate extra live bytes, not RSS, and can vary with scheduling. Full-stream
wall-time comparisons include decoding and scheduling; they do not isolate bridge
cost. The bare spawn/join measurement is only an isolated baseline.

### Worker window sizing

Each kernel wave splits decoded chunks into at most `worker_tasks` window jobs,
each aggregating into its own cell maps. Windows are sized so those maps fit a
quarter of `aggregation_budget_bytes` even in the worst case, where every sample
of every pixel lands in a new cell at each resolution:

```text
worker_pixels = (budget / 4 / worker_tasks − fixed map slack)
                / (worker_bytes_per_sample × resolutions × sample points)
```

clamped to 1–16,384 pixels. The per-sample cost comes from the kernel's
`HorizonStreamKernel::worker_bytes_per_sample`:

| Kernel | Bytes per sample and resolution |
| :--- | :--- |
| Continuous, no quantiles | `inline_worker_bytes_per_sample::<H3Accumulator>()` (308 B) |
| Continuous with quantiles, categorical | 1 KiB (covers sketch and class-count state) |
| Custom kernels (default) | `max(1 KiB, 8 × size_of::<Accumulator>())` |

`inline_worker_bytes_per_sample::<A>()` is stated in the terms the budget check
measures: one table entry charged at two buckets per unit of capacity, with
reused map capacity under twice the entry count, plus an eviction key in a
`Vec` that is likewise under twice full. A custom kernel whose accumulator owns
no heap memory can return it; a kernel that owns heap state per cell must
return at least that state's worst case on top, or worker maps can exceed the
budget. Small windows are expensive with dense sampling: each wave wakes and
parks the Rayon workers, so with 5-point sampling the old flat 1 KiB allowance
(about 409-pixel windows at the default budget) spent roughly half of all CPU
time in the kernel scheduler once the per-sample work was fast. When memory
allows, raising `aggregation_budget_bytes` widens windows further.

### Worker scaling

The `worker_scaling` example measures how wall time splits between the parallel
kernel waves and the serial horizon merge as `aggregation_workers` grows:

```bash
cargo run --locked --release -p raster-h3-core --example worker_scaling -- --workers 1,2,4,8 --json scaling.json
```

It streams synthetic rasters through six workloads: resolution 9, coarse (7), fine
(11), three resolutions, quantiles and categorical. For each worker count it reports
wall time, throughput, `kernel_wall_ns`, `merge_ns` and the merge's share of wall
time, plus parallel-stage efficiency, decode wait and spill time. `--quick` uses
1024² rasters and one run; `--side` and `--reps` override the size and repetitions.
Every worker count must produce the same cells and exact per-cell statistics
(count, sum, min, max, class counts; not quantiles), or the run aborts. These stage timers are always on, so a normal release build suffices.
Worker counts above the machine's cores oversubscribe it; the report prints the
available parallelism. Results and the resulting decision are recorded in
[Worker Scaling](../worker-scaling.md).

### Remote COG prefetching

`raster::remote_prefetch::RemoteChunkPrefetchQueue` fetches HTTP/S3 ranges ahead
of decoding for a single GeoTIFF or a mosaic. Its Rust configuration is separate
from aggregation and decoded-buffer budgets (it is not a total-process RAM cap):

```rust
use raster_h3_core::raster::remote_prefetch::{RemoteChunkPrefetchQueue, RemotePrefetchConfig};

let config = RemotePrefetchConfig {
    max_buffered_bytes: 16 * 1024 * 1024, // default: 32 MiB
    ..Default::default()
};
// reader is an open remote GeoTiffStreamReader; chunk_indices selects its strips/tiles.
match RemoteChunkPrefetchQueue::spawn_single(&reader, &chunk_indices, Some(config)) {
    Some(queue) => {
        if let Some(payload) = queue.get_chunk_payload(0, chunk_indices[0])? {
            let compressed_bytes: &[u8] = &payload;
            // Decode compressed_bytes here, then drop payload before requesting more chunks.
        }
    }
    // Nothing to prefetch: read chunks from `reader` directly.
    None => {}
}
```

`spawn_single` returns `None` for a local reader, a reader without chunk offsets,
or a remote file whose server ignored `Range` and returned the whole body when the
reader opened: that file is already in memory, and range requests to such a server
would fail. `spawn_mosaic` likewise returns `None` when every tile is local or
already in memory, and skips in-memory tiles in a mixed mosaic.

`max_buffered_bytes` reserves each coalesced response's full byte length **before**
the HTTP request and counts requests in flight as well as retained responses.
If a range alone exceeds the budget (including a zero-byte budget), it is admitted
only when no other response is held. `queue_capacity` additionally throttles
untaken chunks (default 64, minimum 16); a coalesced response may exceed this count.
Defaults are 8 HTTP workers, a 32 KiB coalescing gap and a 2 MiB range target.

Blocking `get_chunk_payload` requests are prioritized over speculative prefetches
and may arrive in any order, including from concurrent decode workers. When a
requested range cannot fit, responses with no waiting consumers may be evicted.
An evicted, untaken chunk is fetched again on a later blocking request; chunks
already taken are not republished. `try_get_chunk_payload` only takes ready chunks
and does not trigger a fetch. In-order scans normally avoid refetching.

Both methods now return `PrefetchedChunk` instead of `Arc<Vec<u8>>`. It dereferences
to `[u8]`, implements `AsRef<[u8]>`, and clones cheaply without copying the response.
Every slice or clone keeps the **entire coalesced response** alive and charged until
the last reference is dropped. `response_len()` reports that full length, whereas
`len()` is the chunk's slice length. Eviction cannot release bytes held by callers:
drop payloads and their clones promptly after decoding, before blocking for another
range that needs the same budget. Handles remain valid after the queue is dropped.
`held_bytes()` / `peak_held_bytes()` report current / peak reserved response bytes.

All engine adapters use this core prefetcher; `RemotePrefetchConfig` is a Rust
queue-level API, not an additional SQL, Python or CLI option.

The built-in HTTP transport pins the first successful response's **strong ETag** per URL.
Subsequent range requests send `If-Match` and request `Accept-Encoding: identity`.
HTTP 412 is a terminal read error, not a transient retry. A successful range response
with a changed, missing, or downgraded-to-weak ETag also fails before its bytes enter
caches or decoders, including when a same-size replacement slips past range validation.
Transport clones, parallel reads, and retries share the original validator; they never
silently adopt a new version. Previously cached bytes remain from the original response.
Changing `HttpTransport.url` selects that URL's independent validator; returning to
a previously read URL retains its original pin, including across clones.

Servers supplying only weak ETags or no ETag remain supported with the existing size
and range checks, but those checks **cannot guarantee snapshot consistency**. Use an
immutable/versioned URL for those sources. Custom `RemoteTransport` implementations
are responsible for their own snapshot guarantees. See [HTTP conditional requests](https://www.rfc-editor.org/rfc/rfc9110.html#section-13.1.1).

A server that ignores `Range` returns the whole file, which is held in memory. The
built-in transport accepts such a body only up to 1 GiB
(`http_range::DEFAULT_MAX_FULL_BODY_BYTES`) and fails at open beyond that, before
reading past the limit; a larger announced `Content-Length` fails without reading.
Raise the limit with `RASTER_H3_MAX_FULL_BODY_BYTES` (bytes; invalid or zero values
keep the default) or `HttpTransport::with_max_full_body_bytes`.

Transient failures are retried up to four times, each after a random delay between
half and the full exponential backoff, so concurrent workers do not retry in
lockstep; a numeric `Retry-After` header of up to 30 s lengthens the delay (the
HTTP-date form is ignored). Retried failures include request errors, a connection
error while reading a response body or a body shorter than announced
(`Content-Range` or `Content-Length`). Header, ETag and over-length failures are not
retried. A truncated chunked response without `Content-Length` cannot be detected
and is accepted.

### Custom byte sources

By default local paths are memory-mapped and remote URLs use the built-in HTTP/S3
client (`http(s)://` and `s3://`; any other `scheme://` path counts as remote but is
rejected by that client). An engine can supply its own byte source instead:

- Implement `raster::http_range::RemoteTransport` (`fetch_range(start, end, expected_total)`
  returning exactly those bytes, and `probe(initial_len)` reporting the total size). It must
  be safe to call from several threads, because prefetch and decode workers share it.
- Wrap a closure that opens one in `RemoteSourceFactory::new(|path| ...)`, building the
  source with `RemoteHttpSource::from_transport(path, Box::new(transport))`.
- Set `MultiResolutionConfig::remote_source_factory`, or call
  `GeoTiffStreamReader::open_with_remote_source_factory` /
  `MosaicReader::open_with_remote_source_factory`.

When a factory is set it opens **every** path, local ones included, and the prefetcher,
cache and coalescing work on top of it unchanged. The DuckDB 2.0 build uses this to read
through DuckDB's file system (`SET raster_h3_use_duckdb_fs = true`).

### Mosaic decoder cache

`PrefetchedMosaicReader` keeps at most `MAX_OPEN_DECODERS_PER_WORKER` (16) TIFF
decoders per worker, dropping an evicted decoder before opening its replacement.
It evicts the cached tile whose next chunk is furthest away in the global job
order, preferring tiles with no remaining chunks. Unlike LRU, this avoids a miss
on every chunk when a latitude band repeatedly visits more than 16 tiles.
Each worker advances past predicted uses already assigned to other workers;
the predictions are exact for a single worker, not worker-specific scheduling.
Decoding and result delivery remain in their original order, so queue and byte
budget backpressure are unchanged.

For mosaics with more than 16 tiles, a shared index adds one `usize` per chunk
reference (8 bytes on 64-bit platforms), with a temporary `usize` per tile while
building it. Smaller mosaics do not allocate this index. Decoder state and this
metadata are separate from the decoded-buffer and aggregation budgets.

### Overlap ownership cost

With `OverlapRule::Cutline`, each sample in an overlapping chunk is compared
against every other tile's centre. Centre unit vectors are cached on each
`TileDescriptor`, so a comparison costs a few multiplies (about 1 ns), and the
exact containment test, which may inverse-project, runs only for tiles whose
centre is nearer. `First` checks containment against lower-index tiles. Both
remain linear in tile count per overlapping sample; only chunks flagged
`has_overlap` pay it. The `profile_hotspots` example measures this scaling.

### Calling from a Rayon pool

A streamer driven from a worker of another Rayon pool cannot run kernel waves
on that pool without risking deadlock (see `aggregation_workers`):

- `aggregation_workers: None` runs every wave **serially** on the calling
  thread, about 2.5× slower than a direct call with 4 workers in the
  `profile_hotspots` example, at every budget.
- `aggregation_workers: Some(n)` hands each wave to the dedicated pool through
  a short-lived helper thread, roughly one thread spawn (≈40 µs) per wave. That
  is a few percent at budgets of 16 MiB and above, but grows as small
  `aggregation_budget_bytes` shrink windows (3.4× slower at 256 KiB).

So when calling from inside a Rayon pool, set `aggregation_workers` and keep the
aggregation budget at its default or larger. Calls from ordinary threads,
including DuckDB's scan threads, are unaffected.

### Partitioned scans

A mosaic can be split across independent workers (for example Spark tasks) that each
read a disjoint set of chunks, with results identical to a single scan up to
floating-point summation order:

1. **Plan** (driver, metadata only): `plan_partitions(&mosaic, target_chunks)` cuts each
   tile's chunk grid into rectangles of about `target_chunks` chunks, packing small tiles
   together. Each `ScanPartition` serializes with `to_json` / `from_json` and carries a
   fingerprint of the mosaic layout (dimensions, chunk layouts, geotransforms, overlap
   rule and the bbox-pruned chunk list, but not file paths).
2. **Scan** (each worker): reopen the mosaic with the same sources, bbox, CRS override and
   overlap rule (a different mosaic is refused), set `MultiResolutionConfig::partition`
   and drain with `drain_partitioned_into(max, on_record, on_partial)`. Only the
   partition's chunks are decoded; the rest of the pipeline is the single-node one.
3. **Merge**: a finalized cell that no chunk outside the partition can reach is
   *interior* and is emitted as a normal record. Any other cell is *boundary* and is
   emitted as an unfiltered `PartialCell`. Ship partials with `encode_partial_state` (the
   lossless spill format with a versioned `RH3P` header; version 2 adds the continuous sum of squared weights, and states from older releases are rejected, so every worker must run the same release), merge them with
   `PartialMerger::push_encoded` (built by the streamer's `partial_merger(&config)`), and
   `finish()` applies the filters to the merged states.

Classification is conservative: a cell is interior only when its padded lat/lon box misses
the certified bounds of every outside chunk (`rejection_bounds`). Uncertified CRS bounds
and cells near a pole or across the antimeridian count as boundary, which is always
correct and only adds merge work. The test runs once per finalized cell, and an
ancestor-level cache settles most interior cells without computing their boundary.
Boundary cells are a perimeter effect and shrink as partitions grow. `partition_cell_counts()`
reports the split.

Partitioning adds no measurable CPU. The `partition_overhead` example scans a 6144 x 6144 px
f32 raster (576 tiles of 256 px) once, then as all partitions plus the merge, and compares
total process CPU time (median of 5 interleaved runs, 4 vCPUs):

| resolutions | 4 partitions (144 tiles each) | 16 partitions (36 tiles each) |
| :--- | :--- | :--- |
| 8 | 0.99x CPU, 4.6% boundary cells | 0.98x, 13.2% |
| 10 | 0.99x, 0.6% | 1.01x, 1.9% |
| 7, 8, 9 | 0.97x, 2.3% | 1.02x, 6.8% |

Ratios within a few percent of 1.0 are run-to-run noise on that machine. The merge takes
milliseconds (under 3 ms for about 35,000 boundary cells at resolution 10).

```bash
cargo run --release -p raster-h3-core --example partition_overhead -- [--quick] [--json out.json]
```

Child compaction cannot be combined with a partition (sibling groups may span
partitions), and record-only reads (`drain_completed_into`, `fetch_next_batch`) are
refused so partials cannot be dropped silently.

## Columnar Model

| Type | Role |
| :--- | :--- |
| `ArrowDataType` | `UInt64`, `Int64`, `UInt8`, `Float64`, `Boolean`, `Utf8`, `Binary`, with their Arrow C format strings (`L`, `l`, `C`, `g`, `b`, `u`, `z`). |
| `ColumnField`, `ColumnSchema` | Named, typed, nullable fields. `ColumnSchema::continuous(&quantiles)`, `categorical_wide()` and `categorical_long()` are the canonical schemas (listed in the [overview](README.md#output-schemas)). `project(&[usize])` and `index_of(name)` support projection. |
| `ColumnArray` | One typed column (`values` + optional `validity: Vec<bool>`), with `len`, `slice`, `null_count`. |
| `RecordBatch` | `schema` + `columns` + `num_rows`. `try_new` validates column count, types and lengths. `from_continuous_records` / `from_categorical_wide_records` build batches from streamer records, materialising only the projected columns (so `h3_hex` and `wkb` are not formatted when not selected). |

### Batch readers

```rust
use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::columnar::{CategoricalBatchReader, CategoricalFormat, ContinuousBatchReader};
use raster_h3_core::raster::mosaic::resolve_raster_sources;

let paths = resolve_raster_sources("dem.tif")?;
let mut config = MultiResolutionConfig::new(vec![7, 8]);
config.custom_crs = Some("EPSG:4326".into());

// projection = Some(vec![0, 2]) -> only h3_index and mean
let reader = ContinuousBatchReader::open(&paths, &config, Some(vec![0, 2]), 2048, Some(10_000))?;
for batch in reader {
    let batch = batch?;
    // batch.columns[0], batch.columns[1] ...
}

let cat = CategoricalBatchReader::open(&paths, &config, CategoricalFormat::Long, None, 2048, None)?;
```

- `batch_size` is clamped to at least 1 and counts output rows. Continuous and
  categorical wide readers emit one row per cell, so each `next()` drains up to
  `min(batch_size, remaining_limit)` completed cells.
- Categorical long readers emit one row per `(cell, category)`. Each `next()` fills
  up to `min(batch_size, remaining_limit)` rows by draining one completed cell at a
  time, resuming any partially emitted cell before draining another. Categories
  within a cell are sorted, contiguous, and may span batches; batches are full
  unless EOF or the limit is reached. Cells with `total_count <= 0` are skipped.
- `limit` counts output rows: cells for continuous/wide output, unnested category
  rows for long output. A long-format limit may stop mid-cell without expanding
  that cell's remaining rows or draining the next cell.
- `ContinuousBatchReader::open` builds per-cell quantile sketches only when the
  projection selects a quantile column (full-schema index 10 and up), or when
  `projection` is `None`. The schema still lists every configured quantile column;
  `tracks_quantiles()` reports whether sketches are being built, and the public
  `columnar::quantiles_projected(projection, n)` applies the same rule. DuckDB applies
  it to its own projection (which includes columns referenced by `WHERE`).
- `from_streamer(...)` accepts a streamer you built yourself (for example with custom
  sampling, spectral formulas or remapping). Its sketch tracking is whatever that
  streamer's config requested.

#### Public long-format cursor

`raster_h3_core::aggregator::multi_horizon::{CategoricalLongCursor, CategoricalLongRow}`
provides the incremental expansion shared by the core and DuckDB long readers:

- `CategoricalLongCursor::new(with_entropy)` controls per-cell entropy computation.
- `load(&MultiCategoricalRecord)` replaces any unconsumed rows with one cell's
  categories, sorted by category, reusing the entry buffer across cells.
- `remaining()` reports the current cell's unconsumed category rows.
- `emit_into(max_rows, callback)` emits at most that many `CategoricalLongRow`s and
  returns the number emitted; subsequent calls resume the same cell.

For a cell with 300 categories and `batch_size = 32`, the reader drains that one
cell and returns its first 32 rows, retaining the remaining categories for later
batches. Pending expansion state is bounded by the largest single cell's category
count, separate from the output batch and the streamer's aggregation budget.
The core and DuckDB long readers compute entropy only when `shannon_entropy` or
`entropy` is projected (full-schema indices 7 and 8); a cursor constructed with
`with_entropy = false` puts `NaN` in `CategoricalLongRow::entropy`.

#### Batched quantile evaluation

`QuantileSketch::quantiles_batch(&targets, min, max)` answers several quantiles with
one sorted sweep of a sketch's bins, returning exactly what `quantile(q, min, max)`
returns for each target in input order. Targets may be unsorted or repeated; NaN
targets yield NaN, `q <= 0` yields `min` and `q >= 1` yields `max`.

Each nonzero normal value goes to a logarithmic bucket, which keeps the sketch's 1%
relative error at any magnitude, including values far below 1. Only zero and
subnormal magnitudes (below `quantiles::ZERO_THRESHOLD`, which is `f64::MIN_POSITIVE`)
are counted in the exact zero bucket. Data spanning many orders of magnitude keeps
about 115 buckets per decade per sign. When `min == max` exactly, every quantile is
`min`. NaN, infinite and non-positive weights are ignored by `update`.

`raster_h3_core::aggregator::multi_horizon::QuantilePlan` builds on it for
`QuantileTarget` lists: `QuantilePlan::new(&targets)` deduplicates the probabilities
(IQR shares p25/p75 with any explicit targets), and
`evaluate_into(&H3Accumulator, &mut Vec<f64>)` writes one value per target, equal to
`H3Accumulator::quantile` / `iqr` (NaN when the accumulator has no sketch). The core
continuous reader and the DuckDB scan evaluate all projected quantile columns of a
cell this way. Plans with just one distinct probability use the scalar sketch
query directly (including repeated projections of that probability), avoiding the
batch query's result and rank-vector allocations. Empty plans skip evaluation.
The `single_quantile` Criterion group compares scalar, batch and plan queries for
p50/p90 on small positive and larger mixed-sign sketches; these are query-only
microbenchmarks, not whole-raster throughput measurements.

### Arrow C Data / C Stream interface

[`columnar/arrow_c.rs`](../../crates/raster-h3-core/src/columnar/arrow_c.rs) implements the
`#[repr(C)]` `ArrowSchema`, `ArrowArray` and `ArrowArrayStream` structs from the Arrow
[C Data](https://arrow.apache.org/docs/format/CDataInterface.html) and
[C Stream](https://arrow.apache.org/docs/format/CStreamInterface.html) specifications,
with `release` callbacks that free Rust-owned buffers.

- `ArrowSchema::from_column_schema(&schema)` exports a struct-typed schema.
- `ArrowArray::from_record_batch(batch)` moves a batch into C-owned buffers (validity is
  converted to Arrow bitmaps; UTF-8 and binary use 32-bit offsets).
- `ArrowArrayStream::from_iterator(schema, iter)` wraps any
  `Iterator<Item = Result<RecordBatch>> + Send + 'static`. Batches are produced lazily on
  each `get_next`; an iterator error is surfaced through `get_last_error`.
- `unsafe stream.collect_batches()` imports a stream back into Rust (used in tests and by
  in-process consumers).

This is the interface the Python bindings hand to PyArrow/Polars as PyCapsules and the
DataFusion adapter returns from `ExecutionPlan::execute_arrow_c_stream`.

### `RH3IPC01` stream and NDJSON

[`columnar/ipc.rs`](../../crates/raster-h3-core/src/columnar/ipc.rs) provides a small,
dependency-free binary format for piping batches between processes. **It is not the Apache
Arrow IPC format**; read it with `read_ipc_stream` (Rust) or a reader written against the
layout below.

```text
"RH3IPC01"                      8-byte magic
u32 n_fields
  repeat n_fields: u32 name_len, name bytes, u8 type_tag, u8 nullable
repeat per non-empty batch:
  u8 1                          batch marker
  u32 num_rows
  repeat per column:
    u8 has_validity [u32 len, len × u8 (0|1)]
    values: u64/i64/f64 little-endian, u8 raw, bool as u8,
            utf8/binary as (u32 len, bytes) per row
u8 0                            end of stream
```

Type tags: `1` UInt64, `2` Int64, `3` UInt8, `4` Float64, `5` Boolean, `6` Utf8, `7` Binary.

`write_jsonl_stream` writes one JSON object per row (`JSONEachRow`). Null and non-finite
floats become `null`; binary columns (`wkb`) are lowercase hex strings.

## Shared Helpers: `scalar_ops`

| Function | Purpose |
| :--- | :--- |
| `parse_resolutions_str(s)` | Lenient: splits on commas/whitespace, drops invalid entries and values > 15, sorts, dedups. |
| `try_parse_resolutions_str(s)` | Strict: `None` if any entry is invalid or the list is empty (used by the CLI). |
| `parse_bbox_str(s)` | `"min_lon,min_lat,max_lon,max_lat"` → `[f64; 4]`; `None` unless exactly four numbers. |
| `estimate_raster_cardinality(paths, resolutions)` | Planner estimate of emitted cells from the first raster's extent and `H3_AREA_M2`. |
| `h3_to_string`, `string_to_h3`, `try_string_to_h3`, `h3_to_lat`, `h3_to_lng`, `h3_get_resolution`, `h3_is_valid_u64`, `h3_is_valid_str`, `h3_to_wkb_u64`, `h3_to_wkb_str`, `h3_cell_to_parent_u64`, `h3_cell_to_parent_str` | The scalar helpers every engine exposes. Invalid-input behaviour is listed in the [overview](README.md#scalar-helpers). |
| `h3_polygon_to_cells_wkb(wkb, res, mode) -> Result<Vec<u64>, PolygonToCellsError>` | Polyfills a WKB `POLYGON`/`MULTIPOLYGON` with h3o (`h3o` `geo` feature) and returns the cells sorted and deduplicated. `PolygonToCellsError` is `InvalidResolution`, `InvalidGeometry` or `TooManyCells` (over `H3_POLYGON_TO_CELLS_MAX_CELLS`). |
| `parse_containment_mode(s)`, `H3_CONTAINMENT_MODES`, `ContainmentMode` | Mode names → h3o `ContainmentMode` (see the [mode table](../sql-api-reference.md#containment-modes)). |
| `encoding::cell_to_wkb(cell, &mut WkbBuf)`, `h3_index_to_wkb` | Cell geometry into a `WKB_BUF_LEN`-byte stack buffer: `Polygon`, or `MultiPolygon` cut at ±180°, or a polar cap ([cell geometry](../sql-api-reference.md#cell-geometry)). |
| `encoding::wkb_to_multipolygon(bytes)` | WKB/EWKB `POLYGON`/`MULTIPOLYGON` reader (either byte order; Z/M skipped) → `geo_types::MultiPolygon`. |

## File Exporters

| API | Output |
| :--- | :--- |
| `H3ParquetWriter::write_continuous_streamer_to_parquet(streamer, path, ParquetExportConfig)` / `write_categorical_streamer_to_parquet` | Parquet or OGC GeoParquet 1.1 (`config.geoparquet`); the file is published only after the stream completes successfully. |
| `H3PmtilesTiler::process_raster_source_to_pmtiles(source, path, config)` / `process_categorical_source_to_pmtiles` | PMTiles v3 archive with one zoom range per requested resolution. See [PMTiles & Multi-Resolution](../pmtiles.md). |
| `transcode::process_parquet_to_pmtiles` | Pre-aggregated H3 Parquet → PMTiles. An explicit `h3_column_name` that isn't in the schema returns `Err` listing available columns; `None` auto-detects. |
| `H3PmtilesTiler::export_h3_features(features, path)` | PMTiles v3 from `H3Feature`s in any order, with bounded memory (external sort by latitude, 256 MiB buffer). Invalid H3 indexes are counted in `PmtilesExportSummary::invalid_features_dropped`. |
| `pmtiles::ExternalFeatureExport::new(budget_bytes, spill_dir)` → `push(feature)` → `finish(path)` / `finish_into(writer)` | The exporter behind `export_h3_features`, with a custom buffer budget, spill directory (created only when a run is spilled) and output; `finish_into` streams the archive to any `std::io::Write`, such as a database file system. |
| `PmtilesWriter::new_in(..., spill_dir)` / `finish_into(writer)` | Low-level archive writer: tile spill file in a chosen directory, and the finished archive streamed to any writer (`finish(path)` writes atomically via a temporary file next to `path`). |

## Errors

All fallible APIs return `raster_h3_core::error::Result<T>` with `RasterH3Error`
(`StreamFailed`, `Io`, `Tiff`, `InvalidMetadata`, `CrsNotDetected`, `CrsProjectionFailed`,
`CrsError`, `UnsupportedEpsg`, `Proj`, `H3Error`, `InvalidParameter`, `DuckDbError`).
`safe_panic_payload_to_string` turns a caught panic payload into a message without letting
a secondary panic escape; every adapter uses it at its FFI boundary.

## Cargo Features

| Feature | Effect |
| :--- | :--- |
| `stream-profile` | Enables per-sample worker instrumentation. Stream-level `StreamProfile` counters (including `waves`, `bridged_waves`, `serial_waves` and memory peaks) are always available. The root crate forwards the feature to core and DuckDB. |
| `test-hooks` | Exposes test-only synchronisation hooks (for example `OrderedPrefetchQueue::wait_for_blocked_job`) to other workspace crates' tests. Not for release builds. |
