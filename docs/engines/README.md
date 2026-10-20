# Multi-Engine Workspace Overview

[← Back to README](../../README.md)

`raster_h3` is a Cargo workspace. One pure-Rust engine (`raster-h3-core`) does all raster
decoding, CRS transformation, H3 aggregation and file export; five thin adapter crates
expose it to different query engines and runtimes. Every adapter drives the same
streamers, so the same raster and parameters produce the same cells and statistics
in every engine.

```mermaid
flowchart TD
    CORE["raster-h3-core<br/>GeoTIFF/COG I/O · CRS · scanline H3 aggregation<br/>RecordBatch · Arrow C Stream · IPC/JSONL · Parquet · PMTiles"]
    DUCK["raster-h3-duckdb<br/>DuckDB C-API extension"]
    DF["raster-h3-datafusion<br/>TableProvider / ExecutionPlan / ScalarUDF"]
    PY["raster-h3-python<br/>ctypes C-ABI + Arrow PyCapsules + Polars IO source"]
    PG["raster-h3-pg<br/>PostgreSQL V1 fmgr extension"]
    CLI["raster-h3-cli<br/>ipc · jsonl · parquet · pmtiles"]
    ROOT["raster_h3 (root crate)<br/>facade + libraster_h3 cdylib (DuckDB)"]
    CORE --> DUCK & DF & PY & PG & CLI
    DF --> PY
    DUCK & DF & PY & PG & CLI --> ROOT
```

## Workspace Layout

| Crate | Path | Artifact | Deep Dive |
| :--- | :--- | :--- | :--- |
| `raster-h3-core` | [`crates/raster-h3-core`](../../crates/raster-h3-core) | `rlib` | [Core Engine](core.md) |
| `raster-h3-duckdb` | [`crates/raster-h3-duckdb`](../../crates/raster-h3-duckdb) | `rlib`, linked into `libraster_h3.{so,dylib,dll}` by the root crate | [DuckDB](duckdb.md) |
| `raster-h3-datafusion` | [`crates/raster-h3-datafusion`](../../crates/raster-h3-datafusion) | `rlib` | [DataFusion](datafusion.md) |
| `raster-h3-python` | [`crates/raster-h3-python`](../../crates/raster-h3-python) | `cdylib` + `raster-h3` wheel | [Python & Polars](python-polars.md) |
| `raster-h3-pg` | [`crates/raster-h3-pg`](../../crates/raster-h3-pg) | `cdylib` (`raster_h3_pg.so` with `--features pg-extension`) | [PostgreSQL](postgresql.md) |
| `raster-h3-cli` | [`crates/raster-h3-cli`](../../crates/raster-h3-cli) | `raster-h3-cli` executable (also built as `raster-h3` from the root crate) | [CLI](cli.md) |
| `raster_h3` | [`.`](../../) | `cdylib` + `rlib` facade | — |

The root `raster_h3` crate re-exports each member under a short name
(`raster_h3::core`, `::cli`, `::duckdb`, `::datafusion`, `::python`, `::pg`) and
glob-re-exports `raster_h3_duckdb::*`, so existing `raster_h3::...` paths and the DuckDB
extension entry points (`raster_h3_init`, `raster_h3_init_c_api`, `raster_h3_version`)
keep working. `cargo build --release` at the root still produces
`target/release/libraster_h3.{so,dylib,dll}`, the loadable DuckDB extension.

## Engine Capability Matrix

| Capability | DuckDB | DataFusion | Python / Polars | PostgreSQL | CLI |
| :--- | :---: | :---: | :---: | :---: | :---: |
| Continuous aggregation | ✅ | ✅ | ✅ | ✅ | ✅ |
| Categorical aggregation (wide) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Categorical aggregation (long) | ✅ | ✅ | ✅ | — | ✅ |
| Multiple resolutions per scan | ✅ | ✅ | ✅ | — (one `h3_res`) | ✅ |
| CRS override / NoData override / bbox | ✅ | ✅ | ✅ | ✅ | ✅ |
| Sub-pixel sampling, band, spectral formulas, quantiles | ✅ | ✅ (builder) | — | — | — |
| Category remapping, `min_majority_fraction` | ✅ | ✅ | — | — | — |
| Column projection pushdown | ✅ | ✅ | ✅ | — | ✅ (`--columns`) |
| Row-limit pushdown | — (DuckDB stops pulling) | ✅ | ✅ | — (SRF stops on `LIMIT`) | ✅ (`--limit`) |
| Filter pushdown | named params | `FilterPredicate` | — (applied per batch) | — | — |
| Parquet / GeoParquet export | ✅ | — | ✅ | — | ✅ |
| PMTiles v3 export | ✅ | — | ✅ | — | ✅ |
| Parquet → PMTiles transcoding | ✅ | — | — | — | — |
| H3 scalar helpers | ✅ | ✅ (vectorized) | ✅ | ✅ | — |
| Output transport | DuckDB `DataChunk` | `RecordBatch` / `ArrowArrayStream` | Arrow PyCapsule | heap tuples (value-per-call SRF) | `RH3IPC01` / NDJSON / files |

Mosaics (globs, comma-separated lists, VRT) are accepted by every adapter through
`resolve_raster_sources`. DuckDB exposes the `overlap_rule` choice; DataFusion exposes it via
`with_overlap_rule`; Python, PostgreSQL and the CLI always use `cutline`.

## Shared Semantics

### Output schemas

All engines derive their columns from `raster_h3_core::columnar::ColumnSchema`:

| Schema | Columns |
| :--- | :--- |
| `ColumnSchema::continuous(quantiles)` | `h3_index` u64, `h3_hex` utf8, `mean`, `stddev` (nullable), `count`, `min`, `max`, `sum` f64, `resolution` u8, `wkb` binary, then one nullable f64 per quantile target |
| `ColumnSchema::categorical_wide()` | `h3_index`, `h3_hex`, `majority_class` i64, `majority_fraction`, `majority_count`, `unique_classes` i64, `total_count`, `histogram` (JSON text), `resolution`, `shannon_entropy`, `entropy`, `distinct_classes` i64, `wkb` |
| `ColumnSchema::categorical_long()` | `h3_index`, `h3_hex`, `category` i64, `count`, `fraction`, `total_count`, `resolution`, `shannon_entropy`, `entropy`, `distinct_classes`, `unique_classes`, `wkb` |

The DuckDB table functions keep their own historical column sets (documented in the
[API Reference](../api-reference.md)); PostgreSQL returns a subset (see
[PostgreSQL](postgresql.md#output-columns)).

### H3 index representation

| Engine | Type of `h3_index` |
| :--- | :--- |
| DuckDB | `UBIGINT` |
| DataFusion / Arrow / Python / CLI | `UInt64` (Arrow format `L`) |
| PostgreSQL | `bigint` — the same 64 bits reinterpreted as signed, matching the `h3-pg` convention. Valid H3 cells have the high bit clear, so values are positive. |

### Scalar helpers

The H3 helpers live once in [`raster_h3_core::scalar_ops`](../../crates/raster-h3-core/src/scalar_ops.rs)
and each adapter wraps them. Core behaviour on invalid input, with the adapter
differences that remain:

| Function | Core behaviour on invalid input | Adapter differences |
| :--- | :--- | :--- |
| `h3_to_string(cell)` | `None` for `0` | DuckDB formats any `UBIGINT`, so `0` becomes `'0'` |
| `string_to_h3(hex)` | `0` | PostgreSQL returns `NULL` |
| `h3_to_lat(cell)`, `h3_to_lng(cell)` | `NaN` | — |
| `h3_get_resolution(cell)` | `-1` | — |
| `h3_is_valid(cell \| hex)` | `false` | — |
| `h3_to_wkb(cell \| hex)` (aliases `h3_to_geometry`, `h3_cell_to_geometry`) | `None` | PostgreSQL has the `bigint` overload only |
| `h3_cell_to_parent(cell, res)` | `None` when `res` is outside `0..=15` or finer than the cell | DuckDB returns `0` |

## Cross-Engine Parity Testing

For the full test inventory, see [Test Coverage](../testing.md).

| Test | What it checks | How to run |
| :--- | :--- | :--- |
| [`tests/test_multi_engine_e2e.rs`](../../tests/test_multi_engine_e2e.rs) | Generates synthetic continuous and categorical GeoTIFFs and compares per-cell results from the core batch readers and Arrow C stream, the CLI (subprocess: `ipc`, `jsonl`, `parquet`, `pmtiles`), DataFusion (scan, pushdown, scalar UDFs), Python (a `python3` subprocess loading `libraster_h3_python`), the PostgreSQL SRFs and fmgr scalars in-process, and DuckDB when a `duckdb` binary is on `PATH`. | `cargo test --test test_multi_engine_e2e` |
| [`tests/test_pg_extension_e2e.rs`](../../tests/test_pg_extension_e2e.rs) | Builds the real PostgreSQL module in Docker, installs it into `postgres:<major>` containers and compares `psql` output to the in-process SRFs. | `cargo test --test test_pg_extension_e2e -- --ignored --nocapture` |
| [`crates/raster-h3-python/tests/wheel_smoke.py`](../../crates/raster-h3-python/tests/wheel_smoke.py) | Installs a built wheel and exercises the Python API and the bundled `raster-h3-cli`. | Run by `cibuildwheel` in [`wheels.yml`](../../.github/workflows/wheels.yml) |

## Choosing an Engine

| You want… | Use |
| :--- | :--- |
| SQL joins against other tables, Parquet/PMTiles export from SQL | [DuckDB](duckdb.md) |
| A Rust query engine or service that consumes Arrow batches | [DataFusion](datafusion.md) or the [Core Engine](core.md) directly |
| Notebooks, Polars `LazyFrame`s, PyArrow, GeoPandas/Lonboard | [Python & Polars](python-polars.md) |
| Rasters aggregated inside an existing PostgreSQL / PostGIS database | [PostgreSQL](postgresql.md) |
| Shell pipelines, ClickHouse executable table functions, batch jobs | [CLI](cli.md) |
