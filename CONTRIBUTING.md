# Contributing to raster_h3

Thank you for your interest in contributing! This guide will help you get started.

## Development Setup

### Prerequisites
- [Rust](https://rustup.rs/) (Edition 2021+, stable toolchain)
- [DuckDB CLI](https://duckdb.org/) (Version 1.0.0+)
- [Docker](https://www.docker.com/) (optional, for container testing)

### Build & Test
```bash
# Build the DuckDB extension (root crate) and the `raster-h3` CLI
cargo build --release

# Run the full workspace test suite
cargo test --release --workspace

# Core engine only (does not compile DuckDB, DataFusion, PostgreSQL or Python)
cargo test -p raster-h3-core

# One module of a crate's integration binary (tests/it/<module>.rs)
cargo test -p raster-h3-core --test it pmtiles

# Cross-engine parity and the Docker-based PostgreSQL test (root crate)
cargo test -p raster_h3 --test it multi_engine_e2e
cargo test -p raster_h3 --test it pg_extension_e2e -- --ignored --nocapture

# Run tests with output visible
cargo test --workspace -- --nocapture
```

### Docker
The Docker build automatically runs the entire test suite in an isolated container environment:

```bash
# Build and run test suite in Docker
docker build -t raster_h3:latest .

# Run the interactive DuckDB CLI container with the preloaded extension and demo dataset
docker run -it --rm raster_h3:latest
```

## Project Structure

```
Cargo.toml                   # Workspace manifest + root `raster_h3` facade crate
src/                         # Root crate: libraster_h3 (DuckDB extension) and `raster-h3` CLI binary
├── lib.rs                   # Re-exports workspace crates, keeps DuckDB entry symbols
└── bin/                     # Standalone CLI utilities
    ├── conus_batch_and_evict.rs  # CONUS-scale batch ingestion with eviction tracking
    ├── convert_to_pmtiles.rs     # High-speed standalone TIFF-to-PMTiles converter
    ├── download_burn_probability.rs # USFS Burn Probability tile downloader
    ├── inspect_tif.rs       # GeoTIFF metadata & CRS inspection tool
    └── package_extension.rs # Pure-Rust DuckDB extension packager & footer generator

crates/
├── raster-h3-core/          # Engine: raster I/O, CRS, aggregation, columnar, Parquet, PMTiles
├── raster-h3-duckdb/        # DuckDB C-API extension (ffi/, functions/)
├── raster-h3-datafusion/    # TableProvider / ExecutionPlan / ScalarUDF adapters
├── raster-h3-python/        # C-ABI cdylib, python/raster_h3 package, hatch wheel build
├── raster-h3-pg/            # PostgreSQL fmgr extension, raster_h3.control, sql/
└── raster-h3-cli/           # raster-h3-cli executable (ipc, jsonl, parquet, pmtiles)

scripts/pg_e2e.sh            # Build + run the PostgreSQL extension in Docker

crates/raster-h3-core/src/
├── aggregator/              # Core aggregation & scanline horizon streaming
│   ├── accumulator.rs       # Welford online statistics (continuous)
│   ├── categorical.rs       # Categorical frequency accumulator & streamer
│   ├── h3_scanline.rs       # Exact H3 indexing & sequential span discovery
│   ├── horizon_streamer.rs  # Southernmost scanline horizon eviction engine
│   ├── multi_horizon/       # Multi-resolution concurrent horizon aggregator
│   │   ├── config.rs        # Configuration & quantile targets
│   │   ├── continuous.rs    # Continuous numeric chunk aggregation
│   │   ├── continuous_streamer.rs # Multi-resolution continuous horizon streamer
│   │   ├── categorical.rs   # Categorical landcover chunk aggregation
│   │   ├── categorical_streamer.rs # Multi-resolution categorical horizon streamer
│   │   ├── controller.rs    # Resolution controller & batch horizon sizing
│   │   ├── sharded_map.rs   # Fixed-array sharded accumulator map
│   │   ├── spectral.rs      # On-the-fly spectral formulas (NDVI, NDWI, NBR, EVI)
│   │   └── walker.rs        # Generic ScanlineEngine trait & unified scanline_walk
│   ├── nodata.rs            # NodataCast trait, dispatch_decoding! macro, all-nodata checks
│   ├── quantiles.rs         # DDSketch-inspired streaming QuantileSketch (P50, P90, IQR)
│   ├── remap.rs             # CategoryRemapper: exact, range, wildcard mapping rules
│   ├── sampling.rs          # Sub-pixel super-sampling presets (RGSS, Hex, 8-Rooks, Gaussian)
│   └── simd.rs              # SIMD-vectorized span accumulation for Welford statistics
├── crs/                     # Geodetic reprojection layer
│   ├── mod.rs
│   └── transformer.rs       # 3-tier CRS reprojection (Identity → Analytical → PROJ4)
├── encoding/                # Shared zero-allocation encoding & geometry serialization
│   ├── fast_hex.rs          # Zero-allocation SIMD/LUT hex formatter & parser
│   ├── wkb.rs               # Stack-allocated OGC WKB 2D Polygon hexagon geometry serialization
│   └── mod.rs
├── parquet/                 # Native OGC GeoParquet 1.1 streaming exporter
│   ├── geoparquet_metadata.rs # Specification-compliant GeoParquet 1.1 JSON metadata builder
│   ├── pipeline.rs          # Double-buffered channel streaming pipeline & row-group buffers
│   ├── writer.rs            # High-level H3ParquetWriter facade & columnar schema buffers
│   └── mod.rs
├── pmtiles/                 # Native PMTiles v3 & Mapbox Vector Tile (MVT) generation
│   ├── features.rs          # H3 feature property extraction & JSON encoding
│   ├── mvt.rs               # Pure-Rust Protobuf MVT vector tile encoder
│   ├── pyramid.rs           # Multi-resolution zoom-level pyramid builder
│   ├── tiler.rs             # Multi-resolution pyramid tiling & Hilbert indexer
│   ├── writer.rs            # PMTiles v3 container writer & header serializer
│   └── mod.rs               # Re-exports parquet_tiler from crate::transcode
├── raster/                  # GeoTIFF, COG, and Mosaic streaming
│   ├── geotiff.rs           # Streaming chunk reader with SIMD Deflate & fast LZW
│   ├── geotransform.rs      # Affine geotransform & coordinate mapping
│   ├── http_range.rs        # HTTP/S3 byte-range client & chunk coalescing
│   ├── metadata.rs          # GeoTIFF tag & IFD parser (GeoKeys, EPSG, WKT)
│   ├── mosaic.rs            # Multi-file mosaic reader & overlap rules
│   ├── predictor.rs         # Horizontal & floating-point TIFF predictor decoders
│   ├── prefetch.rs          # Lock-free buffer pool (Injector) & OrderedPrefetchQueue
│   ├── remote_prefetch.rs   # Asynchronous range request prefetch queue for COGs
│   └── mod.rs
├── transcode/               # Cross-format dataset transcoding pipelines
│   ├── parquet_tiler.rs     # Parquet-to-PMTiles conversion with streaming horizon eviction
│   └── mod.rs
├── columnar/                # RecordBatch model, Arrow C Data/Stream, RH3IPC01 & NDJSON
│   ├── arrow_c.rs           # ArrowSchema / ArrowArray / ArrowArrayStream
│   ├── ipc.rs               # RH3IPC01 binary stream + JSONEachRow writer
│   └── mod.rs               # Schemas, ColumnArray, Continuous/CategoricalBatchReader
├── scalar_ops.rs            # Engine-neutral H3 helpers & parameter parsers
├── error.rs                 # Error types & conversions
└── lib.rs                   # Engine module declarations

crates/raster-h3-duckdb/src/
├── ffi/                     # DuckDB C-FFI bindings
│   ├── duckdb_c.rs          # Low-level DuckDB C API types & function pointers
│   ├── spatial_detect.rs    # DuckDB Spatial extension detection & geometry interop
│   └── mod.rs
├── functions/               # DuckDB table & scalar function registrations
│   ├── bind_utils/          # Shared bind state management & parameter infrastructure
│   │   ├── bind_helper.rs   # BindHelper: common bind lifecycle and parameter extraction
│   │   ├── chunk_writer.rs  # ChunkWriter: DuckDB DataChunk row emission
│   │   ├── lifecycle.rs     # Init/cleanup state management & thread-local allocation
│   │   ├── parsing.rs       # Parameter parsing (CRS, bbox, sampling, spectral formulas)
│   │   ├── record_queue.rs  # Bounded record queue for streaming row emission
│   │   ├── registration.rs  # Named/positional parameter registration helpers
│   │   └── mod.rs
│   ├── categorical_table_function.rs  # h3_raster_categorical_aggregate registration
│   ├── parquet_table_function.rs      # h3_raster_to_parquet registration
│   ├── pmtiles_table_function.rs      # h3_raster_to_pmtiles registration
│   ├── scalar.rs            # Scalar helper functions (h3_to_string, string_to_h3, etc.)
│   ├── table_function.rs    # h3_raster_continuous_aggregate registration
│   └── mod.rs               # Re-exports fast_hex and wkb from crate::encoding
└── lib.rs                   # DuckDB entry points (raster_h3_init, raster_h3_init_c_api, raster_h3_version)

crates/raster-h3-core/tests/it/      # Core engine suites (no DuckDB/DataFusion/pg)
├── main.rs                  # One test binary per crate: declares every module below
├── helpers.rs               # TestGeoTiffBuilder & shared fixtures (also used by duckdb and root)
├── support/mvt.rs           # MVT protobuf decoder for PMTiles assertions
├── accumulators.rs          # Welford/SIMD span and categorical accumulators
├── analytic_south_lat.rs    # Cell south-latitude bounds for eviction
├── antimeridian_bbox.rs     # ±180° bboxes, tile wrap and polar clamping
├── categorical_remap.rs     # Category remapping engine tests
├── chunks_and_prefetch.rs   # Chunk layout, bbox chunk pruning, prefetched readers
├── codecs.rs                # SIMD Deflate and fast LZW decompression parity
├── compaction_streaming.rs  # Compacted stream eviction
├── conic_horizon_soundness.rs  # Albers/conic horizon bounds
├── conus_pipeline.rs        # CONUS-scale pipeline smoke tests
├── exact_cell_assignment.rs # Exhaustive per-sample cell assignment reference
├── geodetic_reference.rs    # PROJ golden points within 0.01 m
├── geoparquet.rs            # OGC GeoParquet 1.1 metadata & geometry tests
├── geotiff_crs.rs           # GeoTransform and CRS detection/reprojection
├── horizon_streaming.rs     # Scan-horizon streaming, NoData and error paths
├── memory_budgets.rs        # Decoded-chunk and compaction budgets
├── mosaic.rs                # Multi-file mosaics, overlap rules and footprints
├── multi_resolution.rs      # Multi-resolution fusion & conservation tests
├── multiband_and_query_pushdown.rs  # Multi-band, spectral formulas & predicate pushdown
├── performance.rs           # Performance regression guards
├── pmtiles.rs               # PMTiles v3 archive & MVT encoding tests
├── projected_eviction.rs    # Projected-CRS eviction soundness
├── quantiles.rs             # Streaming quantile sketch tests
├── remote_cog.rs            # Remote COG/S3 streaming tests
├── rotated_grid_correctness.rs  # Sheared/rotated grids vs exhaustive reference
├── sharding.rs              # H3 shard balance and sharded merges
├── sparse_chunks_and_spilling.rs  # Sparse COG chunks and aggregation spilling
├── streaming_failures.rs    # Corrupt chunks reach stream consumers and exporters
├── super_sampling.rs        # Super-sampling conservation & fused path parity
└── threading.rs             # Multi-threaded determinism & stress tests

crates/raster-h3-duckdb/tests/it/    # DuckDB binding suites
├── cell_bbox_pushdown.rs    # cell_wgs84_bbox pushdown soundness
├── ffi_sql.rs               # Mock C-API callbacks for every registered function
├── prefetch_resilience.rs   # Prefetch queues under panics and remote mosaics
├── record_queue.rs          # Bind-side record queue propagates stream failures
└── spatial_geometry.rs      # WKB geometry emission and hex formatting

tests/it/                    # Root crate: needs the raster_h3 cdylib or bins
├── duckdb_sql_smoke.rs      # Real SQL through the DuckDB CLI (ignored by default)
├── multi_engine_e2e.rs      # Cross-engine parity (core, CLI, DataFusion, Python, PostgreSQL, DuckDB)
├── package_extension.rs     # DuckDB extension packaging tests
└── pg_extension_e2e.rs      # Real PostgreSQL servers in Docker (ignored by default)

examples/                    # CLI conversion & benchmark examples
├── benchmark_deflate.rs     # SIMD Deflate vs flate2 benchmark
├── benchmark_e2e.rs         # End-to-end performance suite
├── benchmark_features.rs    # Multi-band, compaction, pushdown benchmark
├── benchmark_lzw.rs         # LZW decompression benchmark
├── benchmark_multi_resolution.rs  # Single-pass vs multi-pass benchmark
├── benchmark_sampling_lookahead.rs  # Super-sampling evaluation
├── benchmark_scaling.rs     # Multi-core scaling benchmark
├── generate_sample.rs       # Synthetic GeoTIFF generator
├── parquet_to_pmtiles.rs    # CLI: Parquet to PMTiles converter
├── raster_to_pmtiles.rs     # CLI: GeoTIFF to PMTiles converter
├── test_hawaii_files.rs     # Hawaii real-world dataset benchmark
├── test_mosaic_burn_probability.rs  # Multi-tile mosaic benchmark
├── verify_pushdown.rs       # Predicate pushdown verification
└── debug/                   # Developer diagnostics (inspect_pmtiles, etc.)

pmtiles_viewer/              # MapLibre GL JS + PMTiles Studio Web Viewer
├── index.html               # Web interface with colormaps, 3D extrusion, HUD
├── server.py                # Python HTTP Range & CORS server
└── debug/                   # Headless Puppeteer & MVT decoder test harness
    ├── test.js              # Automated browser test
    └── test_mvt.js          # Protobuf vector tile geometry validator
```

## How to Contribute

### Reporting Bugs
Open a GitHub Issue with:
- DuckDB version and OS (`uname -a`)
- Minimal reproducing SQL query or CLI command
- Sample GeoTIFF (or description of raster dimensions, CRS, and band layout)
- Full error message or unexpected output

### Suggesting Features
Open a GitHub Issue describing:
- The use case and expected SQL interface
- Whether it affects continuous, categorical, or PMTiles export paths
- Any relevant geospatial standards or references

### Submitting Pull Requests
1. Fork the repository and create a feature branch from `main`
2. Write or update tests in the matching module of the owning crate's `tests/it/` binary (add a `mod` line to its `main.rs` for a new suite) ([Test Coverage](docs/testing.md) lists each suite by category)
3. Ensure all tests pass:
   ```bash
   cargo test --workspace
   ```
4. Ensure no compiler warnings or clippy lints:
   ```bash
   cargo clippy --workspace --all-targets
   ```
5. Ensure documentation builds cleanly:
   ```bash
   cargo doc --workspace --no-deps
   ```
6. If modifying `pmtiles_viewer/`, run the browser debug suite:
   ```bash
   cd pmtiles_viewer/debug && npm test
   ```
7. Update `README.md`, `CHANGELOG.md` and the affected [engine deep dive](docs/engines/README.md) if your change affects a public API
8. Open a pull request with a clear description of what changed and why

### Code Style
- Follow standard Rust formatting: run `cargo fmt` before committing
- Use `thiserror` for error types — no panics across FFI boundaries
- Prefer zero-allocation patterns in hot paths (scan loop, hex formatting)
- Document public APIs with `///` doc comments
- Add `//!` module-level documentation when creating new modules

## Architecture Notes

The aggregation engines share the same I/O pipeline (`memmap2` → prefetch → chunk decode) and scan-line traversal logic (latitude hoisting, longitude stepping, scanline lookahead). They diverge at the accumulator level:

- **Continuous** (`ScanHorizonStreamer`): Uses `H3Accumulator` with Welford online stats
- **Categorical** (`CategoricalHorizonStreamer`): Uses `CategoricalAccumulator` with `HashMap<i64, f64>` frequency tracking
- **Multi-Resolution Pyramids** (`MultiHorizonStreamer` / `pmtiles`): Single-pass concurrent aggregation across multiple H3 resolutions directly generating MVT protobuf tiles and PMTiles v3 archives.

All engines use certified horizon eviction where available and a configurable aggregation budget with disk spilling. The default budget is 64 MiB; decoding and prefetching require additional RAM.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
