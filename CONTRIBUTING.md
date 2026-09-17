# Contributing to raster_h3

Thank you for your interest in contributing! This guide will help you get started.

## Development Setup

### Prerequisites
- [Rust](https://rustup.rs/) (Edition 2021+, stable toolchain)
- [DuckDB CLI](https://duckdb.org/) (Version 1.0.0+)
- [Docker](https://www.docker.com/) (optional, for container testing)

### Build & Test
```bash
# Build the extension
cargo build --release

# Run the full test suite
cargo test --release

# Run tests with output visible
cargo test -- --nocapture
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
src/
├── aggregator/              # Core aggregation & scanline horizon streaming
│   ├── accumulator.rs       # Welford online statistics (continuous)
│   ├── categorical.rs       # Categorical frequency accumulator & streamer
│   ├── h3_scanline.rs       # Scanline lookahead & jump-guess traversal
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
├── bin/                     # Standalone CLI utilities
│   ├── conus_batch_and_evict.rs  # CONUS-scale batch ingestion with eviction tracking
│   ├── convert_to_pmtiles.rs     # High-speed standalone TIFF-to-PMTiles converter
│   ├── download_burn_probability.rs # USFS Burn Probability tile downloader
│   ├── inspect_tif.rs       # GeoTIFF metadata & CRS inspection tool
│   └── package_extension.rs # Pure-Rust DuckDB extension packager & footer generator
├── crs/                     # Geodetic reprojection layer
│   ├── mod.rs
│   └── transformer.rs       # 3-tier CRS reprojection (Identity → Analytical → PROJ4)
├── encoding/                # Shared zero-allocation encoding & geometry serialization
│   ├── fast_hex.rs          # Zero-allocation SIMD/LUT hex formatter & parser
│   ├── wkb.rs               # Stack-allocated OGC WKB 2D Polygon hexagon geometry serialization
│   └── mod.rs
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
├── error.rs                 # Error types & conversions
└── lib.rs                   # Extension entry point & registration

tests/                       # Integration test suites (18 files)
├── helpers.rs               # TestGeoTiffBuilder & shared fixture generators
├── test_categorical_remap.rs    # Category remapping engine tests
├── test_conus_pipeline.rs       # CONUS-scale pipeline smoke tests
├── test_deflate_simd.rs         # SIMD Deflate decompression & corruption fuzzing
├── test_geoparquet.rs           # OGC GeoParquet 1.1 metadata & geometry tests
├── test_lzw_fast.rs             # Accelerated LZW decompression tests
├── test_mosaic_and_overlap.rs   # Multi-file mosaic & overlap rule tests
├── test_multi_resolution.rs     # Multi-resolution fusion & conservation tests
├── test_multiband_and_query_pushdown.rs  # Multi-band & predicate pushdown tests
├── test_package_extension.rs    # DuckDB extension packaging tests
├── test_performance.rs          # Performance regression guards
├── test_pmtiles.rs              # PMTiles v3 archive & MVT encoding tests
├── test_quantiles.rs            # Streaming quantile sketch tests
├── test_raster_h3.rs            # Core H3 aggregation & CRS tests
├── test_remote_cog.rs           # Remote COG/S3 streaming tests
├── test_spatial_geometry.rs     # WKB geometry emission tests
├── test_super_sampling_lookahead.rs  # Super-sampling conservation tests
└── test_threading.rs            # Multi-threaded determinism & stress tests

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
2. Write or update tests in the appropriate `tests/test_*.rs` file (see project tree above for the 18 modular test suites)
3. Ensure all tests pass:
   ```bash
   cargo test
   ```
4. Ensure no compiler warnings or clippy lints:
   ```bash
   cargo clippy --all-targets
   ```
5. Ensure documentation builds cleanly:
   ```bash
   cargo doc --no-deps
   ```
6. If modifying `pmtiles_viewer/`, run the browser debug suite:
   ```bash
   cd pmtiles_viewer/debug && npm test
   ```
7. Update `README.md` and `CHANGELOG.md` if your change affects the public API
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

All engines use the Southernmost Scan-Line Horizon Eviction algorithm to maintain bounded memory (< 15 MB RAM).

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
