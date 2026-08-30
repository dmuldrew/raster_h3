# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

### Fixed
- **Sub-pixel super-sampling** with 8 presets: center, RGSS, hexagonal, Gaussian PSF, 5-point quincunx, 8-rooks, 9-point grid, 16-point grid
- **Scalar helper functions**: `h3_to_string`, `string_to_h3`, `h3_to_lat`, `h3_to_lng`, `h3_get_resolution`
- **Spatial ROI bounding box pruning** via `min_lon`, `min_lat`, `max_lon`, `max_lat` parameters
- **CRS override support** via `source_crs` parameter with proj4rs pure-Rust reprojection
- **Zero-copy I/O** via `memmap2` with async double-buffered prefetching
- **Docker container** with multi-stage build, bundled DuckDB CLI, and demo script
- **Query planner integration**: cardinality estimation and parallel `init_local` distribution
