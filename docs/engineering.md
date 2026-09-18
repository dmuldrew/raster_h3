# Core Engineering Architecture

[← Back to README](../README.md)

This document details the architectural principles that enable `raster_h3` to aggregate multi-gigabyte rasters into H3 hexagonal grids at hardware-saturating throughput while keeping memory strictly bounded below 15 MB.

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

## 4. Linear Longitude Stepping
Once a row's latitude is evaluated, the physical coordinates of pixels within that horizontal scanline step across longitude uniformly:
- **1-Cycle Arithmetic**: Rather than computing an affine projection matrix multiplication `(c * X + d * Y + ...)`, column coordinates advance via a single hardware addition: `lng += delta_lng`.
- **Inner Loop Vectorization**: Stepping coordinates with a uniform delta allows the compiler to unroll loops and generate SIMD auto-vectorized code paths across scanline pixel batches without transcendental trigonometry or matrix inversions.

## 5. In-Register Run Accumulation
Most pixels reside in the interior of an H3 cell. As long as contiguous pixels share the same cell index, running statistics are updated directly in CPU registers without memory access:
- **Zero Memory Access**: Running statistics (Welford running mean, variance accumulator $M_2$, weighted count, sum, minimum, and maximum) remain in CPU registers during contiguous interior runs.
- **Batched Horizon Updates**: Only when a cell boundary transition is detected does a single batched flush transfer accumulated weights to the active horizon `FxHashMap<u64, CellAccumulator>`, eliminating ~98% of hash map hashing, bucket lookups, and memory barrier synchronization.

## 6. Branchless Hardware Min/Max
Computing running minimum and maximum values over millions of pixels traditionally causes frequent CPU pipeline stalls due to unpredictable branch mispredictions:
- **Hardware-Native Instructions**: `raster_h3` compiles min/max updates into hardware-native branchless instructions (`minsd`/`maxsd` on x86_64 SSE2/AVX, `fminnm`/`fmaxnm` on ARM64 NEON).
- **Zero Misprediction Penalty**: Guarantees zero branch misprediction penalties even on rugged, noisy, or alternating terrain values.

## 7. Zero-Copy `memmap2` & Async Prefetching
Local GeoTIFF files are accessed via virtual memory mapping rather than standard read syscalls:
- **Userspace Virtual Memory**: `MmapFile` maps files into userspace virtual memory with `memmap2`. Chunk byte slices (`&[u8]`) are referenced directly from the memory map without intermediate kernel-to-userspace copying.
- **Asynchronous Decompression**: Multi-threaded decompression workers read directly from mapped pages in parallel, decoupling disk I/O and page cache faults from pixel aggregation.

## 8. Zero-Allocation Fast Hex Formatting
Formatting 64-bit integer H3 cell IDs into standard lowercase 15-character or 16-character hexadecimal strings (e.g. `'8828308281fffff'`) avoids all heap allocations, `format!()` macro formatting overhead, and dynamic string copies:
- **16-Byte Stack-Allocated LUT**: Using a 16-byte lookup table (`[b'0', b'1', ..., b'f']`) and bitwise shifts (`(val >> 60) as usize & 0xF`), indices are formatted directly into DuckDB vector memory in ~2–3 nanoseconds per cell *(measured on Apple M-series workstation)*.
- **Zero-Copy Parsing**: Companion parser `parse_hex_u64` decodes hexadecimal strings back to 64-bit integers using branchless byte-offset lookups.

## 9. ROI Bounding Box Chunk Pruning
Queries specifying a spatial Region of Interest (ROI) bounding box (`min_lat`, `min_lon`, `max_lat`, `max_lon`) prune non-intersecting chunks upfront:
- **Pre-Decompression Pruning**: `chunk_intersects_bbox` evaluates the projected spatial extent of each strip or tile against the target bounding box before initiating decompression.
- **Zero I/O Overhead**: Chunks falling entirely outside the target ROI are skipped immediately—zero bytes are transferred from disk/network, zero decompression threads are spawned, and zero buffer memory is allocated.

## 10. Dynamic Work-Stealing Parallelism & DuckDB init_local Pipeline
DuckDB's vectorized execution engine parallelizes custom table functions across arbitrary CPU threads via the `duckdb_table_function_set_init_local` callback:
- **Zero Thread Contention**: Each worker thread maintains its own independent scan state and local horizon hash map, eliminating global mutex contention during pixel aggregation.
- **Work-Stealing Chunk Distribution**: Input GeoTIFF strips or COG tiles are managed as a shared, lock-free task queue. Fast threads that finish their assigned chunks immediately steal remaining chunks from the pool, preventing worker stragglers caused by uneven spatial density or ocean tiles.
- **Accurate Cardinality Estimation**: `estimate_raster_cardinality()` provides DuckDB's cost-based query optimizer with exact row count bounds based on raster bounding boxes and H3 resolution area formulas, enabling optimal hash join planning and vector pipeline scheduling.

## 11. Bounded Lock-Free Buffer Recycling Pool (DecodingBufferPool)
High-resolution continental datasets (such as CONUS 30m) require decompressing tens of thousands of tiles (e.g. 15,840+ chunks). Continuously allocating, reallocating, and freeing multi-megabyte decompression buffers causes heavy memory fragmentation, allocator lock contention, and kernel `brk`/`mmap` syscall overhead:
- **Bounded Lock-Free Recycling**: `PrefetchedChunkReader` and `PrefetchedMosaicReader` use `DecodingBufferPool` (wrapping `crossbeam_deque::Injector<DecodingResult>` with an atomic retention counter) as a concurrent, lock-free buffer recycling pool.
- **Contention-Resilient Acquisition**: When a background decompression thread prepares to decode a chunk, it attempts to acquire an existing buffer from the pool (`buffer_pool.pop()`). If concurrent steals collide (`crossbeam_deque::Steal::Retry`), the worker spins briefly rather than falsely falling back to fresh memory allocation. Only if the pool is genuinely empty (`Steal::Empty`) does it allocate fresh storage.
- **Strict Retention Bound & Minimized Allocation Churn**: Once the downstream consumer finishes processing a chunk batch, allocated buffers are returned to the pool via `recycle_batch()`. If the pool has reached its configured capacity, excess buffers are immediately dropped. The limit counts buffers, not bytes: differently sized chunks can retain different amounts of storage. Reuse minimizes allocation churn but does not guarantee zero allocations.

## 12. Single-Hop Bounded In-Order Prefetcher (OrderedPrefetchQueue<T>)
Traditional background prefetchers often suffer from thread thrashing: either unbounded queues that risk out-of-memory (OOM) bloat, or intermediate "collector" threads that copy data through multiple OS synchronization channels:
- **Direct Worker-to-Consumer Deposit**: `OrderedPrefetchQueue<T>` connects decompression workers directly to the aggregator through a fixed-capacity ring buffer indexed by `job_id % capacity`.
- **Single-Hop Thread Architecture**: Workers calculate and decompress chunks concurrently, depositing their result directly into their assigned ring buffer slot without intermediate collector threads. The aggregator drains contiguous, sequence-ordered chunks in bulk using `drain_into()`, using one queue guard per batch; condition-variable waits release and reacquire the mutex.
- **Strict Backpressure & Deadlock-Free Draining**: If background workers outpace the aggregator by more than `capacity` chunks, they block on a condition variable (`not_full`) until the consumer drains slots. When draining batches that exceed ring capacity, `drain_into()` publishes freed slots before sleeping on incomplete batches, avoiding a circular wait between producers and the consumer. Consumers pull ready chunks via `next_chunk_batch(max_batch)` or bulk batch drains.


### Downstream backpressure and memory limits

`ChunkWriter` bounds writes to DuckDB's output vectors; it does not throttle or schedule decompression. Continuous and wide categorical scan callbacks pull from `ConcurrentRecordQueue::pop_or_refill()` before writing. A refill requests at most four vector-sized batches from the streamer, returns one, and retains at most three. With no further scan calls, no further refills occur. The prefetch ring then fills and each decompression worker eventually blocks in `push()`, after finishing its current decode. An already-running scan may finish its current refill before stalling.

For a single prefetcher, let **C** be ring capacity, **W** decoder workers, **B** the consumer's chunk batch size, and **P** idle-pool capacity. Decoded buffer ownership is bounded by **C + W + B + P** buffers along this path (including a worker's completed buffer waiting to be deposited). This is a count bound, not a fixed byte budget. If each buffer's allocated capacity is at most **S** bytes, those buffers occupy at most **(C + W + B + P) × S** bytes, excluding allocator overhead and decoder scratch storage.

Whole-query memory also includes raster metadata and job lists, compressed remote payloads, decoder scratch buffers, aggregation maps, compaction state, output records, and DuckDB execution state. `OutputBuffer::with_capacity(2048)` reserves initial space; it is not a hard limit. Horizon eviction and EOF flushing can emit more records than the requested row count. Long categorical output additionally expands each cell into one row per category and can overshoot its refill threshold. Therefore these queue bounds do **not** establish a fixed whole-query RAM limit or a universal zero-churn guarantee.

Regression coverage includes an explicit producer-wait handshake for oversized batch drains and a 15,840-item test connecting the real record and prefetch queues. The latter pauses record consumption, observes the producer blocked at the expected capacity boundary, and checks ordered completion after resuming. It uses one synthetic record per chunk; it is not a live DuckDB/TIFF memory benchmark.

## 13. Cloud-Native COG & Mosaic Ingestion
`raster_h3` provides end-to-end cloud-native ingestion for both standalone Cloud-Optimized GeoTIFFs (COGs) and large-scale multi-file mosaics:

### Remote COG & S3 Streaming (Range Coalescing)
Streams Cloud-Optimized GeoTIFFs directly from HTTP/HTTPS endpoints or AWS S3 buckets without copying the entire multi-gigabyte file to local disk:
- **Sparse Header Indexing**: Reads the TIFF header, Image File Directories (IFDs), and embedded GeoKey tags in a single initial 16 KB byte-range request.
- **Spatial Request Coalescing**: Consecutive or proximate chunk byte ranges within the same spatial region are automatically coalesced into single combined HTTP range requests, drastically cutting HTTP round-trip latency and AWS S3 request costs.
- **Asynchronous Remote Prefetching**: Dedicated background I/O tasks prefetch required remote tile bytes ahead of the decompression workers with automated retry and exponential backoff resilience.

### Multi-File Raster Mosaics & Voronoi Cutline Partitioning
Large geospatial datasets are frequently distributed across tiled collections of adjacent or overlapping GeoTIFF files (e.g., Sentinel-2 granules, national DEM tiles, LANDFIRE map zones):
- **Unified Stream Ingestion**: `h3_raster_continuous_aggregate` and `h3_raster_categorical_aggregate` accept glob patterns (e.g. `'tiles/*.tif'`), comma-delimited file lists, or GDAL VRT XML files.
- **Globally Latitude-Interleaved Streaming**: `PrefetchedMosaicReader` coordinates chunks across all constituent tiles, yielding chunks in global North-to-South scanline order to maintain horizon eviction guarantees across the entire mosaic.
- **Configurable Overlap Resolution Rules**:
  - `'cutline'` *(default)*: Dynamically calculates Voronoi bisector cutlines between tile bounding boxes, partitioning pixels so that boundary pixels are assigned to their nearest tile center. Guarantees **exactly zero double-counting** of pixels in overlapping tile borders.
  - `'first'`: Applies the Painter's Algorithm, giving strict precedence to earlier tiles in the file list.
  - `'average'`: Computes multi-observation running averages across overlapping pixels.

## 14. Native OGC GeoParquet 1.1 Exporter (Stack-Allocated WKB Hexagons & PROJJSON)
Exporting aggregated hexagonal grids to standard GIS formats traditionally required multi-step ETL pipelines involving intermediate shapefiles, GeoJSON scratch disks, and GDAL conversions:
- **Direct SQL Parquet Export**: `h3_raster_to_parquet` streams aggregated hexagons directly into highly compressed Apache Parquet files with zero intermediate files.
- **Stack WKB Polygon Serialization**: Converts 64-bit integer H3 cell indices directly into standard OGC 2D Polygon Well-Known Binary (WKB) bytes in a 192-byte stack buffer in ~10–15 nanoseconds *(measured on Apple M-series workstation)*. Layout: 1 byte endianness + 4 bytes geometry type + 4 bytes ring count + 4 bytes point count + (n+1) closed-ring vertices $\times$ 16 bytes. Class II (even) resolutions yield 125 bytes (hexagon) / 109 bytes (pentagon); Class III (odd) resolutions add icosahedron-edge crossing vertices, giving 141–157 bytes for edge-straddling hexagons and 189 bytes for 10-vertex pentagons.
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
- **Strict Rejection of Non-Zero Datum Shifts**: PROJ definition strings containing non-zero Helmert datum shift parameters (`+towgs84` with non-zero parameters) or mandatory non-null datum grids (`+nadgrids` other than `@null`) are rejected with an explicit `RasterH3Error::CrsError` to prevent silent geodetic inaccuracies.

### Recommendation on High Resolutions (Resolution ≥ 14) for Non-WGS84 Inputs
- H3 Resolution 14 has an average hexagon edge length of $\approx 1.34\text{ m}$ (area $\approx 6.3\text{ m}^2$), and Resolution 15 has an edge length of $\approx 0.51\text{ m}$ (area $\approx 0.9\text{ m}^2$).
- Because the geodetic frame uncertainty between NAD83 and WGS84 (~1–2 m) and projection interpolation approximations equal or exceed the entire physical diameter of Resolution 14 and 15 cells, performing raster hexification at `res >= 14` on non-WGS84 inputs is geodetically unsound without sub-meter surveyed datum controls. Users are strongly recommended to limit non-WGS84 ingestion to `res <= 13`, or reproject source rasters to native WGS84 using high-precision geodetic tools (e.g. `gdalwarp` with NADCON5 grids) before ingestion.

---

See [Rust API migration](refactor-migration.md) for configuration defaults, compatibility adapters, and updated safety contracts.
