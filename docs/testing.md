# Test Coverage Deep Dive

[← Back to README](../README.md)

This page lists every test suite in the workspace, grouped by what it protects, and
explains how to run each group and what CI runs. The counts below are the number of
`#[test]` (or pytest `test_*`) functions at the time of writing.

| Location | Suites | Tests |
| :--- | ---: | ---: |
| [`crates/raster-h3-core/tests/it/`](../crates/raster-h3-core/tests/it) (`core/…`) | 40 modules, 1 binary | 398 |
| [`crates/raster-h3-core/tests/memory/`](../crates/raster-h3-core/tests/memory) (`memory`, one child process per scenario variant) | 7 scenarios, 1 binary | 38 checks |
| [`crates/raster-h3-duckdb/tests/it/`](../crates/raster-h3-duckdb/tests/it) (`duckdb/…`) | 5 modules, 1 binary | 25 |
| [`crates/raster-h3-datafusion/tests/it/`](../crates/raster-h3-datafusion/tests/it) (`datafusion/…`) | 8 modules (plus shared `rows.rs`), 1 binary | 28 |
| [`crates/raster-h3-cli/tests/it/`](../crates/raster-h3-cli/tests/it) (`cli/…`, runs the crate's `raster-h3-cli` bin) | 2 modules, 1 binary | 14 |
| [`crates/raster-h3-python/tests/it/`](../crates/raster-h3-python/tests/it) (`python/…`) | 7 modules, 1 binary | 24 |
| [`crates/raster-h3-pg/tests/it/`](../crates/raster-h3-pg/tests/it) (`pg/…`) | 2 modules, 1 binary | 12 (8 need `pg-extension`) |
| [`tests/it/`](../tests/it) (`root/…`, needs the root cdylib or bins) | 5 modules, 1 binary | 18 |
| [`crates/raster-h3-sedona/tests/`](../crates/raster-h3-sedona/tests) (`sedona/…`, standalone workspace; one binary per module) | 6 modules | 34 (5 need `sedona-e2e`) |
| Crate unit tests (`#[cfg(test)]` in `crates/*/src`) | 7 crates | 256 |
| [`crates/raster-h3-python/tests/pytest/`](../crates/raster-h3-python/tests/pytest) (`pytest/…`, needs `polars`, `pyarrow`, `h3`) | 7 modules | 84 |
| [`crates/raster-h3-sedona/python/tests/`](../crates/raster-h3-sedona/python/tests) (`sedona-pytest/…`, needs the `sedonadb` 0.5.0 wheel) | 1 module | 14 |
| [`examples/sedona_spark/tests/`](../examples/sedona_spark/tests) (`spark/…`, needs PySpark, Apache Sedona and `raster_h3`, as in the `sedona` image) | 2 modules | 17 (23 cases) |
| [`crates/raster-h3-python/tests/spark/`](../crates/raster-h3-python/tests/spark) (`spark41/…`, needs PySpark 4.1+ and `raster_h3`) | 1 module | 4 (12 cases) |
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
19. [DataFusion](#19-datafusion)
20. [SedonaDB](#20-sedonadb)
21. [Python & Polars](#21-python--polars)
22. [PostgreSQL](#22-postgresql)
23. [CLI](#23-cli)
24. [Cross-engine parity](#24-cross-engine-parity)
25. [Performance guards & benchmarks](#25-performance-guards--benchmarks)
26. [Partitioned scans](#26-partitioned-scans)

The former catch-all `tests/test_raster_h3.rs` (58 tests) was split by topic; rows that
list individual test names come from it or from a suite that mostly covers another category.

---

## 1. Geodetic & CRS correctness

Coordinates must land where PROJ puts them, for every transform tier.

| Suite | What it checks |
| :--- | :--- |
| [`core/geodetic_reference.rs`](../crates/raster-h3-core/tests/it/geodetic_reference.rs) (25) | Golden forward and inverse points from PROJ 9.5.1 for fast paths and fallbacks, each within **0.01 m**: Web Mercator, Albers CONUS/Alaska and southern parallels with false offsets (including non-polar latitudes through ±89.9999°), UTM north/south, polar stereographic north/south, EASE-Grid 2.0 and Lambert conformal; EPSG aliases are bit-identical. Affine round trips `coord_to_pixel(pixel_to_coord(p)) == p` on rotated/sheared grids. Coverage/duplicate properties include antimeridian-crossing Albers Alaska, EPSG:32756, pole-containing and supersampled EPSG:3413/3031 rasters, plus prior horizon, longitude-wrapping and mosaic cases. |
| [`core/certified_bounds.rs`](../crates/raster-h3-core/tests/it/certified_bounds.rs) (13, plus 1 generated) | Rejection bounds enclose the true image: certified UTM/BNG bounds checked against dense samples and used to prune distant bboxes; generated rotated/sheared chunks for WGS84, Web Mercator, northern and southern Albers (including across the antimeridian, around the cone apex and past the cut opposite the central meridian) and a US-foot transverse Mercator have rect bounds and an eviction bound enclosing every sample; extreme `tmerc` scale factors stay enclosing and certificates use proj4rs's effective scale (`k0`, then `k`; `k_0` ignored); chunks ending at or one ulp below 180° keep non-reversed bounds; pole-containing UTM chunks get full longitude; huge `+towgs84` shifts and other projections fail open; partly unprojectable rectangles get whole-globe bounds; fine (1e-8°) affines invert; singular and non-finite affines are rejected on open; duplicate, quoted, non-lowercase or spaced TM parameters fail open; polar radial/angular bounds enclose dense rotated/sheared grids and preserve per-cell sums/counts across bbox and mosaic ownership modes. |
| [`core/datum_shifts.rs`](../crates/raster-h3-core/tests/it/datum_shifts.rs) (9) | Helmert transforms match PROJ with identical parameters and stay within published accuracy of PROJ defaults; inverse round trips; explicit `+towgs84`; prime-meridian and axis-order composition; nonzero shifts avoid fast paths; grid-shift datums are rejected clearly. A nonzero shift never takes a fast path; grid-shift datums are rejected with a clear error; raster cells are assigned from independent PROJ datum references; geographic pixel weights are shown not to be ground-area weights. |
| [`core/geotiff_crs.rs`](../crates/raster-h3-core/tests/it/geotiff_crs.rs) (11) | CRS detection from ESRI PE-string citations and GeoTIFF GeoKeys, EPSG/WKT handling, projected transformations and invalid/unspecified CRS paths. An ESRI PE citation with a nonzero `TOWGS84` shift is not detected (an all-zero shift is). |
| [`core/geotiff_crs.rs`](../crates/raster-h3-core/tests/it/geotiff_crs.rs), [`core/antimeridian_bbox.rs`](../crates/raster-h3-core/tests/it/antimeridian_bbox.rs) | `test_crs_web_mercator_to_wgs84`, `test_crs_utm_to_wgs84`, `test_crs_southern_hemisphere_utm`, `test_crs_custom_albers_proj_string`, `test_crs_invalid_proj_string_error_handling`, `test_crs_unspecified_fallback_and_invalid_proj_error`, `test_plain_tiff_without_geokeys`, `test_geotransform_accuracy`, `test_geotransform_inverse_precision`, `test_extreme_polar_latitudes_clamping`. |
| Unit: `crs/transformer.rs` (40) | Tier selection (identity, analytical, Proj4), authalic-series inverse Albers, public source-field mutation and struct-literal compatibility, parameter parsing, error paths, certified and fail-open rectangle bounds. |
| Unit: `raster/metadata.rs` (16), `raster/geotransform.rs` (3) | GeoKey directory parsing, tiepoint/pixel-scale/model-transformation tags, EPSG and WKT extraction; scale-independent affine inversion. |

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
| [`core/super_sampling.rs`](../crates/raster-h3-core/tests/it/super_sampling.rs) (2) | The fused projection/assignment/accumulation path matches a reference (one observation per pixel and cell) for statistics including `sum_w2` and variance, histograms and quantiles. A throughput comparison is ignored by default. |
| [`core/super_sampling.rs`](../crates/raster-h3-core/tests/it/super_sampling.rs) | `test_subpixel_sampling_patterns`, `test_subpixel_rgss_streaming`, `test_subpixel_hex_streaming`, `test_sampling_pattern_fallback_to_center`. |
| Unit: `aggregator/sampling.rs` (2) | Pattern offsets and weights. |

See [Super-Sampling](super-sampling.md) for the patterns themselves.

## 5. Statistics & accumulators

| Suite | What it checks |
| :--- | :--- |
| [`core/quantile_pushdown.rs`](../crates/raster-h3-core/tests/it/quantile_pushdown.rs) (8) | `quantiles_batch` and `QuantilePlan` equal per-target `quantile()`/`iqr()` bit for bit on empty, single-value, negative, zero, duplicate and wide-range sketches, for unsorted, repeated, endpoint, out-of-range and NaN targets and (near-)degenerate clamp ranges; single-probability plans (including duplicates and missing sketches) match scalar queries and empty plans clear output; the parser rejects NaN/infinite percentiles; continuous readers skip sketches unless a quantile column is projected, without changing other columns, and projected quantile columns (reordered, repeated) match per-record values. |
| [`core/quantiles.rs`](../crates/raster-h3-core/tests/it/quantiles.rs) (13) | DDSketch accuracy on empty, single-value, uniform, negative, zero-bucket and mixed inputs; associative/commutative merges; weighted inserts; preset and custom quantile specs; disabled quantiles cost nothing; log-key mapping over wide ranges. |
| [`core/accumulators.rs`](../crates/raster-h3-core/tests/it/accumulators.rs), [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs), [`core/chunks_and_prefetch.rs`](../crates/raster-h3-core/tests/it/chunks_and_prefetch.rs) | Welford mean/variance (`test_accumulator_operations`, `test_accumulator_single_sample_variance_and_empty_default`, `test_welford_accumulator_extreme_mixed_bathymetry_numerical_stability`); reliability-weighted sample variance is invariant to weight rescaling, survives merges and keeps unit-weight results (`weighted_sample_variance_handles_fractional_weights_and_merges`, `weighted_sample_variance_edge_cases`); SIMD span kernels equal scalar Welford for f32, f64, variable lengths, integer types and large integers (`test_simd_span_*`); NoData/NaN filtering keeps statistics intact (`test_nodata_filtering_preserves_statistics`, `test_nan_filtering_in_streaming`, `test_custom_nodata_override`, `test_is_chunk_all_nodata`, `test_all_nodata_full_stream_scan_and_categorical`). |
| Unit: `aggregator/nodata.rs` (5), `aggregator/simd.rs` (1) | NoData casting rules (rejects fractional, non-finite and out-of-range integer markers) and span accumulation. |

## 6. Categorical aggregation & remapping

| Suite | What it checks |
| :--- | :--- |
| [`core/categorical_long_streaming.rs`](../crates/raster-h3-core/tests/it/categorical_long_streaming.rs) (3) | Long-format output on a 300-class raster: batch sizes 1/7/32/4096 equal rows built from the streamer, batches are full except the last, cells split across batches stay contiguous and sorted, limits stop mid-cell, and projecting without entropy keeps values. |
| [`core/categorical_remap.rs`](../crates/raster-h3-core/tests/it/categorical_remap.rs) (9) | Exact, range, list and wildcard remap syntax; sparse and negative categories; invalid syntax; malformed specs (empty tokens, multibyte first characters) error instead of panicking; 10 000-deep nested brackets; nested-bracket and hyphen specs stream like their plain equivalents; remapping inside single- and multi-resolution categorical streamers. |
| [`core/accumulators.rs`](../crates/raster-h3-core/tests/it/accumulators.rs) | Accumulator operations and merges, tied majorities, homogeneous rasters, RLE with interspersed NoData, high-cardinality Shannon entropy, the 16-slot inline array spilling to a heap map, SIMD uniformity across types (`test_categorical_*`). |
| Unit: `aggregator/categorical.rs` (5), `aggregator/remap.rs` (4) | Histogram and remapper internals; NaN, infinite and non-positive weights are ignored in inline and heap storage and across merges. |

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
| [`core/multiband_and_query_pushdown.rs`](../crates/raster-h3-core/tests/it/multiband_and_query_pushdown.rs) (14), [`duckdb/cell_bbox_pushdown.rs`](../crates/raster-h3-duckdb/tests/it/cell_bbox_pushdown.rs) (2) | Band selection and samples-per-pixel; NDVI, NDWI, NBR and EVI on the fly; zero-denominator and NoData resilience; out-of-range bands (and spectral formula bands) refused when the stream opens; borrowed interleaved windows match across job sizes; `min_count`/`min_mean`/`min_majority_fraction` pushdown; `h3_cell`/`h3_hex` bbox pushdown at high latitude and across the antimeridian without dropping raster samples. A band or spectral formula band the raster does not have is refused when the stream opens; multi-band planar TIFFs are rejected. |
| [`core/band_calibration.rs`](../crates/raster-h3-core/tests/it/band_calibration.rs) (12) | GDAL `SCALE`/`OFFSET` calibrate continuous statistics (a negative scale swaps min and max) and spectral indices per band; categorical classes are not scaled; mosaic tiles use their own calibration; malformed calibration metadata is rejected in every mode; overflow drops only non-finite physical values, on interleaved and spectral paths too. Zero-alpha pixels contribute no observation, unspecified `ExtraSamples` are data, and unknown compression is never read as raw samples. |
| Unit: `multi_horizon/spectral.rs` (5) | Formula evaluation and singularity guards. |

## 9. Threading & concurrency

Results must not depend on thread count, scheduling or pool nesting.

| Suite | What it checks |
| :--- | :--- |
| [`core/threading.rs`](../crates/raster-h3-core/tests/it/threading.rs) (10) | Identical output across Rayon thread counts; pool scaling; a high-contention stress run; multithreaded prefetched chunk reading; aggregation workers run concurrently inside `ThreadPool::install`; a dedicated pool behind a shared mutex does not deadlock. A dedicated pool under a shared streamer mutex does not deadlock; workers precompute every eviction key the merge needs (no fallbacks) with output identical to one worker; batch cells do not grow with the worker count; mixed-mosaic batches follow their own chunk sizes. |
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
| Unit: `pmtiles/external_export.rs` (6) | The external-sort PMTiles exporter: with a 4 KiB budget (several spilled runs) and eviction every 8 features, the archive is byte-identical to an unspilled export and has the same tile count as single-pass accumulation; run records round-trip every `MvtValue` kind; an empty export is a valid archive; the spill directory is created only when a run is written. |
| [`memory`](../crates/raster-h3-core/tests/memory/scenarios.rs) (7 scenarios, 38 checks) | Measured, not estimated, memory. A counting allocator records peak live heap and allocations; each scenario variant runs in its own process. `spill_sorted_run`: the sorted vector built for a spill run is the only new heap while the map's shards drain (accumulators move, nothing is cloned), it fits within the map's accounted bytes, and the map is freed afterwards (plain, quantile and categorical maps). `spill_stream_quantiles`, `categorical_high_cardinality`: p50/p90 sketches and ~46-class heap histograms forced to spill reproduce the in-memory output bit for bit per cell, stay within the budget and lower peak heap. `slow_http_consumer`: with a slow consumer, held response bytes stay within `max_buffered_bytes`, peak heap within twice it and under half of the chunk-count bound alone, with one request per coalesced range. `slow_parquet_writer`: a writer that sleeps per row group adds at most three row-group buffers. `many_tiny_chunks`: 512 one-row strips cost at most 16 allocations each and coalesce into one HTTP range. `concurrent_streams`: four streams use at most 5× the heap of one. |

See [Engineering Architecture](engineering.md) for what the budgets cover.

### Memory benchmarks

`cargo test --release -p raster-h3-core --test memory` runs every scenario (a name
filter after `--` selects some). The binary re-runs itself once per scenario variant, so
allocator peaks and RSS high-water marks never mix, and builds fixtures before measuring.
Checks compare allocator bytes (requested sizes, the same on every platform) with
configured budgets or with another variant, plus HTTP request counts, spill runs and
exact output digests, so they hold on any machine. Streams pin two decode and two
aggregation workers so the numbers do not scale with core count. RSS (high-water mark
reset after fixtures on Linux, whole-process maximum on macOS, absent on Windows) and wall
time are reported only; compare them on one machine with
`-- --repeat 5 --json mem.json` (medians of five processes per variant). In CI the
tables go to the job summary and one `::notice` annotation lists each scenario's checks
and peaks.

**Prefetch byte budget (#60), before and after.** The same program streamed a 64 MiB
remote GeoTIFF (4096×4096 f32) through `RemoteChunkPrefetchQueue` with the default
configuration (8 workers, 2 MiB ranges, 32 MiB budget, 2 ms server latency), built
against the commit before #60 and against #60, five processes each on a 4-core Linux VM
(medians; requests were identical, 32):

| Strips, consumer hold per chunk | Peak heap MiB | RSS growth MiB | Allocated MiB | Wall ms |
| :--- | ---: | ---: | ---: | ---: |
| 512 KiB, 5 ms | 49.0 → 32.0 | 65.8 → 32.3 | 128.6 → 64.6 | 686 → 675 |
| 128 KiB, 1 ms | 25.5 → 24.1 | 39.7 → 26.4 | 128.7 → 64.8 | 599 → 588 |
| 32 KiB, none | 18.2 → 16.3 | 32.2 → 16.7 | 129.0 → 65.3 | 74 → 56 |

Before #60 the queue counted ready chunks (64) and copied each out of its response, so a
slow consumer of large chunks held 64 chunks plus eight ranges in flight; with the budget
the peak is the 32 MiB budget, and shared response buffers halve the bytes allocated.
`slow_http_consumer` keeps checking the same effect at a small scale.

## 11. Failure propagation & resilience

A failure after a valid header must reach the consumer as an error, never as a short but
"successful" result.

| Suite | What it checks |
| :--- | :--- |
| [`core/streaming_failures.rs`](../crates/raster-h3-core/tests/it/streaming_failures.rs) (6), [`duckdb/record_queue.rs`](../crates/raster-h3-duckdb/tests/it/record_queue.rs) (1) | Corrupt-chunk errors latch in both streamers; a missing or non-directory spill directory fails both streamers with a latched error and no records after it, and spill runs leave no files in a usable one (on Unix, not even mid-stream); the DuckDB record queue propagates failure instead of EOF; failed exports leave existing outputs untouched; a Parquet file is not published after a late failure; premature prefetch termination is an error. |
| [`core/transform_failures.rs`](../crates/raster-h3-core/tests/it/transform_failures.rs) (7) | Samples beyond the pole and failed inverse projections are counted; valid rasters and bbox exclusions report none; batch readers and exporters report through the shared counter; `strict_transforms` fails the stream instead of dropping samples and accepts rasters without failures. |
| [`datafusion/transform_failures.rs`](../crates/raster-h3-datafusion/tests/it/transform_failures.rs) (1), [`python/transform_failures.rs`](../crates/raster-h3-python/tests/it/transform_failures.rs) (3), [`pytest/test_transform_failures.py`](../crates/raster-h3-python/tests/pytest/test_transform_failures.py) (5) | DataFusion providers, the Python batch reader and `raster_h3_py_scan_json` count failures in lenient mode and fail in strict mode; a strict Arrow stream fails while reading; lenient Python scans warn once with the count (and free the result even when warnings are errors), and valid rasters do not warn. A strict scan's error also reaches the caller when PyArrow (`pa.table`, `RecordBatchReader.from_stream`) drops the stream capsule while raising it. |
| [`duckdb/prefetch_resilience.rs`](../crates/raster-h3-duckdb/tests/it/prefetch_resilience.rs) (3) | Mixed local/remote mosaics finish without deadlock; a decode-worker panic latches into the queue; a corrupt chunk unblocks readers instead of hanging. |
| [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs) | `test_nonexistent_file_path_error`, `test_corrupted_and_empty_tiff_error_handling`. |
| [`cli/binary.rs`](../crates/raster-h3-cli/tests/it/binary.rs), [`python/batch_reader.rs`](../crates/raster-h3-python/tests/it/batch_reader.rs), [`python/exports.rs`](../crates/raster-h3-python/tests/it/exports.rs), [`pg/srf.rs`](../crates/raster-h3-pg/tests/it/srf.rs) | A strip that fails to decode part-way is a non-zero CLI exit, an `out_err` from `raster_h3_py_batch_reader_next` / `scan_json` / the exporters, and `-1` from `pg_srf_continuous_next`, never a short successful result; failed Python and CLI Parquet exports leave no file. |

## 12. GeoTIFF decoding & codecs

| Suite | What it checks |
| :--- | :--- |
| [`core/codecs.rs`](../crates/raster-h3-core/tests/it/codecs.rs) (15) | `libdeflater` output is bit-identical to the `tiff` decoder for striped u8/f32, tiled f32 with padding, floating-point and horizontal predictors; buffer recycling and auto-resizing; corrupted streams; a short-filling deflate tile is rejected and falls back to the file instead of keeping stale samples; cropped edge tiles in both streaming paths; whole, right-clipped, bottom-clipped and corner tiles all decode on the accelerated path; a corrupt or unreadable stored tile fails with the accelerated decoder's or the read's own error; a tile that inflates past its declared layout is still read by the generic decoder. |
| [`core/codecs.rs`](../crates/raster-h3-core/tests/it/codecs.rs) (9) | Fast LZW parity with the `tiff` decoder on synthetic tiled data with predictors and edge padding; corrupted and truncated streams; buffer resizing. Four parity/recycling tests use the Hawaii datasets and are ignored by default. |
| [`core/conus_pipeline.rs`](../crates/raster-h3-core/tests/it/conus_pipeline.rs) | `test_tiff_magic_validation_edge_cases`. |
| [`core/codecs.rs`](../crates/raster-h3-core/tests/it/codecs.rs) (3) | Opening rejects unknown SampleFormat, Predictor and PhotometricInterpretation tags and offset/byte-count arrays shorter than the tile grid. |
| [`core/chunks_and_prefetch.rs`](../crates/raster-h3-core/tests/it/chunks_and_prefetch.rs) | Grid partitioning covers every pixel exactly once, rejects a zero chunk size, and layout constructors reject zero dimensions and overflowing tile counts. |
| [`core/horizon_streaming.rs`](../crates/raster-h3-core/tests/it/horizon_streaming.rs), [`core/chunks_and_prefetch.rs`](../crates/raster-h3-core/tests/it/chunks_and_prefetch.rs) | `test_u16_raster_streaming`, `test_f64_raster_streaming`, `test_tiled_chunk_layout_calculation`, `test_tiled_uneven_chunk_layout_and_bounds`. |
| Unit: `raster/predictor.rs` (9) | Predictor 2 (including NEON), predictor 3 unpacking, and rejection of truncated buffers and invalid/overflowing layouts. |

## 13. Remote COG & cloud streaming

| Suite | What it checks |
| :--- | :--- |
| [`core/remote_cog.rs`](../crates/raster-h3-core/tests/it/remote_cog.rs) (50) | Against a local mock HTTP server: URL normalisation and S3 regional/custom endpoints; remote-path detection (cloud schemes, case, `file://`, Windows drive letters, malformed schemes); header read budget; numerical equality with local reads; ROI-selective streaming; 404 handling; transient retry and recovery; header injection; the bearer token sent only to hosts listed in `RASTER_H3_AUTH_HOSTS`, not following a retargeted transport, and named in the error of a refused request it was withheld from; strong ETag pinning across cached/direct/concurrent reads and transport clones; URL-scoped validators for retargeted transports, divergent concurrent clones, and unversioned sources; same-size replacement and HTTP 412 without retry; rejection of changed/missing/weak response validators despite ignored preconditions; compatibility without strong initial tags; conditional transient retries; range coalescing; rejection of truncated, ignored or wrong ranges; retry of truncated range, probe and full-body responses; full-body responses served from cache and capped by `RASTER_H3_MAX_FULL_BODY_BYTES`; reads across cache blocks; in-flight prefetch completion, final fetch errors and cancellation waking all consumers; the prefetch byte budget bounding held bytes under a slow consumer with no duplicate requests, coalesced responses shared by their chunks and charged until the last slice drops, oversized ranges fetched one at a time, and cancellation waking workers blocked on the budget; out-of-order and concurrent demand, coalesced sibling refetch, full count capacity, caller-clone retention and payload validity after queue drop; remote mosaics and the multi-resolution streamer; remote COG → Parquet. |
| Unit: `raster/http_range.rs` (13), `raster/remote_prefetch.rs` (7) | Range validation, caching, coalescing, backoff, which URLs receive the bearer and session tokens (listed hosts, S3 endpoints, https or loopback only, look-alike hosts rejected) and the private terminal-update locking invariant. |

## 14. Mosaics & overlap rules

| Suite | What it checks |
| :--- | :--- |
| [`core/mosaic.rs`](../crates/raster-h3-core/tests/it/mosaic.rs) (11) | Glob matching, comma lists, VRT XML resolution; adjacent-tile conservation; `cutline` vs `first` vs `average`; categorical overlaps; mixed VRT paths and export pipelines. |
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
| Unit: `encoding/wkb.rs` (9), `encoding/fast_hex.rs` (2) | Ring construction, antimeridian splitting and polar caps (every cell at resolutions 0–2 checked for range, closure, orientation, simplicity and area), hex formatting/parsing. |

## 16. GeoParquet export

| Suite | What it checks |
| :--- | :--- |
| [`core/geoparquet.rs`](../crates/raster-h3-core/tests/it/geoparquet.rs) (11) | Continuous and categorical metadata and WKB; compact and full schemas; GeoParquet off by default; raster → GeoParquet end to end; deep validation against OGC GeoParquet 1.1 (PROJJSON, edges, bbox); column projection. The `geo` bbox covers every cell geometry, including split antimeridian cells; every leaf carries the type-defined column order, so Polars' native reader opens the file. |
| [`core/multi_resolution.rs`](../crates/raster-h3-core/tests/it/multi_resolution.rs) | Sorted streaming Parquet exports. |
| Unit: `parquet/geoparquet_metadata.rs` (1) | Metadata JSON. |

## 17. PMTiles & MVT export

| Suite | What it checks |
| :--- | :--- |
| [`core/pmtiles.rs`](../crates/raster-h3-core/tests/it/pmtiles.rs) (26) | MVT protobuf encoding; PMTiles v3 header and archive validation; Hilbert tile IDs (ordering and bijective round trip); resolution ↔ zoom monotonicity; cell → tile mapping; coarse-zoom parent aggregation over all children; property filtering; GeoTIFF and Parquet (custom column, hex strings, multi row-group eviction) to PMTiles; all-NoData rasters; Class III 10-vertex pentagons; antimeridian-straddling cells and world tiles; the exporter's `h3_hex` property holds the full hex index; `parquet_to_pmtiles` reads nullable H3 columns. |
| [`core/mvt_geometry.rs`](../crates/raster-h3-core/tests/it/mvt_geometry.rs) (3) | Every feature ring has positive area; cells at the poles become caps that reach the map edge, and `add_hexagon` builds the same cap. |
| [`sedona/pmtiles.rs`](../crates/raster-h3-sedona/tests/pmtiles.rs) (6) | `COPY (query) TO '*.pmtiles'` in a plain DataFusion `SessionContext`: every cell is written with its columns as properties; `STORED AS PMTILES` options and zoom range; NULL cells skipped; signed and hex H3 columns; the `write_pmtiles` DataFrame API; an empty query writes a valid archive; bad copies fail with a message. |
| Unit: `pmtiles/pyramid.rs` (3), `pmtiles/overview.rs` (2) | Tile math and overview levels. |

See [PMTiles & Multi-Resolution](pmtiles.md).

## 18. DuckDB extension

| Suite | What it checks |
| :--- | :--- |
| [`duckdb/ffi_sql.rs`](../crates/raster-h3-duckdb/tests/it/ffi_sql.rs) (9) | Default (v1.2.0) build only; the harness emulates the 357-entry v1.2.0 function table, so the DuckDB 2.0 build relies on `real_sql_v156_features` below. Mock C-API callbacks: init and registration completeness, scalar NULL propagation, `h3_polygon_to_cells` BLOB overload registration, both `h3_cell_to_geometry` overloads and output parity, continuous scans with projection pushdown, quantile sketches built only when a quantile column is projected (`geom` and `wkb` excluded) with reordered/repeated quantile columns matching a full scan, categorical wide and long, and export table functions with CRS overrides. No SQL is executed. Transform-failure counts in export summaries; `strict_transforms` fails row scans, and fails exports with the count. |
| [`root/duckdb_sql_smoke.rs`](../tests/it/duckdb_sql_smoke.rs) (6, ignored) | Real SQL through DuckDB v1.2.0, v1.5.6, and v2.0.0: continuous/categorical scan names and aliases, pixel totals and constant means, projected CRS locations, scan/Parquet bidirectional `EXCEPT` parity, Parquet-to-PMTiles positional/named arguments, status/count/magic checks, `h3_cell_to_geometry` overloads and WKB bytes, and polygon modes/aliases/invalid-WKB NULL behavior. DuckDB Spatial is installed for v1.5.6 and v2.0.0 to serialize native geometry as WKB. `real_sql_v156_features` runs only against the DuckDB 2.0 build (`DUCKDB_VERSION=v1.5.6`): `SET` options round-trip through `current_setting`; a raster export reads through DuckDB's file system; `COPY ... (FORMAT pmtiles)` overwrites a larger existing file (archive length matches its header) and writes tiles for valid cells; non-H3 values, unknown options and `memory_budget_mb 0` are rejected; a 1 MiB-budget COPY of ~23k cells is byte-identical to an unspilled one and creates `temp_directory` only when spilling; invalid constant `h3_polygon_to_cells` arguments fail at plan time; `typeof(h3_cell_to_geometry(...))` is `GEOMETRY`. Partitioned scans merged with `h3_merge_partials` equal the single scan. |
| [`root/duckdb_trusted_repository.rs`](../tests/it/duckdb_trusted_repository.rs) (1, ignored) | DuckDB 2.0+ trusted repository end to end, with signature checking on (no `-unsigned`) and an isolated `HOME`: a repository with a signed extension and `.well-known/duckdb-extension-repo.json` is created from a local prefix, the pinned fingerprint matches the key, `INSTALL`/`LOAD ... FROM` run the extension, a later session reloads it without opting in, and extensions that are unsigned or signed with an unpinned key are rejected. No-op against DuckDB < 2.0. |
| [`test/sql/raster_h3.test`](../test/sql/raster_h3.test) (sqllogictest) | Packaged `build/release/raster_h3.duckdb_extension` from `make release` loads through DuckDB's Python sqllogictest runner (the community-extensions test path); `h3_to_string` is absent before `require raster_h3` and returns the expected cell after. |
| [`root/package_extension.rs`](../tests/it/package_extension.rs) (6) | Extension footer layout: custom section headers, ABI fields, version and platform strings (including `v2.0.0`) and `--print-platform` parity, zeroed signature, gzip output, 1 MiB chunked SHA-256 RSA-2048 signing and verification, tamper and mismatched-key rejection, inline PEM env-var signing, and DuckDB 2.0 `.well-known/duckdb-extension-repo.json` manifest generation. |
| Unit: `raster-h3-duckdb` (28) | `ffi/duckdb_c.rs` (9: `duckdb_string_t` layout), `ffi/mod.rs` (3), `bind_utils/parsing.rs` (6), `bind_helper.rs` (2), `chunk_writer.rs` (3), `record_queue.rs` (1), `scalar.rs` (4). |

Also covered elsewhere: bbox pushdown in [§8](#8-bands-spectral-indices--pushdown); record queue and prefetch resilience in [§11](#11-failure-propagation--resilience); WKB and `GEOMETRY` output in [§15](#15-geometry-encoding-wkb); partitioned scans in [§26](#26-partitioned-scans); cross-engine parity in [§24](#24-cross-engine-parity).

See [DuckDB](engines/duckdb.md).

## 19. DataFusion

The `raster-h3-datafusion` table providers must return what a core batch reader returns for the same configuration, with every pushed filter either exact or left to DataFusion.

| Suite | What it checks |
| :--- | :--- |
| [`datafusion/continuous_options.rs`](../crates/raster-h3-datafusion/tests/it/continuous_options.rs) (8) | Each `ContinuousRasterTableProvider` builder option equals a core `ContinuousBatchReader` run with the same config and changes the output: `with_nodata` (drops exactly the matching pixels), `with_sampling`, `with_band` (0 clamps to 1), `with_spectral_formula`, `with_quantiles` (extra columns, projectable), `with_min_count`/`with_min_mean`/`with_max_mean` intersected with pushed `MinCount`/`MinMean`/`MaxMean` (the stricter bound wins), `with_compact` (mass conserved; adjacent resolutions rejected), `with_overlap_rule` on a two-file source. |
| [`datafusion/categorical_options.rs`](../crates/raster-h3-datafusion/tests/it/categorical_options.rs) (3) | `CategoricalRasterTableProvider` `with_nodata`, `with_remapper` (wide and long) and `with_min_majority_fraction` / `with_min_count` intersected with pushed filters, against `CategoricalBatchReader`. |
| [`datafusion/statistics.rs`](../crates/raster-h3-datafusion/tests/it/statistics.rs) (3) | `statistics()` returns the inexact core cardinality estimate for both providers, grows with resolution, resolution count and mosaic files, stays within 4× of the real row count, and falls back to 10 000 per resolution for unreadable rasters. |
| [`datafusion/scalar_udfs.rs`](../crates/raster-h3-datafusion/tests/it/scalar_udfs.rs) (3) | `string_to_h3`, `h3_to_lat`, `h3_to_lng` against `h3o` and the core scalars on cells near the poles, the antimeridian and a pentagon: validity pass-through, NaN for invalid cells, type errors, round trip back to the same cell. |
| [`datafusion/compaction_pushdown.rs`](../crates/raster-h3-datafusion/tests/it/compaction_pushdown.rs) (2) | With `compact` or `compact_h3_children` set, every predicate reports `Unsupported` and `scan` emits the unfiltered compacted table: residual resolution and aggregate filters see completed parent rows, and `limit` is spent only after them — for both providers (categorical in wide and long formats). |
| [`datafusion/bbox_pushdown.rs`](../crates/raster-h3-datafusion/tests/it/bbox_pushdown.rs) (5) | A pushed `Bbox` keeps edge cells whole (continuous and categorical statistics equal an unpruned scan); compacted scans ignore it; `LIMIT` is not spent on cells outside the box; a box off the raster scans empty. |
| Unit: `raster-h3-datafusion` (6) | Projection, limit, filter narrowing, Arrow stream, UDFs. |

Also covered elsewhere: transform failures in [§11](#11-failure-propagation--resilience); overlap rules in [§14](#14-mosaics--overlap-rules); partitioned scans in [§26](#26-partitioned-scans); cross-engine parity in [§24](#24-cross-engine-parity).

See [DataFusion](engines/datafusion.md).

## 20. SedonaDB

`raster-h3-sedona` is a standalone workspace pinned to SedonaDB's DataFusion; its suites run in the `sedona-adapter` CI job. Apache Sedona on Spark does not use this adapter: it reads cells from the Python package, and its tests, which run in the `sedona-spark` CI job, are listed under [Partitioned scans](#26-partitioned-scans).

| Suite | What it checks |
| :--- | :--- |
| [`sedona/sql.rs`](../crates/raster-h3-sedona/tests/sql.rs) (12) | Real DataFusion SQL over continuous/categorical tables, including compacted parent resolution/count filters with `LIMIT`. Direct scans with small batches cover both compaction flags, strict comparisons, partially translated `BETWEEN`, unsupported and mixed predicates: reader and execution-plan limits are deferred for residual filters, while unfiltered/exact scans retain limit pushdown. |
| [`sedona/pushdown_correctness.rs`](../crates/raster-h3-sedona/tests/pushdown_correctness.rs) (6) | Many random query geometries through envelope-only `st_intersects` / `st_dwithin` stand-ins: every column of every matching row equals the same filter over a materialised table, on lon/lat, UTM, Web Mercator and high-latitude rasters, multiple resolutions and categorical data. |
| [`sedona/extension_abi.rs`](../crates/raster-h3-sedona/tests/extension_abi.rs) (2) | `RasterH3Table` exported through SedonaDB's C extension ABI and imported back scans like the original, and spatial pushdown survives the round trip. |
| [`sedona/sedona.rs`](../crates/raster-h3-sedona/tests/sedona.rs) (5, `sedona-e2e` only) | Inside a real `SedonaContext`: cells read as lon/lat geometry, Sedona's `ST_*` predicates on raster cells, bbox pushdown keeps edge cells whole, `ST_DWithin` and `h3_to_wkb` predicates match an unpruned scan, and a spatial query copies to PMTiles. |
| Unit: `raster-h3-sedona` (12) | `spatial.rs` (5): WKB envelopes of points, lines and collections, malformed and empty WKB, clamping a grown envelope to the lon/lat range, predicate-to-envelope translation and CRS metadata; `filters.rs` (2): continuous and categorical comparison pushdown; `pmtiles.rs` (5): property names and native types, H3 column types and selection, zoom filtering, invalid cells and schemas, `COPY` options. |
| [`sedona-pytest/test_sedonadb.py`](../crates/raster-h3-sedona/python/tests/test_sedonadb.py) (14) | `raster_h3_sedona.RasterH3Table` scanned by the `sedonadb` wheel through its table-provider capsule: projection, limit and count; cells as Sedona geometry; spatial filters equal an unpruned scan, prune it across the extension ABI, and return nothing off the raster; scalar UDF registration; `h3_polygon_to_cells` joins and rejects projected geometry; categorical tables; invalid arguments; `write_pmtiles` from SedonaDB queries and Arrow sources, and its errors. |

Also covered elsewhere: PMTiles `COPY` in [§17](#17-pmtiles--mvt-export); partitioned scans in [§26](#26-partitioned-scans).

See [DataFusion](engines/datafusion.md) for the SedonaDB adapter.

## 21. Python & Polars

The `raster-h3-python` C ABI is tested from Rust; the `raster_h3` package is tested with pytest against the locally built library.

| Suite | What it checks |
| :--- | :--- |
| [`python/batch_reader.rs`](../crates/raster-h3-python/tests/it/batch_reader.rs) (3) | `raster_h3_py_open_batch_reader` / `_batch_reader_next` / `_free_batch_reader`: one bounded batch per stream, rows equal to core, projection, limit, categorical long, NULL-reader and open errors, early free. |
| [`python/c_abi.rs`](../crates/raster-h3-python/tests/it/c_abi.rs) (5) | `raster_h3_py_scan_json` columns equal core output with NoData, bbox, CRS, projection, limit, categorical wide/long; errors set `out_err`, including malformed bbox, resolutions and format, which are rejected rather than ignored or defaulted. |
| [`python/exports.rs`](../crates/raster-h3-python/tests/it/exports.rs) (3) | `raster_h3_py_export_parquet` / `_pmtiles` error paths return `-1` with a message and no output; successful exports return core row counts and PMTiles bytes identical to the core tiler. Exports report transform failures. |
| [`python/capsules.rs`](../crates/raster-h3-python/tests/it/capsules.rs) (3) | The native `PyCapsule` destructors, driven through stand-ins for `PyCapsule_GetName` / `PyCapsule_GetPointer`: a stream capsule releases its stream under its original or a consumer's new name, a schema capsule frees its schema, and both are inert for a null capsule or before the capsule API is registered. |
| [`python/scalars.rs`](../crates/raster-h3-python/tests/it/scalars.rs) (2) | `raster_h3_py_h3_is_valid_str` and `raster_h3_py_h3_cell_to_parent_str` against `h3o` and their `u64` variants. |
| Unit: `raster-h3-python` (4) | C-ABI helpers. |
| [`pytest/test_arrow_stream.py`](../crates/raster-h3-python/tests/pytest/test_arrow_stream.py) (12) | The in-tree `raster_h3` package with the locally built cdylib, read by real PyArrow and Polars: continuous `h3_index`/`count`/`sum`/`min`/`max`/`mean` equal an independent reference that indexes every pixel centre with the `h3` package; `pl.from_arrow`, `to_polars()`, `read_raster_h3` and `iter_batches` equal `pyarrow.table` on `RasterH3ArrowStream` (`__arrow_c_stream__`), and `pyarrow.schema` (`__arrow_c_schema__`) equals the stream schema; `columns` and `limit`; categorical long counts/fractions and wide majority class/count/fraction against the `h3` reference; missing files, non-TIFF input, unknown columns and a non-intersecting bbox raise `RuntimeError`. |
| [`pytest/test_polars_scan.py`](../crates/raster-h3-python/tests/pytest/test_polars_scan.py) (12) | `scan_raster_h3` as a Polars IO source (`register_io_source`, Polars ≥ 1.4): projection pushdown opens the native scan with only the selected columns; predicate pushdown (numeric, combined, string, `is_in` on `h3_index`) and `head(n)` (pushed as the scan limit), `head` after `filter`, and the `limit`/`columns` arguments give the same rows as the eager full scan filtered in Polars; empty results keep the schema; categorical wide/long pushdown; scan errors and unknown columns raise. |
| [`pytest/test_scan_options.py`](../crates/raster-h3-python/tests/pytest/test_scan_options.py) (23) | The `_v2` scan options against values computed independently from pixels and the `h3` package: `band` (default, beyond the raster, below one, non-int), spectral formulas and their bands, `sampling`, `overlap_rule` (including in `plan_partitions`), `min_majority_fraction`; every option reaches every scan path; `parquet_to_pmtiles` with default, named and missing hex columns. |
| [`pytest/test_scan_filters.py`](../crates/raster-h3-python/tests/pytest/test_scan_filters.py) (21) | Quantile columns (specs, projection, exact single-pixel quantiles, Arrow schema, errors), `remap` (merging, dropping, pass-through, errors), the `min_count` / `min_mean` / `max_mean` filters, and Polars predicate pushdown into them: rows equal a full scan filtered in Polars, and the opened scan carries the pushed options. Resolution predicates narrow the scan but keep invalid resolutions an error; older native libraries fall back to Polars-only filtering. |
| [`pytest/test_parquet_export.py`](../crates/raster-h3-python/tests/pytest/test_parquet_export.py) (1) | `raster_to_parquet` output opens with Polars' native Parquet reader. |
| [`wheel_smoke.py`](../crates/raster-h3-python/tests/wheel_smoke.py) | After `cibuildwheel` installs a wheel: the bundled library loads, Arrow/Polars paths work, `raster-h3-cli` is on `PATH`, and `h3_polygon_to_cells` handles a cell boundary and invalid resolution. |

Also covered elsewhere: transform failures in [§11](#11-failure-propagation--resilience); partitioned scans, including Apache Sedona (Spark), in [§26](#26-partitioned-scans); cross-engine parity in [§24](#24-cross-engine-parity).

See [Python & Polars](engines/python-polars.md).

## 22. PostgreSQL

| Suite | What it checks |
| :--- | :--- |
| [`pg/srf.rs`](../crates/raster-h3-pg/tests/it/srf.rs) (4) | `PgCategoricalSrf` with resolution, CRS, NoData and bbox options against `CategoricalBatchReader`; the `pg_srf_continuous_*` C-ABI lifecycle with options, WKB (including the full 189-byte boundary of an odd-resolution pentagon) and hex columns, NULL handles and errors. |
| [`pg/fmgr.rs`](../crates/raster-h3-pg/tests/it/fmgr.rs) (8, `pg-extension` only) | The V1 `fmgr` entry points in process, against server stand-ins in [`pg/fmgr/server.rs`](../crates/raster-h3-pg/tests/it/fmgr/server.rs) (`palloc`, `funcapi` multi-call helpers, reset callbacks): `pg_h3_to_string`, `pg_string_to_h3` (1- and 4-byte varlena headers), `pg_h3_is_valid_text`, `pg_h3_to_wkb`, the finfo records and magic block, and the categorical and continuous value-per-call SRFs against core output, including state release on early termination. Error reporting (`ereport` longjmps) is left to `root/pg_extension_e2e.rs`. The SRFs raise a `WARNING` when failed transforms drop samples. |
| Unit: `raster-h3-pg` (4 + 3 in `fmgr.rs`) | SRF iterators. The `fmgr.rs` tests only compile with `--features pg-extension`. |
| [`root/pg_extension_e2e.rs`](../tests/it/pg_extension_e2e.rs) (1, ignored) | Builds the PostgreSQL module in Docker, loads it into `postgres:<major>` for each `RASTER_H3_PG_VERSIONS`, compares `psql` output with in-process SRFs and checks that bad `bbox`/`h3_res` raise SQL errors without breaking the session. |

Also covered elsewhere: cross-engine parity in [§24](#24-cross-engine-parity).

See [PostgreSQL](engines/postgresql.md).

## 23. CLI

| Suite | What it checks |
| :--- | :--- |
| [`cli/binary.rs`](../crates/raster-h3-cli/tests/it/binary.rs) (10) | The built `raster-h3-cli` binary against core readers and exporters: `--bbox`, `--nodata` and `--limit` change the output exactly as the core config does; stdout (no `-o`, `-o -`) equals file output; projected JSONL and categorical long format; categorical Parquet rows and a categorical PMTiles archive byte-identical to the core tiler; unknown subcommands, bad options and missing, non-TIFF or corrupt inputs exit non-zero with a stderr message. `parquet-to-pmtiles` round trips the CLI Parquet export, reads a nullable hex column under a custom name and reports skipped null/invalid rows on stderr, and rejects bad invocations. |
| Unit: `raster-h3-cli` (2) | Argument validation, IPC and JSONL output. |

Also covered elsewhere: partitioned fan-outs in [§26](#26-partitioned-scans); cross-engine parity in [§24](#24-cross-engine-parity).

See [CLI](engines/cli.md).

## 24. Cross-engine parity

One raster, every engine: per-cell results must agree.

| Suite | What it checks |
| :--- | :--- |
| [`root/multi_engine_e2e.rs`](../tests/it/multi_engine_e2e.rs) (4) | Continuous and categorical per-cell parity across core batch readers, the Arrow C stream, the CLI binary (`ipc`, `jsonl`, `parquet`, `pmtiles`), DataFusion (scan, pushdown, UDFs), Python (a `python3` subprocess loading the cdylib), the PostgreSQL SRFs and fmgr scalars in process, and DuckDB when `duckdb` is on `PATH`; continuous parity also covers EPSG:5070 and EPSG:27700 rasters, and polygon results are compared exactly across core, Python and DuckDB. |
| Unit: `raster-h3-core` columnar (6), `scalar_ops.rs` (6) | Arrow C stream round trip, release callback null-safety/idempotence, cross-thread owned export release, `RH3IPC01` round trip, scalar helpers. |

See the [engine deep dives](engines/README.md).

## 25. Performance guards & benchmarks

| Suite | What it checks |
| :--- | :--- |
| [`core/performance.rs`](../crates/raster-h3-core/tests/it/performance.rs) (4) | Continuous, multi-resolution, categorical and supersampled streaming conserve pixel mass, with no wall-clock pass/fail thresholds. |
| [`core/properties.rs`](../crates/raster-h3-core/tests/it/properties.rs) (11 generated tests) | Weighted merge equivalence, accumulator merge associativity/commutativity, sketch merge-vs-single-stream and bounded relative error, `quantiles_batch`/`quantile` agreement, IPC stream round trips, Hilbert tile-id round trips and level bounds, WKB validity/truncation, affine and Mercator round trips, and nodata/supersampled conservation plus cell uniqueness across the antimeridian. |
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
Targets cover the core WKB/EWKB parser, GeoTIFF metadata reader, integer/floating-point
chunk unpacking and predictors, the columnar IPC stream reader, Arrow `ArrowArray` import
with forged variable-length offsets, the categorical remap parser, and `Content-Range`
header parsing. Dimensions and allocations in the predictor harness are bounded;
GeoTIFF metadata fuzzing deliberately does not decode attacker-controlled image dimensions.
These are not exhaustive CRS or database ABI fuzzers.

```bash
cargo install cargo-fuzz --version 0.13.1 --locked
cargo +nightly run --locked --manifest-path fuzz/Cargo.toml --example seed_corpus
cargo +nightly fuzz run wkb -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run geotiff -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run predictor -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run ipc -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run arrow_import -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run remap -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run content_range -- -max_total_time=60 -max_len=65536
rustup component add --toolchain nightly miri rust-src
cargo +nightly miri test --locked -p raster-h3-core --lib -- columnar:: aggregator::categorical:: raster::predictor::
```

Miri checks Arrow schema/array/stream ownership, callbacks, round trips and slicing, the
categorical raw-slice casts, and the predictor byte-cast paths (NEON on aarch64 hosts); it
does not run mmap or the external DuckDB/PostgreSQL/Python hosts. Existing real-host smoke
tests remain necessary. The `tsan` job in the same workflow runs the prefetch,
remote-prefetch and controller unit tests plus the `threading::`, `chunks_and_prefetch::`
and `streaming_failures::` integration suites under ThreadSanitizer
(`RUSTFLAGS=-Zsanitizer=thread cargo +nightly test -Zbuild-std --locked -p raster-h3-core
--test it -- threading:: chunks_and_prefetch:: streaming_failures::`, with `--target` for
cross builds). The [coverage workflow](../.github/workflows/coverage.yml) measures
`--lib --test it` across the core, cli, datafusion, python and pg crates with
`cargo llvm-cov` and uploads `lcov.info` to Codecov (informational statuses); run it
locally as `RASTER_H3_PG_MAJOR=16 cargo llvm-cov --locked --lcov --output-path lcov.info
-p raster-h3-core -p raster-h3-cli -p raster-h3-datafusion -p raster-h3-python
-p raster-h3-pg --lib --test it`.

Fuzz crash artifacts are retained on CI failure. Minimize a reproducer
with `cargo +nightly fuzz tmin <target> <artifact>` and add a deterministic regression test;
report security-sensitive findings privately per [SECURITY.md](../SECURITY.md).

---

## 26. Partitioned scans

A partitioned scan merged from its parts must equal one unpartitioned scan, cell for cell,
in every engine.

| Suite | What it checks |
| :--- | :--- |
| [`core/partitioned_scan.rs`](../crates/raster-h3-core/tests/it/partitioned_scan.rs) (13) | Partitioned scans (tiled, striped, mosaics, categorical, spilling) merged through encoded partial states match a single scan, including `sum_w2` and variance; `fractional_sampling_variance_survives_spills_and_partitions` repeats this with RGSS sampling, a spilling budget and two resolutions; partial states round-trip fractional weights and version 1 states are rejected. |
| [`datafusion/partitions.rs`](../crates/raster-h3-datafusion/tests/it/partitions.rs) (3) | Continuous and categorical `with_partition` providers merge to the unpartitioned provider; pushed filters keep partial rows mergeable. |
| [`sedona/partitions.rs`](../crates/raster-h3-sedona/tests/partitions.rs) (3) | SQL `h3_raster_plan_partitions`, a partition spec argument and the `h3_merge_partials` aggregates equal an unpartitioned query, also with filters and limits. |
| [`cli/partitions.rs`](../crates/raster-h3-cli/tests/it/partitions.rs) (4) | `plan` / `ipc --partition` / `merge` fan-outs equal a single scan for continuous, categorical and projected rasters; a spec on the command line and the plan on stdout; misuse is rejected. |
| [`python/partitions.rs`](../crates/raster-h3-python/tests/it/partitions.rs) (5) | Through the C ABI: continuous and categorical partitions merge to a single scan; unsupported options are rejected; the merge accepts the scan's projection including `partial_state`, and rejects rows without a state. |
| [`pytest/test_partitions.py`](../crates/raster-h3-python/tests/pytest/test_partitions.py) (11) | `plan_partitions`, `RasterH3ArrowStream(partition=...)` and `merge_partials` from Python: continuous and categorical parity, projection through the merge, Spark-style types (signed `h3_index`, `int16` resolution), batch and dict output, a warning for dropped samples, scan options and filters surviving the merge, and misuse errors. |
| [`root/duckdb_sql_smoke.rs`](../tests/it/duckdb_sql_smoke.rs) | `real_sql_partitioned_scans_merge_to_the_single_scan` (ignored): the same through real DuckDB SQL. |
| [`spark/test_datasource.py`](../examples/sedona_spark/tests/test_datasource.py) (11, 16 cases) | The Spark 4 `raster_h3` data source (`raster_h3.spark`) against one unpartitioned scan with the same options: one Spark partition per planned partition, signed types, and `merge_partials` (scanning the raster once) equal to a single scan at two resolutions; the `raster_h3_merge` SQL table function on continuous data, with quantiles, filters and one hash bucket, and on categorical data it recognises, and its errors (a table without `partial_state`, unknown or malformed options); the `read` helper carrying quantiles, a projection and cell filters into the merge; categorical wide data with `min_majority_fraction`; a single partition with no partial rows; `CREATE TEMPORARY VIEW ... USING raster_h3` from SQL with Sedona reading the WKB and agreeing on cell ids; unknown or malformed options and scan errors (an invalid resolution) reach the caller. |
| [`spark41/test_pushdown.py`](../crates/raster-h3-python/tests/spark/test_pushdown.py) (4, 12 cases) | Spark 4.1 filter pushdown: `resolution` comparisons (`=`, `IN`, ranges) and `partial_state IS [NOT] NULL` leave Spark's plan and give the same rows as Spark evaluating them (values within 1e-9 relative: fewer resolutions change summation order); statistic filters and `OR`s stay in the plan; a pushed scan merges like a narrower scan, also through a SQL view; filters that exclude everything scan nothing; `register` follows `spark.sql.python.filterPushdown.enabled`. |
| [`spark/test_spark_partitions.py`](../examples/sedona_spark/tests/test_spark_partitions.py) (6, 7 cases) | On local Spark (`local[2]`) with Apache Sedona, through the [demo](../examples/sedona_spark/raster_h3_spark_demo.py)'s own pipeline: the driver plans partitions, each Spark task scans one through `mapInArrow`, edge cells are shuffled by `h3_index` and merged with `merge_partials`. The demo's `verify` compares the union with one unpartitioned scan of each raster (the same cells, each exactly once, statistics within 1e-9 relative, identical WKB) for one tiled raster in 9 partitions at resolution 9 and at 7–9 (asserting that edge cells were merged), two overlapping rasters that share cell ids (merged separately per file), and a raster in one partition (nothing to merge); `verify` fails when the merged cells are dropped. Sedona reads raster_h3's WKB as valid geometry, and its H3 functions agree on every cell: `ST_H3CellIDs` of each centroid is that cell and `ST_H3ToGeom` centroids differ by under 1e-9°. The command-line demo runs end to end with `--verify` and its Sedona SQL. |

See [Raster H3 Core](engines/core.md) for the protocol and its measured overhead, and
[Spark & Apache Sedona](engines/spark.md) for the Spark pipeline.

## Running the Tests

```bash
cargo test --release --workspace                    # every crate: unit tests + tests/it binaries
cargo test --release -p raster-h3-core              # core only; does not compile other engines
cargo test --release -p raster-h3-core --test it threading::  # one suite (module filter)
cargo test --release -p raster-h3-core prefetch     # tests matching a filter
cargo test --release -p raster-h3-core --test memory -- spill  # memory benchmarks (name filter)
cargo test --release --workspace -- --ignored       # only the gated tests
RASTER_H3_PG_MAJOR=16 cargo test --release -p raster-h3-pg --features pg-extension  # adds the fmgr suites
```

The Python package tests run with pytest against the locally built library:

```bash
cargo build -p raster-h3-python --lib
python -m pip install pytest polars pyarrow h3
python -m pytest crates/raster-h3-python/tests/pytest   # RASTER_H3_PYTHON_LIB picks a specific library
```

The SedonaDB adapter is a standalone workspace, so `--workspace` does not include it:

```bash
cd crates/raster-h3-sedona
cargo test --locked                        # sedona/… suites except sedona.rs
cargo test --locked --features sedona-e2e  # adds sedona.rs; compiles SedonaDB from git
```

Its Python bindings are tested with a built wheel, as CI does:

```bash
cd crates/raster-h3-sedona/python
maturin build --release
python -m pip install target/wheels/*.whl pytest pandas geoarrow-pyarrow "sedonadb==0.5.0"
python -m pytest
```

The Apache Sedona (Spark) tests run inside the `sedona` image, as CI does (about 30 s once
the image is built). To run them on a multi-container standalone cluster instead (driver
and executors in separate containers), see
[Spark & Apache Sedona](engines/spark.md#a-local-multi-node-cluster).

```bash
docker build -f Dockerfile.sedona -t raster_h3-sedona .
docker run --rm -v "$PWD/examples/sedona_spark:/opt/raster_h3/examples" raster_h3-sedona sh -c \
  "pip3 install --break-system-packages pytest && python3 -m pytest -v /opt/raster_h3/examples/tests"
```

Use `--release` for anything that streams real data; debug builds are much slower.

### Gated tests

| Test | Gate | How to run |
| :--- | :--- | :--- |
| `duckdb_sql_smoke` (6 root tests) | `#[ignore]`; needs the DuckDB CLI and a built library; v1.5.6 needs network access to install DuckDB Spatial; `real_sql_v156_features` needs a DuckDB 2.0 CLI, a library built with `--features duckdb-api-v1_5_6` (`RASTER_H3_LIBRARY`) and `DUCKDB_VERSION=v1.5.6`, and is skipped otherwise | `cargo build --release --lib && DUCKDB_BIN=/path/to/duckdb cargo test --release -p raster_h3 --test it duckdb_sql_smoke -- --ignored` (`DUCKDB_VERSION`, `DUCKDB_BIN`, and `RASTER_H3_LIBRARY` select the footer version, CLI, and library) |
| `pg_extension_e2e::pg_extension_in_real_server` (root) | `#[ignore]`; needs Docker | `cargo test -p raster_h3 --test it pg_extension_e2e -- --ignored --nocapture` (`RASTER_H3_PG_VERSIONS=14,16,18`, `RASTER_H3_PG_E2E_KEEP=1`) |
| 4 × `codecs` LZW Hawaii parity, 2 × `horizon_streaming` Hawaii aggregation (core) | `#[ignore]`; need `data/CFL_HI.tif` / `data/LF2024_FBFM40_HI.tif` | Download the datasets into the workspace-root `data/`, then `-- --ignored`. They fail explicitly if the files are missing. |
| `super_sampling::benchmark_supersampling` (core) | `#[ignore]` | `cargo test --release -p raster-h3-core --test it benchmark_supersampling -- --ignored --nocapture` |
| `pg/fmgr.rs` (8) and the `fmgr.rs` unit tests | `--features pg-extension`; needs `RASTER_H3_PG_MAJOR` or `pg_config` on `PATH` | `RASTER_H3_PG_MAJOR=16 cargo test -p raster-h3-pg --features pg-extension` |
| `sedona/sedona.rs` (5) | `--features sedona-e2e`; compiles SedonaDB from git | `cargo test --features sedona-e2e` in `crates/raster-h3-sedona` |
| `examples/sedona_spark/tests` | Skipped unless PySpark, Apache Sedona (Python package and jars), `raster_h3` and NumPy are importable, so they run only in the `sedona` image | See [Running the Tests](#running-the-tests); the demo alone: `docker compose up --build sedona`, then `docker compose exec sedona python3 /opt/raster_h3/examples/raster_h3_spark_demo.py --verify` |
| DuckDB part of `multi_engine_e2e` | Skipped when `duckdb --version` fails | Put the DuckDB CLI on `PATH` |
| `crates/raster-h3-python/tests/pytest` | Each module skips when `polars` or `pyarrow` is missing; `test_polars_scan.py` also skips without `polars.io.plugins` (Polars < 1.4), the `h3` reference checks skip without `h3`, the `pl.from_arrow(stream)` check skips on Polars without PyCapsule support, and the string-predicate case skips on Polars < 1.22 (it drops such predicates on IO sources) | `pip install pytest polars pyarrow h3` and build the cdylib |
| Python part of `multi_engine_e2e` | Runs `/usr/bin/python3`; the test fails if that path is absent | Run on a system with `/usr/bin/python3` |

## What CI Runs

| Workflow | Command | Covers |
| :--- | :--- | :--- |
| [`ci.yml`](../.github/workflows/ci.yml), per platform (Linux, macOS, Windows) | `cargo test --release --no-fail-fast -p raster_h3 -p raster-h3-core -p raster-h3-duckdb` | Unit tests and `tests/it` binaries of the root, core and DuckDB crates (gated ones excluded), and the core `memory` benchmark checks |
| `ci.yml` | `cargo build --release --lib`, plus `cargo build --features duckdb-api-v1_5_6` into `target-v156/` (packaged with `--duckdb-version v1.5.6` under `dist/v2.0.0/`), then ignored `duckdb_sql_smoke` tests against downloaded v1.2.0 and v1.5.6 CLIs with the default build and and `duckdb_trusted_repository` against v2.0.0, both with the v1.5.6-API build | Real-SQL coverage on all three DuckDB versions and both extension builds, packaging with each CLI's `PRAGMA platform` tag; signed install through `CREATE EXTENSION REPOSITORY` |
| `ci.yml` (`community-build`) | `make configure release` via the `extension-ci-tools` submodule, then `test/sql` with `duckdb_sqllogictest` on linux_amd64 | The community-extensions build path (`build: cargo`) keeps working |
| `ci.yml` (`test-crates`, Linux) | `cargo test --locked --release -p raster-h3-cli -p raster-h3-datafusion -p raster-h3-python -p raster-h3-pg`, then PostgreSQL with `--features pg-extension` (core and DuckDB run in the build matrix) | Every adapter's unit/integration suites, including PostgreSQL's fmgr helpers |
| `ci.yml` (`python-pytest`, Linux) | `cargo build --locked --release -p raster-h3-python --lib`, then `pytest crates/raster-h3-python/tests/pytest` with `RASTER_H3_PYTHON_LIB` set to the built library | The in-tree `raster_h3` package against real Polars and PyArrow on every code change, with the same three dependency sets as the manual `python-polars` engine below: declared minimums, first Polars with `register_io_source`, and latest |
| `ci.yml` (`sedona-adapter`, Linux) | `cargo test --locked --no-fail-fast` in `crates/raster-h3-sedona`, Clippy on the Python bindings, then a built wheel tested with `pytest` against `sedonadb==0.5.0` | The standalone SedonaDB adapter workspace (without `sedona-e2e`) and its Python bindings |
| `ci.yml` (`sedona-spark`, Linux) | `docker build -f Dockerfile.sedona`, then `pytest /opt/raster_h3/examples/tests` in the image with `SPARK_MASTER=local[2]`, and `pytest crates/raster-h3-python/tests/spark` with PySpark 4.1 installed beside the image's Spark 4.0 | Partitioned scans on real Spark with Apache Sedona, the Sedona SQL interop and the demo; also that the `sedona` image builds |
| `ci.yml` (`metadata`, `package-check`) | Version-check script and its Python unit tests; `cargo package --locked -p raster-h3-core` | Version/publication policy, license consistency, and a clean-room packaged-core compile |
| [`safety.yml`](../.github/workflows/safety.yml) | Bounded cargo-fuzz targets, scoped Miri tests over the columnar/categorical/predictor unsafe code, and a ThreadSanitizer run over prefetch and aggregation tests | Malformed input, sanitizer smoke checks, FFI ownership and data races; longer fuzz runs weekly |
| [`coverage.yml`](../.github/workflows/coverage.yml) | `cargo llvm-cov --locked --lcov --output-path lcov.info -p raster-h3-core -p raster-h3-cli -p raster-h3-datafusion -p raster-h3-python -p raster-h3-pg --lib --test it`, uploaded to Codecov | Line/region/function coverage on every push to main and PR; statuses informational |
| [`benchmarks.yml`](../.github/workflows/benchmarks.yml) | Criterion streaming benchmarks | Weekly/manual informational timings, not correctness or release gates |
| `ci.yml`, per platform | `cargo test --locked --release -p raster-h3-core --example profile_hotspots` | Hotspot harness ownership-sampling correctness; profiling experiments are excluded |
| `ci.yml`, per platform | `cargo test --locked --release -p raster-h3-core --example worker_scaling` | Worker-scaling benchmark: every workload produces identical output at 1 and 3 aggregation workers; timings are not run |
| [`profile.yml`](../.github/workflows/profile.yml) (optional, manual-only) | `cargo run --release -p raster-h3-core --example worker_scaling -- --json worker-scaling.json` | Informational: wall time, kernel and serial-merge time per workload at 1/2/4/8 aggregation workers; see [Worker Scaling](worker-scaling.md). Hosted runners have few cores, so high worker counts oversubscribe them |
| [`profile.yml`](../.github/workflows/profile.yml) (optional, manual-only) | `cargo run --release -p raster-h3-core --example profile_hotspots -- --json profile.json` | Informational: mosaic planning/ownership scaling with tile count, end-to-end cost per overlap rule, and nested-Rayon wave dispatch (waves, helper-thread hand-offs, allocations). Prefer [local profiling](engines/core.md#profiling-and-wave-dispatch) for comparisons. Optional hosted results appear in the job summary, `::notice` annotations and a JSON artifact |
| `ci.yml` | `duckdb -unsigned -c "LOAD …; SELECT h3_to_string(…)"` | Packaged extension loads |
| [`wheels.yml`](../.github/workflows/wheels.yml) | `cibuildwheel` + `wheel_smoke.py` | Each built wheel installs and works |
| [`manual-tests.yml`](../.github/workflows/manual-tests.yml) (manual) | `cargo test --locked [--release] --no-fail-fast -p <engine>` plus `--features pg-extension` for `pg`/`all` (not on Windows) | Any engine (`all`, `core`, `duckdb`, `datafusion`, `cli`, `python`, `pg`, `root`) on one or all of Linux amd64/arm64, macOS arm64 and Windows, for any branch, tag or SHA, with optional libtest arguments |
| `manual-tests.yml`, engine `python-polars` | `cargo build --locked [--release] -p raster-h3-python --lib` then `pytest crates/raster-h3-python/tests/pytest` | The pytest suite on platforms other than Linux amd64 (which `ci.yml` covers), or for any ref, with three dependency sets: `polars==1.0.*` + `pyarrow==14.*` (Python 3.11, `numpy<2`), `polars==1.4.*` + `pyarrow==14.*` (first Polars with `register_io_source`) and the latest of both (Python 3.13); `test_args` are passed to pytest |

To run it: **Actions → Manual Tests → Run workflow**, or

```bash
gh workflow run manual-tests.yml -f ref=my-branch -f engine=datafusion -f platform=all
```

The workflow file must exist on the default branch for GitHub to list it; `ref` can then
name any branch, even one without the file.

### Gaps

- CLI, DataFusion, Python and PostgreSQL unit/integration suites run on Linux in CI;
  their cross-platform coverage is through the root parity tests or the manual workflow.
- The real PostgreSQL test, the Hawaii-dataset tests and the `sedona-e2e` suite never run in CI.
- CI runs the Spark tests in local mode (`local[2]`). They also pass (23/23) on the local
  multi-container standalone cluster of the `spark-cluster` Compose profile, with executors
  in separate containers, but that is run by hand, and no test runs on separate machines.
- The `fmgr.rs` unit tests and `pg/fmgr.rs` only build with `--features pg-extension`.
- EPSG:6933 and EPSG:7856 have no coverage tests.


## Accuracy and weighting contracts

Projection accuracy and datum accuracy are separate. The 0.01 m golden-point
checks in `geodetic_reference.rs` isolate projection mathematics by using the
source ellipsoid. They do not promise centimetre-level WGS84 positioning.
`datum_shifts.rs` checks the supported Helmert operations against PROJ with
identical parameters, then separately compares with PROJ's preferred operation
using the selected operation's published accuracy. These finite fixtures are
regression checks, not proof of accuracy everywhere in an operation's area of use.

The raster-to-cell datum test places a GeoTIFF pixel center at each stored PROJ
reference coordinate and checks the emitted H3 cell, count and sum at resolutions
5, 9 and 12. Expected geographic coordinates come directly from the PROJ fixture;
they are never generated using `CrsTransformer`. H3 indexing still uses h3o on
both sides, so this tests the raster/CRS integration rather than independently
validating the H3 implementation. See the fixture comments for operation IDs,
reference provenance and accuracy tolerances.

EPSG:4269 (NAD83) is treated as WGS84 without a datum shift. NAD83 Albers fast
paths likewise invert the projection without a grid-based datum correction.
ETRS89/WGS84 equivalence is also approximate and epoch dependent. Metre-scale
position errors can change H3 assignment near a boundary at any resolution;
finer cells increase the practical sensitivity. Unsupported grid shifts are
rejected. Reproject externally using an appropriate operation, grids and epoch
when that accuracy is needed.

Counts and category fractions measure pixel contributions. With built-in
supersampling presets, a valid pixel contributes total weight one before spatial
filtering; sum is the sum of values times their sample weights, and mean divides
that sum by retained weight. These weights are not square metres. Equal angular
pixels have different ground areas at different latitudes, and fixed-point
supersampling is not exact pixel/hexagon intersection. The geographic weighting
regression contrasts pixel means at the equator and 60 degrees north with an
independent spherical rectangle-area calculation. There is no ground-area
weighting mode implied by these statistics. Fractional weights use the
reliability-weighted sample variance `M2 / (W - sum(w_i^2) / W)`; the super-sampling
reference test checks `sum_w2` and the variance it implies.

Calibration is applied after stored-value nodata checks. Non-finite calibrated
samples are omitted from continuous statistics and quantiles. Spectral indices
also omit pixels with non-finite calibrated input bands, non-finite numerator or
denominator, an effectively zero denominator, or a non-finite quotient. This
policy applies to direct pixels, interleaved bands and spans. Finite statistics
can still exceed floating-point range when accumulating extreme finite data;
these tests do not establish arbitrary-precision aggregation.

Run the added regressions with:

```sh
cargo test -p raster-h3-core calibrated_overflow_agrees_between_samples_and_spans
cargo test -p raster-h3-core --test it band_calibration
cargo test -p raster-h3-core --test it datum_shifts
```
