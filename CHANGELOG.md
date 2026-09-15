# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Single-Pass Multi-Resolution Fusion**:
  - Concurrent multi-resolution aggregation (e.g. H3 Res 8 + Res 9) in a single unified scanline pass over each row slice.
  - Common sub-span stepping ($\text{step\_end} = \min_i(\text{span\_ends}[i])$) with single SIMD vectorized accumulation across active resolutions.
  - Multi-resolution super-sampling core intersection lookahead with boundary isolation.
  - Unified categorical run-length and class frequency evaluation.
  - Zero-overhead fast path preserved for single-resolution aggregation (`num_res == 1`).
  - Delivers up to 1.31× speedup on in-memory rasters and 1.17× speedup on 257-Megapixel datasets while halving I/O reads and tile decompression passes.
- **Parallel Background Decompression & Ingestion Optimization (Option 25)**:
  - **Lock-Free Buffer Recycling Pool**: Integrated `crossbeam_deque::Injector<DecodingResult>` for concurrent work-stealing buffer reuse across worker threads, eliminating continuous buffer allocation and deallocation across 15,840+ chunks.
  - **Single-Hop Bounded In-Order Queue (`OrderedPrefetchQueue<T>`)**: Removed the intermediate OS collector thread and dual-channel bottleneck in favor of direct worker-to-ring-buffer deposits, reducing thread context switches and providing zero-allocation batch drains (`drain_chunk_batch_into`).
  - **Adaptive Batch Horizon Sizing & Fixed-Array Sharding**: Scaled chunk batch horizons based on hardware parallelism (`(threads * 8).clamp(64, 256)`) and replaced dynamic shard allocation with fixed-size arrays (`std::array::from_fn`), eliminating 250k+ allocations.
  - **Hawaii Benchmark Throughput Gains**: Delivers +8.3% speedup on continuous scans (`CFL_HI.tif`), +5.7% speedup on categorical scans (`LF2024_FBFM40_HI.tif`), +9.7% speedup on dual-pyramid generation, and +21.1% faster Shannon entropy calculation.
- **Remote Cloud-Optimized GeoTIFF (COG) & S3 Streaming**:
  - Direct zero-copy streaming from remote HTTP, HTTPS, and AWS S3 sources without downloading full raster files to local disk.
  - HTTP range request prefetching and spatial chunk request coalescing with retry/backoff resilience.
- **Multi-File Raster Mosaics & Cutline Overlap Resolution**:
  - Multi-file mosaic ingestion via glob patterns, comma-separated lists, and GDAL VRT XML specifications.
  - Flexible spatial overlap rules: `Cutline` (Voronoi bisector partitioning with zero double-counting), `First` (painter's algorithm precedence), and `Average` (multi-observation blending).
  - Multi-threaded `PrefetchedMosaicReader` providing globally latitude-interleaved chunk decoding.
- **OGC GeoParquet 1.1 Specification Support**:
  - Built-in Parquet streaming writer emitting 125-byte closed WKB 2D Polygon hexagon geometries.
  - Generates compliant OGC GeoParquet 1.1 JSON metadata with PROJJSON `OGC:CRS84` datum ensemble and coordinate systems in Parquet FileMetaData.
  - Supports both standard (10-column) and compact (7-column) formats for continuous and categorical exports.
- **On-the-Fly Multi-Band Spectral Index Formulas**:
  - Streaming calculation of `NDVI`, `NDWI`, `NBR`, and `EVI` directly during raster ingestion.
  - $|denom| \le 10^{-12}$ division-by-zero singularity protection preventing `NaN` and `Inf` emissions.
- **DuckDB C-FFI String & Memory Safety**:
  - Verified 16-byte ABI layout for `duckdb_string_t` across inlined (lengths 0..=12), boundary (13, 15), and heap-allocated (32) strings.
  - Safe null-pointer and invalid UTF-8 fallback, with verified `delete_boxed` destructor lifecycle on custom bind state.
- **Comprehensive Test Suite Hardening**:
  - *Category 1*: `PrefetchedMosaicReader` multi-worker concurrency, in-order job delivery, buffer recycling, and clean early drop.
  - *Category 2*: OGC GeoParquet 1.1 JSON metadata deep validation and column projection row reading.
  - *Category 3*: Antimeridian crossing ($\pm 180^\circ$) UTM zone continuity, Arctic/Antarctic polar stereographic projections, and malformed PROJ handling.
  - *Category 4*: Categorical histogram 16-slot inline array vs heap `HashMap` spillover and Shannon entropy theoretical bounds ($\ln K$, $0.0$).
  - *Category 5*: SIMD Deflate and fast LZW corrupted byte stream fuzzing, truncated payload detection, and buffer auto-resizing.
- **Streaming Quantile Sketches**:
  - DDSketch-inspired log-key histogram (`QuantileSketch`) for streaming P50, P90, P95, P99, IQR, deciles, and custom percentile targets with sub-1% relative error.
  - Configurable via `quantiles` parameter (e.g. `quantiles := 'p50,p90,p99'`, `quantiles := 'iqr'`, `quantiles := 'deciles'`).
  - Zero-cost disabled path when quantiles are not requested.
- **Category Remapping**:
  - `CategoryRemapper` supporting exact (`10=Forest`), range (`20-29=Urban`), and wildcard (`*=Other`) mapping syntax for reclassifying categorical rasters during ingestion.
  - Configurable via `remap` / `remapping` / `classes` parameter.
- **Predicate Pushdown & Query Filters**:
  - Server-side filtering via `h3_cell`, `h3_hex`, `min_mean`, `max_mean`, `min_count`, and `min_majority_fraction` parameters.
  - `bbox` string parameter for bounding box specification (alternative to individual coordinate parameters).
- **Native WKB Geometry Emission**:
  - 125-byte OGC-compliant closed WKB 2D Polygon hexagon geometries via `geom := true` parameter.
  - Enables direct DuckDB Spatial extension interop and GeoParquet output.
- **Direct Native Parquet Export**:
  - `h3_raster_to_parquet` table function for single-command GeoTIFF → Parquet streaming export.
  - Supports OGC GeoParquet 1.1 metadata, compact format, and configurable compression (`snappy`, `zstd`, `gzip`, `lz4`, `brotli`).

### Changed
- **Strict CRS Validation on GeoTIFF Ingestion**:
  - Eliminated silent fallback to `Wgs84Identity` when GeoTIFF metadata lacks embedded CRS definitions or uses unrecognized projection codes.
  - Ingestion now fails immediately with `RasterH3Error::CrsError` instructing the user to supply `crs` or `source_crs` explicitly (e.g. `crs := 'EPSG:4326'`, `crs := 'EPSG:5070'`).
  - Prevents silent data corruption, invalid hexagon emission, or empty result sets when ingesting rasters in projected meter coordinates.
  - User-specified `custom_crs` / `crs` now takes strict priority over any embedded raster metadata.
- **`bind_utils` Modularization**:
  - Split monolithic bind state management into focused submodules: `bind_helper`, `chunk_writer`, `lifecycle`, `parsing`, `record_queue`, and `registration`.
- **Codebase Refactoring**:
  - Centralized NoData casting and dispatch into `src/aggregator/nodata.rs` with unified `dispatch_decoding!` macro.
  - Macro-collapsed 10-arm buffer decoding and 6-arm direct-decoding branches in `src/raster/geotiff.rs`.
  - Added structured `RasterH3Error::UnsupportedEpsg { code, detail }` error variant.
  - Unified continuous and categorical scanline loops into generic `ScanlineEngine` trait with `scanline_walk` in `src/aggregator/multi_horizon/walker.rs`.
  - Consolidated test GeoTIFF generation across all integration test suites into generic `TestGeoTiffBuilder`.

## [0.2.0] - 2026-08-30

### Added
- **PMTiles v3 Leaf Directories Implementation**:
  - Implemented automatic chunking of PMTiles directory indices into 4,096-entry leaf directory blocks with `run_length = 0` root pointer entries.
  - Compressed root directories from ~87 KB down to ~122 bytes, strictly adhering to the PMTiles v3 initial 16 KB HTTP range request specification (`bytes=0-16383`) and eliminating `unexpected EOF` buffer truncation in `pmtiles.js`.
- **Per-Resolution Statistical Metadata (`h3_resolution_stats`)**:
  - Automatically computes and embeds multi-resolution statistical envelopes (cell count, min, max, mean, stddev, sum, purity, distinct classes, Shannon entropy) directly in PMTiles JSON metadata.
- **Enhanced PMTiles Studio Viewer (`pmtiles_viewer/`)**:
  - Dual Continuous and Categorical visualization modes with auto-detection from metadata.
  - LANDFIRE FBFM40 fuel model preset colormaps, custom category label/color editor, and class-level filtering.
  - Offline local file drag-and-drop and file picker support via `pmtiles.FileSource` (FileReader API) allowing 100% offline inspection on `file://` URLs.
  - Interactive 3D hexagon extrusion, wireframe mesh overlay, and resolution-adaptive color normalization.
  - Python HTTP server with wildcard CORS and byte-range request streaming support.

### Changed
- **Single-Pass Multi-Resolution Vector Tiler**:
  - Eliminated premature mid-stream tile eviction to ensure all vector tiles are accumulated across the entire raster and encoded with unique Hilbert/ZXY tile IDs.
  - Expanded categorical class frequency tracking from 20 up to 256 classes per hexagon.

### Fixed
- **PMTiles v3 Web Compatibility**:
  - Resolved `Search engine null is not supported` extension collisions and added solid fallbacks in WebGL shaders for unmapped categorical classes.
  - Added MapLibre GL error interceptors and diagnostic visual wireframes for instant debugging.

## [0.1.0] - 2026-08-26

### Added
- **Continuous raster aggregation** via `h3_raster_continuous_aggregate` (alias: `h3_raster_continuous`)
  - Single-pass Welford online statistics: mean, stddev, count, min, max, sum
  - Southernmost Scan-Line Horizon Eviction for constant O(scan front) memory (~15 MB)
  - Row-constant latitude hoisting eliminating 99.8% of coordinate projection math
  - Linear longitude stepping with single-cycle addition per pixel
  - In-register run accumulation eliminating ~98% of hash table probes
  - Scanline run-skipping for 8–16 pixels per CPU cycle throughput
  - Branchless hardware min/max via `minsd`/`maxsd` instructions
- **Categorical raster aggregation** via `h3_raster_categorical_aggregate` (alias: `h3_raster_categorical`)
  - Wide format: majority class, majority fraction, unique class count, JSON histogram
  - Long format: normalized (hex, category, count, fraction) rows
  - Shared streaming engine with categorical-specific `CategoricalAccumulator`
  
### Changed
- Replaced `SpatialCoherenceCache` with `H3ScanlineLookahead` algorithm, resolving a massive spherical trigonometry bottleneck by using exponential jump-guessing and binary search, resulting in a ~2.6x overall throughput speedup.

### Added
- **Sub-pixel super-sampling** with 8 presets: center, RGSS, hexagonal, Gaussian PSF, 5-point quincunx, 8-rooks, 9-point grid, 16-point grid
- **Scalar helper functions**: `h3_to_string`, `string_to_h3`, `h3_to_lat`, `h3_to_lng`, `h3_get_resolution`
- **Spatial ROI bounding box pruning** via `min_lon`, `min_lat`, `max_lon`, `max_lat` parameters
- **CRS override support** via `source_crs` parameter with proj4rs pure-Rust reprojection
- **Zero-copy I/O** via `memmap2` with async double-buffered prefetching
- **Docker container** with multi-stage build, bundled DuckDB CLI, and demo script
- **Query planner integration**: cardinality estimation and parallel `init_local` distribution

[Unreleased]: https://github.com/dmuldrew/raster_h3_hexification/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/dmuldrew/raster_h3_hexification/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dmuldrew/raster_h3_hexification/releases/tag/v0.1.0
