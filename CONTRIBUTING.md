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
├── aggregator/              # Core aggregation engines
│   ├── accumulator.rs       # Welford online statistics (continuous)
│   ├── categorical.rs       # Categorical frequency accumulator & streamer
│   ├── coherence.rs         # Spatial coherence cache (inscribed bounding box)
│   ├── h3_map.rs            # Legacy hash-map aggregation path
│   ├── horizon_streamer.rs  # Scan-line horizon eviction engine (continuous)
│   └── sampling.rs          # Sub-pixel super-sampling presets
├── crs/
│   └── transformer.rs       # CRS reprojection via proj4rs
├── ffi/                     # DuckDB C-FFI bindings
├── functions/
│   ├── table_function.rs    # h3_raster_continuous_aggregate registration
│   ├── categorical_table_function.rs  # h3_raster_categorical_aggregate registration
│   ├── scalar.rs            # Scalar helper functions
│   └── fast_hex.rs          # Zero-allocation hex formatter
├── raster/                  # GeoTIFF I/O, geotransform, prefetching
├── error.rs                 # Error types
└── lib.rs                   # Extension entry point
```

## How to Contribute

### Reporting Bugs
Open a GitHub Issue with:
- DuckDB version and OS
- Minimal reproducing SQL query
- Sample GeoTIFF (or description of raster dimensions, CRS, and band layout)
- Full error message or unexpected output

### Suggesting Features
Open a GitHub Issue describing:
- The use case and expected SQL interface
- Whether it affects continuous, categorical, or both aggregation paths
- Any relevant geospatial standards or references

### Submitting Pull Requests
1. Fork the repository and create a feature branch from `main`
2. Write or update tests in `tests/test_raster_h3.rs`
3. Ensure `cargo test` passes with no failures
4. Ensure `cargo clippy` reports no warnings
5. Update `README.md` and `CHANGELOG.md` if your change affects the public API
6. Open a pull request with a clear description of what changed and why

### Code Style
- Follow standard Rust formatting: run `cargo fmt` before committing
- Use `thiserror` for error types — no panics across FFI boundaries
- Prefer zero-allocation patterns in hot paths (scan loop, hex formatting)
- Document public APIs with `///` doc comments

## Architecture Notes

The two aggregation engines share the same I/O pipeline (`memmap2` → prefetch → chunk decode) and scan-line traversal logic (latitude hoisting, longitude stepping, coherence cache). They diverge at the accumulator level:

- **Continuous** (`ScanHorizonStreamer`): Uses `H3Accumulator` with Welford online stats
- **Categorical** (`CategoricalHorizonStreamer`): Uses `CategoricalAccumulator` with `HashMap<i64, f64>` frequency tracking

Both use the Southernmost Scan-Line Horizon Eviction algorithm to maintain bounded memory.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
