# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Release quality gates covering lint, MSRV, engine/community builds, blocking dependency policy, package/version checks, wheels, fuzzing and Miri; weekly advisory and safety checks.
- GitHub release SHA-256 checksums and build provenance, validated wheel assets, contributor issue forms and PR checklist.
- WKB/EWKB, GeoTIFF metadata and chunk-predictor fuzz targets, generated conservation/merge/CRS tests, scoped Arrow Miri checks and Criterion streaming baselines.
- Core-only crates.io package policy and automated release metadata consistency checks (without publishing packages).

- CI `lint` job (rustfmt, Clippy with `-D warnings` including the `pg-extension` feature, rustdoc with `-D warnings`) and `msrv` job.
- Workspace `rust-version = "1.94"` (required by h3o 0.11) and shared `[workspace.lints]`.
- `SECURITY.md` and Dependabot configuration for Cargo, GitHub Actions and pip.
- `raster_h3_core::error::catch_unwind_or` for panic-safe `extern "C"` entry points.
- `CrsTransformer::bounds_certified` and certified (Lipschitz-enclosure) WGS84 bounds for transverse Mercator sources (`utm`, `etmerc`, exact `tmerc` on named ellipsoids), so bbox pruning stays sound for them. Explicit `+towgs84` shifts are certified only up to 1.5 km translation, 10″ rotation and 50 ppm scale.
- `spill_run_counts()` on the continuous and categorical multi-resolution streamers: spill runs per configured resolution.

### Changed

- TM rejection certificates now follow proj4rs scale precedence (`k0`, then `k`) and fail open for duplicate or non-canonical PROJ parameters, preventing incorrect bbox pruning.

- Replaced unmaintained `fxhash` with maintained `rustc-hash`, removed obsolete advisory exceptions, and denied wildcard dependency requirements.
- Replaced brittle two-second streaming test assertions with correctness-only tests and separate informational benchmarks.

### Fixed

- TIFF sample unpacking now rejects truncated buffers, overflowing dimensions, invalid strides and undersized destinations before indexing or copying; the new fuzz target caught the truncated-input regression.

- PostgreSQL scalar functions, `pg_srf_continuous_next`, the `fmgr` text/bytea functions, Python scalar and `h3_polygon_to_cells` exports, and Arrow stream / batch-reader release no longer abort the host process if Rust code panics.
- Replaced `AtomicUsize::try_update` (Rust 1.95) with `fetch_update` so the crate builds on its declared MSRV.
- Broken rustdoc link in `RemoteHttpSource::read_exact_range`; all Clippy warnings.
- `GeoTransform::coord_to_pixel` used an absolute determinant cutoff that rejected valid fine-resolution grids (e.g. 1e-8 degree pixels); it is now scale-relative and rejects non-finite affines. Opening a GeoTIFF now rejects a non-finite or singular affine transform.
- Sampled PROJ bounds could reject valid data: `transform_rect_bounds` now returns the whole globe if any probe fails, and `rejection_bounds` fails open for uncertified bounds instead of padding them by 10%.
- WGS84 and Web Mercator rect bounds kept unwrapped corner longitudes while per-sample bbox tests wrap every longitude, so a sample on a chunk edge could fall one ulp outside its chunk's rejection bounds; the corners are now wrapped the same way, and a corner just below 180° that rounds to -180° falls back to full longitude instead of reversing the bounds.

## [0.4.0] - 2026-10-21

### Added
- DuckDB community-extension build scaffolding: root `Makefile` and `extension-ci-tools` submodule (DuckDB's `c_api_extensions` makefiles), so `make configure && make release` produces `build/release/raster_h3.duckdb_extension` (stable C API, minimum DuckDB v1.2.0); `test/sql/raster_h3.test` sqllogictest run by `make test_release` ([DuckDB](docs/engines/duckdb.md#community-extension-build)).
- `h3_polygon_to_cells(geometry, resolution [, containment])` scalar (h3o polyfill) in DuckDB (`BLOB`/`GEOMETRY` → `UBIGINT[]`) and Python, with the `contains_centroid` (default), `contains_boundary`, `intersects_boundary` and `covers` containment modes ([API Reference](docs/api-reference.md#h3_polygon_to_cells)). Backed by `raster_h3_core::scalar_ops::h3_polygon_to_cells_wkb` and a new WKB polygon reader.
- **Multi-engine Cargo workspace** ([Engines Overview](docs/engines/README.md)): the engine moved to `crates/raster-h3-core`; the DuckDB extension to `crates/raster-h3-duckdb`. The root `raster_h3` crate re-exports every member and still builds `libraster_h3` for DuckDB.
- `raster-h3-core` `columnar` module: `RecordBatch`/`ColumnSchema` model, `ContinuousBatchReader` / `CategoricalBatchReader`, Arrow C Data and C Stream interface, `RH3IPC01` binary stream and NDJSON writer; `scalar_ops` module with H3 helpers shared by all engines ([Core](docs/engines/core.md)).
- `raster-h3-datafusion`: DataFusion-style `TableProvider` / `ExecutionPlan` / `ScalarUDFImpl` with projection, filter and limit pushdown ([DataFusion](docs/engines/datafusion.md)).
- `raster-h3-python` and the `raster-h3` Python package: Arrow PyCapsule stream, Polars IO source (`scan_raster_h3`, `read_raster_h3`), H3 helpers, Parquet/PMTiles exporters; `py3-none-<platform>` wheels built by `.github/workflows/wheels.yml` ([Python & Polars](docs/engines/python-polars.md)).
- `raster-h3-pg`: PostgreSQL 13–18 extension with `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate` SRFs and H3 scalar functions, plus a Docker-based real-server test (`scripts/pg_e2e.sh`, `tests/it/pg_extension_e2e.rs`) ([PostgreSQL](docs/engines/postgresql.md)).
- `raster-h3-cli`: `ipc`, `jsonl`, `parquet` and `pmtiles` subcommands with strict argument validation ([CLI](docs/engines/cli.md)).
- `tests/it/multi_engine_e2e.rs`: cross-engine parity on shared synthetic rasters.
- Built-in European national-grid EPSG codes with Helmert datum shifts: `27700` (OSGB36), `28992` (Amersfoort / RD New), `31466`–`31469` (DHDN Gauss-Kruger), `23028`–`23038` (ED50 / UTM); and ETRS89 codes treated as WGS84: `2154`, `3035`, `25828`–`25838` ([Datum Shifts](docs/crs-and-projection.md#datum-shifts)).

### Changed
- Upgraded tiff to 0.11. 16-bit floating-point GeoTIFFs, which tiff 0.9 could not decode, are now read and widened to `f32`; the TIFF floating-point predictor (Predictor 3) is now implemented in `raster_h3_core::raster::predictor` because tiff no longer exports it.
- Upgraded h3o to 0.11 (polyfill now uses `h3o::geom::TilerBuilder`) and parquet to 60 (`flate2-rust_backend` codec feature). With h3o 0.11, `h3_polygon_to_cells(..., 'intersects_boundary')` returns the enclosing cell for a polygon that fits inside one cell, matching `covers`, instead of an empty list.
- Workspace, Python package and PostgreSQL extension versions bumped to `0.4.0` (PostgreSQL install script renamed to `sql/raster_h3--0.4.0.sql`; there is no 0.3.0 release).
- The DuckDB community-extension manifest moved from `community_extension/` to `packaging/duckdb-community/description.yml`; it now points at `dmuldrew/raster_h3`, declares version `0.4.0` and excludes the opt-in musl and `windows_amd64_rtools` platforms.
- Repository and homepage URLs point to `github.com/dmuldrew/raster_h3` (previously `raster_h3_hexification`).
- `h3_parquet_to_pmtiles` (and `process_parquet_to_pmtiles` / `convert_to_pmtiles`) now return an error naming the available columns when an explicit `h3_column` is not found, instead of silently falling back to auto-detection or column 0.
- PROJ strings with 3- or 7-parameter Helmert shifts (`+towgs84=...`, or a named datum that implies one such as `+datum=OSGB36`) are now transformed via proj4rs instead of rejected. Grid-based datums (`+nadgrids` other than `@null`, `+datum=NAD27`) and unknown `+datum` names are still rejected with `CrsError`; a non-zero shift never takes a fast path ([Datum Shifts](docs/crs-and-projection.md#datum-shifts)).
- Bound decoded chunk memory in bytes, not only by queue length: the mosaic prefetcher charges buffers being decoded, queued, aggregated and idle against `decode_budget_bytes` (SQL `decode_budget_mb`, default 256 MiB). Reservations are granted in job order before decoding; a chunk larger than the budget is decoded only when no other is held. Peak usage is reported as `StreamProfile::peak_decoded_bytes`.
- Account pending compaction groups in bytes; they now count toward `peak_active_bytes`, and `StreamProfile` reports `peak_compaction_bytes` and `forced_compaction_flushes`. An opt-in `compaction_budget_bytes` (SQL `compaction_budget_mb`) decomposes the groups nearest completion into child records once exceeded. Samples and totals are unchanged, but which groups compact then depends on batch timing and can vary between runs; unset, compaction stays deterministic.
- Index pending compaction groups with per-resolution heaps, so streaming completion checks and the published horizon cost O(log n) per finished group instead of a scan of every pending group per batch. Streaming completion flushes stop once the output buffer reaches its byte limit and resume on the next call.
- Fuse supersample projection, H3 assignment, and accumulation with reusable per-pixel scratch buffers; avoid repeated certification work and preserve individual quantile sample updates.
- Use absolute pixel coordinates consistently for indexing, sequential span discovery, and supersampling; remove incremental coordinate stepping and obsolete width-prediction state.
- Remove unused profiling counters, sampling-bound helpers, and the `raster_h3::h3` compatibility alias. Rust callers should use `h3o` directly; callers of removed scanline helpers must migrate to the stateless indexing/span functions.
- Replace Hawaii aggregation goldens with per-pixel reference comparisons. External-data tests remain ignored by default and fail explicitly if invoked without their datasets.
- Document aggregation budgets, spill behavior, and uncertified projected eviction without unconditional process-memory or geometric-skip claims.
- DuckDB scalar overloads (`h3_is_valid`, `h3_to_wkb`, `h3_cell_to_parent`, `h3_to_geometry`, `h3_cell_to_geometry`, `h3_polygon_to_cells`) are registered as scalar function sets. DuckDB before 1.5 rejected the second registration under a name, so only the first overload (e.g. `h3_is_valid(UBIGINT)`) was available there.
- Reorganise the documentation for the multi-engine workspace: the README is engine-neutral (Engines section after the introduction, topic deep dives at the end), the worked example has DuckDB and Polars versions, DuckDB install/Docker/SQL recipes/client examples moved to the [DuckDB deep dive](docs/engines/duckdb.md), and the Python, PostgreSQL, CLI and DataFusion deep dives gained matching recipes and troubleshooting sections. `docs/api-reference.md` is now titled the DuckDB SQL API Reference.

### Fixed
- Reject fractional, nonfinite, and out-of-range integer NoData markers instead of truncating or saturating them to valid pixel values.
- Return complete remote byte reads across cache blocks and use a cached full-file HTTP response for large reads.
- Preserve supersamples that fall inside a bounding box when their pixel center falls outside it.
- Reject compact multi-resolution requests that would emit both a requested parent cell and a compacted child-cell parent.
- Validate HTTP partial-content status, range offsets, total size, and body length before accepting remote COG bytes.
- Preserve finite Float32 values when the NoData marker is NaN, including span statistics and variance.
- Apply full affine coordinates to rotated/sheared raster centers and subpixel samples, including WGS84 and Web Mercator. Disable north-up scanline shortcuts for these rasters.
- Mix all 64 bits of H3 indices when choosing aggregation shards, preventing resolutions 0–13 from collapsing into a single shard. Added distribution tests across resolutions 0–15 and local neighborhoods, plus continuous/categorical merge and eviction checks with one and four Rayon threads.
- Propagate chunk decoding and remote prefetch errors through continuous/categorical streamers, DuckDB table functions, and Parquet/PMTiles exporters. Failed streams continue returning an error on subsequent reads instead of reporting successful EOF.
- Deliver prefetch initialization failures even when earlier chunk slots are still empty.
- Replace approximate nearest-centroid subpixel assignment and core-span checks with direct H3 indexing.
- Publish Parquet exports only after successful completion, preserving an existing destination on stream failure.

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
- **Rust API change:** `fetch_next_batch`, `drain_completed_into`, and `advance_until_completed` now return `Result`; callers must propagate or handle failures (for example, `streamer.fetch_next_batch(2048)?`). The `ParquetStreamer` and record-queue interfaces also propagate `Result`.
- Removed the approximate `H3NeighborDiskCache` and `can_use_neighbor_cache` helpers and their unused per-row geometry fields. Boundary-heavy supersampling may be slower with exact indexing; center-sampled scanline aggregation retains its existing fast path.
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

[Unreleased]: https://github.com/dmuldrew/raster_h3/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/dmuldrew/raster_h3/compare/v0.2.0...v0.4.0
[0.2.0]: https://github.com/dmuldrew/raster_h3/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dmuldrew/raster_h3/releases/tag/v0.1.0
