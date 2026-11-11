# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Test infrastructure: four new cargo-fuzz targets (IPC stream, Arrow array import, remap parser, `Content-Range` parsing), a wider Miri scope (columnar, categorical and predictor unsafe code), a ThreadSanitizer job over the prefetch and aggregation suites, a `cargo llvm-cov` coverage workflow uploading to Codecov, and new proptest invariants in `core/properties.rs`.
- `HorizonStreamKernel::worker_bytes_per_sample`: the worst-case worker bytes one sample can add to one resolution's cell map (a new table entry, its owned accumulator state and its eviction key). Kernel worker windows are sized from it, so it must not understate. The default keeps the previous 1 KiB allowance, so custom kernels are unaffected. `inline_worker_bytes_per_sample::<A>()` gives the bound for accumulators that own no heap state.

### Changed

- Continuous streams without quantiles size kernel worker windows by `inline_worker_bytes_per_sample::<H3Accumulator>()` (260 B per sample and resolution) instead of 1 KiB, so windows are larger under the same `aggregation_budget_bytes` and worker maps still fit their allowance when every sample is a new cell. With 5-point sampling at r9 on `BP_CONUS.tif`, system CPU (Rayon workers sleeping and waking between tiny waves) drops by about 62% and wall time from 39.0 to 36.0 min. Per-cell `count` and `sum` can differ from earlier releases in the last bits, because windows group samples differently; cells, `min`, `max` and totals are unchanged. Categorical and quantile streams keep the 1 KiB allowance.
- Kernel windows run as a pipeline instead of waves. Each batch's windows go to long-lived workers that take a job as soon as they are free, and the calling thread merges finished map sets strictly in window order while later windows still run, so output is unchanged by timing (a run matches the serial run of the same windows bit for bit). Previously every wave of up to `worker_tasks` windows waited for its slowest window, and workers idled during the serial merge. Two map sets per worker share the worker allowance, so windows are half as large. New cells' eviction keys are computed by the workers as extra jobs instead of being filtered against the horizon maps, which the merging thread now writes concurrently; `eviction_key_fallbacks` stays 0. `waves`, `bridged_waves` and `serial_waves` now count batches. Windows are cut lazily as they are dispatched, and sets finished out of order wait in a ring of one slot per map set, so scheduling memory is bounded by the map-set count, not the number of windows in a tile.
- Rasters (and mosaics) whose tiles all use the same Albers projection evict horizon-map cells by raster row instead of latitude. Albers rows curve across parallels, so under the latitude rule cells near the east and west edges of a CONUS raster stayed in memory for thousands of rows, overflowed the map budget and spilled, which also stopped eviction for the rest of the scan. A cell is now evicted once the scan passes a certified upper bound of the rows its points project to (`multi_horizon::eviction_space`). Grids reaching past the projection's longitude cut keep latitude keys. On `BP_CONUS.tif` (5-point, r9, default budget) nothing spills any more: wall time drops from 39.0 to 36.6 min, and peak horizon-map memory halves on a 10,000-row crop. The latitude horizon published to tilers and used by compaction still covers every cell not yet emitted. Per-cell `count` and `sum` can differ from earlier releases in the last bits, because cells no longer pass through spill runs; cells, `min`, `max` and totals are unchanged.
- `ShardedResolutionMap::merge_owned_with` takes a closure returning `eviction_space::CellKeys` (eviction key and southern-latitude bound) instead of a latitude. `ShardedResolutionMap::set_lat_tracking` and `held_south_lat_bound` are new.
- Remote files served by HTTP servers without `Range` support are now capped at 1 GiB (`DEFAULT_MAX_FULL_BODY_BYTES`), because a full-body probe response is held in memory. Larger files now fail at open unless the limit is raised through the `RASTER_H3_MAX_FULL_BODY_BYTES` environment variable or `HttpTransport::with_max_full_body_bytes`.
- **Breaking:** the built-in HTTP client no longer sends credentials from the environment to every URL it is given. `RASTER_H3_AUTH_TOKEN` is sent only to hosts listed in the new `RASTER_H3_AUTH_HOSTS` variable (comma-separated; `.example.com` or `*.example.com` covers a domain and its subdomains), and `AWS_SESSION_TOKEN` only to S3 endpoints (`*.amazonaws.com`, `*.amazonaws.com.cn`, or the configured `AWS_ENDPOINT_URL` / `AWS_S3_ENDPOINT` host). Neither is sent over plain `http://` except to a loopback host. Previously any URL passed to a scan, including one supplied through SQL, received them. Deployments that rely on `RASTER_H3_AUTH_TOKEN` must set `RASTER_H3_AUTH_HOSTS`; a 401 or 403 from a server the token was withheld from says so in its error. The scope follows the transport's current `url`, so a retargeted `HttpTransport` does not carry a token to another host.
- `ChunkDecoder` no longer hides chunk read and decode failures behind the generic TIFF decoder. A chunk that cannot be read from the source now fails with that read's error instead of being requested a second time, and a stored chunk whose bytes do not decode to its declared layout fails with the accelerated decoder's error. The generic decoder is still used for layouts the accelerated kernels do not cover, for Deflate streams that inflate past their declared layout, and to re-read a chunk from the source when a caller-supplied payload (`read_chunk_from_compressed_bytes`) fails to decode.

### Removed

- `aggregator::accumulator::FastRunAccumulator`, which had no callers. Its `sum_sq - sum²/n` variance was also numerically unstable for values with a large mean.
- The `serde::Serialize` / `Deserialize` implementations of `H3Accumulator`. They skipped the quantile sketch, so a round trip silently lost it, and nothing used them; spill runs and partial states have their own lossless encoding (`multi_horizon::spill`, `encode_partial_state`).
- The free functions `pmtiles::tiler::evict_and_write_tiles` and `pmtiles::tiler::flush_all_tiles`, which had no callers; use the `TilePyramidAccumulator` methods `evict_and_write_tiles` and `flush_all` instead.

### Fixed

- Deflate tiles clipped by the bottom image edge are decoded once. The direct-decode path sized its output for the clipped tile while the stored tile inflates whole, so libdeflate rejected it and every such tile was decoded a second time by the generic TIFF decoder. Output was correct; only the bottom tile row of rasters whose height is not a multiple of the tile height was slower.
- `PmtilesWriter::finish_into` rejects duplicate tile IDs instead of writing both entries into the directory, where readers would see only one and silently lose the other's features.
- Python: a scan error raised through the Arrow stream (for example `strict_transforms=True` with untransformable samples) reached PyArrow callers as `SystemError: ... returned NULL without setting an exception`, and the stream was leaked. PyArrow drops the stream capsule while the error is propagating, and the capsule destructor, written in Python, could not run in that state without clearing the pending exception. `pa.table(stream)` was affected with every PyArrow version, and `to_polars()` / `read_raster_h3()` on Polars versions that read through PyArrow. Capsules now use destructors exported by the native library (`raster_h3_py_stream_capsule_destructor`, `raster_h3_py_schema_capsule_destructor`, registered through `raster_h3_py_set_capsule_api`); the Python destructors remain as a fallback for an older library.
- Python: `merge_partials` accepts a Polars `DataFrame` when the installed PyArrow cannot import Polars' Arrow stream (PyArrow 14 rejects the string-view format `vu`); Polars frames are converted with `DataFrame.to_arrow()`.
- `CategoricalAccumulator::update_weighted` ignores NaN and infinite weights, as `H3Accumulator::update_weighted` and `QuantileSketch::update` already do. NaN slipped through the `weight <= 0.0` guard and turned `total_count`, the class count and every fraction derived from them into NaN; an infinite weight made its class the permanent majority.
- `QuantileSketch::update` ignores NaN and infinite weights; they previously slipped through the `weight <= 0.0` guard and turned `total_count` into NaN or infinity.
- Remote range reads retry a response body that fails mid-read or arrives shorter than announced (an io error while reading, or fewer bytes than the requested range or the stated `Content-Length`), using the same backoff as request-level retries instead of failing the whole scan.
- `QuantileSketch` no longer counts every value below 1e-7 in magnitude as zero, so quantiles of small values keep their documented 1% relative error instead of collapsing to the minimum. Streams with many such values now keep more sketch buckets, so their quantile state is larger; only zero and subnormal magnitudes use the exact zero bucket.
- `PmtilesWriter` leaf directories now grow (through `build_directories`) until the root directory fits the PMTiles v3 limit of 16,384 bytes minus the 127-byte header; the root was not checked once leaf directories were in use, so an archive with tens of millions of tiles could exceed it.
- Remote retry delays now use random jitter between half and the full exponential backoff, so concurrent workers no longer retry in lockstep, and a numeric `Retry-After` response header (up to 30 s) is honoured when it is longer than the backoff.

- Arrow C Data import (`ArrowArray::to_record_batch`) rejects Utf8/Binary columns whose offsets are negative or decreasing, or that reference data through a null data buffer, with `InvalidParameter`. Such input used to panic or slice the wrong bytes.
- `IpcStreamReader` no longer allocates from declared lengths. A row count, string length, validity length or field-name length larger than the bytes that follow now fails as a truncated stream instead of requesting up to gigabytes first.
- A panic inside an aggregation batch no longer leaves `MultiHorizonStreamer` without its worker maps. The pool is restored to full size before the panic continues, so a caller that catches it cannot go on to drop every later sample silently. The stream is also latched as failed: the batch in flight is lost, so later reads return an error instead of continuing without its samples.
- PMTiles tiling skips a cell at any zoom where it would cover more than 65,536 tiles (`MAX_TILE_OPS_PER_CELL_ZOOM`), which only happens for cells containing a pole at fine zooms, where the old enumeration produced millions of features per cell. `for_each_cell_tile_mercator` now returns the number of tiles emitted, and the continuous and categorical tilers report each skipped `(cell, zoom)` pair, overview parents included, as `h3_cells_over_tile_cap` in the archive metadata.

## [0.7.0] - 2026-11-06

### Added

- Python scan options `band`, `sampling`, `overlap_rule`, `min_majority_fraction` and `spectral_formula` (with `nir_band`, `red_band`, `green_band`, `blue_band`, `swir_band`) on `scan_raster_h3`, `read_raster_h3` and `RasterH3ArrowStream`, with DuckDB's names and defaults. Unknown names raise instead of falling back to a default. `plan_partitions` takes `overlap_rule` so partitioned scans can use it. The options travel as a JSON object through new `raster_h3_py_*_v2` C entry points; the older symbols keep their signatures.
- Python `quantiles` (DuckDB's syntax, or a list such as `[0.05, 0.95]`; one `Float64` column per target), `remap` (DuckDB's string, or a mapping with `int`, `range` and `"else"` keys) and the cell filters `min_count`, `min_mean` and `max_mean`. `merge_partials` takes `quantiles` and the filters, so partitioned scans support all of them, including `min_majority_fraction`. New C entry points `raster_h3_py_merge_partials_v2`, `raster_h3_py_create_arrow_schema_v2` and `raster_h3_py_quantile_columns`.
- Polars predicate pushdown in `scan_raster_h3`: comparisons with a literal on `count` / `total_count`, `mean`, `majority_fraction` and `resolution`, joined by `&` (and `is_between`), become `min_count`, `min_mean`, `max_mean`, `min_majority_fraction` and a narrower set of scanned resolutions. Polars still applies the whole predicate, so results are unchanged; other expressions are evaluated by Polars as before.
- `raster-h3-sedona` PMTiles output: `register_all` registers a write-only `pmtiles` file format, so DataFusion and SedonaDB SQL can `COPY (query) TO 'cells.pmtiles'` (or `STORED AS PMTILES`) with options `h3_column`, `min_zoom`, `max_zoom` and `memory_budget_mb`. Each row becomes a hexagon feature with the other columns as named properties (binary geometry columns left out); NULL H3 values are skipped and invalid ones fail the write. Rows are external-sorted with bounded memory through core's `ExternalFeatureExport` on a blocking thread, and the archive is uploaded through the session's object store. Rust: `write_pmtiles(DataFrame, path, options)`, `write_record_batches`. Python: `raster_h3_sedona.write_pmtiles(data, path, ...)` for any `__arrow_c_stream__` source (SedonaDB DataFrame, PyArrow, Polars), since SedonaDB's extension ABI cannot register file formats. Core: `ExternalFeatureExport::with_layer_fields` lists property names and types in the archive's `vector_layers` metadata.
- `raster-h3-cli parquet-to-pmtiles <in.parquet> -o <out.pmtiles> [--h3-column NAME]` transcodes any H3-indexed Parquet file (from `raster-h3-cli parquet`, DuckDB, Spark, Polars) to PMTiles v3, reporting rows with a null or invalid H3 value on stderr. Library: `transcode_parquet_to_pmtiles`, `parse_parquet_to_pmtiles_args`.
- Python `parquet_to_pmtiles(parquet_path, output_path, h3_column=None)`, the counterpart of DuckDB's `h3_parquet_to_pmtiles`.
- `worker_scaling` example and `profile.yml` job: wall, kernel, merge and spill time for six workloads at 1/2/4/8 aggregation workers, with output-equality checks (`--budget-mb`, `--only`, `--quick`). Results and the decision not to parallelise the merge yet are in `docs/worker-scaling.md`: the serial merge's main cost is computing each new cell's eviction latitude (~584 ns per cell). `metrics()` on `MultiCategoricalHorizonStreamer`.
- `raster-h3-core` partitioned scans, so one mosaic can be split across independent workers (e.g. Spark tasks) without reading any chunk twice: `plan_partitions` builds serializable `ScanPartition`s (chunk-grid rectangles plus a mosaic fingerprint) from metadata, `MultiResolutionConfig::partition` restricts a stream to one partition, and `drain_partitioned_into` returns completed interior cells as records and cells that other partitions may also reach as unfiltered `PartialCell`s. `encode_partial_state` / `decode_partial_state` exchange partial states in the lossless spill format, and `PartialMerger` merges them and applies the filters. Results match a single scan for tiled and striped rasters, geographic and projected CRSs, mosaics under every overlap rule, quantiles, categorical data, filters and spilling. Child compaction is not supported with partitions.
- Python partitioned scans: `raster_h3.plan_partitions(path, target_chunks=...)` returns serializable partition specs, `RasterH3ArrowStream(..., partition=spec)` scans one of them with an extra nullable `partial_state` column marking cells that other partitions also reach, and `raster_h3.merge_partials(rows)` merges those rows into final cells (accepting PyArrow, Polars or Spark-typed input). Core gains `columnar::PartitionedBatchReader` and `merge_partial_batches` for engines. `examples/partition_overhead` measures the CPU cost of partitioning against a single scan.
- Partitioned scans in the DuckDB, DataFusion and SedonaDB adapters. DuckDB: `h3_raster_plan_partitions(path, target_chunks := ...)`, a `partition := spec` parameter on `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate` (adds a `partial_state` BLOB column) and the `h3_merge_partials` / `h3_merge_categorical_partials` aggregates, which return a STRUCT of the scan's statistics. DataFusion: `with_partition` / `plan_partitions` on both providers (only `resolution` filters are pushed into partitioned scans). SedonaDB SQL: `h3_raster_plan_partitions`, a partition spec argument to the aggregate table functions, and the same two merge aggregates.
- `raster-h3-cli` partitioned scans for shell fan-outs (`xargs -P`, batch jobs): `plan <input> [--target-chunks N]` prints one partition spec per line, `ipc --partition <SPEC|@FILE> [--partition-index K]` scans one partition (adding the `partial_state` column), and `merge <part>... [--jsonl]` writes the final rows of all partition files plus their merged partial rows, equal to a single scan. Partition files carry an `RH3PART1` header, so `merge` refuses an incomplete or repeated set of partitions, or files from another plan, input or option set, before writing anything; it streams each file instead of loading it. Library: `plan_raster_partitions`, `read_partition_spec`, `merge_partition_files`. Core: `columnar::ipc::IpcStreamReader` reads an `RH3IPC01` stream one batch at a time (`read_ipc_stream` now uses it).
- `raster_h3.spark`: a Spark 4.0+ data source (Python Data Source API). `raster_h3.spark.register(spark)` then `spark.read.format("raster_h3").option("resolutions", "8,9").load(path)` (or `CREATE TEMPORARY VIEW ... USING raster_h3 OPTIONS (...)`) scans each partition planned by `plan_partitions` in its own Spark task, with the scan options of `RasterH3ArrowStream` as Spark options and Spark's signed types. `raster_h3.spark.merge_partials(df, ...)` merges the edge cells into final cells, equal to one unpartitioned scan; `raster_h3.spark.read(spark, path, **options)` does both, scanning the raster once. In SQL, `SELECT * FROM raster_h3_merge(TABLE(t) [, 'quantiles=p50, min_count=20'])` (a Python UDTF registered with the data source) merges a `raster_h3` scan without Python code. On Spark 4.1+, `register(spark, pushdown=True)` pushes `resolution` comparisons and `partial_state IS [NOT] NULL` into the scan; filters on statistics stay in Spark, because before the merge they would see a partition's share of a cell. PySpark is imported only by this module. Tested on Apache Sedona 1.9.1 (Spark 4.0.1), and with PySpark 4.1 for pushdown, in the `sedona-spark` CI job, together with the Apache Sedona demo's `mapInArrow` pipeline (`examples/sedona_spark/tests`). New guide: `docs/engines/spark.md`.

### Changed

- Horizon batches are sized by pixels (about two waves of worker windows, at least one chunk) instead of `aggregation_workers × 2` chunks. A horizon map holds a whole batch of cells, so with large strips or tiles each extra worker added whole chunks of cells, and a three-resolution run spilled even at a 2 GiB budget. That run is now 1.5× faster at 4 workers with no spill, and resolution 9 with quantiles no longer spills under the 64 MiB default (2.1× faster at 4 workers). Peak horizon-map memory no longer grows with the worker count. `docs/engineering.md` adds an aggregation-budget sizing estimate for fine resolutions.
- Horizon eviction keys (`compute_cell_south_lat`, a cell-boundary computation of ~584 ns) are now computed in the parallel aggregation workers instead of on the serial merge thread. Keys and output are unchanged. At resolution 11 with 4 workers the merge drops from 12.1 s to 2.0 s and the stream from 18.0 s to 10.6 s; see `docs/worker-scaling.md`. `StreamProfile::eviction_key_fallbacks` counts keys the merge still had to compute itself (normally 0).

### Fixed

- Built-in remote HTTP reads pin strong ETags per URL and send `If-Match` on subsequent range requests. Same-size object replacements, changed/missing response validators, and HTTP 412 fail instead of mixing cached metadata with another version’s pixels. Clones and retries retain each URL's validator, even when transports are retargeted; sources without strong ETags remain supported without a snapshot guarantee.
- `RasterH3Table` no longer passes a premature `LIMIT` to the inner reader or `StreamingTableExec` when any supplied filter is inexact or unsupported. Compacted scans and strict comparisons now leave enough rows for caller-side filtering; unfiltered and exact-filter scans retain limit pushdown.
- Continuous `stddev` (and `H3Accumulator::variance`) is now a reliability-weighted sample estimate, `M2 / (W - sum(w_i^2) / W)` with `W = sum(w_i)`, instead of `M2 / (W - 1)`. Fractional weights from super-sampling or bbox clipping made `W - 1` meaningless: two pixels of 100 and 200 with weights 0.4 and 0.6 reported a `stddev` of 0. The new estimator is invariant to rescaling the weights and equals the previous result whenever every pixel has weight one (center sampling, and super-sampled pixels wholly inside one cell), so most outputs are unchanged. Each pixel is one observation per cell: its sample weights in the same cell are combined before entering `sum(w_i^2)`, with or without quantiles, so a pixel's contribution does not depend on the number of sample points. `H3Accumulator` gains a public `sum_w2` field and `from_weighted_stats`; `from_stats` assumes unit weights. Spill runs and partial states carry `sum_w2`; the `RH3P` partial-state encoding is now version 2, and version 1 states are rejected because their weighted variance cannot be recovered, so all workers of a partitioned scan must run the same release.
- A `band` (or spectral formula band) the raster does not have is now an error when the stream opens, in every engine. The sample kernels clamped it to the last band, so `band := 5` on an RGBA raster silently aggregated the alpha band.
- `h3_parquet_to_pmtiles` (and the new Python `parquet_to_pmtiles`) wrote an empty archive when the H3 column was nullable, the default for Parquet written by Polars, PyArrow and DuckDB: the row-group pre-scan read it without a definition-level buffer, and the resulting error was swallowed.
- DataFusion-style providers leave predicates to the caller when compaction is enabled, and defer limits until after those predicates. Resolution and aggregate filters no longer discard compacted parents or change their statistics by filtering children. Applies to both compaction flags and continuous/categorical scans. The SedonaDB/DataFusion `RasterH3Table` now caps each filter's pushdown at the wrapped provider's status, so compacted SQL queries keep their `WHERE` clauses as DataFusion filters instead of returning unfiltered rows.
- A GeoTIFF whose WKT carries a nonzero `TOWGS84[...]` datum shift, or whose user-defined GeoKeys carry a nonzero `GeogTOWGS84GeoKey` (2062), no longer yields a CRS that silently drops the shift; no CRS is reconstructed from it. All-zero shifts (which GDAL commonly writes for NAD83 and WGS84) and CRS names that merely contain `TOWGS84` are still accepted.
- Calibrated (`scale`/`offset`) values and spectral indices that overflow to a non-finite value are now excluded the same way in every path: per-sample, same-cell spans and quantile sketches. Previously a span could fold an infinite sum or extreme into a cell that the per-sample path would have skipped.
- PMTiles `h3_hex` feature properties were written with a leading NUL and without their last digit; they now hold the full hex index.
- Malformed `remap` strings (an empty source token, a multibyte first character, or deeply nested brackets) return an error instead of panicking or exhausting the stack. They were reachable from DuckDB SQL.
- A deflate or zlib chunk that decompresses to fewer bytes than the chunk size is now an error. The short tail previously kept stale bytes from a recycled buffer, which were decoded as pixels.
- Remote reads from servers that ignore `Range` (answering the probe with the whole file) are served from the cached body instead of failing every later range request.
- Multi-band TIFFs with `PlanarConfiguration=Planar` (one band per strip or tile) are rejected when opened instead of being decoded as interleaved samples.

## [0.6.0] - 2026-11-06

### Added

- `raster-h3-sedona`: a real Apache DataFusion adapter (DataFusion `=54.1.0`, arrow `58`, matching SedonaDB) that registers into a plain `SessionContext` or SedonaDB's `SedonaContext::ctx`. It provides `RasterH3Table` (a `TableProvider` with projection, `LIMIT` and `Expr` filter pushdown, streamed zero-copy over the Arrow C Stream), the `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate` table functions (with an optional CRS argument), and the H3 scalar UDFs. `wkb` and `h3_to_wkb` output are `geoarrow.wkb` fields in `OGC:CRS84`, so Sedona's `ST_*` functions accept them directly. It is a standalone workspace, so it adds nothing to the root `Cargo.lock`. A `sedona-e2e` feature runs the tests inside a real `SedonaContext`.
- `raster-h3-sedona` `h3_polygon_to_cells(geometry, resolution [, containment])` UDF returning `List<UInt64>`, with DuckDB's containment modes and NULL handling (the rows of a batch are polyfilled in parallel on the rayon pool), so polygons can be polyfilled and joined to raster cells on `h3_index` in DataFusion and SedonaDB.
- `raster-h3-sedona` spatial pushdown: `ST_Intersects`-style predicates and `ST_DWithin(..., d)` between a cell's geometry (`wkb` or `h3_to_wkb(h3_index)`) and a constant lon/lat geometry (a `geoarrow.wkb` literal or `h3_to_wkb(<cell>)`) push the constant's envelope, grown by `d` degrees for `ST_DWithin`, into the scan as a `Bbox`. `raster-h3-datafusion` widens it past the largest cell, so results match an unpruned scan.
- `raster-h3-sedona` Python bindings (`crates/raster-h3-sedona/python`, module `raster_h3_sedona`): `RasterH3Table.continuous(...)` / `.categorical(...)` implement SedonaDB's `__sedonadb_table_provider__` protocol (a `sedonadb_table_provider` capsule wrapping `sedona_extension::ExportedTableProvider`), so `sedonadb.connect().create_data_frame(table)` scans a raster with the same projection, limit and filter pushdown. `raster_h3_sedona.scalar_functions()` exports the scalar UDFs (with their aliases) as `sedonadb_scalar_kernel` capsules for `sd.register`. Tested against the `sedonadb` 0.5.0 wheel.
- Second DuckDB extension build targeting the stable **v1.5.6 C extension API**, shipped alongside the existing v1.2.0-API build (#76). Release artifacts now carry two trees: `v1.2.0/<platform>/` keeps the v1.2.0-API build (runs on DuckDB 1.2 through 1.5), while `v2.0.0/<platform>/` now ships a real v1.5.6-API build instead of a copy. Enable it with `cargo build --features duckdb-api-v1_5_6`.
  - `SET raster_h3_use_duckdb_fs = true` routes raster byte-range reads through DuckDB's file system, so `s3://`, `gs://`, `azure://` and `http(s)://` sources use the loaded httpfs configuration and `CREATE SECRET` credentials. The new `RASTER_H3_USE_DUCKDB_FS` environment variable enables it too.
  - `SET raster_h3_fetch_workers`, `SET raster_h3_decode_budget_mb` and `SET raster_h3_workers` configure the fetch/decode/aggregation pools per session. Values default to 0 (unset); an unset option falls back to the new `RASTER_H3_FETCH_WORKERS`, `RASTER_H3_DECODE_BUDGET_MB` and `RASTER_H3_WORKERS` environment variables. Explicit table-function arguments take precedence over both. These options and environment variables exist only in the v1.5.6-API build.
  - `COPY (...) TO '<path>' (FORMAT pmtiles, min_zoom N, max_zoom N)` writes a PMTiles pyramid through DuckDB's file system, replacing any existing file. The first column is the H3 index (`UBIGINT`, `BIGINT` or hex `VARCHAR`): rows with a NULL index are skipped and any other value that is not a valid H3 cell fails the COPY. Every other column becomes a feature property named `column_<position>`. Unknown options are rejected. Memory is bounded by `MEMORY_BUDGET_MB` (default 256): larger inputs are sorted through temporary files in DuckDB's `temp_directory`, and the archive streams straight to the destination without a temporary copy.
  - `h3_polygon_to_cells` validates a constant `resolution` (0..=15) and containment mode at plan time via a scalar bind callback and expression folding.
  - `h3_cell_to_geometry`/`h3_to_geometry`, the `h3_polygon_to_cells` geometry argument and the table functions' geometry column use the native `GEOMETRY` type unconditionally (the v1.2.0 build still falls back to `BLOB`/alias on DuckDB < 1.5).
  - `H3PmtilesTiler::export_h3_features` accepts features in any order with bounded memory (an external sort by latitude through temporary files, 256 MiB buffer by default) instead of holding every feature until the end. `pmtiles::ExternalFeatureExport` exposes the budget, spill directory and an output `Write` target; `PmtilesWriter` gains `new_in` (spill directory) and `finish_into` (stream to any writer).
  - Raster paths with any `scheme://` prefix other than `file://` (for example `gs://`, `az://`, `hf://`) are now treated as remote in every build. Without the DuckDB file system, schemes other than `http(s)://` and `s3://` fail with an "Unsupported remote protocol" error instead of a missing-file error.
- DuckDB 2.0 extension signing and custom repository support: `package_extension` gains `--signing-key` (`-k`, or `DUCKDB_EXTENSION_SIGNING_KEY`), `--public-key` (`-K`, or `DUCKDB_EXTENSION_PUBLIC_KEY`), `--repo-manifest` (`-m`), `--verify` and `--print-platform` to compute DuckDB's 1 MiB chunked SHA-256 tree hash, sign the 256-byte footer slot (`footer[278..534]`) with RSA-2048 PKCS#1 v1.5, verify signed `.duckdb_extension[.gz]` binaries, and generate `.well-known/duckdb-extension-repo.json` (`signature_keys`) for `CREATE EXTENSION REPOSITORY`. CI packages both `linux_amd64` (DuckDB ≥ 1.3 & 2.0+) and `linux_amd64_gcc4` (DuckDB 1.2) on Linux x86_64 and runs `duckdb_sql_smoke` against DuckDB v2.0.0, plus `duckdb_trusted_repository`, which installs and loads a signed build through `CREATE EXTENSION REPOSITORY` with signature checking on and rejects unsigned and wrong-key builds. The release repository bundle adds a `v2.0.0/<platform>` tree, the directory DuckDB 2.0 requests.
- Memory benchmark suite (`cargo test --release -p raster-h3-core --test memory`): seven scenarios (spill-run construction, forced spills with quantile sketches and high-cardinality categories, slow HTTP and Parquet consumers, many tiny chunks, concurrent streams), each variant in its own process, with allocator, request-count and spill checks that run in CI on every platform; RSS and wall time are reported only. `docs/testing.md` records a before/after measurement of the #60 prefetch byte budget. The counting allocator is shared with `profile_hotspots`.

### Changed

- Index each sample at every resolution through `h3o::LatLngIndexer`, sharing H3's resolution-independent projection and reusing the last cell per resolution while consecutive samples stay in the same face `IJK` cell; cells are identical. Requires h3o 0.11.1, the first release with `LatLngIndexer`. Add opt-in `.cargo/fma.toml` for x86_64 CPUs with FMA3.

### Fixed

- DataFusion: a pushed `FilterPredicate::Bbox` that misses every raster tile now scans empty instead of failing with "No mosaic tiles intersect the requested bounding box", so a spatial filter over an area without data returns no rows. A table-definition `with_bbox` that misses the raster still errors. Core exports the message as `raster::mosaic::NO_TILES_IN_BBOX`.
- DataFusion `FilterPredicate::Bbox` pushdown no longer changes the aggregates of cells straddling the box edge. The pushed box is widened by more than the largest cell diameter at the coarsest resolution, so every cell intersecting the box keeps the values of an unfiltered scan and the pushdown is a true `Inexact` superset. It is not pushed when compaction is on. `with_bbox` keeps its pixel-clipping semantics.

## [0.5.0] - 2026-10-28

### Added

- Strict transform mode: `MultiResolutionConfig::strict_transforms` fails the stream with the new `RasterH3Error::TransformFailures` as soon as a valid sample point is dropped by a failed coordinate transform. Exposed as DuckDB `strict_transforms := true` (every table function; exports report it in `status`), Python `strict_transforms=True` (`scan_raster_h3`, `read_raster_h3`, `RasterH3ArrowStream`) and DataFusion `with_strict_transforms(true)`. Off by default.
- Lenient scans report dropped samples too: Python `scan_raster_h3`, `RasterH3ArrowStream.iter_batches()` and `.to_dict()` emit a `RuntimeWarning` once the scan completes, and DataFusion providers take `with_transform_failure_counter`. The Python native library adds `raster_h3_py_create_arrow_stream_ex`, `raster_h3_py_open_batch_reader_ex`, `raster_h3_py_scan_json_ex` and `raster_h3_py_batch_reader_transform_failures`; the existing scan symbols keep their signatures.
- Transform-failure counts reach the engines: a `transform_failures` column in DuckDB's `h3_raster_to_parquet` / `h3_raster_to_pmtiles` (and `h3_parquet_to_pmtiles`, always 0) summary rows, one PostgreSQL `WARNING` at the end of the aggregate SRFs, a Python `RuntimeWarning` from `raster_to_parquet` / `raster_to_pmtiles`, and a CLI stderr warning from `parquet` / `pmtiles`. Core adds `MultiResolutionConfig::transform_failure_counter` for pipelines that own the streamer. The Python native library adds `raster_h3_py_export_parquet_with_stats` / `raster_h3_py_export_pmtiles_with_stats`; the existing export symbols keep their signatures.
- `transform_failures()` on the continuous and categorical streamers and batch readers: valid sample points dropped because their coordinate transform failed or left the valid latitude/longitude range (previously silent). Counted in every build. The CLI warns on stderr when it is non-zero.
- `profile_hotspots` example and `profile.yml` workflow measuring mosaic tile-scan scaling and nested-Rayon wave dispatch, with a counting allocator. `StreamProfile` gains `waves`, `bridged_waves` and `serial_waves` counters.
- GDAL band `SCALE`/`OFFSET` calibration: continuous statistics, quantiles and spectral indices now use `stored * scale + offset` per band (and per mosaic file). NoData is still matched on stored values. Opt out with DuckDB `scale_offset := false`, CLI `--raw-values`, DataFusion `with_scale_offset(false)` or `MultiResolutionConfig::apply_scale_offset`. Malformed scale/offset metadata is rejected when the file is opened.
- TIFF alpha: pixels whose `ExtraSamples` alpha sample is zero are excluded from continuous and categorical aggregation.

- `QuantilePlan` evaluates a list of `QuantileTarget`s with one sketch sweep per cell; `ContinuousBatchReader::tracks_quantiles()` and `columnar::quantiles_projected()` expose the quantile pushdown decision.
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
- pytest suite for the Python package (`crates/raster-h3-python/tests/pytest`) against real Polars and PyArrow, and a `python-polars` engine in `manual-tests.yml` covering Polars 1.0, 1.4 and latest.

### Changed

- Workspace, Python package, PostgreSQL extension and DuckDB community-extension manifest versions bumped to `0.5.0` (PostgreSQL install script renamed to `sql/raster_h3--0.5.0.sql`; its definitions are unchanged from 0.4.0, and `sql/raster_h3--0.4.0--0.5.0.sql` lets existing databases run `ALTER EXTENSION raster_h3 UPDATE`). The community-extension manifest's `repo.ref` points at the 0.5.0 version-bump commit.
- Upgraded tiff to 0.11. 16-bit floating-point GeoTIFFs, which tiff 0.9 could not decode, are now read and widened to `f32`; the TIFF floating-point predictor (Predictor 3) is now implemented in `raster_h3_core::raster::predictor` because tiff no longer exports it.
- Upgraded h3o to 0.11 (polyfill now uses `h3o::geom::TilerBuilder`) and parquet to 60 (`flate2-rust_backend` codec feature). With h3o 0.11, `h3_polygon_to_cells(..., 'intersects_boundary')` returns the enclosing cell for a polygon that fits inside one cell, matching `covers`, instead of an empty list.
- The DuckDB reference moved to [`docs/sql-api-reference.md`](docs/sql-api-reference.md) (SQL API Reference) and a new [Polars API Reference](docs/polars-api-reference.md) documents the Python package's scan options, output schemas, `RasterH3ArrowStream`, exporters and H3 helpers. `docs/api-reference.md` now links to both.
- Python scans and exporters reject an empty `resolutions` (`""` or `[]`) with `Invalid resolutions` instead of silently using resolution 8. A NULL `resolutions` pointer in the C ABI still means `[8]`.
- Remote COG prefetching is bounded by bytes instead of chunk count: each range's size is reserved against `RemotePrefetchConfig::max_buffered_bytes` (new, default 32 MiB) before its request, covering requests in flight, and stays charged until the last chunk sliced from the response is dropped. Chunks share the response buffer instead of being copied out under the lock, and workers wait on a condition variable instead of a 5 ms polling loop. `get_chunk_payload`/`try_get_chunk_payload` now return `PrefetchedChunk` (derefs to `[u8]`) instead of `Arc<Vec<u8>>`; `held_bytes()`/`peak_held_bytes()` report budget use. Blocking requests are prioritized in any order, evicting unrequested prefetched responses when necessary and refetching their untaken chunks on demand without exceeding the byte budget (or one oversized range).
- `Cutline` overlap ownership caches each tile's centre unit vector and computes the sample's once per call, removing 8 trig calls per tile per overlapping sample (about 29 ns → 1.3 ns per tile); decisions are bit-identical. Documented the cost of driving streamers from a Rayon pool and when to set `aggregation_workers`.
- Continuous readers (core `ContinuousBatchReader::open`, and so the CLI, Python and DataFusion paths, plus the DuckDB scan) build per-cell quantile sketches only when a quantile column is projected, and compute all projected quantile columns of a cell in one sorted sweep instead of one sweep (and bin-key sort) per column. `H3Accumulator::iqr` uses one sweep for p25/p75.
- Keep Albers inverse constants in an immutable walker-local prepared projection, preserving public source-field construction and mutation without stale caches. Use portable multiply/add series evaluation, share row terms and exact cell-run advancement, and retain row hoisting for horizontal shear and `stream-profile` builds. The series can differ from the previous Newton iteration at sub-nanometre scale, which moves pixels lying on a cell edge (1 of 28.4M pixels in the Hawaii CFL Albers benchmark); the Hawaii WGS84 outputs were bit-for-bit identical.
- Label withdrawn stride-probing benchmark results as historical row-count comparisons only; use a monotonic benchmark timer and the direct Parquet exporter when the Polars IO plugin is unavailable.

- Long-format categorical output (core `CategoricalBatchReader` and the DuckDB `format := 'long'` scan) now expands one cell at a time and only as far as each batch needs, instead of eagerly queueing every category of a whole batch of cells (e.g. ~2M rows for 2,048 cells × 1,000 classes). Shannon entropy is skipped when no entropy column is projected.
- Restore safe bbox and mosaic-overlap pruning for built-in EPSG:3413/3031 using analytic radial/angular bounds; skip discarded bounds estimates for uncertified bbox tests. Add end-to-end pruning benchmarks with Lambert/LAEA controls.

- Python C ABI: a malformed `bbox`, invalid `resolutions` or unknown `format` now raises instead of scanning the whole raster, falling back to resolution 8 or treating the format as `wide`; Parquet and PMTiles exports reject invalid resolutions the same way.
- Python `scan_raster_h3` works on Polars 1.4–1.24 (positional `register_io_source` argument) and returns an empty frame with the schema instead of panicking on Polars before 1.28 when nothing matches.

- TM rejection certificates now follow proj4rs scale precedence (`k0`, then `k`) and fail open for duplicate or non-canonical PROJ parameters, preventing incorrect bbox pruning.

- Mosaic decode workers keep at most `MAX_OPEN_DECODERS_PER_WORKER` (16) tile decoders open, evicting the tile needed furthest in the future instead of using LRU, which thrashes on latitude bands wider than the cache. The next-use index is shared across workers; chunk delivery order and decoded-buffer budgets are unchanged.
- WKB export looks up the two pole cells per resolution from a precomputed table instead of indexing both poles for every emitted polygon, and only when a ring spans more than 180° of longitude.

- Replaced unmaintained `fxhash` with maintained `rustc-hash`, removed obsolete advisory exceptions, and denied wildcard dependency requirements.
- Replaced brittle two-second streaming test assertions with correctness-only tests and separate informational benchmarks.

### Fixed

- Polars' native Parquet reader (`pl.read_parquet` / `pl.scan_parquet`) failed with "Invalid thrift: bad data" on every Parquet / GeoParquet file from `raster_to_parquet`, the CLI `parquet` subcommand and DuckDB `h3_raster_to_parquet`. parquet 60 writes the `IEEE_754_TOTAL_ORDER` column order for FLOAT/DOUBLE columns with no way to opt out; Polars cannot decode it and PyArrow ignores those columns' min/max statistics. The export pipeline now rewrites those footer entries as the type-defined order after closing the file (`parquet::column_order::restore_type_defined_float_orders`). Files written before this fix still need `use_pyarrow=True`, or have to be regenerated.
- PMTiles vector-tile rings now have positive area in tile coordinates, as MVT 2.1 requires for exterior rings (they were all reversed), and the two pole cells are polar caps reaching the map edge instead of rings jumping back across the tile. `MvtFeature` holds up to `MAX_FEATURE_POINTS` (14) points.
- The GeoParquet `bbox` covers the cell geometries actually written instead of the raster's bounds, which cells extend past; the `geo` metadata is appended when the file is closed. Adds `encoding::extend_wkb_bounds`.
- WKB cell geometries are valid for planar GIS tools: cells crossing the antimeridian are written as a `MultiPolygon` cut at ±180° (instead of a ring with longitudes beyond ±180°), and the two pole cells are written as counter-clockwise polar-cap polygons (instead of an unchanged ring that self-intersects in longitude/latitude). Ordinary cells are unchanged. `WKB_BUF_LEN` grows from 192 to 771 bytes, and GeoParquet `geometry_types` now lists `Polygon` and `MultiPolygon`. Python's `h3_to_wkb` sizes its buffer from the new `raster_h3_py_wkb_buf_len()` instead of a hard-coded 192 bytes.
- Uncompressed GeoTIFFs with unspecified extra samples (for example RGB plus NIR) failed with "decoded chunk shorter than its declared dimensions" because the `tiff` decoder drops those samples; uncompressed chunks are now decoded from the `SamplesPerPixel` layout. Unrecognised compression codes are no longer mapped to uncompressed.
- Single-probability `QuantilePlan` evaluations use the scalar sketch query, avoiding batch result/rank-vector allocations per cell; empty plans skip sketch evaluation. Added p50/p90 scalar/batch/plan Criterion comparisons.
- `QuantileSketch::quantiles_batch` now returns exactly what `quantile()` returns for every target: NaN targets yield NaN instead of corrupting the sort (which could misorder results or panic), and degenerate `min`/`max` ranges match `q <= 0`/`q >= 1` handling.
- Quantile specs reject NaN percentiles (`'nan'`, `'pnan'`), which previously parsed because NaN fails both range comparisons. The DuckDB API reference lists the real presets (`'box'`, not the unsupported `'quartiles'`).
- Remote COG prefetch no longer reports a chunk as unavailable while its range request is still in flight. The queue previously treated "all ranges claimed" as "all ranges fetched", so decoders near the end of a scan could re-download chunk bytes and leave the late payload orphaned in the ready map.
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
- `h3_polygon_to_cells(geometry, resolution [, containment])` scalar (h3o polyfill) in DuckDB (`BLOB`/`GEOMETRY` → `UBIGINT[]`) and Python, with the `contains_centroid` (default), `contains_boundary`, `intersects_boundary` and `covers` containment modes ([API Reference](docs/sql-api-reference.md#h3_polygon_to_cells)). Backed by `raster_h3_core::scalar_ops::h3_polygon_to_cells_wkb` and a new WKB polygon reader.
- **Multi-engine Cargo workspace** ([Engines Overview](docs/engines/README.md)): the engine moved to `crates/raster-h3-core`; the DuckDB extension to `crates/raster-h3-duckdb`. The root `raster_h3` crate re-exports every member and still builds `libraster_h3` for DuckDB.
- `raster-h3-core` `columnar` module: `RecordBatch`/`ColumnSchema` model, `ContinuousBatchReader` / `CategoricalBatchReader`, Arrow C Data and C Stream interface, `RH3IPC01` binary stream and NDJSON writer; `scalar_ops` module with H3 helpers shared by all engines ([Core](docs/engines/core.md)).
- `raster-h3-datafusion`: DataFusion-style `TableProvider` / `ExecutionPlan` / `ScalarUDFImpl` with projection, filter and limit pushdown ([DataFusion](docs/engines/datafusion.md)).
- `raster-h3-python` and the `raster-h3` Python package: Arrow PyCapsule stream, Polars IO source (`scan_raster_h3`, `read_raster_h3`), H3 helpers, Parquet/PMTiles exporters; `py3-none-<platform>` wheels built by `.github/workflows/wheels.yml` ([Python & Polars](docs/engines/python-polars.md)).
- `raster-h3-pg`: PostgreSQL 13–18 extension with `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate` SRFs and H3 scalar functions, plus a Docker-based real-server test (`scripts/pg_e2e.sh`, `tests/it/pg_extension_e2e.rs`) ([PostgreSQL](docs/engines/postgresql.md)).
- `raster-h3-cli`: `ipc`, `jsonl`, `parquet` and `pmtiles` subcommands with strict argument validation ([CLI](docs/engines/cli.md)).
- `tests/it/multi_engine_e2e.rs`: cross-engine parity on shared synthetic rasters.
- Built-in European national-grid EPSG codes with Helmert datum shifts: `27700` (OSGB36), `28992` (Amersfoort / RD New), `31466`–`31469` (DHDN Gauss-Kruger), `23028`–`23038` (ED50 / UTM); and ETRS89 codes treated as WGS84: `2154`, `3035`, `25828`–`25838` ([Datum Shifts](docs/crs-and-projection.md#datum-shifts)).

### Changed
- Workspace, Python package and PostgreSQL extension versions bumped to `0.4.0` (PostgreSQL install script renamed to `sql/raster_h3--0.4.0.sql`).
- The DuckDB community-extension manifest moved from `community_extension/` to `packaging/duckdb-community/description.yml`; it now points at `dmuldrew/raster_h3`, declares version `0.4.0` and excludes the opt-in musl and `windows_amd64_rtools` platforms.
- Repository and homepage URLs point to `github.com/dmuldrew/raster_h3` (previously `raster_h3_hexification`).
- `h3_parquet_to_pmtiles` (and `process_parquet_to_pmtiles` / `convert_to_pmtiles`) now return an error naming the available columns when an explicit `h3_column` is not found, instead of silently falling back to auto-detection or column 0.
- PROJ strings with 3- or 7-parameter Helmert shifts (`+towgs84=...`, or a named datum that implies one such as `+datum=OSGB36`) are now transformed via proj4rs instead of rejected. Grid-based datums (`+nadgrids` other than `@null`, `+datum=NAD27`) and unknown `+datum` names are still rejected with `CrsError`; a non-zero shift never takes a fast path ([Datum Shifts](docs/crs-and-projection.md#datum-shifts)).
- DuckDB scalar overloads (`h3_is_valid`, `h3_to_wkb`, `h3_cell_to_parent`, `h3_to_geometry`, `h3_cell_to_geometry`, `h3_polygon_to_cells`) are registered as scalar function sets. DuckDB before 1.5 rejected the second registration under a name, so only the first overload (e.g. `h3_is_valid(UBIGINT)`) was available there.
- Reorganise the documentation for the multi-engine workspace: the README is engine-neutral (Engines section after the introduction, topic deep dives at the end), the worked example has DuckDB and Polars versions, DuckDB install/Docker/SQL recipes/client examples moved to the [DuckDB deep dive](docs/engines/duckdb.md), and the Python, PostgreSQL, CLI and DataFusion deep dives gained matching recipes and troubleshooting sections. `docs/api-reference.md` is now titled the DuckDB SQL API Reference.

## [0.3.0] - 2026-10-15

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
- Bound decoded chunk memory in bytes, not only by queue length: the mosaic prefetcher charges buffers being decoded, queued, aggregated and idle against `decode_budget_bytes` (SQL `decode_budget_mb`, default 256 MiB). Reservations are granted in job order before decoding; a chunk larger than the budget is decoded only when no other is held. Peak usage is reported as `StreamProfile::peak_decoded_bytes`.
- Account pending compaction groups in bytes; they now count toward `peak_active_bytes`, and `StreamProfile` reports `peak_compaction_bytes` and `forced_compaction_flushes`. An opt-in `compaction_budget_bytes` (SQL `compaction_budget_mb`) decomposes the groups nearest completion into child records once exceeded. Samples and totals are unchanged, but which groups compact then depends on batch timing and can vary between runs; unset, compaction stays deterministic.
- Index pending compaction groups with per-resolution heaps, so streaming completion checks and the published horizon cost O(log n) per finished group instead of a scan of every pending group per batch. Streaming completion flushes stop once the output buffer reaches its byte limit and resume on the next call.
- Fuse supersample projection, H3 assignment, and accumulation with reusable per-pixel scratch buffers; avoid repeated certification work and preserve individual quantile sample updates.
- Use absolute pixel coordinates consistently for indexing, sequential span discovery, and supersampling; remove incremental coordinate stepping and obsolete width-prediction state.
- Remove unused profiling counters, sampling-bound helpers, and the `raster_h3::h3` compatibility alias. Rust callers should use `h3o` directly; callers of removed scanline helpers must migrate to the stateless indexing/span functions.
- Replace Hawaii aggregation goldens with per-pixel reference comparisons. External-data tests remain ignored by default and fail explicitly if invoked without their datasets.
- Document aggregation budgets, spill behavior, and uncertified projected eviction without unconditional process-memory or geometric-skip claims.
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

[Unreleased]: https://github.com/dmuldrew/raster_h3/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/dmuldrew/raster_h3/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/dmuldrew/raster_h3/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/dmuldrew/raster_h3/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/dmuldrew/raster_h3/compare/c63dd46...v0.4.0
[0.3.0]: https://github.com/dmuldrew/raster_h3/tree/c63dd46
