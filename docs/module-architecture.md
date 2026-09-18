# Source Module Architecture & File Responsibilities

[← Back to Engineering Innovations](engineering.md) · [← Back to README](../README.md)

This document details the responsibility of every source file in [`src/`](https://github.com/dmuldrew/raster_h3/tree/main/src), organized by module. Each module maps to one or more of the core engineering innovations described in [Core Engineering Innovations](engineering.md).

---

### [`src/lib.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/lib.rs) — Crate Root & Extension Entry Points
Declares and re-exports all top-level submodules. Implements the DuckDB loadable extension entry points (`raster_h3_init`, `raster_h3_init_c_api`, `raster_h3_version`) and coordinates registration of all table and scalar functions.

### [`src/error.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/error.rs) — Domain Error Types
Defines `RasterH3Error` via `thiserror`, unifying all recoverable error types across the pipeline (I/O, TIFF decoding, metadata parsing, CRS detection, PROJ4, H3, invalid parameters, DuckDB C-FFI, and streaming failures). Exports the crate-wide `Result<T>` alias.

---

### [`src/crs/`](https://github.com/dmuldrew/raster_h3/tree/main/src/crs) — Coordinate Reference System Detection & Reprojection
*Maps to: [Engineering Innovations §3 Row-Constant Latitude Hoisting](engineering.md#3-row-constant-latitude-hoisting--coordinate-hierarchy)*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/crs/mod.rs) | CRS detection and parsing from EPSG codes (4326, 3857, 5070, 3338, UTM zones 32601–32760) or PROJ definition strings. Automatic UTM zone string synthesis. |
| [`transformer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/crs/transformer.rs) | Three-tier coordinate reprojection pipeline: **Tier 1** `Wgs84Identity` (zero-cost passthrough for EPSG:4326/4269), **Tier 2** `WebMercatorFast` and `AlbersConicFast` (closed-form analytical transforms including 2-iteration Newton-Raphson inverse solver), **Tier 3** `Proj4` (general-purpose fallback via pure-Rust `proj4rs`). |

---

### [`src/encoding/`](https://github.com/dmuldrew/raster_h3/tree/main/src/encoding) — Zero-Allocation Hex & WKB Geometry Serialization
*Maps to: [Engineering Innovations §8 Zero-Allocation Fast Hex Formatting](engineering.md#8-zero-allocation-fast-hex-formatting), [Engineering Innovations §14 Native OGC GeoParquet 1.1 Exporter](engineering.md#14-native-ogc-geoparquet-11-exporter-stack-allocated-wkb-hexagons--projjson)*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/mod.rs) | Module declarations and public re-exports (`fast_hex_u64`, `parse_hex_u64`, `cell_to_wkb`, `h3_index_to_wkb`). |
| [`fast_hex.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/fast_hex.rs) | Zero-allocation hexadecimal formatting (`fast_hex_u64`) and parsing (`parse_hex_u64`) between 64-bit integer H3 cell IDs and lowercase hexadecimal ASCII strings using a 16-byte stack lookup table. |
| [`wkb.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/encoding/wkb.rs) | Stack-allocated OGC 2D Polygon WKB serialization (`cell_to_wkb`, `h3_index_to_wkb`). Converts H3 cell boundaries directly into a 192-byte stack buffer (`WkbBuf`) in ~10–15 ns with zero heap allocations, accommodating 5-to-6 vertex Class II cells as well as Class III (odd) resolutions with up to 10 boundary vertices (189 bytes) and icosahedron-edge crossings (141–157 bytes). |

---

### [`src/ffi/`](https://github.com/dmuldrew/raster_h3/tree/main/src/ffi) — DuckDB C-API Foreign Function Interface
*Maps to: [Engineering Innovations §10 Native DuckDB Parallelism](engineering.md#10-dynamic-work-stealing-parallelism--duckdb-init_local-pipeline)*

| File | Responsibility |
| :--- | :--- |
| [`duckdb_c.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/ffi/duckdb_c.rs) | Low-level `extern "C"` declarations and type definitions for the DuckDB C API (database, connection, table functions, scalar functions, bind/init/function info, vectors, data chunks, logical types). Includes inline/pointer string layout handling (`duckdb_string_t`). |
| [`spatial_detect.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/ffi/spatial_detect.rs) | Dynamic runtime detection via `dlsym` (POSIX) / `GetProcAddress` (Windows) of the host DuckDB library version and whether DuckDB ≥ v1.5 (built-in `GEOMETRY` type) or the `spatial` extension is loaded. |

---

### [`src/raster/`](https://github.com/dmuldrew/raster_h3/tree/main/src/raster) — GeoTIFF I/O, Cloud Streaming & Mosaic Ingestion
*Maps to: [Engineering Innovations §7 Zero-Copy memmap2](engineering.md#7-zero-copy-memmap2--async-prefetching), [Engineering Innovations §11 Bounded Buffer Pool](engineering.md#11-bounded-lock-free-buffer-recycling-pool-decodingbufferpool), [Engineering Innovations §12 Single-Hop Bounded Prefetcher](engineering.md#12-single-hop-bounded-in-order-prefetcher-orderedprefetchqueuet), [Engineering Innovations §13 Cloud-Native COG & Mosaic Ingestion](engineering.md#13-cloud-native-cog--mosaic-ingestion)*

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
*Maps to: [Engineering Innovations §1 Horizon Eviction](engineering.md#1-southernmost-scan-line-horizon-eviction), [Engineering Innovations §2 Scanline Lookahead](engineering.md#2-h3-scanline-lookahead-algorithm), [Engineering Innovations §4 Linear Longitude Stepping](engineering.md#4-linear-longitude-stepping), [Engineering Innovations §5 In-Register Run Accumulation](engineering.md#5-in-register-run-accumulation), [Engineering Innovations §6 Branchless Min/Max](engineering.md#6-branchless-hardware-minmax), [Engineering Innovations §9 ROI Chunk Pruning](engineering.md#9-roi-bounding-box-chunk-pruning)*

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
*Maps to: [Engineering Innovations §8 Fast Hex Formatting](engineering.md#8-zero-allocation-fast-hex-formatting), [Engineering Innovations §10 DuckDB init_local Pipeline](engineering.md#10-dynamic-work-stealing-parallelism--duckdb-init_local-pipeline), [Engineering Innovations §14 GeoParquet Exporter](engineering.md#14-native-ogc-geoparquet-11-exporter-stack-allocated-wkb-hexagons--projjson)*

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
*Maps to: [Engineering Innovations §14 Native OGC GeoParquet 1.1 Exporter](engineering.md#14-native-ogc-geoparquet-11-exporter-stack-allocated-wkb-hexagons--projjson)*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/mod.rs) | Module declarations and public re-exports (`build_geoparquet_metadata`, `run_parquet_streaming_pipeline`, `ParquetStreamer`, `ParquetRowGroupBuffer`, `H3ParquetWriter`, etc.). |
| [`geoparquet_metadata.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/geoparquet_metadata.rs) | Specification-compliant OGC GeoParquet 1.1 JSON metadata builder (`build_geoparquet_metadata`) embedded in Parquet `FileMetaData`, including official PROJJSON `OGC:CRS84` datum ensemble definitions, planar edge definitions, and per-column bounding boxes. |
| [`pipeline.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/pipeline.rs) | Double-buffered channel streaming pipeline (`run_parquet_streaming_pipeline`, `run_parquet_streaming_pipeline_with_progress`), streaming source abstraction (`ParquetStreamer`), row group buffer abstraction (`ParquetRowGroupBuffer`), in-memory spatial sorting by H3 index, and low-level typed column writers. |
| [`writer.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/parquet/writer.rs) | Schema-specific columnar row group buffers (`ContinuousRowGroupBuffer`, `CategoricalRowGroupBuffer`), export configuration (`ParquetExportConfig`), and high-level export facade (`H3ParquetWriter`). |

---

### [`src/pmtiles/`](https://github.com/dmuldrew/raster_h3/tree/main/src/pmtiles) — PMTiles v3 & MVT Vector Tile Generation
*Maps to: [Engineering Innovations §10 Native DuckDB Parallelism](engineering.md#10-dynamic-work-stealing-parallelism--duckdb-init_local-pipeline)*

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
*Maps to: [Engineering Innovations §1 Horizon Eviction](engineering.md#1-southernmost-scan-line-horizon-eviction), [Engineering Innovations §10 Native DuckDB Parallelism](engineering.md#10-dynamic-work-stealing-parallelism--duckdb-init_local-pipeline)*

| File | Responsibility |
| :--- | :--- |
| [`mod.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/transcode/mod.rs) | Module declarations and public re-exports (`process_parquet_to_pmtiles`, `RowGroupExtent`, `scan_row_group_h3_extent`). |
| [`parquet_tiler.rs`](https://github.com/dmuldrew/raster_h3/blob/main/src/transcode/parquet_tiler.rs) | Parquet-to-PMTiles v3 transcoding engine (`process_parquet_to_pmtiles`). Reads pre-aggregated H3 records from Parquet files, pre-scans row group extents, and transcodes them into multi-zoom PMTiles archives using streaming latitude eviction to bound memory. |
