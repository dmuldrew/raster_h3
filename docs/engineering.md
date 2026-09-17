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
| 11 | **Lock-Free Work-Stealing Buffer Pool** | Work-stealing buffer injector (`crossbeam_deque::Injector`) eliminates buffer allocation churn across 15,840+ chunks. |
| 12 | **Single-Hop Bounded In-Order Prefetcher** | Direct worker-to-ring-buffer queue eliminates intermediate OS thread context switches with zero-allocation batch drains. |
| 13 | **Cloud-Native COG & Mosaic Ingestion** | Asynchronous HTTP/S3 range prefetching and multi-file Voronoi cutline mosaic blending with zero double-counting. |
| 14 | **Native OGC GeoParquet 1.1 Exporter** | Direct streaming export of 125-byte WKB polygon geometries with embedded PROJJSON `OGC:CRS84` metadata. |

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

## 7. Lock-Free Work-Stealing Buffer Pool (crossbeam_deque::Injector)
High-resolution continental datasets (such as CONUS 30m) require decompressing tens of thousands of tiles (e.g. 15,840+ chunks). Continuously allocating, reallocating, and freeing multi-megabyte decompression buffers causes heavy memory fragmentation, allocator lock contention, and kernel `brk`/`mmap` syscall overhead:
- **Global Work-Stealing Injector**: `PrefetchedChunkReader` uses `crossbeam_deque::Injector<DecodingResult>` as a concurrent, lock-free buffer recycling pool.
- **Zero-Allocation Reuse**: When a background decompression thread prepares to decode a chunk, it attempts to steal an existing buffer from the pool (`buffer_pool.steal()`). Only if the pool is empty does it allocate fresh memory.
- **Recycle on Eviction**: Once the downstream consumer finishes processing a chunk's pixels and advances past the scanline horizon, the allocated buffer is sanitized and recycled back into the injector via `recycle_buffer()`, delivering sustained hardware-saturating throughput with zero heap allocation churn.

## 8. Single-Hop Bounded In-Order Prefetcher (OrderedPrefetchQueue<T>)
Traditional background prefetchers often suffer from thread thrashing: either unbounded queues that risk out-of-memory (OOM) bloat, or intermediate "collector" threads that copy data through multiple OS synchronization channels:
- **Direct Worker-to-Consumer Deposit**: `OrderedPrefetchQueue<T>` connects decompression workers directly to the aggregator through a fixed-capacity ring buffer indexed by `job_id % capacity`.
- **Single-Hop Zero Context Switches**: Workers calculate and decompress chunks concurrently, depositing their result directly into their assigned ring buffer slot. The aggregator drains contiguous, sequence-ordered chunks in bulk using `drain_into()`, acquiring the queue lock only once per batch.
- **Strict Backpressure**: If background workers outpace the aggregator by more than `capacity` chunks, they block on a condition variable until the consumer drains slots, ensuring that memory usage remains strictly bounded regardless of file size.

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

## 11. Native OGC GeoParquet 1.1 Exporter (125-Byte WKB Hexagons & PROJJSON)
Exporting aggregated hexagonal grids to standard GIS formats traditionally required multi-step ETL pipelines involving intermediate shapefiles, GeoJSON scratch disks, and GDAL conversions:
- **Direct SQL Parquet Export**: `h3_raster_to_parquet` streams aggregated hexagons directly into highly compressed Apache Parquet files with zero intermediate files.
- **125-Byte Stack WKB Polygon Serialization**: Converts 64-bit integer H3 cell indices directly into standard OGC 2D Polygon Well-Known Binary (WKB) bytes on the stack in ~10–15 nanoseconds *(measured on Apple M-series workstation)* (1 byte endianness + 4 bytes geometry type + 4 bytes ring count + 4 bytes point count + 7 vertices $\times$ 16 bytes = 125 bytes; 109 bytes for pentagons).
- **Official GeoParquet 1.1 Compliance**: Emits compliant OGC GeoParquet 1.1 JSON metadata in the Parquet `FileMetaData`, including official PROJJSON `OGC:CRS84` datum ensemble specifications, planar edge definitions, and per-column bounding boxes. Compatible out-of-the-box with DuckDB Spatial (`ST_Read`), Apache Sedona, GeoPandas, GDAL, QGIS, and BigQuery.

## 12. Source Module Architecture & File Responsibilities

This section documents the responsibility of every source file in `src/`, organized by module. Each module maps to one or more of the engineering pillars described in §1–§11 above.

---

### `src/lib.rs` — Crate Root & Extension Entry Points
Declares and re-exports all top-level submodules. Implements the DuckDB loadable extension entry points (`raster_h3_init`, `raster_h3_init_c_api`, `raster_h3_version`) and coordinates registration of all table and scalar functions.

### `src/error.rs` — Domain Error Types
Defines `RasterH3Error` via `thiserror`, unifying all recoverable error types across the pipeline (I/O, TIFF decoding, metadata parsing, CRS detection, PROJ4, H3, invalid parameters, DuckDB C-FFI, and streaming failures). Exports the crate-wide `Result<T>` alias.

---

### `src/crs/` — Coordinate Reference System Detection & Reprojection
*Maps to: §3 Row-Constant Latitude Hoisting*

| File | Responsibility |
| :--- | :--- |
| `mod.rs` | CRS detection and parsing from EPSG codes (4326, 3857, 5070, 3338, UTM zones 32601–32760) or PROJ definition strings. Automatic UTM zone string synthesis. |
| `transformer.rs` | Three-tier coordinate reprojection pipeline: **Tier 1** `Wgs84Identity` (zero-cost passthrough for EPSG:4326/4269), **Tier 2** `WebMercatorFast` and `AlbersConicFast` (closed-form analytical transforms including 2-iteration Newton-Raphson inverse solver), **Tier 3** `Proj4` (general-purpose fallback via pure-Rust `proj4rs`). |

---

### `src/ffi/` — DuckDB C-API Foreign Function Interface
*Maps to: §6 Dynamic Work-Stealing Parallelism*

| File | Responsibility |
| :--- | :--- |
| `duckdb_c.rs` | Low-level `extern "C"` declarations and type definitions for the DuckDB C API (database, connection, table functions, scalar functions, bind/init/function info, vectors, data chunks, logical types). Includes inline/pointer string layout handling (`duckdb_string_t`). |
| `spatial_detect.rs` | Dynamic runtime detection via `dlsym` (POSIX) / `GetProcAddress` (Windows) of the host DuckDB library version and whether DuckDB ≥ v1.5 (built-in `GEOMETRY` type) or the `spatial` extension is loaded. |

---

### `src/raster/` — GeoTIFF I/O, Cloud Streaming & Mosaic Ingestion
*Maps to: §7 Zero-Copy memmap2, §8 Single-Hop Bounded Prefetcher, §9 Cloud-Native COG Streaming, §10 Multi-File Mosaics*

| File | Responsibility |
| :--- | :--- |
| `mod.rs` | `RasterChunk` windowing and grid generation — partitions raster extents into processable strip/tile work units. |
| `geotiff.rs` | Streaming GeoTIFF reader with on-demand chunk decoding and buffer recycling. Reads strip and tile layouts, manages memory-mapped files (`memmap2`), and decodes chunks (DEFLATE, LZW, uncompressed) into reusable memory buffers. Supports both local files and remote cloud storage via `HttpRangeReader`. |
| `geotransform.rs` | Six-parameter affine geotransform representation (`pixel_to_coord`, `coord_to_pixel`). Supports GDAL-style arrays, tiepoint/pixel-scale tags, and 4×4 model transformation matrices. |
| `metadata.rs` | GeoTIFF metadata and GeoKey directory extraction — parses TIFF tags and GeoKeys to resolve affine geotransforms, NoData values, WKT strings, and EPSG CRS codes. |
| `predictor.rs` | TIFF Predictor 2 (horizontal differencing) with ARM NEON SIMD acceleration for `u8`, Predictor 3 (floating-point differencing), and byte-order-aware sample unpacking for all integer and float types. |
| `mosaic.rs` | Multi-file directory, globbing, and VRT mosaic ingestion. Resolves directory globs and file lists, orders chunks from multiple files north-to-south for scanline horizon streaming, and provides centroid Voronoi cutline ownership tests for overlap resolution (`cutline`, `first`, `average`). |
| `prefetch.rs` | Asynchronous local prefetching via `OrderedPrefetchQueue<T>` — a bounded ring buffer connecting multi-threaded background decompression workers to the aggregator in strict sequence order with backpressure. Workers deposit decompressed chunks directly into assigned ring buffer slots; the aggregator drains contiguous batches with a single lock acquisition. |
| `remote_prefetch.rs` | Parallel asynchronous chunk prefetching for remote COGs. Coalesces consecutive or nearby chunk byte ranges into single HTTP range requests to cut round-trips by 2×–5×. Streams compressed payloads ahead of scanline processing with automated retry and exponential backoff. |
| `http_range.rs` | Cloud-native HTTP/HTTPS/S3 range request transport. Implements URL normalization, `ByteCache` block caching, exponential backoff retries, byte validation, and a standard `Read + Seek` adapter (`HttpRangeReader`) enabling remote COG files to be treated identically to local files. |

---

### `src/aggregator/` — Statistical Accumulation & Scanline Horizon Engine
*Maps to: §1 Horizon Eviction, §2 Scanline Lookahead, §4 Linear Longitude Stepping & In-Register Accumulation, §5 Branchless Min/Max*

#### Core Accumulation & Pixel Processing

| File | Responsibility |
| :--- | :--- |
| `accumulator.rs` | High-performance pixel accumulator (`H3Accumulator`) for continuous H3 cell statistics — single-pass Welford online mean/variance ($M_2$), running min/max, weighted count, sum, and optional streaming DDSketch quantile estimation. |
| `categorical.rs` | Categorical class frequency accumulator (`CategoricalAccumulator`) per H3 cell. Uses an inline 16-slot array for zero-heap allocation in >99.99% of cells, with an optional boxed hash map spillover for complex multi-class boundaries. Tracks mode/majority class, class fractions, and Shannon entropy. |
| `quantiles.rs` | Streaming non-parametric quantile sketch (`QuantileSketch`) via DDSketch with bounded relative error ($\alpha \le 0.01$). Constant-memory, fully commutative and associative across multi-core Rayon threads. Estimates arbitrary percentiles (p01–p99, IQR). |
| `nodata.rs` | NoData and decoding utilities for typed raster buffers. Provides safe casting between f64 metadata NoData values and native pixel types (`NodataCast`), fast chunk-level NoData validation, and zero-cost static dispatch over `DecodingResult`. Enforces floating-point NaN/epsilon checks vs exact discrete integer comparison. |
| `remap.rs` | Categorical class remapper (`CategoryRemapper`) with L1-cache direct array lookup table. Supports exact value and inclusive range rules with pass-through, drop, and default actions for unmapped categories. |
| `sampling.rs` | Sub-pixel sample offset definitions (`SamplingPattern`) — center, bilinear, RGSS 4-point, 5-point quincunx, Gaussian 5-point, 9-point grid, jittered, and stratified random patterns with fractional weights. |
| `simd.rs` | Trait `SimdSpanAccumulate` and vectorized multi-lane scanline span accumulation for high-throughput pixel aggregation across native data types (`f32`, `f64`, `u8`–`u64`, `i8`–`i64`). |
| `h3_scanline.rs` | H3 scanline lookahead buffer (`H3ScanlineLookahead`) — tracks horizontal hexagon span widths across raster rows to predict span boundaries and minimize H3 coordinate lookups by exploiting spatial pixel coherence. |
| `horizon_streamer.rs` | Priority queue entry (`HexEvictionEntry`) for streaming scanline horizon eviction, ordered by southernmost latitude. Provides `compute_cell_south_lat` for cell boundary calculation and `chunk_intersects_bbox` for spatial pruning. |

#### `multi_horizon/` — Multi-Resolution Streaming Engine

| File | Responsibility |
| :--- | :--- |
| `controller.rs` | Central multi-resolution scanline horizon streamer controller (`MultiHorizonStreamer`). Orchestrates chunk prefetch dispatch, Rayon parallel chunk-row execution, 32-way sharded aggregation maps, horizon latitude progression, eviction & compaction delegation, and lifecycle state transitions. |
| `config.rs` | Query-level configuration (`MultiResolutionConfig`) — resolution arrays, sub-pixel sampling patterns, spectral index formulas (`SpectralFormula` for NDVI/NDWI/NBR/EVI computation), streaming quantile targets (`QuantileTarget`), value filters, and category remapping settings. |
| `lifecycle.rs` | Stream lifecycle state machine (`StreamLifecycle`) with latched three-state transitions (`Running` → `Finished` / `Failed`). Ensures stream failures are never mistaken for normal EOF. Includes `OutputBuffer` for record queue buffering. |
| `sharded_map.rs` | 32-way partitioned lock-free hash map (`ShardedEvictionMap`) using `SplitMix64` on H3 cell indices to distribute accumulator entries across shards. Eliminates thread contention during concurrent row aggregation and horizon eviction. |
| `compaction.rs` | Hierarchical aperture-7 child-to-parent compaction (`HierarchicalCompactor`). Merges 7 fine child cells at resolution R into a single coarse parent cell at resolution R−1 during horizon eviction, with state management for incomplete parents. |
| `continuous.rs` | Continuous chunk payload processing (`MultiContinuousRecord`). Drives pixel-by-pixel H3 cell statistics accumulation across multiple resolution levels with strict tile ownership resolution for overlapping mosaic chunks. |
| `continuous_streamer.rs` | Continuous raster horizon streamer (`ContinuousKernel`, `MultiScanHorizonStreamer`). Implements single-pass streaming aggregation across multiple H3 resolutions for raw raster bands and spectral index formulas. |
| `categorical.rs` | Categorical chunk payload processing (`MultiCategoricalRecord`). Processes discrete integer chunk data into class histograms per H3 cell, enforcing mosaic tile ownership rules and class remappings. |
| `categorical_streamer.rs` | Categorical raster horizon streamer (`CategoricalKernel`, `MultiCategoricalHorizonStreamer`). Drives single-pass multi-resolution class aggregation, remapping, and majority fraction filtering. |
| `overlap_walker.rs` | Pixel-by-pixel mosaic overlap walker (`walk_overlap_pixel_cells`). Evaluates per-pixel tile ownership when chunks intersect overlapping mosaic tiles, applying cutline/Voronoi, first, or average rules. |

##### `walker.rs`, `coordinates.rs`, `span.rs` — Hot-Path Scanline Traversal

> **Architectural Note — Intentional Coupling for Cache Locality**
>
> These three files form the performance-critical inner loop of the scanline engine and are **intentionally tightly coupled**. They interleave affine coordinate math, CRS reprojection, H3 index lookups, sub-pixel sampling, and accumulator state updates within the same call chain to maximize L1/L2 cache locality and minimize pointer indirection during the per-pixel hot path.
>
> Separating these concerns into independent modules would introduce additional function call overhead, break spatial locality of data access patterns, and risk measurable throughput regression on the ~100M+ pixel/sec inner loop. This coupling is a deliberate performance trade-off, not an oversight.

| File | Responsibility |
| :--- | :--- |
| `walker.rs` | Unified geometric pixel walker and spatial math driver. Defines the `ScanlineEngine<T, Acc>` trait that continuous and categorical kernels implement, and orchestrates the generic scanline traversal loop: row iteration, coordinate setup, span discovery, accumulator updates, and chunk bounding-box pruning. Re-exports key types from `coordinates.rs` and `span.rs`. |
| `coordinates.rs` | Coordinate transformation for multi-horizon aggregators (`CoordinateTransformer`, `RowCoordinates`, `RowGeometryContext`). Provides reference coordinate transforms from raster pixel space to WGS84, explicit fast paths for north-up WGS84 and Web Mercator grids, and exact per-sample projected CRS transformation. |
| `span.rs` | Scanline span discovery and core/boundary classification (`H3SpanOptimizer`). Identifies which horizontal samples share an H3 cell assignment (the "span") and classifies subpixel samples as strictly interior (core) vs boundary. Does not update statistics, mutate accumulators, or manage streaming state. |

---

### `src/functions/` — DuckDB SQL Function Bindings & Execution
*Maps to: §5 Stack-Allocated Hex LUT, §6 DuckDB init_local Pipeline, §11 GeoParquet Exporter*

#### Table Functions

| File | Responsibility |
| :--- | :--- |
| `table_function.rs` | Registers and executes `h3_raster_continuous_aggregate` (alias `raster_h3`). Manages parameter binding (`RasterH3BindData`), parallel scan initialization (`RasterH3GlobalData`), thread-local scratch state (`TableFunctionLocalData`), and row output into DuckDB vector chunks. |
| `categorical_table_function.rs` | Registers and executes `h3_raster_categorical_aggregate` (alias `raster_h3_categorical`). Supports Wide and Long output format schemas (`CategoricalOutputFormat`) with majority fraction filtering and unnested long-row pivoting. |
| `pmtiles_table_function.rs` | Registers `h3_raster_to_pmtiles`. Invokes the PMTiles tiling engine and emits a 1-row summary result containing output path, tile count, and archive size. |
| `parquet_table_function.rs` | Registers `h3_raster_to_parquet`. Invokes the Parquet streaming writer and emits a 1-row summary result containing output path, row count, and file size. |

#### Scalar Functions

| File | Responsibility |
| :--- | :--- |
| `scalar.rs` | Registers DuckDB scalar functions: `h3_to_string`, `h3_string_to_h3`, `h3_to_lat`, `h3_to_lng`, `h3_get_resolution`, `h3_is_valid`, `h3_to_wkb`, `h3_cell_to_parent`, and `h3_to_geometry` / `h3_cell_to_geometry`. Uses generic zero-cost unary scalar execution kernels. |

#### Encoding Utilities

| File | Responsibility |
| :--- | :--- |
| `fast_hex.rs` | Zero-allocation hexadecimal formatting (`fast_hex_u64`) and parsing (`parse_hex_u64`) between 64-bit integer H3 cell IDs and lowercase hexadecimal ASCII strings using a 16-byte stack lookup table. |
| `wkb.rs` | Stack-allocated OGC 2D Polygon WKB serialization (`cell_to_wkb`, `h3_index_to_wkb`). Converts H3 cell boundaries directly into 125-byte (hexagon) or 109-byte (pentagon) WKB buffers in ~10–15 ns with zero heap allocations. |

#### `bind_utils/` — Shared Table Function Infrastructure

| File | Responsibility |
| :--- | :--- |
| `bind_helper.rs` | Safe, ergonomic wrapper (`BindHelper`) around DuckDB's `duckdb_bind_info` C structure for extracting positional/named arguments and defining returned column types. |
| `chunk_writer.rs` | Safe wrapper (`ChunkWriter`) around DuckDB's `duckdb_data_chunk` for direct, bounds-checked, auto-vectorized writing into output columnar vectors. |
| `lifecycle.rs` | Table function initialization lifecycle helpers. Provides cardinality estimation (`estimate_raster_cardinality`) based on raster bounds and H3 cell areas, column projection detection, thread-local scratch buffer allocation, and memory cleanup. |
| `parsing.rs` | Parses user-supplied SQL parameters — H3 resolution lists (comma/whitespace separated, sorted, deduplicated, ≤ 15) and bounding box coordinate strings `[min_lon, min_lat, max_lon, max_lat]`. |
| `record_queue.rs` | Multi-threaded concurrent batch queue (`ConcurrentRecordQueue`) bridging the background multi-resolution horizon aggregator to DuckDB execution threads via `pop_or_refill`. |
| `registration.rs` | Parameter registration helpers (`add_positional_parameter`, `add_named_parameter`, `register_common_raster_named_parameters`) with automated DuckDB logical type lifecycle management. |

---

### `src/parquet/` — Native Streaming GeoParquet Writer
*Maps to: §11 Native OGC GeoParquet 1.1 Exporter*

| File | Responsibility |
| :--- | :--- |
| `mod.rs` | Module declarations and public re-exports. |
| `writer.rs` | Streams aggregated multi-resolution H3 records directly into compressed Apache Parquet files (`write_continuous_parquet`, `write_categorical_parquet`). Employs a lock-free double-buffered channel pipeline where the streaming aggregator drains into one buffer while a background thread sorts and flushes the previous buffer. Generates OGC GeoParquet 1.1 JSON metadata (PROJJSON `OGC:CRS84` datum, planar edges, per-column bounding boxes). Configurable Snappy/ZSTD/Gzip compression and spatial locality sorting by H3 cell index within row groups. |

---

### `src/pmtiles/` — PMTiles v3 & MVT Vector Tile Generation
*Maps to: §6 Work-Stealing Parallelism (Rayon tile encoding)*

| File | Responsibility |
| :--- | :--- |
| `mod.rs` | Module declarations and public re-exports. |
| `writer.rs` | PMTiles v3 single-file archive writer (`PmtilesWriter`). Implements the open PMTiles v3 specification with Hilbert curve tile ID indexing (`zxy_to_tile_id`), delta/varint compressed directory encoding, 127-byte header serialization, and `libdeflater` Gzip tile compression. |
| `mvt.rs` | Mapbox Vector Tile (MVT v2) protobuf encoder. Encodes H3 hexagonal geometry rings and cell attribute key-value pairs directly into MVT protocol buffer byte streams with varint encoding, zigzag tags, command integer sequences, tile coordinate quantization (extent 4096), and property dictionary compression. |
| `pyramid.rs` | Web Mercator tile pyramid coordinate math. Implements `lon_lat_to_tile_xy`, `tile_xy_to_bbox`, bidirectional H3 resolution ↔ zoom level mapping (`h3_res_to_zoom`, `zoom_to_h3_res`), and cell boundary Mercator projection for tile intersection ranges. |
| `features.rs` | PMTiles feature definitions, per-resolution summary statistics (`TilePyramidAccumulator`), layer metadata JSON construction (`build_pmtiles_metadata`), and export summary metrics (`PmtilesExportSummary`). |
| `tiler.rs` | Multi-resolution H3-to-PMTiles v3 tiling engine (`stream_raster_to_pmtiles`, `stream_categorical_raster_to_pmtiles`). Orchestrates streaming raster aggregation into multi-zoom PMTiles archives, connecting multi-resolution horizon streamers to Rayon parallel MVT feature encoding with tile eviction when scanline horizons pass tile southern boundaries. |
| `parquet_tiler.rs` | Parquet-to-PMTiles v3 transcoding engine (`transcode_parquet_to_pmtiles`). Reads pre-aggregated H3 records from Parquet files, pre-scans row group extents, and transcodes them into multi-zoom PMTiles archives using streaming latitude eviction to bound memory. |
