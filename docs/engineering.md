# Core Engineering Architecture

[← Back to README](../README.md)

This document details the architectural principles that enable `raster_h3` to aggregate multi-gigabyte rasters into H3 hexagonal grids in parallel across CPU cores, with configurable aggregation memory budgets and disk spilling.

> [!NOTE]
> Performance benchmarks, timing metrics, and micro-architectural numbers cited in this document reflect native release builds evaluated on an Apple M-series workstation (8 performance cores, 16 GB Unified Memory) and modern x86_64 processors with AVX2/SSE2 support.

`raster_h3`'s throughput and memory profile come from seven principles:

| # | Engineering Pillar | Description & Impact |
| :---: | :--- | :--- |
| 1 | **Southernmost Scan-Line Horizon Eviction** | Cells are sealed only when their certified south bound exceeds every remaining chunk’s north bound, bounding aggregation memory. |
| 2 | **Sequential H3 Spans & In-Register Run Accumulation** | Every pixel is indexed exactly; contiguous equal-cell pixels update running statistics in CPU registers, eliminating ~98% of hash table updates. |
| 3 | **ROI Bounding Box Chunk Pruning** | Skips non-intersecting raster chunks upfront before reading or decompressing data. |
| 4 | **Work-Stealing Parallelism** | Rayon processes chunk windows in parallel in every engine, each into its own cell maps, then merges them into the horizon. DuckDB additionally emits rows from every scan thread. |
| 5 | **Bounded Prefetch & Decode Pipeline** | `memmap2` zero-copy reads, parallel decompression into a single-hop in-order ring buffer, and a lock-free buffer pool, all under `decode_budget_bytes`. |
| 6 | **Coalesced Cloud Range Reads** | Adjacent COG/S3 tile byte ranges are merged into single HTTP requests and prefetched ahead of decompression. |
| 7 | **Exact CRS Transformation with Analytical Fast Paths** | Pixel centers and subpixel samples are transformed at their actual locations; identity and analytical Web Mercator/Albers paths avoid Proj4 and certify the chunk bounds that pruning and eviction depend on. |

Pixel coordinate handling, mosaic overlap rules, and the GeoParquet exporter are covered under [Correctness and Output Design](#correctness-and-output-design).

## 1. Southernmost Scan-Line Horizon Eviction
A cell is complete only when its certified south-latitude bound exceeds the
maximum certified north-latitude bound of all remaining chunks. This supports
rotated grids and curved cell edges without assuming scanline order alone is
sufficient. Unknown Proj4 bounds and Albers chunks that may contain the cone apex
disable early eviction until the remaining suffix is certifiable or EOF.

The aggregation budget defaults to 64 MiB. Disk spilling limits active
accumulators. Pending compaction groups are accounted in bytes and can be
limited with the opt-in `compaction_budget_bytes`: past it, the groups nearest
completion are emitted as individual children instead of waiting for missing
siblings. That never drops or duplicates a sample, but pending size depends on
chunk batch timing, so which groups compact can vary between runs. Without the
limit, compaction output is deterministic and its memory is unbounded. Decoded chunks have a
separate budget (`decode_budget_bytes`, default 256 MiB). Output and runtime
overhead also use RAM.

## 2. Sequential H3 Spans & In-Register Run Accumulation
Every candidate pixel center is indexed directly. Consecutive equal-cell pixels
share accumulation work. The first differing cell is reused by the next run;
there is no width prediction, geometric jump, or binary search.

Most pixels reside in the interior of an H3 cell. As long as contiguous pixels share the same cell index, running statistics are updated directly in CPU registers without memory access:
- **Zero Memory Access**: Running statistics (Welford running mean, variance accumulator $M_2$, weighted count, sum, minimum, and maximum) remain in CPU registers during contiguous interior runs.
- **Batched Horizon Updates**: Only when a cell boundary transition is detected does a single batched flush transfer accumulated weights to the active horizon `FxHashMap<u64, CellAccumulator>`, eliminating ~98% of hash map hashing, bucket lookups, and memory barrier synchronization.

## 3. ROI Bounding Box Chunk Pruning
Queries specifying a spatial Region of Interest (ROI) bounding box (`min_lat`, `min_lon`, `max_lat`, `max_lon`) prune non-intersecting chunks upfront:
- **Pre-Decompression Pruning**: `chunk_intersects_bbox` evaluates the projected spatial extent of each strip or tile against the target bounding box before initiating decompression.
- **Zero I/O Overhead**: Chunks falling entirely outside the target ROI are skipped immediately—zero bytes are transferred from disk/network, zero decompression threads are spawned, and zero buffer memory is allocated.

## 4. Work-Stealing Parallelism
Every engine (DuckDB, DataFusion, PostgreSQL, Python/Polars, CLI) drives the same multi-resolution controller, so aggregation parallelism is shared:
- **Parallel Kernel Windows**: Decoded chunks are grouped into waves of worker windows. Rayon processes the windows of a wave in parallel (`par_iter`), each into its own cell maps, so pixel accumulation takes no shared lock. The worker count defaults to Rayon's thread count capped at 8, or `aggregation_workers` (1–64), and is further limited by the aggregation budget.
- **Horizon Merge**: After each wave, the worker maps are merged into the per-resolution horizon maps, where eviction and spilling happen. Worker map bytes count against the aggregation budget.
- **Deadlock-Safe Execution**: A wave started from inside a Rayon worker either runs on the dedicated pool through a helper OS thread, or runs serially, so a joining worker never steals a task that needs a lock its caller holds.
- **DuckDB `init_local`**: DuckDB additionally registers a per-thread local state for its scan callbacks. Its threads pull completed records from a shared record queue and write them to output vectors in parallel; aggregation itself runs in the kernel windows above.
- **Cardinality Estimate**: `estimate_raster_cardinality()` gives DuckDB's optimizer an approximate output row count from the raster extent and H3 cell area at each requested resolution.

## 5. Bounded Prefetch & Decode Pipeline

### Zero-Copy `memmap2` Reads
Local GeoTIFF files are accessed via virtual memory mapping rather than standard read syscalls:
- **Userspace Virtual Memory**: `MmapFile` maps files into userspace virtual memory with `memmap2`. Chunk byte slices (`&[u8]`) are referenced directly from the memory map without intermediate kernel-to-userspace copying.
- **Asynchronous Decompression**: Multi-threaded decompression workers read directly from mapped pages in parallel, decoupling disk I/O and page cache faults from pixel aggregation.

### Single-Hop Bounded In-Order Prefetcher (OrderedPrefetchQueue<T>)
Traditional background prefetchers often suffer from thread thrashing: either unbounded queues that risk out-of-memory (OOM) bloat, or intermediate "collector" threads that copy data through multiple OS synchronization channels:
- **Direct Worker-to-Consumer Deposit**: `OrderedPrefetchQueue<T>` connects decompression workers directly to the aggregator through a fixed-capacity ring buffer indexed by `job_id % capacity`.
- **Single-Hop Thread Architecture**: Workers calculate and decompress chunks concurrently, depositing their result directly into their assigned ring buffer slot without intermediate collector threads. The aggregator drains contiguous, sequence-ordered chunks in bulk using `drain_into()`, using one queue guard per batch; condition-variable waits release and reacquire the mutex.
- **Strict Backpressure & Deadlock-Free Draining**: If background workers outpace the aggregator by more than `capacity` chunks, they block on a condition variable (`not_full`) until the consumer drains slots. When draining batches that exceed ring capacity, `drain_into()` publishes freed slots before sleeping on incomplete batches, avoiding a circular wait between producers and the consumer. Consumers pull ready chunks via `next_chunk_batch(max_batch)` or bulk batch drains.


#### Downstream backpressure and memory limits

`ChunkWriter` bounds writes to DuckDB's output vectors; it does not throttle or schedule decompression. Continuous and wide categorical scan callbacks pull from `ConcurrentRecordQueue::pop_or_refill()` before writing. A refill requests at most four vector-sized batches from the streamer, returns one, and retains at most three. With no further scan calls, no further refills occur. The prefetch ring then fills and each decompression worker eventually blocks in `push()`, after finishing its current decode. An already-running scan may finish its current refill before stalling.

For a single prefetcher, let **C** be ring capacity, **W** decoder workers, **B** the consumer's chunk batch size, and **P** idle-pool capacity. Decoded buffer ownership is bounded by **C + W + B + P** buffers along this path (including a worker's completed buffer waiting to be deposited). The streamer's mosaic prefetcher also charges every one of those buffers against `decode_budget_bytes`: a worker reserves its chunk's expected size before decoding and settles to the actual allocation afterwards; idle buffers stay charged until reused or evicted for a waiting reservation. Reservations are granted in job order, so the chunk the ordered ring is waiting for never waits behind later chunks, and the consumer returns its whole batch before draining again. A chunk larger than the budget is admitted only when no other decoded buffer is held, so the bound is **max(budget, largest chunk)**, excluding allocator overhead and decoder scratch storage. `PrefetchedChunkReader` (single file, not used by the streamer) keeps only the count bound.

Whole-query memory also includes raster metadata and job lists, compressed remote payloads, decoder scratch buffers, aggregation maps, compaction state (bounded only when `compaction_budget_bytes` is set), output records, and DuckDB execution state. `OutputBuffer::with_capacity(2048)` reserves initial space; it is not a hard limit. Horizon eviction and EOF flushing can emit more records than the requested row count. Long categorical output additionally expands each cell into one row per category and can overshoot its refill threshold. Therefore these queue bounds do **not** establish a fixed whole-query RAM limit or a universal zero-churn guarantee.

Regression coverage includes an explicit producer-wait handshake for oversized batch drains and a 15,840-item test connecting the real record and prefetch queues. The latter pauses record consumption, observes the producer blocked at the expected capacity boundary, and checks ordered completion after resuming. It uses one synthetic record per chunk; it is not a live DuckDB/TIFF memory benchmark.

### Bounded Lock-Free Buffer Recycling Pool (DecodingBufferPool)
High-resolution continental datasets (such as CONUS 30m) require decompressing tens of thousands of tiles (e.g. 15,840+ chunks). Continuously allocating, reallocating, and freeing multi-megabyte decompression buffers causes heavy memory fragmentation, allocator lock contention, and kernel `brk`/`mmap` syscall overhead:
- **Bounded Lock-Free Recycling**: `PrefetchedChunkReader` and `PrefetchedMosaicReader` use `DecodingBufferPool` (wrapping `crossbeam_deque::Injector<DecodingResult>` with an atomic retention counter) as a concurrent, lock-free buffer recycling pool.
- **Contention-Resilient Acquisition**: When a background decompression thread prepares to decode a chunk, it attempts to acquire an existing buffer from the pool (`buffer_pool.pop()`). If concurrent steals collide (`crossbeam_deque::Steal::Retry`), the worker spins briefly rather than falsely falling back to fresh memory allocation. Only if the pool is genuinely empty (`Steal::Empty`) does it allocate fresh storage.
- **Strict Retention Bound & Minimized Allocation Churn**: Once the downstream consumer finishes processing a chunk batch, allocated buffers are returned to the pool via `recycle_batch()`. If the pool has reached its configured capacity, excess buffers are immediately dropped. The limit counts buffers, not bytes: differently sized chunks can retain different amounts of storage. Reuse minimizes allocation churn but does not guarantee zero allocations.

## 6. Coalesced Cloud Range Reads
Streams Cloud-Optimized GeoTIFFs directly from HTTP/HTTPS endpoints or AWS S3 buckets without copying the entire multi-gigabyte file to local disk:
- **Sparse Header Indexing**: Reads the TIFF header, Image File Directories (IFDs), and embedded GeoKey tags in a single initial 16 KB byte-range request.
- **Spatial Request Coalescing**: Consecutive or proximate chunk byte ranges within the same spatial region are automatically coalesced into single combined HTTP range requests, drastically cutting HTTP round-trip latency and AWS S3 request costs.
- **Asynchronous Remote Prefetching**: Dedicated background I/O tasks prefetch required remote tile bytes ahead of the decompression workers with automated retry and exponential backoff resilience.

## 7. Exact CRS Transformation with Analytical Fast Paths
The CRS transformer selects identity, analytical Web Mercator/Albers, or Proj4
transforms. Pixel centers and subpixel samples are transformed at their actual
locations; projected rows are not assumed to have constant latitude.

Transformation runs for every sample, so the fast paths set its per-pixel cost: analytical inversions match PROJ (`cs2cs`) to within 0.01 m without the general `proj4rs` pipeline. The same transformer certifies chunk latitude bounds; when it cannot (unknown Proj4 bounds, an Albers chunk that may contain the cone apex), ROI pruning and early horizon eviction fall back to their conservative paths. See [CRS and Projection](crs-and-projection.md).

---

## Correctness and Output Design

### Absolute Pixel Positions
Walking, span discovery, and supersampling use the full affine transform from
absolute pixel positions. Coordinates do not accumulate through repeated
longitude additions, so changing run lengths cannot alter pixel coordinates.

### Multi-File Raster Mosaics & Voronoi Cutline Partitioning
Large geospatial datasets are frequently distributed across tiled collections of adjacent or overlapping GeoTIFF files (e.g., Sentinel-2 granules, national DEM tiles, LANDFIRE map zones):
- **Unified Stream Ingestion**: `h3_raster_continuous_aggregate` and `h3_raster_categorical_aggregate` accept glob patterns (e.g. `'tiles/*.tif'`), comma-delimited file lists, or GDAL VRT XML files.
- **Globally Latitude-Interleaved Streaming**: `PrefetchedMosaicReader` coordinates chunks across all constituent tiles, yielding chunks in global North-to-South scanline order to maintain horizon eviction guarantees across the entire mosaic.
- **Configurable Overlap Resolution Rules**:
  - `'cutline'` *(default)*: Dynamically calculates Voronoi bisector cutlines between tile bounding boxes, partitioning pixels so that boundary pixels are assigned to their nearest tile center. Guarantees **exactly zero double-counting** of pixels in overlapping tile borders.
  - `'first'`: Applies the Painter's Algorithm, giving strict precedence to earlier tiles in the file list.
  - `'average'`: Computes multi-observation running averages across overlapping pixels.

### Native OGC GeoParquet 1.1 Exporter (Stack-Allocated WKB Hexagons & PROJJSON)
Exporting aggregated hexagonal grids to standard GIS formats traditionally required multi-step ETL pipelines involving intermediate shapefiles, GeoJSON scratch disks, and GDAL conversions:
- **Direct SQL Parquet Export**: `h3_raster_to_parquet` streams aggregated hexagons directly into highly compressed Apache Parquet files with zero intermediate files.
- **Stack WKB Polygon Serialization**: Converts 64-bit integer H3 cell indices directly into standard OGC 2D Polygon Well-Known Binary (WKB) bytes in a fixed stack buffer (`WKB_BUF_LEN`) in ~10–15 nanoseconds *(measured on Apple M-series workstation)*. Layout: 1 byte endianness + 4 bytes geometry type + 4 bytes ring count + 4 bytes point count + (n+1) closed-ring vertices $\times$ 16 bytes. Class II (even) resolutions yield 125 bytes (hexagon) / 109 bytes (pentagon); Class III (odd) resolutions add icosahedron-edge crossing vertices, giving 141–157 bytes for edge-straddling hexagons and 189 bytes for 10-vertex pentagons. Antimeridian cells are split into a two-part `MultiPolygon` and pole cells become polar-cap polygons, so every geometry stays within [-180, 180] × [-90, 90].
- **Official GeoParquet 1.1 Compliance**: Emits compliant OGC GeoParquet 1.1 JSON metadata in the Parquet `FileMetaData`, including official PROJJSON `OGC:CRS84` datum ensemble specifications, planar edge definitions, and per-column bounding boxes. Compatible out-of-the-box with DuckDB Spatial (`ST_Read`), Apache Sedona, GeoPandas, GDAL, QGIS, and BigQuery.

---

## Geodetic Tolerances and Semantics

This section outlines fundamental geodetic assumptions, error budgets, and numerical precision considerations across the `raster_h3` processing pipeline.

### Pixel Counts vs. Physical Ground Area
The `count` and `sum` statistics emitted by all aggregators represent discrete counts of sampled raster pixels (or fractional sample weights when supersampling is enabled), **not physical surface areas** in square meters:
- **EPSG:4326 (Plate Carrée / WGS84)**: Rasters with constant degree cell spacing exhibit a $\cos \phi$ ground area distortion (where $\phi$ is latitude). A $0.01^\circ \times 0.01^\circ$ pixel covers $\approx 1.23\text{ km}^2$ at the equator but only $\approx 0.61\text{ km}^2$ at $60^\circ\text{ N}$. Aggregated `sum` values on EPSG:4326 inputs reflect pixel sums rather than true surface integrals.
- **EPSG:3857 (Web Mercator)**: Conformal planar grid cells expand by $1 / \cos \phi$ in linear dimensions, causing pixel ground area to scale as $\cos^2 \phi$ relative to projected planar area.
- *Recommendation*: Workflows requiring rigorous surface flux integration (e.g. biomass totals, volumetric rainfall, solar irradiance) must either apply ellipsoidal area scaling factors ($A \approx R^2 \cos \phi \, \Delta\lambda \, \Delta\phi$) or supply inputs in an equal-area projection such as EPSG:5070 (CONUS Albers Equal Area Conic) or EPSG:6933 (EASE-Grid 2.0).

### NoData at Cell Boundaries Under Supersampling
When sub-pixel supersampling patterns (such as RGSS 4-point or 16-point grid) are enabled, each sub-sample point evaluates whether the underlying pixel value is valid or NoData:
- If a pixel intersecting an H3 hexagon boundary contains NoData, all sub-samples originating from that pixel are discarded.
- Because validity is evaluated at pixel level rather than through exact polygon-clipping intersection between the hexagonal boundary and valid data masks, the effective sample weights near NoData boundaries are not area-consistent across partially masked border pixels. Cells touching masked borders will reflect sample weights proportional to valid pixel encounters rather than true geometric intersection area.

### Floating-Point Associativity and Parallel Merge Order
Aggregators utilize multi-core chunk parallelism (`init_local`) where independent worker threads accumulate local statistics using Welford's online algorithm and merge them into the global scanline horizon:
- Floating-point addition is non-associative in IEEE-754 arithmetic ($(a + b) + c \ne a + (b + c)$).
- Because chunks complete in non-deterministic order depending on operating system thread scheduling and I/O latency, minor least-significant-bit (ULP) differences can arise in cumulative statistics (`mean`, `variance` / $M_2$, and `sum`) across repeated runs on the same input dataset.

### Accepted Geodetic Tolerances and Datum Policy
- **NAD83 vs. WGS84 Continental Offset**: `EPSG:4269` (NAD83) and `EPSG:5070` (CONUS Albers, which uses the GRS80 ellipsoid with NAD83) are processed via fast analytical paths that treat coordinates as equivalent to WGS84 without applying datum shift grids. This accepts the continental plate difference between NAD83 and WGS84 (approximately ~1–2 meters across North America), reflecting the fact that pure-Rust `proj4rs` does not embed high-resolution national datum shift grids (NADCON5 / HARN).
- **PROJ Tooling Parity**: Analytical fast paths (`WebMercatorFast`, `AlbersConicFast`) match reference PROJ (`cs2cs` 9.8.1) coordinate inversions to within $\le 0.01\text{ m}$. Arbitrary projections delegated to pure-Rust `proj4rs` match PROJ within millimeter-to-centimeter precision on identical ellipsoids.
- **Helmert Shifts Only**: PROJ definition strings with 3- or 7-parameter Helmert shifts (`+towgs84`, explicit or implied by a named datum such as `+datum=OSGB36`) are transformed by `proj4rs`. Grid-based datum shifts (`+nadgrids` other than `@null`, and `+datum=NAD27`) are rejected with an explicit `RasterH3Error::CrsError`. See [Datum Shifts](crs-and-projection.md#datum-shifts).

### Recommendation on High Resolutions (Resolution ≥ 14) for Non-WGS84 Inputs
- H3 Resolution 14 has an average hexagon edge length of $\approx 1.34\text{ m}$ (area $\approx 6.3\text{ m}^2$), and Resolution 15 has an edge length of $\approx 0.51\text{ m}$ (area $\approx 0.9\text{ m}^2$).
- Because the geodetic frame uncertainty between NAD83 and WGS84 (~1–2 m) and projection interpolation approximations equal or exceed the entire physical diameter of Resolution 14 and 15 cells, performing raster hexification at `res >= 14` on non-WGS84 inputs is geodetically unsound without sub-meter surveyed datum controls. Users are strongly recommended to limit non-WGS84 ingestion to `res <= 13`, or reproject source rasters to native WGS84 using high-precision geodetic tools (e.g. `gdalwarp` with NADCON5 grids) before ingestion.

---

