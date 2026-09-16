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
