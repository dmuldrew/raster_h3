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
│   ├── h3_map.rs            # In-memory H3 aggregation map fallback
│   ├── h3_scanline.rs       # Scanline lookahead & jump-guess traversal
│   ├── horizon_streamer.rs  # Southernmost scanline horizon eviction engine
│   ├── multi_horizon/       # Multi-resolution concurrent horizon aggregator submodules
│   │   ├── config.rs        # Configuration & quantile/spectral formula targets
│   │   ├── continuous.rs    # Continuous numeric chunk aggregation
│   │   ├── continuous_streamer.rs # Multi-resolution continuous horizon streamer
│   │   ├── categorical.rs   # Categorical landcover chunk aggregation
│   │   └── categorical_streamer.rs # Multi-resolution categorical horizon streamer
│   └── sampling.rs          # Sub-pixel super-sampling presets (RGSS, Hex, 8-Rooks, Gaussian)
├── bin/                     # Standalone CLI utilities
│   ├── convert_to_pmtiles.rs# High-speed standalone TIFF-to-PMTiles converter
│   ├── inspect_tif.rs       # GeoTIFF metadata & CRS inspection tool
│   └── package_extension.rs # Pure-Rust DuckDB extension packager & footer generator
├── crs/                     # Geodetic reprojection layer
│   ├── mod.rs
│   └── transformer.rs       # Standalone CRS reprojection via proj4rs
├── ffi/                     # DuckDB C-FFI bindings
│   ├── duckdb_c.rs          # Low-level DuckDB C API types & function pointers
│   └── mod.rs
├── functions/               # DuckDB table & scalar function registrations
│   ├── categorical_table_function.rs  # h3_raster_categorical_aggregate registration
│   ├── fast_hex.rs          # Zero-allocation SIMD/LUT hex formatter
│   ├── pmtiles_table_function.rs      # h3_raster_to_pmtiles registration
│   ├── scalar.rs            # Scalar helper functions (h3_to_string, string_to_h3, etc.)
│   ├── table_function.rs    # h3_raster_continuous_aggregate registration
│   └── mod.rs
├── pmtiles/                 # Native PMTiles v3 & Mapbox Vector Tile (MVT) generation
│   ├── mvt.rs               # Pure-Rust Protobuf MVT vector tile encoder
│   ├── tiler.rs             # Multi-resolution pyramid tiling & Hilbert indexer
│   ├── writer.rs            # PMTiles v3 container writer & header serializer
│   └── mod.rs
├── raster/                  # GeoTIFF I/O & geotransform
│   ├── geotiff.rs           # Baseline/tiled/BigTIFF reader & decompression
│   ├── geotransform.rs      # Affine geotransform & coordinate mapping
│   ├── prefetch.rs         # Lock-free background memory prefetcher
│   └── mod.rs
├── error.rs                 # Error types & conversions
└── lib.rs                   # Extension entry point & registration

pmtiles_viewer/              # MapLibre GL JS + PMTiles Studio Web Viewer
├── index.html               # Web interface with colormaps, 3D extrusion, HUD
├── server.py                # Python HTTP Range & CORS server
└── debug/                   # Headless Puppeteer & MVT decoder test harness
    ├── test.js              # Automated browser test
    └── test_mvt.js          # Protobuf vector tile geometry validator

examples/                    # CLI conversion & benchmark examples
├── raster_to_pmtiles.rs     # CLI: GeoTIFF to PMTiles converter
├── parquet_to_pmtiles.rs    # CLI: Parquet to PMTiles converter
├── benchmark_e2e.rs         # End-to-end performance suite
├── benchmark_scaling.rs     # Multi-resolution scaling benchmark
└── debug/                   # Developer diagnostics (inspect_pmtiles, etc.)
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
2. Write or update tests in `tests/test_raster_h3.rs`
3. Ensure `cargo test` passes with no failures
4. Ensure `cargo clippy` reports no warnings
5. If modifying `pmtiles_viewer/`, run the browser debug suite:
   ```bash
   cd pmtiles_viewer/debug && npm test
   ```
6. Update `README.md` and `CHANGELOG.md` if your change affects the public API
7. Open a pull request with a clear description of what changed and why

### Code Style
- Follow standard Rust formatting: run `cargo fmt` before committing
- Use `thiserror` for error types — no panics across FFI boundaries
- Prefer zero-allocation patterns in hot paths (scan loop, hex formatting)
- Document public APIs with `///` doc comments

## Architecture Notes

The aggregation engines share the same I/O pipeline (`memmap2` → prefetch → chunk decode) and scan-line traversal logic (latitude hoisting, longitude stepping, scanline lookahead). They diverge at the accumulator level:

- **Continuous** (`ScanHorizonStreamer`): Uses `H3Accumulator` with Welford online stats
- **Categorical** (`CategoricalHorizonStreamer`): Uses `CategoricalAccumulator` with `HashMap<i64, f64>` frequency tracking
- **Multi-Resolution Pyramids** (`MultiHorizonStreamer` / `pmtiles`): Single-pass concurrent aggregation across multiple H3 resolutions directly generating MVT protobuf tiles and PMTiles v3 archives.

All engines use the Southernmost Scan-Line Horizon Eviction algorithm to maintain bounded memory (< 15 MB RAM).

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
