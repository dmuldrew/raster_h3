# Test Coverage Deep Dive

[← Back to README](../README.md)

This page lists every test suite in the workspace, grouped by what it protects, and
explains how to run each group and what CI runs. The counts below are the number of
`#[test]` functions at the time of writing.

| Location | Suites | Tests |
| :--- | ---: | ---: |
| [`tests/`](../tests) (root crate integration tests) | 37 files | 283 |
| Crate unit tests (`#[cfg(test)]` in `crates/*/src`) | 6 crates | 172 |
| [`crates/raster-h3-python/tests/wheel_smoke.py`](../crates/raster-h3-python/tests/wheel_smoke.py) | 1 script | — |

Shared fixtures live in [`tests/helpers.rs`](../tests/helpers.rs) (and an MVT decoder in [`tests/support/mvt.rs`](../tests/support/mvt.rs)): `TestGeoTiffBuilder`
writes small synthetic GeoTIFFs (any sample type, strips or tiles, compression,
predictors, GeoKeys, NoData), plus wave and gradient generators. Almost every test builds
its own raster, so the suites need no downloaded data. The few exceptions are listed in
[Gated tests](#gated-tests).

## Contents

1. [Geodetic & CRS correctness](#1-geodetic--crs-correctness)
2. [Exact cell assignment & coordinates](#2-exact-cell-assignment--coordinates)
3. [Horizon eviction soundness](#3-horizon-eviction-soundness)
4. [Super-sampling](#4-super-sampling)
5. [Statistics & accumulators](#5-statistics--accumulators)
6. [Categorical aggregation & remapping](#6-categorical-aggregation--remapping)
7. [Multi-resolution & compaction](#7-multi-resolution--compaction)
8. [Bands, spectral indices & pushdown](#8-bands-spectral-indices--pushdown)
9. [Threading & concurrency](#9-threading--concurrency)
10. [Memory budgets & spilling](#10-memory-budgets--spilling)
11. [Failure propagation & resilience](#11-failure-propagation--resilience)
12. [GeoTIFF decoding & codecs](#12-geotiff-decoding--codecs)
13. [Remote COG & cloud streaming](#13-remote-cog--cloud-streaming)
14. [Mosaics & overlap rules](#14-mosaics--overlap-rules)
15. [Geometry encoding (WKB)](#15-geometry-encoding-wkb)
16. [GeoParquet export](#16-geoparquet-export)
17. [PMTiles & MVT export](#17-pmtiles--mvt-export)
18. [DuckDB extension](#18-duckdb-extension)
19. [Other engines & cross-engine parity](#19-other-engines--cross-engine-parity)
20. [Performance guards & benchmarks](#20-performance-guards--benchmarks)

`tests/test_raster_h3.rs` (58 tests) is the original catch-all suite. Its tests are listed
under the category they belong to rather than as one block.

---

## 1. Geodetic & CRS correctness

Coordinates must land where PROJ puts them, for every transform tier.

| Suite | What it checks |
| :--- | :--- |
| [`test_geodetic_reference.rs`](../tests/test_geodetic_reference.rs) (14) | Golden points from PROJ `cs2cs` 9.8.1 for every fast path and fallback, each within **0.01 m**: Web Mercator, Albers CONUS (EPSG:5070), Albers Alaska (EPSG:3338), UTM 32N, polar stereographic (EPSG:3413) and Lambert conformal. Affine round trips `coord_to_pixel(pixel_to_coord(p)) == p` on rotated/sheared grids. Coverage/duplicate properties on rasters that stress the horizon: the res 2–4 under-reach band, a 0–360° longitude raster, a polar-stereographic raster containing the pole, supersampled WGS84 and EPSG:3413 rasters, and overlapping mosaics. |
| [`test_geotiff_crs.rs`](../tests/test_geotiff_crs.rs) (1) | An ESRI PE-string citation is detected as the CRS when a real GeoTIFF is opened. |
| `test_raster_h3.rs` | `test_crs_web_mercator_to_wgs84`, `test_crs_utm_to_wgs84`, `test_crs_southern_hemisphere_utm`, `test_crs_custom_albers_proj_string`, `test_crs_invalid_proj_string_error_handling`, `test_crs_unspecified_fallback_and_invalid_proj_error`, `test_plain_tiff_without_geokeys`, `test_geotransform_accuracy`, `test_geotransform_inverse_precision`, `test_extreme_polar_latitudes_clamping`. |
| Unit: `crs/transformer.rs` (31) | Tier selection (identity, analytical, Proj4), Newton–Raphson inverse Albers, parameter parsing, error paths. |
| Unit: `raster/metadata.rs` (11), `raster/geotransform.rs` (1) | GeoKey directory parsing, tiepoint/pixel-scale/model-transformation tags, EPSG and WKT extraction. |

## 2. Exact cell assignment & coordinates

Every sample must go to the cell H3 assigns its exact coordinate, regardless of chunking,
grid orientation or bbox edges.

| Suite | What it checks |
| :--- | :--- |
| [`test_exact_cell_assignment.rs`](../tests/test_exact_cell_assignment.rs) (6) | An independent reference indexes every sample directly with `h3o` (no scanline or cache helpers) and must match supersampled continuous and categorical output per cell and class. Also: NaN NoData keeps finite values; fractional integer NoData does not exclude pixel values; compacting adjacent resolutions is rejected; a bbox keeps subsamples whose pixel centre is outside it; boundary assignment is independent of worker partitioning. |
| [`test_rotated_grid_correctness.rs`](../tests/test_rotated_grid_correctness.rs) (1) | Streaming results equal an exhaustive per-sample reference for horizontal shear, latitude-varying shear, general rotation + shear, and south-to-north rasters. |
| [`test_antimeridian_bbox.rs`](../tests/test_antimeridian_bbox.rs) (5) | Bboxes crossing ±180° keep every sample inside them, on grids ending at 180°, crossing 180°, spanning 0–360°, and on multiband rasters. |
| `test_raster_h3.rs` | `test_antimeridian_crossing_and_wrap`, `test_bounding_box_pruning`, `test_chunk_intersects_bbox_tile_filtering`, `test_spatial_filter_pushdown_chunk_skipping`. |
| Unit: `aggregator/multi_horizon/walker.rs` (3) | Pixel walker spans and row pruning. |

## 3. Horizon eviction soundness

A cell may only be emitted once no later input can touch it. These tests check that the
southern-boundary bounds and eviction certificates never under-reach.

| Suite | What it checks |
| :--- | :--- |
| [`test_analytic_south_lat.rs`](../tests/test_analytic_south_lat.rs) (7) | `compute_cell_south_lat` ≤ the true minimum latitude: southern interior-edge extrema, northern vertex extrema, south-pole containment, all 12 base pentagons, face-crossing Class III cells, a dense sampling oracle across the globe, invalid/degenerate cells. |
| [`test_conic_horizon_soundness.rs`](../tests/test_conic_horizon_soundness.rs) (5) | Albers strip northern bounds capture the central meridian (regression for a 2.5–3.6° underestimate); bbox intersection along the interior parallel arc; no duplicate cells in multi-horizon streaming; `max_hex_radius_deg` headroom at every resolution; apex and southern-hemisphere bounds. |
| [`test_projected_eviction.rs`](../tests/test_projected_eviction.rs) (6) | UTM rasters without a certificate retain cells to EOF; Albers rasters evict early and soundly; `eviction_north_bound` holds over curvature; off-grid poles and large polar chunks cannot produce a finite certificate; an Albers apex inside a chunk disables the boundary certificate. |
| [`test_compaction_streaming.rs`](../tests/test_compaction_streaming.rs) (2) | Compacted streams evict early yet match EOF grouping, never emit north of a published horizon, and PMTiles rejects compacting streamers before writing. |
| `test_raster_h3.rs` | `test_compute_cell_south_lat`, `test_scan_horizon_streamer_with_prefetch_and_coherence`, `test_web_mercator_streaming`. |

## 4. Super-sampling

| Suite | What it checks |
| :--- | :--- |
| [`test_super_sampling.rs`](../tests/test_super_sampling.rs) (7) | Mass conservation for every pattern (centre, RGSS, quincunx, Gaussian, hex, grids) on continuous and categorical rasters, gradient consistency, UTM Jacobian conservation and exact cell assignment, Web Mercator samples equal exact projection, custom single samples keep their offsets and weights. |
| [`test_fused_supersampling.rs`](../tests/test_fused_supersampling.rs) (2) | The fused projection/assignment/accumulation path matches a reference for statistics, histograms and quantiles. A throughput comparison is ignored by default. |
| `test_raster_h3.rs` | `test_subpixel_sampling_patterns`, `test_subpixel_rgss_streaming`, `test_subpixel_hex_streaming`, `test_sampling_pattern_fallback_to_center`. |
| Unit: `aggregator/sampling.rs` (2) | Pattern offsets and weights. |

See [Super-Sampling](super-sampling.md) for the patterns themselves.

## 5. Statistics & accumulators

| Suite | What it checks |
| :--- | :--- |
| [`test_quantiles.rs`](../tests/test_quantiles.rs) (13) | DDSketch accuracy on empty, single-value, uniform, negative, zero-bucket and mixed inputs; associative/commutative merges; weighted inserts; preset and custom quantile specs; disabled quantiles cost nothing; log-key mapping over wide ranges. |
| `test_raster_h3.rs` | Welford mean/variance (`test_accumulator_operations`, `test_accumulator_single_sample_variance_and_empty_default`, `test_welford_accumulator_extreme_mixed_bathymetry_numerical_stability`); SIMD span kernels equal scalar Welford for f32, f64, variable lengths, integer types and large integers (`test_simd_span_*`); NoData/NaN filtering keeps statistics intact (`test_nodata_filtering_preserves_statistics`, `test_nan_filtering_in_streaming`, `test_custom_nodata_override`, `test_is_chunk_all_nodata`, `test_all_nodata_full_stream_scan_and_categorical`). |
| Unit: `aggregator/nodata.rs` (5), `aggregator/simd.rs` (1) | NoData casting rules (rejects fractional, non-finite and out-of-range integer markers) and span accumulation. |

## 6. Categorical aggregation & remapping

| Suite | What it checks |
| :--- | :--- |
| [`test_categorical_remap.rs`](../tests/test_categorical_remap.rs) (5) | Exact, range, list and wildcard remap syntax; sparse and negative categories; invalid syntax; remapping inside single- and multi-resolution categorical streamers. |
| `test_raster_h3.rs` | Accumulator operations and merges, tied majorities, homogeneous rasters, RLE with interspersed NoData, high-cardinality Shannon entropy, the 16-slot inline array spilling to a heap map, SIMD uniformity across types (`test_categorical_*`). |
| Unit: `aggregator/categorical.rs` (4), `aggregator/remap.rs` (4) | Histogram and remapper internals. |

## 7. Multi-resolution & compaction

| Suite | What it checks |
| :--- | :--- |
| [`test_multi_resolution.rs`](../tests/test_multi_resolution.rs) (9) | One fused pass over several resolutions equals separate single-resolution runs exactly, for continuous and categorical data, with and without supersampling; empty-resolution errors; prefetch batch draining; sorted streaming Parquet export; compaction configuration validation. |
| [`test_multiband_and_query_pushdown.rs`](../tests/test_multiband_and_query_pushdown.rs) | `test_hierarchical_compaction_continuous_conservation`, `test_hierarchical_compaction_categorical_conservation`. |
| `test_raster_h3.rs` | `test_scalar_resolution_and_parent_calculations`, `test_invalid_resolution_parameter_error`. |
| Unit: `multi_horizon/compaction.rs` (2), `multi_horizon/config.rs` (2) | Compaction groups and config validation. |

## 8. Bands, spectral indices & pushdown

| Suite | What it checks |
| :--- | :--- |
| [`test_multiband_and_query_pushdown.rs`](../tests/test_multiband_and_query_pushdown.rs) (14) | Band selection and samples-per-pixel; NDVI, NDWI, NBR and EVI on the fly; zero-denominator and NoData resilience; out-of-range band clamping; borrowed interleaved windows match across job sizes; `min_count`/`min_mean`/`min_majority_fraction` pushdown; `h3_cell`/`h3_hex` bbox pushdown at high latitude and across the antimeridian without dropping raster samples. |
| Unit: `multi_horizon/spectral.rs` (5) | Formula evaluation and singularity guards. |

## 9. Threading & concurrency

Results must not depend on thread count, scheduling or pool nesting.

| Suite | What it checks |
| :--- | :--- |
| [`test_threading.rs`](../tests/test_threading.rs) (6) | Identical output across Rayon thread counts; pool scaling; a high-contention stress run; multithreaded prefetched chunk reading; aggregation workers run concurrently inside `ThreadPool::install`; a dedicated pool behind a shared mutex does not deadlock. |
| [`test_sharding.rs`](../tests/test_sharding.rs) (5) | Shard balance for H3 indices at resolutions 0–15 and in local neighbourhoods; sharded merge + eviction preserves continuous statistics and categorical histograms; worker results merge before the watermark advances. |
| `test_raster_h3.rs` | `test_prefetched_chunk_reader`, `test_prefetched_chunk_reader_multi_worker_ordering`, `test_prefetched_chunk_reader_early_drop`. |
| [`test_mosaic_and_overlap.rs`](../tests/test_mosaic_and_overlap.rs) | `test_prefetched_mosaic_reader_multi_worker_concurrency`, `test_prefetched_mosaic_reader_early_drop`. |
| Unit: `raster/prefetch.rs` (17), `multi_horizon/sharded_map.rs` (1), `aggregator/cell_hash.rs` (2) | `OrderedPrefetchQueue` ordering, backpressure, decode budgets, cancellation (using the `test-hooks` handshakes); shard hashing. |

## 10. Memory budgets & spilling

| Suite | What it checks |
| :--- | :--- |
| [`test_memory_budgets.rs`](../tests/test_memory_budgets.rs) (2) | The decoded-chunk budget bounds held chunks without changing results; the compaction limit decomposes groups without losing or duplicating samples. |
| [`test_sparse_chunks_and_spilling.rs`](../tests/test_sparse_chunks_and_spilling.rs) (2) | Sparse COG chunks are filled with a non-zero NoData and skipped; a low aggregation budget creates spill runs (`spill_run_count() > 0`) and the external merge matches an in-memory reference. |
| Unit: `multi_horizon/spill.rs` (5) | Spill file writing and merging. |

See [Engineering Architecture](engineering.md) for what the budgets cover.

## 11. Failure propagation & resilience

A failure after a valid header must reach the consumer as an error, never as a short but
"successful" result.

| Suite | What it checks |
| :--- | :--- |
| [`test_streaming_failures.rs`](../tests/test_streaming_failures.rs) (5) | Corrupt-chunk errors latch in both streamers; the DuckDB record queue propagates failure instead of EOF; failed exports leave existing outputs untouched; a Parquet file is not published after a late failure; premature prefetch termination is an error. |
| [`test_prefetch_resilience.rs`](../tests/test_prefetch_resilience.rs) (3) | Mixed local/remote mosaics finish without deadlock; a decode-worker panic latches into the queue; a corrupt chunk unblocks readers instead of hanging. |
| `test_raster_h3.rs` | `test_nonexistent_file_path_error`, `test_corrupted_and_empty_tiff_error_handling`. |

## 12. GeoTIFF decoding & codecs

| Suite | What it checks |
| :--- | :--- |
| [`test_deflate_simd.rs`](../tests/test_deflate_simd.rs) (10) | `libdeflater` output is bit-identical to the `tiff` decoder for striped u8/f32, tiled f32 with padding, floating-point and horizontal predictors; buffer recycling and auto-resizing; corrupted streams; cropped edge tiles in both streaming paths. |
| [`test_lzw_fast.rs`](../tests/test_lzw_fast.rs) (9) | Fast LZW parity with the `tiff` decoder on synthetic tiled data with predictors and edge padding; corrupted and truncated streams; buffer resizing. Four parity/recycling tests use the Hawaii datasets and are ignored by default. |
| [`test_conus_pipeline.rs`](../tests/test_conus_pipeline.rs) | `test_tiff_magic_validation_edge_cases`. |
| `test_raster_h3.rs` | `test_u16_raster_streaming`, `test_f64_raster_streaming`, `test_tiled_chunk_layout_calculation`, `test_tiled_uneven_chunk_layout_and_bounds`. |
| Unit: `raster/predictor.rs` (8) | Predictor 2 (including NEON) and predictor 3 unpacking. |

## 13. Remote COG & cloud streaming

| Suite | What it checks |
| :--- | :--- |
| [`test_remote_cog.rs`](../tests/test_remote_cog.rs) (19) | Against a local mock HTTP server: URL normalisation and S3 regional/custom endpoints; header read budget; numerical equality with local reads; ROI-selective streaming; 404 handling; transient retry and recovery; header injection; range coalescing; rejection of truncated, ignored or wrong ranges; full-body responses served from cache; reads across cache blocks; remote mosaics and the multi-resolution streamer; remote COG → Parquet. |
| Unit: `raster/http_range.rs` (5), `raster/remote_prefetch.rs` (6) | Range validation, caching, coalescing and backoff. |

## 14. Mosaics & overlap rules

| Suite | What it checks |
| :--- | :--- |
| [`test_mosaic_and_overlap.rs`](../tests/test_mosaic_and_overlap.rs) (10) | Glob matching, comma lists, VRT XML resolution; adjacent-tile conservation; `cutline` vs `first` vs `average`; categorical overlaps; mixed VRT paths and export pipelines. |
| [`test_mosaic_footprints.rs`](../tests/test_mosaic_footprints.rs) (4) | Mosaic opening keeps antimeridian tiles; overlap rules test real tile footprints, not WGS84 rectangles, including rotated footprints and mixed longitude branches. |
| [`test_conus_pipeline.rs`](../tests/test_conus_pipeline.rs) | `test_conus_batch_and_evict_pipeline_simulation`: multi-tile ingestion, batch eviction, Parquet progress and PMTiles from partitioned input. |

## 15. Geometry encoding (WKB)

| Suite | What it checks |
| :--- | :--- |
| [`test_spatial_geometry.rs`](../tests/test_spatial_geometry.rs) (8) | WKB for every resolution, Class III pentagons and edge-crossing cells; WKB vs `GEOMETRY` byte equality; hex-string input; invalid cells; DuckDB version parsing and geometry column indices under projection. |
| [`test_geodetic_reference.rs`](../tests/test_geodetic_reference.rs) | `wkb_buffer_capacity_all_resolutions_pentagons_and_icosahedron_edges`. |
| `test_raster_h3.rs` | `test_wkb_ogc_compliance`, `test_fast_hex_formatting`. |
| Unit: `encoding/wkb.rs` (6), `encoding/fast_hex.rs` (2) | Ring construction, dateline unwrapping, hex formatting/parsing. |

## 16. GeoParquet export

| Suite | What it checks |
| :--- | :--- |
| [`test_geoparquet.rs`](../tests/test_geoparquet.rs) (8) | Continuous and categorical metadata and WKB; compact and full schemas; GeoParquet off by default; raster → GeoParquet end to end; deep validation against OGC GeoParquet 1.1 (PROJJSON, edges, bbox); column projection. |
| [`test_multi_resolution.rs`](../tests/test_multi_resolution.rs) | Sorted streaming Parquet exports. |
| Unit: `parquet/geoparquet_metadata.rs` (1) | Metadata JSON. |

## 17. PMTiles & MVT export

| Suite | What it checks |
| :--- | :--- |
| [`test_pmtiles.rs`](../tests/test_pmtiles.rs) (21) | MVT protobuf encoding; PMTiles v3 header and archive validation; Hilbert tile IDs (ordering and bijective round trip); resolution ↔ zoom monotonicity; cell → tile mapping; coarse-zoom parent aggregation over all children; property filtering; GeoTIFF and Parquet (custom column, hex strings, multi row-group eviction) to PMTiles; all-NoData rasters; Class III 10-vertex pentagons; antimeridian-straddling cells and world tiles. |
| Unit: `pmtiles/pyramid.rs` (3), `pmtiles/overview.rs` (2) | Tile math and overview levels. |

See [PMTiles & Multi-Resolution](pmtiles.md).

## 18. DuckDB extension

| Suite | What it checks |
| :--- | :--- |
| [`test_duckdb_ffi_sql.rs`](../tests/test_duckdb_ffi_sql.rs) (5) | Mock C-API callbacks: init and registration completeness, every scalar with NULL propagation, continuous scans with projection pushdown, categorical wide and long, export table functions with CRS overrides. No SQL is executed. |
| [`test_duckdb_sql_smoke.rs`](../tests/test_duckdb_sql_smoke.rs) (1, ignored) | Real SQL through the DuckDB CLI against the built library: export parameters and results. |
| [`test_package_extension.rs`](../tests/test_package_extension.rs) (1) | Extension footer layout: custom section headers, ABI fields, version and platform strings, zeroed signature, gzip output. |
| Unit: `raster-h3-duckdb` (28) | `ffi/duckdb_c.rs` (9: `duckdb_string_t` layout), `ffi/mod.rs` (3), `bind_utils/parsing.rs` (6), `bind_helper.rs` (2), `chunk_writer.rs` (3), `record_queue.rs` (1), `scalar.rs` (4). |

See [DuckDB](engines/duckdb.md).

## 19. Other engines & cross-engine parity

| Suite | What it checks |
| :--- | :--- |
| [`test_multi_engine_e2e.rs`](../tests/test_multi_engine_e2e.rs) (2) | Continuous and categorical per-cell parity across core batch readers, the Arrow C stream, the CLI binary (`ipc`, `jsonl`, `parquet`, `pmtiles`), DataFusion (scan, pushdown, UDFs), Python (a `python3` subprocess loading the cdylib), the PostgreSQL SRFs and fmgr scalars in process, and DuckDB when `duckdb` is on `PATH`. |
| [`test_pg_extension_e2e.rs`](../tests/test_pg_extension_e2e.rs) (1, ignored) | Builds the PostgreSQL module in Docker, loads it into `postgres:<major>` for each `RASTER_H3_PG_VERSIONS`, compares `psql` output with in-process SRFs and checks that bad `bbox`/`h3_res` raise SQL errors without breaking the session. |
| Unit: `raster-h3-core` columnar (2), `scalar_ops.rs` (1) | Arrow C stream round trip, `RH3IPC01` round trip, scalar helpers. |
| Unit: `raster-h3-datafusion` (3) | Projection, limit, filter narrowing, Arrow stream, UDFs. |
| Unit: `raster-h3-cli` (2) | Argument validation, IPC and JSONL output. |
| Unit: `raster-h3-python` (2), `raster-h3-pg` (2 + 3 in `fmgr.rs`) | C-ABI helpers and SRF iterators. The `fmgr.rs` tests only compile with `--features pg-extension`. |
| [`wheel_smoke.py`](../crates/raster-h3-python/tests/wheel_smoke.py) | After `cibuildwheel` installs a wheel: the bundled library loads, Arrow/Polars paths work, `raster-h3-cli` is on `PATH`. |

See the [engine deep dives](engines/README.md).

## 20. Performance guards & benchmarks

| Suite | What it checks |
| :--- | :--- |
| [`test_performance.rs`](../tests/test_performance.rs) (4) | Continuous, multi-resolution, categorical and supersampled streaming of small synthetic rasters each finish in **under 2 s** and conserve pixel mass. These are coarse regression guards and run best with `--release`. |
| [`test_fused_supersampling.rs`](../tests/test_fused_supersampling.rs) | `benchmark_supersampling` (ignored): manual release throughput comparison. |
| `test_raster_h3.rs` | `test_parallel_chunk_aggregation_hawaii_dataset`, `test_parallel_categorical_aggregation_hawaii_dataset` (ignored, need data). |

Throughput benchmarks proper live in [`examples/`](../examples) (`benchmark_*`); see
[Architecture Comparison](architecture-comparison.md).

---

## Running the Tests

```bash
cargo test --release                      # root crate: all tests/ suites + root lib
cargo test --release --workspace          # also every crate's unit tests
cargo test --release --test test_threading          # one suite
cargo test --release -p raster-h3-core prefetch     # unit tests matching a filter
cargo test --release -- --ignored                   # only the gated tests
```

Use `--release` for anything that streams real data; debug builds are much slower and
`test_performance.rs` can exceed its time limits.

### Gated tests

| Test | Gate | How to run |
| :--- | :--- | :--- |
| `test_duckdb_sql_smoke::real_sql_export_parameters_and_results` | `#[ignore]`; needs the DuckDB CLI and a built library | `cargo build --release --lib && cargo test --release --test test_duckdb_sql_smoke -- --ignored` (`DUCKDB_BIN`, `RASTER_H3_LIBRARY` select the binary and library) |
| `test_pg_extension_e2e::pg_extension_in_real_server` | `#[ignore]`; needs Docker | `cargo test --test test_pg_extension_e2e -- --ignored --nocapture` (`RASTER_H3_PG_VERSIONS=14,16,18`, `RASTER_H3_PG_E2E_KEEP=1`) |
| 4 × `test_lzw_fast` Hawaii parity, 2 × `test_raster_h3` Hawaii aggregation | `#[ignore]`; need `data/CFL_HI.tif` / `data/LF2024_FBFM40_HI.tif` | Download the datasets into `data/`, then `-- --ignored`. They fail explicitly if the files are missing. |
| `test_fused_supersampling::benchmark_supersampling` | `#[ignore]` | `cargo test --release --test test_fused_supersampling -- --ignored --nocapture` |
| DuckDB part of `test_multi_engine_e2e` | Skipped when `duckdb --version` fails | Put the DuckDB CLI on `PATH` |
| Python part of `test_multi_engine_e2e` | Runs `/usr/bin/python3`; the test fails if that path is absent | Run on a system with `/usr/bin/python3` |

## What CI Runs

| Workflow | Command | Covers |
| :--- | :--- | :--- |
| [`ci.yml`](../.github/workflows/ci.yml), per platform (Linux, macOS, Windows) | `cargo test --release --no-fail-fast` | Root crate only: all `tests/` suites (gated ones excluded) and the root library |
| `ci.yml` | `cargo build --release --lib` then `cargo test --release --test test_duckdb_sql_smoke -- --ignored` | Real-SQL DuckDB smoke test against the CLI CI downloads (v1.2.0) |
| `ci.yml` | `duckdb -unsigned -c "LOAD …; SELECT h3_to_string(…)"` | Packaged extension loads |
| [`wheels.yml`](../.github/workflows/wheels.yml) | `cibuildwheel` + `wheel_smoke.py` | Each built wheel installs and works |

### Gaps

- **Crate unit tests are not run in CI.** `cargo test` at the workspace root without
  `--workspace` (or `default-members`) only tests the root package, so the 172 unit tests
  in `crates/*/src` (for example the 31 CRS transformer tests and 17 prefetch tests) only
  run locally. Adding `--workspace` to the `ci.yml` test step would cover them.
- The real PostgreSQL test and the Hawaii-dataset tests never run in CI.
- The `fmgr.rs` unit tests only build with `--features pg-extension`.
