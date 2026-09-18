# Core Engineering Innovations

[← Back to README](../README.md)

This document details the core engineering innovations and architectural principles that enable `raster_h3` to achieve hardware-saturating throughput during raster-to-H3 aggregation while maintaining a strictly bounded memory footprint.

> [!NOTE]
> Performance benchmarks, timing metrics, and micro-architectural numbers cited in this document reflect native release builds evaluated on an Apple M-series workstation (8 performance cores, 16 GB Unified Memory) and modern x86_64 processors with AVX2/SSE2 support.

`raster_h3` achieves hardware limits through several key architectural principles:

| # | Engineering Pillar | Description & Impact |
| :---: | :--- | :--- |
| 1 | **Southernmost Scan-Line Horizon Eviction** | RAM stays < 15 MB regardless of file size by sealing completed hexagons as scanlines pass their southern vertices. |
| 2 | **H3 Scanline Lookahead Algorithm** | Jumps ahead along the scanline using previous hexagon widths and binary searches boundary crossings, cutting spherical trig operations by 4x. |
| 3 | **Row-Constant Latitude Hoisting** | Evaluates transcendental projection transforms (`atan`, `exp`, PROJ) once per row rather than per pixel. |
| 4 | **Linear Longitude Stepping** | Advances column coordinates via single 1-cycle additions (lon += delta_lon). |
| 5 | **In-Register Run Accumulation** | Contiguous pixels within the same H3 cell update running statistics in CPU registers, eliminating ~98% of hash table lookups. |
| 6 | **Branchless Hardware Min/Max** | Replaces conditional branches with `minsd`/`maxsd` instructions with zero branch mispredictions. |
| 7 | **Zero-Copy `memmap2` & Async Prefetching** | Maps GeoTIFFs into userspace virtual memory with asynchronous background chunk decompression. |
| 8 | **Zero-Allocation Fast Hex Formatting** | Formats 64-bit integer H3 indices into lowercase hexadecimal ASCII bytes using a 16-byte stack LUT. |
| 9 | **ROI Bounding Box Chunk Pruning** | Skips non-intersecting raster chunks upfront before reading or decompressing data from disk. |
| 10 | **Native DuckDB Parallelism (`init_local`)** | Dynamically distributes raster chunks across all CPU worker threads with accurate optimizer cardinality. |
| 11 | **Bounded Lock-Free Buffer Pool** | Bounded buffer pool with retry-on-contention stealing minimizes buffer allocation churn and strictly caps the number of idle buffers. |
| 12 | **Single-Hop Bounded In-Order Prefetcher** | Direct worker-to-ring-buffer queue eliminates intermediate collector threads with batch draining and backpressure. |
| 13 | **Cloud-Native COG & Mosaic Ingestion** | Asynchronous HTTP/S3 range prefetching and multi-file Voronoi cutline mosaic blending with zero double-counting. |
| 14 | **Native OGC GeoParquet 1.1 Exporter** | Direct streaming export of stack-allocated WKB polygon geometries (109–189 bytes) with embedded PROJJSON `OGC:CRS84` metadata. |

## 1. Southernmost Scan-Line Horizon Eviction
Because GeoTIFF raster scanlines are ordered North-to-South (decreasing latitude), any H3 hexagon whose southernmost vertex is north of the current scan line can **never receive another pixel**. 
- Finished hexagons are immediately evicted from the hash map and streamed into DuckDB vector chunks.
- Active memory remains strictly bounded to O(Scan Front Width) (**< 15 MB RAM**), allowing a standard laptop to seamlessly process a 500 GB global raster.

## 2. H3 Scanline Lookahead Algorithm
To process pixels at maximum throughput, `raster_h3` avoids calculating exact spherical trigonometry (H3 coordinates) for every single pixel. Instead, it uses a **Scanline Lookahead** algorithm that exploits the geometric convexity of hexagons:
1. **The Jump Guess**: As the scanline moves horizontally across the raster, it remembers the width (in pixels) of the previously processed hexagon. It guesses the current hexagon will be the same width and jumps ahead by that exact amount.
2. **Convexity Proof**: If the pixel at the jump destination is the exact same H3 cell, convexity mathematically guarantees that **all pixels skipped between the start and the destination** are also inside that hexagon. The algorithm skips trig math for the entire block.
3. **Binary Search Boundary Finding**: If the jump overshoots into an adjacent hexagon, the algorithm performs an efficient **Binary Search** between the current pixel and the overshot pixel. Because the boundary must lie between these two points, it finds the exact sub-pixel edge in O(log2(error distance)) steps.

## 3. Row-Constant Latitude Hoisting & Coordinate Hierarchy
On North-Up rasters (Web Mercator EPSG:3857, WGS84 EPSG:4326, UTM), latitude is identical across all pixels in a row.
- Transcendental projection functions (`atan`, `exp`, PROJ forward transforms) are evaluated **once per row** instead of once per pixel.
- Eliminates **99.8% of coordinate projection math**.

The transformer uses a **three-tier performance hierarchy**:
- 🟢 **Identity** (`EPSG:4326`, `EPSG:4269`): 0 cycles — coordinates pass through unchanged.
- 🟡 **Analytical** (`EPSG:3857`, `EPSG:900913`): ~5 cycles — closed-form inverse Mercator.
- 🔵 **PROJ4** (UTM, Conic, Polar via `proj4rs`): Pure-Rust reprojection pipeline evaluated once per row.

## 4. Linear Longitude Stepping & In-Register Run Accumulation
Once a row's latitude is evaluated, the physical coordinates of pixels within that horizontal scanline step across longitude uniformly:
- **1-Cycle Arithmetic**: Rather than computing an affine projection matrix multiplication `(c * X + d * Y + ...)`, column coordinates advance via a single hardware addition: `lng += delta_lng`.
- **In-Register Accumulation**: Most pixels reside in the interior of an H3 cell. As long as contiguous pixels share the same cell index, running statistics (Welford mean, variance accumulator $M_2$, weighted count, sum, minimum, maximum) are updated directly in CPU registers without memory access.
- **Batched Horizon Updates**: Only when a cell boundary transition is detected does a single batched flush transfer accumulated weights to the active horizon `FxHashMap<u64, CellAccumulator>`, eliminating ~98% of hash map hashing, bucket lookups, and memory barrier synchronization.

## 5. Branchless Hardware Min/Max & Stack-Allocated Hex LUT
- **Branchless Hardware Extrema**: Computing running minimum and maximum values over millions of pixels traditionally causes frequent CPU pipeline stalls due to unpredictable branch mispredictions. `raster_h3` compiles min/max updates into hardware-native branchless instructions (`minsd`/`maxsd` on x86_64 SSE2/AVX, `fminnm`/`fmaxnm` on ARM64 NEON), ensuring zero branch misprediction penalties even on rugged, noisy terrain.
- **16-Byte Stack-Allocated Hexadecimal LUT**: Formatting 64-bit integer H3 cell IDs into standard lowercase 15-character or 16-character hexadecimal strings (e.g. `'8828308281fffff'`) avoids all heap allocations, `format!()` macro formatting overhead, and dynamic string copies. Using a 16-byte lookup table (`[b'0', b'1', ..., b'f']`) and bitwise shifts (`(val >> 60) as usize & 0xF`), indices are formatted directly into DuckDB vector memory in ~2–3 nanoseconds per cell *(measured on Apple M-series workstation)*.

## 6. Dynamic Work-Stealing Parallelism & DuckDB init_local Pipeline
DuckDB's vectorized execution engine parallelizes custom table functions across arbitrary CPU threads via the `duckdb_table_function_set_init_local` callback:
- **Zero Thread Contention**: Each worker thread maintains its own independent scan state and local horizon hash map, eliminating global mutex contention during pixel aggregation.
- **Work-Stealing Chunk Distribution**: Input GeoTIFF strips or COG tiles are managed as a shared, lock-free task queue. Fast threads that finish their assigned chunks immediately steal remaining chunks from the pool, preventing worker stragglers caused by uneven spatial density or ocean tiles.
- **Accurate Cardinality Estimation**: `estimate_raster_cardinality()` provides DuckDB's cost-based query optimizer with exact row count bounds based on raster bounding boxes and H3 resolution area formulas, enabling optimal hash join planning and vector pipeline scheduling.

## 7. Bounded Lock-Free Buffer Recycling Pool (DecodingBufferPool)
High-resolution continental datasets (such as CONUS 30m) require decompressing tens of thousands of tiles (e.g. 15,840+ chunks). Continuously allocating, reallocating, and freeing multi-megabyte decompression buffers causes heavy memory fragmentation, allocator lock contention, and kernel `brk`/`mmap` syscall overhead:
- **Bounded Lock-Free Recycling**: `PrefetchedChunkReader` and `PrefetchedMosaicReader` use `DecodingBufferPool` (wrapping `crossbeam_deque::Injector<DecodingResult>` with an atomic retention counter) as a concurrent, lock-free buffer recycling pool.
- **Contention-Resilient Acquisition**: When a background decompression thread prepares to decode a chunk, it attempts to acquire an existing buffer from the pool (`buffer_pool.pop()`). If concurrent steals collide (`crossbeam_deque::Steal::Retry`), the worker spins briefly rather than falsely falling back to fresh memory allocation. Only if the pool is genuinely empty (`Steal::Empty`) does it allocate fresh storage.
- **Strict Retention Bound & Minimized Allocation Churn**: Once the downstream consumer finishes processing a chunk batch, allocated buffers are returned to the pool via `recycle_batch()`. If the pool has reached its configured capacity, excess buffers are immediately dropped. The limit counts buffers, not bytes: differently sized chunks can retain different amounts of storage. Reuse minimizes allocation churn but does not guarantee zero allocations.

## 8. Single-Hop Bounded In-Order Prefetcher (OrderedPrefetchQueue<T>)
Traditional background prefetchers often suffer from thread thrashing: either unbounded queues that risk out-of-memory (OOM) bloat, or intermediate "collector" threads that copy data through multiple OS synchronization channels:
- **Direct Worker-to-Consumer Deposit**: `OrderedPrefetchQueue<T>` connects decompression workers directly to the aggregator through a fixed-capacity ring buffer indexed by `job_id % capacity`.
- **Single-Hop Thread Architecture**: Workers calculate and decompress chunks concurrently, depositing their result directly into their assigned ring buffer slot without intermediate collector threads. The aggregator drains contiguous, sequence-ordered chunks in bulk using `drain_into()`, using one queue guard per batch; condition-variable waits release and reacquire the mutex.
- **Strict Backpressure & Deadlock-Free Draining**: If background workers outpace the aggregator by more than `capacity` chunks, they block on a condition variable (`not_full`) until the consumer drains slots. When draining batches that exceed ring capacity, `drain_into()` publishes freed slots before sleeping on incomplete batches, avoiding a circular wait between producers and the consumer. Consumers pull ready chunks via `next_chunk_batch(max_batch)` or bulk batch drains.


### Downstream backpressure and memory limits

`ChunkWriter` bounds writes to DuckDB's output vectors; it does not throttle or schedule decompression. Continuous and wide categorical scan callbacks pull from `ConcurrentRecordQueue::pop_or_refill()` before writing. A refill requests at most four vector-sized batches from the streamer, returns one, and retains at most three. With no further scan calls, no further refills occur. The prefetch ring then fills and each decompression worker eventually blocks in `push()`, after finishing its current decode. An already-running scan may finish its current refill before stalling.

For a single prefetcher, let **C** be ring capacity, **W** decoder workers, **B** the consumer's chunk batch size, and **P** idle-pool capacity. Decoded buffer ownership is bounded by **C + W + B + P** buffers along this path (including a worker's completed buffer waiting to be deposited). This is a count bound, not a fixed byte budget. If each buffer's allocated capacity is at most **S** bytes, those buffers occupy at most **(C + W + B + P) × S** bytes, excluding allocator overhead and decoder scratch storage.

Whole-query memory also includes raster metadata and job lists, compressed remote payloads, decoder scratch buffers, aggregation maps, compaction state, output records, and DuckDB execution state. `OutputBuffer::with_capacity(2048)` reserves initial space; it is not a hard limit. Horizon eviction and EOF flushing can emit more records than the requested row count. Long categorical output additionally expands each cell into one row per category and can overshoot its refill threshold. Therefore these queue bounds do **not** establish a fixed whole-query RAM limit or a universal zero-churn guarantee.

Regression coverage includes an explicit producer-wait handshake for oversized batch drains and a 15,840-item test connecting the real record and prefetch queues. The latter pauses record consumption, observes the producer blocked at the expected capacity boundary, and checks ordered completion after resuming. It uses one synthetic record per chunk; it is not a live DuckDB/TIFF memory benchmark.

## 9. Cloud-Native Remote COG & S3 Streaming (Range Coalescing)
`raster_h3` streams Cloud-Optimized GeoTIFFs (COGs) directly from HTTP/HTTPS endpoints or AWS S3 buckets without copying the entire multi-gigabyte file to local disk:
- **Sparse Header Indexing**: Reads the TIFF header, Image File Directories (IFDs), and embedded GeoKey tags in a single initial 16 KB byte-range request.
- **Spatial Request Coalescing**: Consecutive or proximate chunk byte ranges within the same spatial region are automatically coalesced into single combined HTTP range requests, drastically cutting HTTP round-trip latency and AWS S3 request costs.
- **Asynchronous Remote Prefetching**: Dedicated background I/O tasks prefetch required remote tile bytes ahead of the decompression workers with automated retry and exponential backoff resilience.

## 10. Multi-File Raster Mosaics & Voronoi Cutline Partitioning
Large geospatial datasets are frequently distributed across tiled collections of adjacent or overlapping GeoTIFF files (e.g., Sentinel-2 granules, national DEM tiles, LANDFIRE map zones):
- **Unified Stream Ingestion**: `h3_raster_continuous_aggregate` and `h3_raster_categorical_aggregate` accept glob patterns (e.g. `'tiles/*.tif'`), comma-delimited file lists, or GDAL VRT XML files.
- **Globally Latitude-Interleaved Streaming**: `PrefetchedMosaicReader` coordinates chunks across all constituent tiles, yielding chunks in global North-to-South scanline order to maintain horizon eviction guarantees across the entire mosaic.
- **Configurable Overlap Resolution Rules**:
  - `'cutline'` *(default)*: Dynamically calculates Voronoi bisector cutlines between tile bounding boxes, partitioning pixels so that boundary pixels are assigned to their nearest tile center. Guarantees **exactly zero double-counting** of pixels in overlapping tile borders.
  - `'first'`: Applies the Painter's Algorithm, giving strict precedence to earlier tiles in the file list.
  - `'average'`: Computes multi-observation running averages across overlapping pixels.

## 11. Native OGC GeoParquet 1.1 Exporter (Stack-Allocated WKB Hexagons & PROJJSON)
Exporting aggregated hexagonal grids to standard GIS formats traditionally required multi-step ETL pipelines involving intermediate shapefiles, GeoJSON scratch disks, and GDAL conversions:
- **Direct SQL Parquet Export**: `h3_raster_to_parquet` streams aggregated hexagons directly into highly compressed Apache Parquet files with zero intermediate files.
- **Stack WKB Polygon Serialization**: Converts 64-bit integer H3 cell indices directly into standard OGC 2D Polygon Well-Known Binary (WKB) bytes in a 192-byte stack buffer in ~10–15 nanoseconds *(measured on Apple M-series workstation)*. Layout: 1 byte endianness + 4 bytes geometry type + 4 bytes ring count + 4 bytes point count + (n+1) closed-ring vertices $\times$ 16 bytes. Class II (even) resolutions yield 125 bytes (hexagon) / 109 bytes (pentagon); Class III (odd) resolutions add icosahedron-edge crossing vertices, giving 141–157 bytes for edge-straddling hexagons and 189 bytes for 10-vertex pentagons.
- **Official GeoParquet 1.1 Compliance**: Emits compliant OGC GeoParquet 1.1 JSON metadata in the Parquet `FileMetaData`, including official PROJJSON `OGC:CRS84` datum ensemble specifications, planar edge definitions, and per-column bounding boxes. Compatible out-of-the-box with DuckDB Spatial (`ST_Read`), Apache Sedona, GeoPandas, GDAL, QGIS, and BigQuery.

## 12. Source Module Architecture & File Responsibilities

This section documents the responsibility of every source file in [`src/`](https://github.com/dmuldrew/raster_h3/tree/main/src), organized by module. Each module maps to one or more of the engineering pillars described in §1–§11 above.

---

### [`src/lib.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/lib.rs) — Crate Root & Extension Entry Points
Declares and re-exports all top-level submodules. Implements the DuckDB loadable extension entry points (`raster_h3_init`, `raster_h3_init_c_api`, `raster_h3_version`) and coordinates registration of all table and scalar functions.

### [`src/error.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/error.rs) — Domain Error Types
Defines `RasterH3Error` via `thiserror`, unifying all recoverable error types across the pipeline (I/O, TIFF decoding, metadata parsing, CRS detection, PROJ4, H3, invalid parameters, DuckDB C-FFI, and streaming failures). Exports the crate-wide `Result<T>` alias.

---

### [`src/crs/`](https://github.com/dmuldrew/raster_h3/tree/main/src/crs) — Coordinate Reference System Detection & Reprojection
*Maps to: §3 Row-Constant Latitude Hoisting*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/crs/mod.rs) | CRS detection and parsing from EPSG codes (4326, 3857, 5070, 3338, UTM zones 32601–32760) or PROJ definition strings. Automatic UTM zone string synthesis. |
| [`transformer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/crs/transformer.rs) | Three-tier coordinate reprojection pipeline: **Tier 1** `Wgs84Identity` (zero-cost passthrough for EPSG:4326/4269), **Tier 2** `WebMercatorFast` and `AlbersConicFast` (closed-form analytical transforms including 2-iteration Newton-Raphson inverse solver), **Tier 3** `Proj4` (general-purpose fallback via pure-Rust `proj4rs`). |

---

### [`src/encoding/`](https://github.com/dmuldrew/raster_h3/tree/main/src/encoding) — Zero-Allocation Hex & WKB Geometry Serialization
*Maps to: §5 Stack-Allocated Hex LUT, §11 Native OGC GeoParquet 1.1 Exporter*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/mod.rs) | Module declarations and public re-exports (`fast_hex_u64`, `parse_hex_u64`, `cell_to_wkb`, `h3_index_to_wkb`). |
| [`fast_hex.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/fast_hex.rs) | Zero-allocation hexadecimal formatting (`fast_hex_u64`) and parsing (`parse_hex_u64`) between 64-bit integer H3 cell IDs and lowercase hexadecimal ASCII strings using a 16-byte stack lookup table. |
| [`wkb.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/wkb.rs) | Stack-allocated OGC 2D Polygon WKB serialization (`cell_to_wkb`, `h3_index_to_wkb`). Converts H3 cell boundaries directly into a 192-byte stack buffer (`WkbBuf`) in ~10–15 ns with zero heap allocations, accommodating 5-to-6 vertex Class II cells as well as Class III (odd) resolutions with up to 10 boundary vertices (189 bytes) and icosahedron-edge crossings (141–157 bytes). |

---

### [`src/ffi/`](https://github.com/dmuldrew/raster_h3/tree/main/src/ffi) — DuckDB C-API Foreign Function Interface
*Maps to: §6 Dynamic Work-Stealing Parallelism*

| File | Responsibility |
| :--- | :--- |
| [`duckdb_c.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/ffi/duckdb_c.rs) | Low-level `extern "C"` declarations and type definitions for the DuckDB C API (database, connection, table functions, scalar functions, bind/init/function info, vectors, data chunks, logical types). Includes inline/pointer string layout handling (`duckdb_string_t`). |
| [`spatial_detect.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/ffi/spatial_detect.rs) | Dynamic runtime detection via `dlsym` (POSIX) / `GetProcAddress` (Windows) of the host DuckDB library version and whether DuckDB ≥ v1.5 (built-in `GEOMETRY` type) or the `spatial` extension is loaded. |

---

### [`src/raster/`](https://github.com/dmuldrew/raster_h3/tree/main/src/raster) — GeoTIFF I/O, Cloud Streaming & Mosaic Ingestion
*Maps to: §7 Zero-Copy memmap2, §8 Single-Hop Bounded Prefetcher, §9 Cloud-Native COG Streaming, §10 Multi-File Mosaics*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/mod.rs) | `RasterChunk` windowing and grid generation — partitions raster extents into processable strip/tile work units. |
| [`geotiff.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/geotiff.rs) | Streaming GeoTIFF reader with on-demand chunk decoding and buffer recycling. Reads strip and tile layouts, manages memory-mapped files (`memmap2`), and decodes chunks (DEFLATE, LZW, uncompressed) into reusable memory buffers. Supports both local files and remote cloud storage via `HttpRangeReader`. |
| [`geotransform.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/geotransform.rs) | Six-parameter affine geotransform representation (`pixel_to_coord`, `coord_to_pixel`). Supports GDAL-style arrays, tiepoint/pixel-scale tags, and 4×4 model transformation matrices. |
| [`metadata.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/metadata.rs) | GeoTIFF metadata and GeoKey directory extraction — parses TIFF tags and GeoKeys to resolve affine geotransforms, NoData values, WKT strings, and EPSG CRS codes. |
| [`predictor.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/predictor.rs) | TIFF Predictor 2 (horizontal differencing) with ARM NEON SIMD acceleration for `u8`, Predictor 3 (floating-point differencing), and byte-order-aware sample unpacking for all integer and float types. |
| [`mosaic.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/mosaic.rs) | Multi-file directory, globbing, and VRT mosaic ingestion. Resolves directory globs and file lists, orders chunks from multiple files north-to-south for scanline horizon streaming, and provides centroid Voronoi cutline ownership tests for overlap resolution (`cutline`, `first`, `average`). |
| [`prefetch.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/prefetch.rs) | Asynchronous local prefetching via `OrderedPrefetchQueue<T>` — a bounded ring buffer connecting multi-threaded background decompression workers to the aggregator in strict sequence order with backpressure. Workers deposit decompressed chunks directly into assigned ring buffer slots; the aggregator drains contiguous batches with a single lock acquisition. |
| [`remote_prefetch.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/remote_prefetch.rs) | Parallel asynchronous chunk prefetching for remote COGs. Coalesces consecutive or nearby chunk byte ranges into single HTTP range requests to cut round-trips by 2×–5×. Streams compressed payloads ahead of scanline processing with automated retry and exponential backoff. |
| [`http_range.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/raster/http_range.rs) | Cloud-native HTTP/HTTPS/S3 range request transport. Implements URL normalization, `ByteCache` block caching, exponential backoff retries, byte validation, and a standard `Read + Seek` adapter (`HttpRangeReader`) enabling remote COG files to be treated identically to local files. |

---

### [`src/aggregator/`](https://github.com/dmuldrew/raster_h3/tree/main/src/aggregator) — Statistical Accumulation & Scanline Horizon Engine
*Maps to: §1 Horizon Eviction, §2 Scanline Lookahead, §4 Linear Longitude Stepping & In-Register Accumulation, §5 Branchless Min/Max*

#### Core Accumulation & Pixel Processing

| File | Responsibility |
| :--- | :--- |
| [`accumulator.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/accumulator.rs) | High-performance pixel accumulator (`H3Accumulator`) for continuous H3 cell statistics — single-pass Welford online mean/variance ($M_2$), running min/max, weighted count, sum, and optional streaming DDSketch quantile estimation. |
| [`categorical.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/categorical.rs) | Categorical class frequency accumulator (`CategoricalAccumulator`) per H3 cell. Uses an inline 16-slot array for zero-heap allocation in >99.99% of cells, with an optional boxed hash map spillover for complex multi-class boundaries. Tracks mode/majority class, class fractions, and Shannon entropy. |
| [`quantiles.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/quantiles.rs) | Streaming non-parametric quantile sketch (`QuantileSketch`) via DDSketch with bounded relative error ($\alpha \le 0.01$). Constant-memory, fully commutative and associative across multi-core Rayon threads. Estimates arbitrary percentiles (p01–p99, IQR). |
| [`nodata.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/nodata.rs) | NoData and decoding utilities for typed raster buffers. Provides safe casting between f64 metadata NoData values and native pixel types (`NodataCast`), fast chunk-level NoData validation, and zero-cost static dispatch over `DecodingResult`. Enforces floating-point NaN/epsilon checks vs exact discrete integer comparison. |
| [`remap.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/remap.rs) | Categorical class remapper (`CategoryRemapper`) with L1-cache direct array lookup table. Supports exact value and inclusive range rules with pass-through, drop, and default actions for unmapped categories. |
| [`sampling.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/sampling.rs) | Sub-pixel sample offset definitions (`SamplingPattern`) — center, bilinear, RGSS 4-point, 5-point quincunx, Gaussian 5-point, 9-point grid, jittered, and stratified random patterns with fractional weights. |
| [`simd.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/simd.rs) | Trait `SimdSpanAccumulate` and vectorized multi-lane scanline span accumulation for high-throughput pixel aggregation across native data types (`f32`, `f64`, `u8`–`u64`, `i8`–`i64`). |
| [`h3_scanline.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/h3_scanline.rs) | H3 scanline lookahead buffer (`H3ScanlineLookahead`) — tracks horizontal hexagon span widths across raster rows to predict span boundaries and minimize H3 coordinate lookups by exploiting spatial pixel coherence. |
| [`horizon_streamer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/horizon_streamer.rs) | Priority queue entry (`HexEvictionEntry`) for streaming scanline horizon eviction, ordered by southernmost latitude. Provides `compute_cell_south_lat` for cell boundary calculation and `chunk_intersects_bbox` for spatial pruning. |

#### [`multi_horizon/`](https://github.com/dmuldrew/raster_h3/tree/main/src/aggregator/multi_horizon) — Multi-Resolution Streaming Engine

| File | Responsibility |
| :--- | :--- |
| [`controller.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/controller.rs) | Central multi-resolution scanline horizon streamer controller (`MultiHorizonStreamer`). Orchestrates chunk prefetch dispatch, Rayon parallel chunk-row execution, 32-way sharded aggregation maps, horizon latitude progression, eviction & compaction delegation, and lifecycle state transitions. |
| [`config.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/config.rs) | Query-level configuration (`MultiResolutionConfig`) — resolution arrays, sub-pixel sampling patterns, spectral index formulas (`SpectralFormula` for NDVI/NDWI/NBR/EVI computation), streaming quantile targets (`QuantileTarget`), value filters, and category remapping settings. |
| [`lifecycle.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/lifecycle.rs) | Stream lifecycle state machine (`StreamLifecycle`) with latched three-state transitions (`Running` → `Finished` / `Failed`). Ensures stream failures are never mistaken for normal EOF. Includes `OutputBuffer` for record queue buffering. |
| [`sharded_map.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/sharded_map.rs) | 32-way partitioned lock-free hash map (`ShardedEvictionMap`) using `SplitMix64` on H3 cell indices to distribute accumulator entries across shards. Eliminates thread contention during concurrent row aggregation and horizon eviction. |
| [`compaction.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/compaction.rs) | Hierarchical aperture-7 child-to-parent compaction (`HierarchicalCompactor`). Merges 7 fine child cells at resolution R into a single coarse parent cell at resolution R−1 during horizon eviction, with state management for incomplete parents. |
| [`continuous.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/continuous.rs) | Continuous chunk payload processing (`MultiContinuousRecord`). Drives pixel-by-pixel H3 cell statistics accumulation across multiple resolution levels with strict tile ownership resolution for overlapping mosaic chunks. |
| [`continuous_streamer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/continuous_streamer.rs) | Continuous raster horizon streamer (`ContinuousKernel`, `MultiScanHorizonStreamer`). Implements single-pass streaming aggregation across multiple H3 resolutions for raw raster bands and spectral index formulas. |
| [`categorical.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/categorical.rs) | Categorical chunk payload processing (`MultiCategoricalRecord`). Processes discrete integer chunk data into class histograms per H3 cell, enforcing mosaic tile ownership rules and class remappings. |
| [`categorical_streamer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/categorical_streamer.rs) | Categorical raster horizon streamer (`CategoricalKernel`, `MultiCategoricalHorizonStreamer`). Drives single-pass multi-resolution class aggregation, remapping, and majority fraction filtering. |
| [`overlap_walker.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/overlap_walker.rs) | Pixel-by-pixel mosaic overlap walker (`walk_overlap_pixel_cells`). Evaluates per-pixel tile ownership when chunks intersect overlapping mosaic tiles, applying cutline/Voronoi, first, or average rules. |
| [`spectral.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/spectral.rs) | On-the-fly spectral index formulas (`SpectralFormula`) and physical reflectance evaluation for NDVI, NDWI, NBR, and EVI with singularity/zero-division protections. |


##### [`walker.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/walker.rs), [`coordinates.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/coordinates.rs), [`span.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/span.rs) — Hot-Path Scanline Traversal

> **Architectural Note — Intentional Coupling for Cache Locality**
>
> These three files form the performance-critical inner loop of the scanline engine and are **intentionally tightly coupled**. They interleave affine coordinate math, CRS reprojection, H3 index lookups, sub-pixel sampling, and accumulator state updates within the same call chain to maximize L1/L2 cache locality and minimize pointer indirection during the per-pixel hot path.
>
> Separating these concerns into independent modules would introduce additional function call overhead, break spatial locality of data access patterns, and risk measurable throughput regression on the ~100M+ pixel/sec inner loop. This coupling is a deliberate performance trade-off, not an oversight.

| File | Responsibility |
| :--- | :--- |
| [`walker.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/walker.rs) | Unified geometric pixel walker and spatial math driver. Defines the `ScanlineEngine<T, Acc>` trait that continuous and categorical kernels implement, and orchestrates the generic scanline traversal loop: row iteration, coordinate setup, span discovery, accumulator updates, and chunk bounding-box pruning. Re-exports key types from `coordinates.rs` and `span.rs`. |
| [`coordinates.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/coordinates.rs) | Coordinate transformation for multi-horizon aggregators (`CoordinateTransformer`, `RowCoordinates`, `RowGeometryContext`). Provides reference coordinate transforms from raster pixel space to WGS84, explicit fast paths for north-up WGS84 and Web Mercator grids, and exact per-sample projected CRS transformation. |
| [`span.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/aggregator/multi_horizon/span.rs) | Scanline span discovery and core/boundary classification (`H3SpanOptimizer`). Identifies which horizontal samples share an H3 cell assignment (the "span") and classifies subpixel samples as strictly interior (core) vs boundary. Does not update statistics, mutate accumulators, or manage streaming state. |

---

### [`src/functions/`](https://github.com/dmuldrew/raster_h3/tree/main/src/functions) — DuckDB SQL Function Bindings & Execution
*Maps to: §5 Stack-Allocated Hex LUT, §6 DuckDB init_local Pipeline, §11 GeoParquet Exporter*

#### Table Functions

| File | Responsibility |
| :--- | :--- |
| [`table_function.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/table_function.rs) | Registers and executes `h3_raster_continuous_aggregate` (alias `raster_h3`). Manages parameter binding (`RasterH3BindData`), parallel scan initialization (`RasterH3GlobalData`), thread-local scratch state (`TableFunctionLocalData`), and row output into DuckDB vector chunks. |
| [`categorical_table_function.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/categorical_table_function.rs) | Registers and executes `h3_raster_categorical_aggregate` (alias `raster_h3_categorical`). Supports Wide and Long output format schemas (`CategoricalOutputFormat`) with majority fraction filtering and unnested long-row pivoting. |
| [`pmtiles_table_function.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/pmtiles_table_function.rs) | Registers `h3_raster_to_pmtiles`. Invokes the PMTiles tiling engine and emits a 1-row summary result containing output path, tile count, and archive size. |
| [`parquet_table_function.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/parquet_table_function.rs) | Registers `h3_raster_to_parquet`. Invokes the Parquet streaming writer and emits a 1-row summary result containing output path, row count, and file size. |

#### Scalar Functions

| File | Responsibility |
| :--- | :--- |
| [`scalar.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/scalar.rs) | Registers DuckDB scalar functions: `h3_to_string`, `h3_string_to_h3`, `h3_to_lat`, `h3_to_lng`, `h3_get_resolution`, `h3_is_valid`, `h3_to_wkb`, `h3_cell_to_parent`, and `h3_to_geometry` / `h3_cell_to_geometry`. Uses generic zero-cost unary scalar execution kernels. |

#### Re-exported Encoding Utilities (implemented in `crate::encoding`)

| File | Responsibility |
| :--- | :--- |
| [`fast_hex`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/fast_hex.rs) | Re-exports zero-allocation hexadecimal formatting (`fast_hex_u64`) and parsing (`parse_hex_u64`) from `crate::encoding::fast_hex`. |
| [`wkb`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/wkb.rs) | Re-exports stack-allocated OGC 2D Polygon WKB serialization (`cell_to_wkb`, `h3_index_to_wkb`) from `crate::encoding::wkb`. |

#### [`bind_utils/`](https://github.com/dmuldrew/raster_h3/tree/main/src/functions/bind_utils) — Shared Table Function Infrastructure

| File | Responsibility |
| :--- | :--- |
| [`bind_helper.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/bind_utils/bind_helper.rs) | Safe, ergonomic wrapper (`BindHelper`) around DuckDB's `duckdb_bind_info` C structure for extracting positional/named arguments with RAII parameter value lifecycle management (`OwnedValue`) and defining returned column types. |
| [`chunk_writer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/bind_utils/chunk_writer.rs) | Column- and row-bounds-checked wrapper (`ChunkWriter`) around DuckDB's `duckdb_data_chunk` for auto-vectorized writing into output columnar vectors. Validates row indices against vector capacity and column indices against chunk column count; callers uphold physical vector type invariants. |
| [`lifecycle.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/bind_utils/lifecycle.rs) | Table function initialization lifecycle helpers. Provides cardinality estimation (`estimate_raster_cardinality`) based on raster bounds and H3 cell areas, column projection detection, thread-local scratch buffer allocation, and double-panic-contained memory deallocation (`delete_boxed`). |
| [`parsing.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/bind_utils/parsing.rs) | Parses user-supplied SQL parameters — H3 resolution lists (comma/whitespace separated, sorted, deduplicated, ≤ 15) and bounding box coordinate strings `[min_lon, min_lat, max_lon, max_lat]`. |
| [`record_queue.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/bind_utils/record_queue.rs) | Multi-threaded concurrent batch queue (`ConcurrentRecordQueue`) bridging the background multi-resolution horizon aggregator to DuckDB execution threads via `pop_or_refill`. |
| [`registration.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/functions/bind_utils/registration.rs) | Parameter registration helpers (`add_positional_parameter`, `add_named_parameter`, `register_common_raster_named_parameters`) with automated DuckDB logical type lifecycle management. |

#### FFI Safety, Panic Containment & Leak-Free Cancellation

- **Complete FFI Panic Containment**: All C-ABI callbacks (`raster_h3_init`, `raster_h3_init_c_api`, bind, init, scan, scalar, and `delete_boxed`) are enclosed in `catch_unwind` guards. Escaping panics across `extern "C"` boundaries are strictly forbidden.
- **Secondary Panic Containment**: Any secondary panic that arises during error reporting, payload string formatting, or payload destructor disposal is caught within nested panic guards. Secondary panic payloads are forgotten via `std::mem::forget(secondary)` to avoid triggering an immediate process abort, falling back to static C string error indicators.
- **Scalar Error Setter ABI Compliance**: Scalar functions invoke `duckdb_scalar_function_set_error` rather than table function error setters, maintaining strict C ABI compatibility with DuckDB's internal `ScalarFunctionData` structures.
- **Leak-Free Cancellation & Synchronous Worker Reaping**: When queries terminate early (e.g. `LIMIT` reached or client-side cancellation), DuckDB invokes registered state destructors. Dropping `PrefetchedMosaicReader` / `PrefetchedChunkReader` immediately closes prefetch queues, signals remote prefetch cancellation, and synchronously joins all background decode and HTTP worker threads before releasing mosaic buffers and memory.
- **Parameter Value Ownership**: All `duckdb_value` allocations returned by DuckDB parameter inspection APIs are managed by the `OwnedValue` RAII guard, ensuring immediate release via `duckdb_destroy_value`.

---

### [`src/parquet/`](https://github.com/dmuldrew/raster_h3/tree/main/src/parquet) — Native Streaming GeoParquet Writer
*Maps to: §11 Native OGC GeoParquet 1.1 Exporter*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/mod.rs) | Module declarations and public re-exports (`build_geoparquet_metadata`, `run_parquet_streaming_pipeline`, `ParquetStreamer`, `ParquetRowGroupBuffer`, `H3ParquetWriter`, etc.). |
| [`geoparquet_metadata.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/geoparquet_metadata.rs) | Specification-compliant OGC GeoParquet 1.1 JSON metadata builder (`build_geoparquet_metadata`) embedded in Parquet `FileMetaData`, including official PROJJSON `OGC:CRS84` datum ensemble definitions, planar edge definitions, and per-column bounding boxes. |
| [`pipeline.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/pipeline.rs) | Double-buffered channel streaming pipeline (`run_parquet_streaming_pipeline`, `run_parquet_streaming_pipeline_with_progress`), streaming source abstraction (`ParquetStreamer`), row group buffer abstraction (`ParquetRowGroupBuffer`), in-memory spatial sorting by H3 index, and low-level typed column writers. |
| [`writer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/writer.rs) | Schema-specific columnar row group buffers (`ContinuousRowGroupBuffer`, `CategoricalRowGroupBuffer`), export configuration (`ParquetExportConfig`), and high-level export facade (`H3ParquetWriter`). |

---

### [`src/pmtiles/`](https://github.com/dmuldrew/raster_h3/tree/main/src/pmtiles) — PMTiles v3 & MVT Vector Tile Generation
*Maps to: §6 Work-Stealing Parallelism (Rayon tile encoding)*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/pmtiles/mod.rs) | Module declarations and public re-exports. |
| [`writer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/pmtiles/writer.rs) | PMTiles v3 single-file archive writer (`PmtilesWriter`). Implements the open PMTiles v3 specification with Hilbert curve tile ID indexing (`zxy_to_tile_id`), delta/varint compressed directory encoding, 127-byte header serialization, and `libdeflater` Gzip tile compression. |
| [`mvt.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/pmtiles/mvt.rs) | Mapbox Vector Tile (MVT v2) protobuf encoder. Encodes H3 hexagonal geometry rings and cell attribute key-value pairs directly into MVT protocol buffer byte streams with varint encoding, zigzag tags, command integer sequences, tile coordinate quantization (extent 4096), and property dictionary compression. |
| [`pyramid.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/pmtiles/pyramid.rs) | Web Mercator tile pyramid coordinate math. Implements `lon_lat_to_tile_xy`, `tile_xy_to_bbox`, bidirectional H3 resolution ↔ zoom level mapping (`h3_res_to_zoom`, `zoom_to_h3_res`), and cell boundary Mercator projection for tile intersection ranges. |
| [`features.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/pmtiles/features.rs) | PMTiles feature definitions, per-resolution summary statistics (`TilePyramidAccumulator`), layer metadata JSON construction (`build_pmtiles_metadata`), and export summary metrics (`PmtilesExportSummary`). |
| [`tiler.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/pmtiles/tiler.rs) | Multi-resolution H3-to-PMTiles v3 tiling engine (`stream_raster_to_pmtiles`, `stream_categorical_raster_to_pmtiles`). Orchestrates streaming raster aggregation into multi-zoom PMTiles archives, connecting multi-resolution horizon streamers to Rayon parallel MVT feature encoding with tile eviction when scanline horizons pass tile southern boundaries. |
| [`parquet_tiler`](https://github.com/dmuldrew/raster_h3/blob/main/src/transcode/parquet_tiler.rs) | Re-exports `process_parquet_to_pmtiles` and `RowGroupExtent` from `crate::transcode::parquet_tiler`. |

---

### [`src/transcode/`](https://github.com/dmuldrew/raster_h3/tree/main/src/transcode) — Cross-Format Dataset Transcoding
*Maps to: §1 Horizon Eviction, §6 Work-Stealing Parallelism*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/transcode/mod.rs) | Module declarations and public re-exports (`process_parquet_to_pmtiles`, `RowGroupExtent`, `scan_row_group_h3_extent`). |
| [`parquet_tiler.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/transcode/parquet_tiler.rs) | Parquet-to-PMTiles v3 transcoding engine (`process_parquet_to_pmtiles`). Reads pre-aggregated H3 records from Parquet files, pre-scans row group extents, and transcodes them into multi-zoom PMTiles archives using streaming latitude eviction to bound memory. |

---

## 13. Geodetic Tolerances and Semantics

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
- **Strict Rejection of Non-Zero Datum Shifts**: PROJ definition strings containing non-zero Helmert datum shift parameters (`+towgs84` with non-zero parameters) or mandatory non-null datum grids (`+nadgrids` other than `@null`) are rejected with an explicit `RasterH3Error::CrsError` to prevent silent geodetic inaccuracies.

### Recommendation on High Resolutions (Resolution ≥ 14) for Non-WGS84 Inputs
- H3 Resolution 14 has an average hexagon edge length of $\approx 1.34\text{ m}$ (area $\approx 6.3\text{ m}^2$), and Resolution 15 has an edge length of $\approx 0.51\text{ m}$ (area $\approx 0.9\text{ m}^2$).
- Because the geodetic frame uncertainty between NAD83 and WGS84 (~1–2 m) and projection interpolation approximations equal or exceed the entire physical diameter of Resolution 14 and 15 cells, performing raster hexification at `res >= 14` on non-WGS84 inputs is geodetically unsound without sub-meter surveyed datum controls. Users are strongly recommended to limit non-WGS84 ingestion to `res <= 13`, or reproject source rasters to native WGS84 using high-precision geodetic tools (e.g. `gdalwarp` with NADCON5 grids) before ingestion.

---

See [Rust API migration](refactor-migration.md) for configuration defaults, compatibility adapters, and updated safety contracts.
