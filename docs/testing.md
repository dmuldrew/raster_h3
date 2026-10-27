# Test Coverage Deep Dive

[← Back to README](../README.md)

This page lists every test suite in the workspace, grouped by what it protects, and
explains how to run each group and what CI runs. The counts below are the number of
`#[test]` (or pytest `test_*`) functions at the time of writing.

| Location | Suites | Tests |
| :--- | ---: | ---: |
| [`crates/raster-h3-core/tests/it/`](../crates/raster-h3-core/tests/it) (`core/…`) | 34 modules, 1 binary | 310 |
| [`crates/raster-h3-duckdb/tests/it/`](../crates/raster-h3-duckdb/tests/it) (`duckdb/…`) | 5 modules, 1 binary | 21 |
| [`crates/raster-h3-datafusion/tests/it/`](../crates/raster-h3-datafusion/tests/it) (`datafusion/…`) | 5 modules, 1 binary | 17 |
| [`crates/raster-h3-cli/tests/it/`](../crates/raster-h3-cli/tests/it) (`cli/…`, runs the crate's `raster-h3-cli` bin) | 1 module, 1 binary | 7 |
| [`crates/raster-h3-python/tests/it/`](../crates/raster-h3-python/tests/it) (`python/…`) | 4 modules, 1 binary | 12 |
| [`crates/raster-h3-pg/tests/it/`](../crates/raster-h3-pg/tests/it) (`pg/…`) | 2 modules, 1 binary | 11 (7 need `pg-extension`) |
| [`tests/it/`](../tests/it) (`root/…`, needs the root cdylib or bins) | 4 modules, 1 binary | 11 |
| Crate unit tests (`#[cfg(test)]` in `crates/*/src`) | 6 crates | 207 |
| [`crates/raster-h3-python/tests/pytest/`](../crates/raster-h3-python/tests/pytest) (`pytest/…`, needs `polars`, `pyarrow`, `h3`) | 2 modules | 24 (35 cases) |
| [`crates/raster-h3-python/tests/wheel_smoke.py`](../crates/raster-h3-python/tests/wheel_smoke.py) | 1 script | — |

Each crate links one integration test binary, `tests/it/main.rs`, with one module per
suite. Shared fixtures live in [`core/helpers.rs`](../crates/raster-h3-core/tests/it/helpers.rs), which every
other crate's binary includes by path (and an MVT decoder in [`core/support/mvt.rs`](../crates/raster-h3-core/tests/it/support/mvt.rs)): `TestGeoTiffBuilder`
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

The former catch-all `tests/test_raster_h3.rs` (58 tests) was split by topic; rows that
list individual test names come from it or from a suite that mostly covers another category.

---

## 1. Geodetic & CRS correctness

Coordinates must land where PROJ puts them, for every transform tier.

| Suite | What it checks |
| :--- | :--- |
| [`core/geodetic_reference.rs`](../crates/raster-h3-core/tests/it/geodetic_reference.rs) (25) | Golden forward and inverse points from PROJ 9.5.1 for fast paths and fallbacks, each within **0.01 m**: Web Mercator, Albers CONUS/Alaska and southern parallels with false offsets (including non-polar latitudes through ±89.9999°), UTM north/south, polar stereographic north/south, EASE-Grid 2.0 and Lambert conformal; EPSG aliases are bit-identical. Affine round trips `coord_to_pixel(pixel_to_coord(p)) == p` on rotated/sheared grids. Coverage/duplicate properties include antimeridian-crossing Albers Alaska, EPSG:32756, pole-containing and supersampled EPSG:3413/3031 rasters, plus prior horizon, longitude-wrapping and mosaic cases. |
| [`core/certified_bounds.rs`](../crates/raster-h3-core/tests/it/certified_bounds.rs) (13, plus 1 generated) | Rejection bounds enclose the true image: certified UTM/BNG bounds checked against dense samples and used to prune distant bboxes; generated rotated/sheared chunks for WGS84, Web Mercator, northern and southern Albers (including across the antimeridian, around the cone apex and past the cut opposite the central meridian) and a US-foot transverse Mercator have rect bounds and an eviction bound enclosing every sample; extreme `tmerc` scale factors stay enclosing and certificates use proj4rs's effective scale (`k0`, then `k`; `k_0` ignored); chunks ending at or one ulp below 180° keep non-reversed bounds; pole-containing UTM chunks get full longitude; huge `+towgs84` shifts and other projections fail open; partly unprojectable rectangles get whole-globe bounds; fine (1e-8°) affines invert; singular and non-finite affines are rejected on open; duplicate, quoted, non-lowercase or spaced TM parameters fail open; polar radial/angular bounds enclose dense rotated/sheared grids and preserve per-cell sums/counts across bbox and mosaic ownership modes. |
| [`core/datum_shifts.rs`](../crates/raster-h3-core/tests/it/datum_shifts.rs) (7) | Helmert transforms match PROJ with identical parameters and stay within published accuracy of PROJ defaults; inverse round trips; explicit `+towgs84`; prime-meridian and axis-order composition; nonzero shifts avoid fast paths; grid-shift datums are rejected clearly. |
| [`core/geotiff_crs.rs`](../crates/raster-h3-core/tests/it/geotiff_crs.rs) (10) | CRS detection from ESRI PE-string citations and GeoTIFF GeoKeys, EPSG/WKT handling, projected transformations and invalid/unspecified CRS paths. |
| [`core/geotiff_crs.rs`](../crates/raster-h3-core/tests/it/geotiff_crs.rs), [`core/antimeridian_bbox.rs`](../crates/raster-h3-core/tests/it/antimeridian_bbox.rs) | `test_crs_web_mercator_to_wgs84`, `test_crs_utm_to_wgs84`, `test_crs_southern_hemisphere_utm`, `test_crs_custom_albers_proj_string`, `test_crs_invalid_proj_string_error_handling`, `test_crs_unspecified_fallback_and_invalid_proj_error`, `test_plain_tiff_without_geokeys`, `test_geotransform_accuracy`, `test_geotransform_inverse_precision`, `test_extreme_polar_latitudes_clamping`. |
| Unit: `crs/transformer.rs` (40) | Tier selection (identity, analytical, Proj4), authalic-series inverse Albers, public source-field mutation and struct-literal compatibility, parameter parsing, error paths, certified and fail-open rectangle bounds. |
| Unit: `raster/metadata.rs` (11), `raster/geotransform.rs` (3) | GeoKey directory parsing, tiepoint/pixel-scale/model-transformation tags, EPSG and WKT extraction; scale-independent affine inversion. |

## 2. Exact cell assignment & coordinates

Every sample must go to the cell H3 assigns its exact coordinate, regardless of chunking,
grid orientation or bbox edges.

| Suite | What it checks |
| :--- | :--- |
| [`core/exact_cell_assignment.rs`](../crates/raster-h3-core/tests/it/exact_cell_assignment.rs) (7) | An independent reference indexes every sample directly with `h3o` (no scanline or cache helpers) and must match supersampled continuous and categorical output per cell and class. Short five-pixel cell re-entry and nine-pixel boundary cases match exhaustive continuous and categorical aggregation at one or multiple resolutions. Also: NaN NoData keeps finite values; fractional integer NoData does not exclude pixel values; compacting adjacent resolutions is rejected; a bbox keeps subsamples whose pixel centre is outside it; boundary assignment is independent of worker partitioning. |
| [`core/rotated_grid_correctness.rs`](../crates/raster-h3-core/tests/it/rotated_grid_correctness.rs) (1) | Streaming results equal an exhaustive per-sample reference for horizontal shear, latitude-varying shear, general rotation + shear, and south-to-north rasters. |
| [`core/antimeridian_bbox.rs`](../crates/raster-h3-core/tests/it/antimeridian_bbox.rs) (7) | Bboxes crossing ±180° keep every sample inside them, on grids ending at 180°, crossing 180°, spanning 0–360°, and on multiband rasters. |
| [`core/antimeridian_bbox.rs`](../crates/raster-h3-core/tests/it/antimeridian_bbox.rs), [`core/chunks_and_prefetch.rs`](../crates/raster-h3-core/tests/it/chunks_and_prefetch.rs) | `test_antimeridian_crossing_and_wrap`, `test_bounding_box_pruning`, `test_chunk_intersects_bbox_tile_filtering`, `test_spatial_filter_pushdown_chunk_skipping`. |
| Unit: `aggregator/multi_horizon/walker.rs` (5) | Pixel walker spans and row pruning; horizontal/vertical shear mode selection and bit-identical WGS84/Mercator/Albers center and supersampled projection, including transform counts under `stream-profile`. |

## 3. Horizon eviction soundness

A cell may only be emitted once no later input can touch it. These tests check that the
southern-boundary bounds and eviction certificates never under-reach.

| Suite | What it checks |
| :--- | :--- |
| [`core/analytic_south_lat.rs`](../crates/raster-h3-core/tests/it/analytic_south_lat.rs) (8) | `compute_cell_south_lat` ≤ the true minimum latitude: southern interior-edge extrema, northern vertex extrema, south-pole containment, all 12 base pentagons, face-crossing Class III cells, a dense sampling oracle across the globe, invalid/degenerate cells. |
| [`core/conic_horizon_soundness.rs`](../crates/raster-h3-core/tests/it/conic_horizon_soundness.rs) (5) | Albers strip northern bounds capture the central meridian (regression for a 2.5–3.6° underestimate); bbox intersection along the interior parallel arc; no duplicate cells in multi-horizon streaming; `max_hex_radius_deg` headroom at every resolution; apex and southern-hemisphere bounds. |
| [`core/projected_eviction.rs`](../crates/raster-h3-core/tests/it/projected_eviction.rs) (6) | UTM rasters without a certificate retain cells to EOF; Albers rasters evict early and soundly; `eviction_north_bound` holds over curvature; off-grid poles and large polar chunks cannot produce a finite certificate; an Albers apex inside a chunk disables the boundary certificate. |
| [`core/compaction_streaming.rs`](../crates/raster-h3-core/tests/it/compaction_streaming.rs) (2) | Compacted streams evict early yet match EOF grouping, never emit north of a published horizon, and PMTiles rejects compacting streamers before writing. |
| [`core/analytic_south_lat.rs`](../crates/raster-h3-core/tests/it/analytic_south_lat.rs), [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs) | `test_compute_cell_south_lat`, `test_scan_horizon_streamer_with_prefetch_and_coherence`, `test_web_mercator_streaming`. |

## 4. Super-sampling

| Suite | What it checks |
| :--- | :--- |
| [`core/super_sampling.rs`](../crates/raster-h3-core/tests/it/super_sampling.rs) (13) | Mass conservation for every pattern (centre, RGSS, quincunx, Gaussian, hex, grids) on continuous and categorical rasters, gradient consistency, UTM Jacobian conservation and exact cell assignment, Web Mercator samples equal exact projection, custom single samples keep their offsets and weights. |
| [`core/super_sampling.rs`](../crates/raster-h3-core/tests/it/super_sampling.rs) (2) | The fused projection/assignment/accumulation path matches a reference for statistics, histograms and quantiles. A throughput comparison is ignored by default. |
| [`core/super_sampling.rs`](../crates/raster-h3-core/tests/it/super_sampling.rs) | `test_subpixel_sampling_patterns`, `test_subpixel_rgss_streaming`, `test_subpixel_hex_streaming`, `test_sampling_pattern_fallback_to_center`. |
| Unit: `aggregator/sampling.rs` (2) | Pattern offsets and weights. |

See [Super-Sampling](super-sampling.md) for the patterns themselves.

## 5. Statistics & accumulators

| Suite | What it checks |
| :--- | :--- |
| [`core/quantile_pushdown.rs`](../crates/raster-h3-core/tests/it/quantile_pushdown.rs) (8) | `quantiles_batch` and `QuantilePlan` equal per-target `quantile()`/`iqr()` bit for bit on empty, single-value, negative, zero, duplicate and wide-range sketches, for unsorted, repeated, endpoint, out-of-range and NaN targets and (near-)degenerate clamp ranges; single-probability plans (including duplicates and missing sketches) match scalar queries and empty plans clear output; the parser rejects NaN/infinite percentiles; continuous readers skip sketches unless a quantile column is projected, without changing other columns, and projected quantile columns (reordered, repeated) match per-record values. |
| [`core/quantiles.rs`](../crates/raster-h3-core/tests/it/quantiles.rs) (13) | DDSketch accuracy on empty, single-value, uniform, negative, zero-bucket and mixed inputs; associative/commutative merges; weighted inserts; preset and custom quantile specs; disabled quantiles cost nothing; log-key mapping over wide ranges. |
| [`core/accumulators.rs`](../crates/raster-h3-core/tests/it/accumulators.rs), [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs), [`core/chunks_and_prefetch.rs`](../crates/raster-h3-core/tests/it/chunks_and_prefetch.rs) | Welford mean/variance (`test_accumulator_operations`, `test_accumulator_single_sample_variance_and_empty_default`, `test_welford_accumulator_extreme_mixed_bathymetry_numerical_stability`); SIMD span kernels equal scalar Welford for f32, f64, variable lengths, integer types and large integers (`test_simd_span_*`); NoData/NaN filtering keeps statistics intact (`test_nodata_filtering_preserves_statistics`, `test_nan_filtering_in_streaming`, `test_custom_nodata_override`, `test_is_chunk_all_nodata`, `test_all_nodata_full_stream_scan_and_categorical`). |
| Unit: `aggregator/nodata.rs` (5), `aggregator/simd.rs` (1) | NoData casting rules (rejects fractional, non-finite and out-of-range integer markers) and span accumulation. |

## 6. Categorical aggregation & remapping

| Suite | What it checks |
| :--- | :--- |
| [`core/categorical_long_streaming.rs`](../crates/raster-h3-core/tests/it/categorical_long_streaming.rs) (3) | Long-format output on a 300-class raster: batch sizes 1/7/32/4096 equal rows built from the streamer, batches are full except the last, cells split across batches stay contiguous and sorted, limits stop mid-cell, and projecting without entropy keeps values. |
| [`core/categorical_remap.rs`](../crates/raster-h3-core/tests/it/categorical_remap.rs) (5) | Exact, range, list and wildcard remap syntax; sparse and negative categories; invalid syntax; remapping inside single- and multi-resolution categorical streamers. |
| [`core/accumulators.rs`](../crates/raster-h3-core/tests/it/accumulators.rs) | Accumulator operations and merges, tied majorities, homogeneous rasters, RLE with interspersed NoData, high-cardinality Shannon entropy, the 16-slot inline array spilling to a heap map, SIMD uniformity across types (`test_categorical_*`). |
| Unit: `aggregator/categorical.rs` (4), `aggregator/remap.rs` (4) | Histogram and remapper internals. |

## 7. Multi-resolution & compaction

| Suite | What it checks |
| :--- | :--- |
| [`core/multi_resolution.rs`](../crates/raster-h3-core/tests/it/multi_resolution.rs) (9) | One fused pass over several resolutions equals separate single-resolution runs exactly, for continuous and categorical data, with and without supersampling; empty-resolution errors; prefetch batch draining; sorted streaming Parquet export; compaction configuration validation. |
| [`core/multiband_and_query_pushdown.rs`](../crates/raster-h3-core/tests/it/multiband_and_query_pushdown.rs) | `test_hierarchical_compaction_continuous_conservation`, `test_hierarchical_compaction_categorical_conservation`. |
| [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs) | `test_scalar_resolution_and_parent_calculations`, `test_invalid_resolution_parameter_error`. |
| Unit: `multi_horizon/compaction.rs` (2), `multi_horizon/config.rs` (2) | Compaction groups and config validation. |

## 8. Bands, spectral indices & pushdown

| Suite | What it checks |
| :--- | :--- |
| [`core/multiband_and_query_pushdown.rs`](../crates/raster-h3-core/tests/it/multiband_and_query_pushdown.rs) (12), [`duckdb/cell_bbox_pushdown.rs`](../crates/raster-h3-duckdb/tests/it/cell_bbox_pushdown.rs) (2) | Band selection and samples-per-pixel; NDVI, NDWI, NBR and EVI on the fly; zero-denominator and NoData resilience; out-of-range band clamping; borrowed interleaved windows match across job sizes; `min_count`/`min_mean`/`min_majority_fraction` pushdown; `h3_cell`/`h3_hex` bbox pushdown at high latitude and across the antimeridian without dropping raster samples. |
| Unit: `multi_horizon/spectral.rs` (5) | Formula evaluation and singularity guards. |

## 9. Threading & concurrency

Results must not depend on thread count, scheduling or pool nesting.

| Suite | What it checks |
| :--- | :--- |
| [`core/threading.rs`](../crates/raster-h3-core/tests/it/threading.rs) (6) | Identical output across Rayon thread counts; pool scaling; a high-contention stress run; multithreaded prefetched chunk reading; aggregation workers run concurrently inside `ThreadPool::install`; a dedicated pool behind a shared mutex does not deadlock. |
| [`core/sharding.rs`](../crates/raster-h3-core/tests/it/sharding.rs) (5) | Shard balance for H3 indices at resolutions 0–15 and in local neighbourhoods; sharded merge + eviction preserves continuous statistics and categorical histograms; worker results merge before the watermark advances. |
| [`core/chunks_and_prefetch.rs`](../crates/raster-h3-core/tests/it/chunks_and_prefetch.rs) | `test_prefetched_chunk_reader`, `test_prefetched_chunk_reader_multi_worker_ordering`, `test_prefetched_chunk_reader_early_drop`. |
| [`core/mosaic.rs`](../crates/raster-h3-core/tests/it/mosaic.rs) | Multi-worker ordering and early drop; a 17-tile, ten-strip-row mosaic checks eviction/reopening, Deflate and fallback PackBits pixel values, edge strips, recycling and byte budgets with one/four workers. With `test-hooks`, it also bounds decoder opens to prevent round-robin thrashing. |
| Unit: `raster/prefetch.rs` (20), `multi_horizon/sharded_map.rs` (1), `aggregator/cell_hash.rs` (2) | `OrderedPrefetchQueue` ordering, backpressure, decode budgets, cancellation (using the `test-hooks` handshakes); future-use decoder eviction, skipped jobs, open failures and strict live-decoder bounds; shard hashing. |

## 10. Memory budgets & spilling

| Suite | What it checks |
| :--- | :--- |
| [`core/memory_budgets.rs`](../crates/raster-h3-core/tests/it/memory_budgets.rs) (2) | The decoded-chunk budget bounds held chunks without changing results; the compaction limit decomposes groups without losing or duplicating samples. |
| [`core/sparse_chunks_and_spilling.rs`](../crates/raster-h3-core/tests/it/sparse_chunks_and_spilling.rs) (4) | Sparse COG chunks are filled with a non-zero NoData and skipped; a low aggregation budget creates spill runs (`spill_run_count() > 0`) and the external merge matches an in-memory reference; with resolutions 6 and 8, compaction and four aggregation workers, a budget that spills only resolution 8 (resolution 6 stays in memory, checked per resolution) reproduces the in-memory continuous (count, sum, min, max, p50) and categorical (heap class maps) output exactly. |
| Unit: `multi_horizon/spill.rs` (5) | Spill file writing and merging. |

See [Engineering Architecture](engineering.md) for what the budgets cover.

## 11. Failure propagation & resilience

A failure after a valid header must reach the consumer as an error, never as a short but
"successful" result.

| Suite | What it checks |
| :--- | :--- |
| [`core/streaming_failures.rs`](../crates/raster-h3-core/tests/it/streaming_failures.rs) (6), [`duckdb/record_queue.rs`](../crates/raster-h3-duckdb/tests/it/record_queue.rs) (1) | Corrupt-chunk errors latch in both streamers; a missing or non-directory spill directory fails both streamers with a latched error and no records after it, and spill runs leave no files in a usable one (on Unix, not even mid-stream); the DuckDB record queue propagates failure instead of EOF; failed exports leave existing outputs untouched; a Parquet file is not published after a late failure; premature prefetch termination is an error. |
| [`duckdb/prefetch_resilience.rs`](../crates/raster-h3-duckdb/tests/it/prefetch_resilience.rs) (3) | Mixed local/remote mosaics finish without deadlock; a decode-worker panic latches into the queue; a corrupt chunk unblocks readers instead of hanging. |
| [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs) | `test_nonexistent_file_path_error`, `test_corrupted_and_empty_tiff_error_handling`. |
| [`cli/binary.rs`](../crates/raster-h3-cli/tests/it/binary.rs), [`python/batch_reader.rs`](../crates/raster-h3-python/tests/it/batch_reader.rs), [`python/exports.rs`](../crates/raster-h3-python/tests/it/exports.rs), [`pg/srf.rs`](../crates/raster-h3-pg/tests/it/srf.rs) | A strip that fails to decode part-way is a non-zero CLI exit, an `out_err` from `raster_h3_py_batch_reader_next` / `scan_json` / the exporters, and `-1` from `pg_srf_continuous_next`, never a short successful result; failed Python and CLI Parquet exports leave no file. |

## 12. GeoTIFF decoding & codecs

| Suite | What it checks |
| :--- | :--- |
| [`core/codecs.rs`](../crates/raster-h3-core/tests/it/codecs.rs) (10) | `libdeflater` output is bit-identical to the `tiff` decoder for striped u8/f32, tiled f32 with padding, floating-point and horizontal predictors; buffer recycling and auto-resizing; corrupted streams; cropped edge tiles in both streaming paths. |
| [`core/codecs.rs`](../crates/raster-h3-core/tests/it/codecs.rs) (9) | Fast LZW parity with the `tiff` decoder on synthetic tiled data with predictors and edge padding; corrupted and truncated streams; buffer resizing. Four parity/recycling tests use the Hawaii datasets and are ignored by default. |
| [`core/conus_pipeline.rs`](../crates/raster-h3-core/tests/it/conus_pipeline.rs) | `test_tiff_magic_validation_edge_cases`. |
| [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs), [`core/chunks_and_prefetch.rs`](../crates/raster-h3-core/tests/it/chunks_and_prefetch.rs) | `test_u16_raster_streaming`, `test_f64_raster_streaming`, `test_tiled_chunk_layout_calculation`, `test_tiled_uneven_chunk_layout_and_bounds`. |
| Unit: `raster/predictor.rs` (9) | Predictor 2 (including NEON), predictor 3 unpacking, and rejection of truncated buffers and invalid/overflowing layouts. |

## 13. Remote COG & cloud streaming

| Suite | What it checks |
| :--- | :--- |
| [`core/remote_cog.rs`](../crates/raster-h3-core/tests/it/remote_cog.rs) (32) | Against a local mock HTTP server: URL normalisation and S3 regional/custom endpoints; header read budget; numerical equality with local reads; ROI-selective streaming; 404 handling; transient retry and recovery; header injection; range coalescing; rejection of truncated, ignored or wrong ranges; full-body responses served from cache; reads across cache blocks; in-flight prefetch completion, final fetch errors and cancellation waking all consumers; the prefetch byte budget bounding held bytes under a slow consumer with no duplicate requests, coalesced responses shared by their chunks and charged until the last slice drops, oversized ranges fetched one at a time, and cancellation waking workers blocked on the budget; out-of-order and concurrent demand, coalesced sibling refetch, full count capacity, caller-clone retention and payload validity after queue drop; remote mosaics and the multi-resolution streamer; remote COG → Parquet. |
| Unit: `raster/http_range.rs` (5), `raster/remote_prefetch.rs` (7) | Range validation, caching, coalescing, backoff and the private terminal-update locking invariant. |

## 14. Mosaics & overlap rules

| Suite | What it checks |
| :--- | :--- |
| [`core/mosaic.rs`](../crates/raster-h3-core/tests/it/mosaic.rs) (10) | Glob matching, comma lists, VRT XML resolution; adjacent-tile conservation; `cutline` vs `first` vs `average`; categorical overlaps; mixed VRT paths and export pipelines. |
| [`core/cutline_ownership.rs`](../crates/raster-h3-core/tests/it/cutline_ownership.rs) (3) | `Cutline` ownership with cached centre vectors matches the original per-call computation for every tile at thousands of points: an overlapping 4×4 grid, three identical footprints (exact distance ties go to the lower index), tiles on different longitude branches across the antimeridian, and near-polar tiles. |
| [`core/mosaic.rs`](../crates/raster-h3-core/tests/it/mosaic.rs) (4) | Mosaic opening keeps antimeridian tiles; overlap rules test real tile footprints, not WGS84 rectangles, including rotated footprints and mixed longitude branches. |
| [`core/conus_pipeline.rs`](../crates/raster-h3-core/tests/it/conus_pipeline.rs) | `test_conus_batch_and_evict_pipeline_simulation`: multi-tile ingestion, batch eviction, Parquet progress and PMTiles from partitioned input. |
| [`datafusion/continuous_options.rs`](../crates/raster-h3-datafusion/tests/it/continuous_options.rs) | `with_overlap_rule_on_a_multi_file_source_matches_core`: `cutline`, `first` and `average` over a comma-list source, with exact per-rule pixel counts. |

## 15. Geometry encoding (WKB)

| Suite | What it checks |
| :--- | :--- |
| [`duckdb/spatial_geometry.rs`](../crates/raster-h3-duckdb/tests/it/spatial_geometry.rs) (10) | WKB for every resolution, Class III pentagons and edge-crossing cells; WKB vs `GEOMETRY` byte equality; hex-string input; invalid cells; DuckDB version parsing and geometry column indices under projection. |
| [`core/geodetic_reference.rs`](../crates/raster-h3-core/tests/it/geodetic_reference.rs) | `wkb_buffer_capacity_all_resolutions_pentagons_and_icosahedron_edges`. |
| [`core/polygon_to_cells.rs`](../crates/raster-h3-core/tests/it/polygon_to_cells.rs) (1) | POLYGON/MULTIPOLYGON WKB polyfill against direct `h3o` output, including holes, overlapping members, four containment modes and two resolutions. |
| [`duckdb/spatial_geometry.rs`](../crates/raster-h3-duckdb/tests/it/spatial_geometry.rs) | `test_wkb_ogc_compliance`, `test_fast_hex_formatting`. |
| [`pg/fmgr.rs`](../crates/raster-h3-pg/tests/it/fmgr.rs) | `h3_to_wkb_returns_core_wkb_including_pentagons`: the `bytea` from `pg_h3_to_wkb` equals the core WKB, including 10-vertex pentagons. |
| Unit: `encoding/wkb.rs` (8), `encoding/fast_hex.rs` (2) | Ring construction, antimeridian splitting and polar caps (every cell at resolutions 0–2 checked for range, closure, orientation, simplicity and area), hex formatting/parsing. |

## 16. GeoParquet export

| Suite | What it checks |
| :--- | :--- |
| [`core/geoparquet.rs`](../crates/raster-h3-core/tests/it/geoparquet.rs) (8) | Continuous and categorical metadata and WKB; compact and full schemas; GeoParquet off by default; raster → GeoParquet end to end; deep validation against OGC GeoParquet 1.1 (PROJJSON, edges, bbox); column projection. |
| [`core/multi_resolution.rs`](../crates/raster-h3-core/tests/it/multi_resolution.rs) | Sorted streaming Parquet exports. |
| Unit: `parquet/geoparquet_metadata.rs` (1) | Metadata JSON. |

## 17. PMTiles & MVT export

| Suite | What it checks |
| :--- | :--- |
| [`core/pmtiles.rs`](../crates/raster-h3-core/tests/it/pmtiles.rs) (24) | MVT protobuf encoding; PMTiles v3 header and archive validation; Hilbert tile IDs (ordering and bijective round trip); resolution ↔ zoom monotonicity; cell → tile mapping; coarse-zoom parent aggregation over all children; property filtering; GeoTIFF and Parquet (custom column, hex strings, multi row-group eviction) to PMTiles; all-NoData rasters; Class III 10-vertex pentagons; antimeridian-straddling cells and world tiles. |
| Unit: `pmtiles/pyramid.rs` (3), `pmtiles/overview.rs` (2) | Tile math and overview levels. |

See [PMTiles & Multi-Resolution](pmtiles.md).

## 18. DuckDB extension

| Suite | What it checks |
| :--- | :--- |
| [`duckdb/ffi_sql.rs`](../crates/raster-h3-duckdb/tests/it/ffi_sql.rs) (6) | Mock C-API callbacks: init and registration completeness, scalar NULL propagation, `h3_polygon_to_cells` BLOB overload registration, both `h3_cell_to_geometry` overloads and output parity, continuous scans with projection pushdown, quantile sketches built only when a quantile column is projected (`geom` and `wkb` excluded) with reordered/repeated quantile columns matching a full scan, categorical wide and long, and export table functions with CRS overrides. No SQL is executed. |
| [`root/duckdb_sql_smoke.rs`](../tests/it/duckdb_sql_smoke.rs) (4, ignored) | Real SQL through DuckDB v1.2.0 and v1.5.6: continuous/categorical scan names and aliases, pixel totals and constant means, projected CRS locations, scan/Parquet bidirectional `EXCEPT` parity, Parquet-to-PMTiles positional/named arguments, status/count/magic checks, `h3_cell_to_geometry` overloads and WKB bytes, and polygon modes/aliases/invalid-WKB NULL behavior. DuckDB Spatial is installed for v1.5.6 to serialize native geometry as WKB. |
| [`test/sql/raster_h3.test`](../test/sql/raster_h3.test) (sqllogictest) | Packaged `build/release/raster_h3.duckdb_extension` from `make release` loads through DuckDB's Python sqllogictest runner (the community-extensions test path); `h3_to_string` is absent before `require raster_h3` and returns the expected cell after. |
| [`root/package_extension.rs`](../tests/it/package_extension.rs) (2) | Extension footer layout: custom section headers, ABI fields, version and platform strings, zeroed signature, gzip output. |
| Unit: `raster-h3-duckdb` (28) | `ffi/duckdb_c.rs` (9: `duckdb_string_t` layout), `ffi/mod.rs` (3), `bind_utils/parsing.rs` (6), `bind_helper.rs` (2), `chunk_writer.rs` (3), `record_queue.rs` (1), `scalar.rs` (4). |

See [DuckDB](engines/duckdb.md).

## 19. Other engines & cross-engine parity

| Suite | What it checks |
| :--- | :--- |
| [`root/multi_engine_e2e.rs`](../tests/it/multi_engine_e2e.rs) (4) | Continuous and categorical per-cell parity across core batch readers, the Arrow C stream, the CLI binary (`ipc`, `jsonl`, `parquet`, `pmtiles`), DataFusion (scan, pushdown, UDFs), Python (a `python3` subprocess loading the cdylib), the PostgreSQL SRFs and fmgr scalars in process, and DuckDB when `duckdb` is on `PATH`; continuous parity also covers EPSG:5070 and EPSG:27700 rasters, and polygon results are compared exactly across core, Python and DuckDB. |
| [`root/pg_extension_e2e.rs`](../tests/it/pg_extension_e2e.rs) (1, ignored) | Builds the PostgreSQL module in Docker, loads it into `postgres:<major>` for each `RASTER_H3_PG_VERSIONS`, compares `psql` output with in-process SRFs and checks that bad `bbox`/`h3_res` raise SQL errors without breaking the session. |
| Unit: `raster-h3-core` columnar (6), `scalar_ops.rs` (6) | Arrow C stream round trip, release callback null-safety/idempotence, cross-thread owned export release, `RH3IPC01` round trip, scalar helpers. |
| [`datafusion/continuous_options.rs`](../crates/raster-h3-datafusion/tests/it/continuous_options.rs) (8) | Each `ContinuousRasterTableProvider` builder option equals a core `ContinuousBatchReader` run with the same config and changes the output: `with_nodata` (drops exactly the matching pixels), `with_sampling`, `with_band` (0 clamps to 1), `with_spectral_formula`, `with_quantiles` (extra columns, projectable), `with_min_count`/`with_min_mean`/`with_max_mean` intersected with pushed `MinCount`/`MinMean`/`MaxMean` (the stricter bound wins), `with_compact` (mass conserved; adjacent resolutions rejected), `with_overlap_rule` on a two-file source. |
| [`datafusion/categorical_options.rs`](../crates/raster-h3-datafusion/tests/it/categorical_options.rs) (3) | `CategoricalRasterTableProvider` `with_nodata`, `with_remapper` (wide and long) and `with_min_majority_fraction` / `with_min_count` intersected with pushed filters, against `CategoricalBatchReader`. |
| [`datafusion/statistics.rs`](../crates/raster-h3-datafusion/tests/it/statistics.rs) (3) | `statistics()` returns the inexact core cardinality estimate for both providers, grows with resolution, resolution count and mosaic files, stays within 4× of the real row count, and falls back to 10 000 per resolution for unreadable rasters. |
| [`datafusion/scalar_udfs.rs`](../crates/raster-h3-datafusion/tests/it/scalar_udfs.rs) (3) | `string_to_h3`, `h3_to_lat`, `h3_to_lng` against `h3o` and the core scalars on cells near the poles, the antimeridian and a pentagon: validity pass-through, NaN for invalid cells, type errors, round trip back to the same cell. |
| [`cli/binary.rs`](../crates/raster-h3-cli/tests/it/binary.rs) (7) | The built `raster-h3-cli` binary against core readers and exporters: `--bbox`, `--nodata` and `--limit` change the output exactly as the core config does; stdout (no `-o`, `-o -`) equals file output; projected JSONL and categorical long format; categorical Parquet rows and a categorical PMTiles archive byte-identical to the core tiler; unknown subcommands, bad options and missing, non-TIFF or corrupt inputs exit non-zero with a stderr message. |
| [`python/batch_reader.rs`](../crates/raster-h3-python/tests/it/batch_reader.rs) (3) | `raster_h3_py_open_batch_reader` / `_batch_reader_next` / `_free_batch_reader`: one bounded batch per stream, rows equal to core, projection, limit, categorical long, NULL-reader and open errors, early free. |
| [`python/c_abi.rs`](../crates/raster-h3-python/tests/it/c_abi.rs) (5) | `raster_h3_py_scan_json` columns equal core output with NoData, bbox, CRS, projection, limit, categorical wide/long; errors set `out_err`, including malformed bbox, resolutions and format, which are rejected rather than ignored or defaulted. |
| [`python/exports.rs`](../crates/raster-h3-python/tests/it/exports.rs) (2) | `raster_h3_py_export_parquet` / `_pmtiles` error paths return `-1` with a message and no output; successful exports return core row counts and PMTiles bytes identical to the core tiler. |
| [`python/scalars.rs`](../crates/raster-h3-python/tests/it/scalars.rs) (2) | `raster_h3_py_h3_is_valid_str` and `raster_h3_py_h3_cell_to_parent_str` against `h3o` and their `u64` variants. |
| [`pg/srf.rs`](../crates/raster-h3-pg/tests/it/srf.rs) (4) | `PgCategoricalSrf` with resolution, CRS, NoData and bbox options against `CategoricalBatchReader`; the `pg_srf_continuous_*` C-ABI lifecycle with options, WKB (including the full 189-byte boundary of an odd-resolution pentagon) and hex columns, NULL handles and errors. |
| [`pg/fmgr.rs`](../crates/raster-h3-pg/tests/it/fmgr.rs) (7, `pg-extension` only) | The V1 `fmgr` entry points in process, against server stand-ins in [`pg/fmgr/server.rs`](../crates/raster-h3-pg/tests/it/fmgr/server.rs) (`palloc`, `funcapi` multi-call helpers, reset callbacks): `pg_h3_to_string`, `pg_string_to_h3` (1- and 4-byte varlena headers), `pg_h3_is_valid_text`, `pg_h3_to_wkb`, the finfo records and magic block, and the categorical and continuous value-per-call SRFs against core output, including state release on early termination. Error reporting (`ereport` longjmps) is left to `root/pg_extension_e2e.rs`. |
| Unit: `raster-h3-datafusion` (3) | Projection, limit, filter narrowing, Arrow stream, UDFs. |
| Unit: `raster-h3-cli` (2) | Argument validation, IPC and JSONL output. |
| Unit: `raster-h3-python` (3), `raster-h3-pg` (4 + 3 in `fmgr.rs`) | C-ABI helpers and SRF iterators. The `fmgr.rs` tests only compile with `--features pg-extension`. |
| [`pytest/test_arrow_stream.py`](../crates/raster-h3-python/tests/pytest/test_arrow_stream.py) (12) | The in-tree `raster_h3` package with the locally built cdylib, read by real PyArrow and Polars: continuous `h3_index`/`count`/`sum`/`min`/`max`/`mean` equal an independent reference that indexes every pixel centre with the `h3` package; `pl.from_arrow`, `to_polars()`, `read_raster_h3` and `iter_batches` equal `pyarrow.table` on `RasterH3ArrowStream` (`__arrow_c_stream__`), and `pyarrow.schema` (`__arrow_c_schema__`) equals the stream schema; `columns` and `limit`; categorical long counts/fractions and wide majority class/count/fraction against the `h3` reference; missing files, non-TIFF input, unknown columns and a non-intersecting bbox raise `RuntimeError`. |
| [`pytest/test_polars_scan.py`](../crates/raster-h3-python/tests/pytest/test_polars_scan.py) (12) | `scan_raster_h3` as a Polars IO source (`register_io_source`, Polars ≥ 1.4): projection pushdown opens the native scan with only the selected columns; predicate pushdown (numeric, combined, string, `is_in` on `h3_index`) and `head(n)` (pushed as the scan limit), `head` after `filter`, and the `limit`/`columns` arguments give the same rows as the eager full scan filtered in Polars; empty results keep the schema; categorical wide/long pushdown; scan errors and unknown columns raise. |
| [`wheel_smoke.py`](../crates/raster-h3-python/tests/wheel_smoke.py) | After `cibuildwheel` installs a wheel: the bundled library loads, Arrow/Polars paths work, `raster-h3-cli` is on `PATH`, and `h3_polygon_to_cells` handles a cell boundary and invalid resolution. |

See the [engine deep dives](engines/README.md).

## 20. Performance guards & benchmarks

| Suite | What it checks |
| :--- | :--- |
| [`core/performance.rs`](../crates/raster-h3-core/tests/it/performance.rs) (4) | Continuous, multi-resolution, categorical and supersampled streaming conserve pixel mass, with no wall-clock pass/fail thresholds. |
| [`core/properties.rs`](../crates/raster-h3-core/tests/it/properties.rs) (5 generated tests) | Weighted merge equivalence, WKB validity/truncation, affine and Mercator round trips, and nodata/supersampled conservation plus cell uniqueness across the antimeridian. |
| [`core/benches/streaming.rs`](../crates/raster-h3-core/benches/streaming.rs) | Criterion baselines for continuous, multi-resolution, categorical and supersampled streaming; `single_quantile` scalar/batch/plan p50/p90 queries on small positive and larger mixed-sign sketches; hash-sensitive quantile and 8-class inline / 64-class heap categorical accumulators; raw res-10 H3 map insertion/removal. Fixture/input creation is not timed. |
| [`core/super_sampling.rs`](../crates/raster-h3-core/tests/it/super_sampling.rs) | `benchmark_supersampling` (ignored): manual release throughput comparison. |
| [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs) | `test_parallel_chunk_aggregation_hawaii_dataset`, `test_parallel_categorical_aggregation_hawaii_dataset` (ignored, need data). |

Run `cargo bench --locked -p raster-h3-core --bench streaming`. To compare a local change
on the same machine, first pass `-- --save-baseline before`, then run the changed code
with `-- --baseline before`. Scheduled/manual benchmark runs preserve Criterion results
as workflow artifacts; shared-runner timings are informational, not release gates.
Additional workload demonstrations live in [`examples/`](../examples) (`benchmark_*`);
see [Architecture Comparison](architecture-comparison.md).

`python3 -m unittest discover -s scripts -p 'test_run_polars_res9.py'` runs four
benchmark-harness regressions: LazyFrame export, Arrow-stream fallback, error propagation,
and CLI usage. The benchmark uses `perf_counter` and labels its backend. Its fallback
uses the direct exporter without geometry and writes the full statistics schema,
not the four-column Polars projection; timings from the two backends are not interchangeable.

### Fuzzing and unsafe-code validation

The [safety workflow](../.github/workflows/safety.yml) runs a short coverage-guided,
AddressSanitizer-enabled fuzz smoke test on code changes and tags, and longer weekly runs.
Targets cover the core WKB/EWKB parser, GeoTIFF metadata reader, and integer/floating-point
chunk unpacking and predictors. Dimensions and allocations in the predictor harness are bounded;
GeoTIFF metadata fuzzing deliberately does not decode attacker-controlled image dimensions.
These are not exhaustive CRS or database ABI fuzzers.

```bash
cargo install cargo-fuzz --version 0.13.1 --locked
cargo +nightly run --locked --manifest-path fuzz/Cargo.toml --example seed_corpus
cargo +nightly fuzz run wkb -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run geotiff -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run predictor -- -max_total_time=60 -max_len=65536
rustup component add --toolchain nightly miri rust-src
cargo +nightly miri test --locked -p raster-h3-core --lib columnar::arrow_c::tests
```

Miri checks Arrow schema/array/stream ownership, callbacks, round trips and slicing; it
does not run the external DuckDB/PostgreSQL/Python hosts. Existing real-host smoke tests
remain necessary. Fuzz crash artifacts are retained on CI failure. Minimize a reproducer
with `cargo +nightly fuzz tmin <target> <artifact>` and add a deterministic regression test;
report security-sensitive findings privately per [SECURITY.md](../SECURITY.md).

---

## Running the Tests

```bash
cargo test --release --workspace                    # every crate: unit tests + tests/it binaries
cargo test --release -p raster-h3-core              # core only; does not compile other engines
cargo test --release -p raster-h3-core --test it threading::  # one suite (module filter)
cargo test --release -p raster-h3-core prefetch     # tests matching a filter
cargo test --release --workspace -- --ignored       # only the gated tests
RASTER_H3_PG_MAJOR=16 cargo test --release -p raster-h3-pg --features pg-extension  # adds the fmgr suites
```

The Python package tests run with pytest against the locally built library:

```bash
cargo build -p raster-h3-python --lib
python -m pip install pytest polars pyarrow h3
python -m pytest crates/raster-h3-python/tests/pytest   # RASTER_H3_PYTHON_LIB picks a specific library
```

Use `--release` for anything that streams real data; debug builds are much slower.

### Gated tests

| Test | Gate | How to run |
| :--- | :--- | :--- |
| `duckdb_sql_smoke` (4 root tests) | `#[ignore]`; needs the DuckDB CLI and a built library; v1.5.6 needs network access to install DuckDB Spatial | `cargo build --release --lib && DUCKDB_BIN=/path/to/duckdb cargo test --release -p raster_h3 --test it duckdb_sql_smoke -- --ignored` (`DUCKDB_VERSION`, `DUCKDB_BIN`, and `RASTER_H3_LIBRARY` select the footer version, CLI, and library) |
| `pg_extension_e2e::pg_extension_in_real_server` (root) | `#[ignore]`; needs Docker | `cargo test -p raster_h3 --test it pg_extension_e2e -- --ignored --nocapture` (`RASTER_H3_PG_VERSIONS=14,16,18`, `RASTER_H3_PG_E2E_KEEP=1`) |
| 4 × `codecs` LZW Hawaii parity, 2 × `horizon_streaming` Hawaii aggregation (core) | `#[ignore]`; need `data/CFL_HI.tif` / `data/LF2024_FBFM40_HI.tif` | Download the datasets into the workspace-root `data/`, then `-- --ignored`. They fail explicitly if the files are missing. |
| `super_sampling::benchmark_supersampling` (core) | `#[ignore]` | `cargo test --release -p raster-h3-core --test it benchmark_supersampling -- --ignored --nocapture` |
| `pg/fmgr.rs` (7) and the `fmgr.rs` unit tests | `--features pg-extension`; needs `RASTER_H3_PG_MAJOR` or `pg_config` on `PATH` | `RASTER_H3_PG_MAJOR=16 cargo test -p raster-h3-pg --features pg-extension` |
| DuckDB part of `multi_engine_e2e` | Skipped when `duckdb --version` fails | Put the DuckDB CLI on `PATH` |
| `crates/raster-h3-python/tests/pytest` | Each module skips when `polars` or `pyarrow` is missing; `test_polars_scan.py` also skips without `polars.io.plugins` (Polars < 1.4), the `h3` reference checks skip without `h3`, the `pl.from_arrow(stream)` check skips on Polars without PyCapsule support, and the string-predicate case skips on Polars < 1.22 (it drops such predicates on IO sources) | `pip install pytest polars pyarrow h3` and build the cdylib |
| Python part of `multi_engine_e2e` | Runs `/usr/bin/python3`; the test fails if that path is absent | Run on a system with `/usr/bin/python3` |

## What CI Runs

| Workflow | Command | Covers |
| :--- | :--- | :--- |
| [`ci.yml`](../.github/workflows/ci.yml), per platform (Linux, macOS, Windows) | `cargo test --release --no-fail-fast -p raster_h3 -p raster-h3-core -p raster-h3-duckdb` | Unit tests and `tests/it` binaries of the root, core and DuckDB crates (gated ones excluded) |
| `ci.yml` | `cargo build --release --lib` then ignored `duckdb_sql_smoke` tests against downloaded v1.2.0 and v1.5.6 CLIs | Real-SQL coverage on both DuckDB versions, packaging with each CLI's `PRAGMA platform` tag |
| `ci.yml` (`community-build`) | `make configure release` via the `extension-ci-tools` submodule, then `test/sql` with `duckdb_sqllogictest` on linux_amd64 | The community-extensions build path (`build: cargo`) keeps working |
| `ci.yml` (`test-crates`, Linux) | `cargo test --release -p raster-h3-core -p raster-h3-duckdb -p raster-h3-datafusion -p raster-h3-cli -p raster-h3-python -p raster-h3-pg`, then PostgreSQL with `--features pg-extension` | Every adapter's unit/integration suites, including PostgreSQL's fmgr helpers |
| `ci.yml` (`metadata`, `package-check`) | Version-check script and its Python unit tests; `cargo package --locked -p raster-h3-core` | Version/publication policy, license consistency, and a clean-room packaged-core compile |
| [`safety.yml`](../.github/workflows/safety.yml) | Bounded cargo-fuzz targets and scoped Arrow Miri tests | Malformed input, sanitizer smoke checks, and FFI ownership; longer fuzz runs weekly |
| [`benchmarks.yml`](../.github/workflows/benchmarks.yml) | Criterion streaming benchmarks | Weekly/manual informational timings, not correctness or release gates |
| `ci.yml`, per platform | `cargo test --locked --release -p raster-h3-core --example profile_hotspots` | Hotspot harness ownership-sampling correctness; profiling experiments are excluded |
| [`profile.yml`](../.github/workflows/profile.yml) (optional, manual-only) | `cargo run --release -p raster-h3-core --example profile_hotspots -- --json profile.json` | Informational: mosaic planning/ownership scaling with tile count, end-to-end cost per overlap rule, and nested-Rayon wave dispatch (waves, helper-thread hand-offs, allocations). Prefer [local profiling](engines/core.md#profiling-and-wave-dispatch) for comparisons. Optional hosted results appear in the job summary, `::notice` annotations and a JSON artifact |
| `ci.yml` | `duckdb -unsigned -c "LOAD …; SELECT h3_to_string(…)"` | Packaged extension loads |
| [`wheels.yml`](../.github/workflows/wheels.yml) | `cibuildwheel` + `wheel_smoke.py` | Each built wheel installs and works |
| [`manual-tests.yml`](../.github/workflows/manual-tests.yml) (manual) | `cargo test [--release] --no-fail-fast -p <engine>` plus `--features pg-extension` for `pg`/`all` (not on Windows) | Any engine (`all`, `core`, `duckdb`, `datafusion`, `cli`, `python`, `pg`, `root`) on one or all of Linux amd64/arm64, macOS arm64 and Windows, for any branch, tag or SHA, with optional libtest arguments |
| `manual-tests.yml`, engine `python-polars` | `cargo build [--release] -p raster-h3-python --lib` then `pytest crates/raster-h3-python/tests/pytest` | The pytest suite per platform with three dependency sets: `polars==1.0.*` + `pyarrow==14.*` (Python 3.11, `numpy<2`), `polars==1.4.*` + `pyarrow==14.*` (first Polars with `register_io_source`) and the latest of both (Python 3.13); `test_args` are passed to pytest |

To run it: **Actions → Manual Tests → Run workflow**, or

```bash
gh workflow run manual-tests.yml -f ref=my-branch -f engine=datafusion -f platform=all
```

The workflow file must exist on the default branch for GitHub to list it; `ref` can then
name any branch, even one without the file.

### Gaps

- CLI, DataFusion, Python and PostgreSQL unit/integration suites run on Linux in CI;
  their cross-platform coverage is through the root parity tests or the manual workflow.
- The real PostgreSQL test and the Hawaii-dataset tests never run in CI.
- The `fmgr.rs` unit tests and `pg/fmgr.rs` only build with `--features pg-extension`.
- EPSG:6933 and EPSG:7856 have no coverage tests.
