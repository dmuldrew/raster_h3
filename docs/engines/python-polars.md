# Python & Polars Deep Dive: `raster-h3-python`

[← Engines Overview](README.md) · [← Back to README](../../README.md)

`raster-h3-python` ([`crates/raster-h3-python`](../../crates/raster-h3-python)) ships the
`raster-h3` Python package: a native library with a C ABI, a pure-Python wrapper
([`python/raster_h3/__init__.py`](../../crates/raster-h3-python/python/raster_h3/__init__.py))
that loads it with `ctypes`, and the bundled `raster-h3-cli` executable.

Because the library never links against `libpython`, one wheel per platform
(`py3-none-<platform>`) serves every Python ≥ 3.9. Polars and PyArrow are optional.

## Install

The package is not on PyPI yet. Install a wheel from the
[Python Wheels workflow](../../.github/workflows/wheels.yml) artifacts, or build from a
checkout (needs a Rust toolchain):

```bash
pip install "./crates/raster-h3-python[polars]"   # builds via the hatch hook; extras: polars, pyarrow
# or, for development against target/:
cargo build --release -p raster-h3-python
```

The wrapper finds the native library in this order: `RASTER_H3_PYTHON_LIB` (if the file
exists), the package directory, then the workspace `target/release` and `target/debug`.

## API

```python
import polars as pl
import raster_h3

# Lazy Polars scan
lf = raster_h3.scan_raster_h3("dem.tif", resolutions=[7, 8], crs="EPSG:4326")
df = lf.filter(pl.col("count") > 10).select("h3_index", "mean").collect()

# Eager
df = raster_h3.read_raster_h3("landcover.tif", 8, categorical=True, format="long")

# Arrow PyCapsule producer
stream = raster_h3.RasterH3ArrowStream("dem.tif", 8, columns=["h3_index", "mean"])
table = stream.to_pyarrow()                 # pyarrow.table(stream)
for batch in stream.iter_batches(batch_size=50_000):
    ...                                     # Polars DataFrames

# Exports
raster_h3.raster_to_parquet("dem.tif", "dem.parquet", resolutions="7,8")
raster_h3.raster_to_pmtiles("dem.tif", "dem.pmtiles", resolutions=[6, 7, 8])
```

### Scan parameters

`RasterH3ArrowStream`, `scan_raster_h3` and `read_raster_h3` share these arguments:

| Argument | Type | Default | Meaning |
| :--- | :--- | :--- | :--- |
| `path` | `str \| Path` | — | Raster path, glob, comma-separated list or VRT |
| `resolutions` | `int \| str \| Sequence[int]` | `8` | `8`, `"7,8"` or `[7, 8]`; parsed leniently (invalid entries dropped) |
| `categorical` | `bool` | `False` | Categorical instead of continuous aggregation |
| `format` | `str` | `"wide"` | Categorical layout: `"wide"` or `"long"` |
| `crs` | `str` | `None` | Source CRS override, e.g. `"EPSG:32610"` |
| `nodata` | `float` | `None` | NoData override |
| `bbox` | `str \| Sequence[float]` | `None` | `min_lon,min_lat,max_lon,max_lat` |
| `columns` | `Sequence[str]` | `None` | Columns to produce (projection pushed into the engine) |
| `limit` | `int` | `None` | Maximum rows |

Columns are the canonical core schemas (see the [overview](README.md#output-schemas)).
Mosaic overlaps always use the `cutline` rule. Sampling, band selection, spectral formulas,
quantiles and remapping are not exposed in Python; use DuckDB or the Rust APIs for those.

### `RasterH3ArrowStream`

A lazy description of a scan. Nothing runs until a method is called, and each call starts
a new scan.

| Method | Behaviour |
| :--- | :--- |
| `__arrow_c_schema__()` | `"arrow_schema"` PyCapsule for the (projected) output schema |
| `__arrow_c_stream__(requested_schema=None)` | `"arrow_array_stream"` PyCapsule; batches are produced as the consumer pulls them |
| `to_pyarrow()` | `pyarrow.table(self)` |
| `to_polars()` | Polars `DataFrame` through the PyCapsule stream; scan errors propagate |
| `iter_batches(batch_size=None)` | Generator of Polars `DataFrame`s of up to `batch_size` rows (default 2048), one native batch reader for the whole iteration |
| `to_dict()` | `dict[str, list]` via a JSON round trip; needs neither Polars nor PyArrow |

Any consumer of the [Arrow PyCapsule interface](https://arrow.apache.org/docs/format/CDataInterface/PyCapsuleInterface.html)
can take the stream directly, e.g. `polars.from_arrow(stream)`, `pyarrow.table(stream)` or
`duckdb.from_arrow(stream)`.

### Polars IO source

When Polars with `polars.io.plugins.register_io_source` is importable, `scan_raster_h3`
returns a `LazyFrame` backed by an IO source; otherwise it returns the
`RasterH3ArrowStream`.

| Polars hint | Handling |
| :--- | :--- |
| `with_columns` (projection) | Pushed into the engine as `columns` |
| `n_rows` (slice) | Pushed into the engine as `limit` when there is no predicate; with a predicate it caps the rows that survive the filter instead |
| `predicate` | Evaluated by Polars on each batch inside the source (not pushed into aggregation) |
| `batch_size` | Passed to `iter_batches` |

The declared `LazyFrame` schema has no quantile columns, which matches the Python API
(quantiles cannot be requested from Python).

`read_raster_h3(path, resolutions, **kwargs)` is `RasterH3ArrowStream(...).to_polars()`,
falling back to the stream object if Polars is missing.

### Scalar helpers

`h3_to_string`, `string_to_h3`, `h3_to_lat`, `h3_to_lng`, `h3_get_resolution`,
`h3_is_valid(int | str)`, `h3_to_wkb(int | str) -> bytes | None` and
`h3_cell_to_parent(int | str, res)` and
`h3_polygon_to_cells(wkb: bytes, resolution, containment="contains_centroid") -> list[int] | None`
call the shared core helpers (behaviour on invalid input is listed in the
[overview](README.md#scalar-helpers); containment modes are described in the
[API reference](../api-reference.md#containment-modes)). `h3_polygon_to_cells` takes WKB,
e.g. `shapely.to_wkb(geom)`; it raises `ValueError` for an unknown mode or a polygon over the
10,000,000-cell limit. They operate on single Python
values; for whole columns use the Arrow stream or a Polars expression such as
`map_elements`.

### Exporters

| Function | Arguments | Returns |
| :--- | :--- | :--- |
| `raster_to_parquet(input, output, resolutions=8, *, categorical=False, crs=None, geoparquet=True)` | Parquet or GeoParquet 1.1 | Number of cells written |
| `raster_to_pmtiles(input, output, resolutions=8, *, categorical=False, crs=None)` | PMTiles v3 archive | Count reported by the tiler |

Errors raise `RuntimeError` with the engine's message.

## Recipes

These mirror the [DuckDB SQL recipes](duckdb.md#sql-usage--recipes) for the parameters the
Python API exposes. Every scan below returns a `LazyFrame` when Polars is installed.

### Basic continuous aggregation

```python
import polars as pl
import raster_h3

df = raster_h3.read_raster_h3("elevation.tif", 8)
df.select("h3_index", "h3_hex", "mean", "count", "min", "max", "sum")
```

### CRS, NoData and bounding box

```python
lf = raster_h3.scan_raster_h3(
    "temperature_global.tif",
    9,
    crs="EPSG:4326",
    nodata=-9999.0,
    bbox=(-122.50, 37.70, -122.35, 37.85),   # min_lon, min_lat, max_lon, max_lat
)
top = (
    lf.select("h3_hex", pl.col("mean").round(2).alias("avg_temp_c"), "count")
      .sort("count", descending=True)
      .collect()
)
```

### Multiple resolutions in one pass

```python
lf = raster_h3.scan_raster_h3("elevation.tif", [6, 7, 8])
per_res = lf.group_by("resolution").agg(pl.len().alias("cells"), pl.col("mean").mean()).collect()
res8 = lf.filter(pl.col("resolution") == 8).collect()
```

### Helper scalar functions

The helpers take single values; apply them to a column with `map_elements`:

```python
df = raster_h3.read_raster_h3("elevation.tif", 8, columns=["h3_index", "mean"])
df = df.with_columns(
    pl.col("h3_index").map_elements(raster_h3.h3_to_lat, return_dtype=pl.Float64).alias("center_lat"),
    pl.col("h3_index").map_elements(raster_h3.h3_to_lng, return_dtype=pl.Float64).alias("center_lng"),
    pl.col("h3_index").map_elements(lambda c: raster_h3.h3_cell_to_parent(c, 6),
                                    return_dtype=pl.UInt64).alias("parent"),
)
raster_h3.string_to_h3("8828308281fffff")   # 613196570331971583
```

### Joins

`h3_index` is `UInt64`, so it joins directly with any other H3-indexed Polars frame:

```python
population = pl.scan_parquet("population_h3.parquet")   # h3_index: UInt64
joined = (
    raster_h3.scan_raster_h3("california_elevation.tif", 8, columns=["h3_index", "h3_hex", "mean"])
    .join(population, on="h3_index")
    .filter(pl.col("mean") > 500.0)
    .select("h3_hex", pl.col("mean").alias("avg_elevation"), "total_population", "median_income")
    .collect()
)
```

See the README's [worked example](../../README.md#raster-meets-table-a-worked-example) for a
full raster × census-tract join in Polars.

### Export to Parquet / GeoParquet

```python
# Streaming exporter: WKB geometry + GeoParquet 1.1 metadata
raster_h3.raster_to_parquet("elevation.tif", "elevation_h3.parquet", 8)

# Or write a projected frame with Polars itself
(raster_h3.scan_raster_h3("elevation.tif", 8, columns=["h3_index", "mean", "count"])
    .sink_parquet("elevation_h3_slim.parquet"))
```

### Remote COG & S3 streaming

```python
lf = raster_h3.scan_raster_h3("https://example.com/rasters/cog_elevation.tif", 8)
lf = raster_h3.scan_raster_h3("s3://my-geospatial-bucket/landsat/scene_01.tif", 9)
```

### Multi-file mosaics

Globs, comma-separated lists and VRTs are accepted; overlaps use the `cutline` rule.

```python
lf = raster_h3.scan_raster_h3("tiles/tile_*.tif", 8)
lf = raster_h3.scan_raster_h3("tile_west.tif,tile_east.tif", 8, categorical=True)
```

### Categorical aggregation

```python
# Wide: majority class and dominance
wide = raster_h3.read_raster_h3("worldcover_2021.tif", 8, categorical=True)
wide.select(
    "h3_hex", "majority_class",
    (pl.col("majority_fraction") * 100).round(1).alias("dominance_pct"),
    "unique_classes", "total_count", "histogram",
)

# The histogram is JSON text keyed by class code
wide.with_columns(
    pl.col("histogram").str.json_path_match("$['10']").cast(pl.Float64).alias("forest_fraction"),
    pl.col("histogram").str.json_path_match("$['50']").cast(pl.Float64).alias("urban_fraction"),
).filter(pl.col("urban_fraction").is_not_null())

# Long: one row per (cell, class)
urban = (
    raster_h3.scan_raster_h3("worldcover_2021.tif", 8, categorical=True, format="long")
    .filter((pl.col("category") == 50) & (pl.col("fraction") >= 0.25))
    .sort("fraction", descending=True)
    .collect()
)
```

### PMTiles v3 export

```python
raster_h3.raster_to_pmtiles("california_elevation.tif", "california_elevation.pmtiles", [6, 7, 8])
raster_h3.raster_to_pmtiles("worldcover_2021.tif", "worldcover.pmtiles", [5, 6, 7, 8], categorical=True)
```

### PyArrow, DuckDB and GeoPandas interop

```python
import duckdb
import geopandas as gpd

stream = raster_h3.RasterH3ArrowStream("elevation.tif", 8)

tbl = stream.to_pyarrow()                                  # pyarrow.Table
duckdb.sql("SELECT avg(mean) FROM tbl").show()            # DuckDB reads the Arrow table

df = stream.to_polars()
gdf = gpd.GeoDataFrame(
    df.drop("wkb").to_pandas(),
    geometry=gpd.GeoSeries.from_wkb(df["wkb"].to_list()),
    crs="EPSG:4326",
)
```

## Troubleshooting

| Symptom | Cause / fix |
| :--- | :--- |
| `RuntimeError: Could not locate libraster_h3_python shared library` on first call | The native library is not next to the package. Install a wheel, run `cargo build --release -p raster-h3-python` in the workspace, or set `RASTER_H3_PYTHON_LIB` to the library path. |
| `scan_raster_h3` returns a `RasterH3ArrowStream`, not a `LazyFrame` | Polars is not installed or is too old to have `polars.io.plugins.register_io_source`. Install the `polars` extra. |
| `RuntimeError: ... No CRS detected ...` | Pass `crs="EPSG:…"`. See the README's [Troubleshooting](../../README.md#troubleshooting--common-pitfalls). |
| Need sampling, bands, spectral formulas, quantiles or remapping | Not exposed in Python. Use [DuckDB](duckdb.md) (for example via `duckdb` from Python) or the Rust [DataFusion](datafusion.md) / [Core](core.md) APIs. |

## Native C ABI

The wrapper calls these exports from `libraster_h3_python`
([`src/lib.rs`](../../crates/raster-h3-python/src/lib.rs)). All entry points catch panics and
return errors through an out-parameter string freed with `raster_h3_py_free_string`.

| Export | Purpose |
| :--- | :--- |
| `raster_h3_py_create_arrow_schema` / `_free_arrow_schema` | Build / release an `ArrowSchema` |
| `raster_h3_py_create_arrow_stream` / `_free_arrow_stream` | Build / release an `ArrowArrayStream` for a scan |
| `raster_h3_py_open_batch_reader` / `_batch_reader_next` / `_free_batch_reader` | Incremental reader; each `next` returns a one-batch stream, or null at EOF |
| `raster_h3_py_scan_json` | Whole scan as a JSON object of columns (`to_dict`) |
| `raster_h3_py_h3_*` | Scalar helpers |
| `raster_h3_py_export_parquet`, `raster_h3_py_export_pmtiles` | File exporters (return `-1` on error) |

PyCapsule destructors free the native struct. A consumer that imports the stream moves it
out and clears `release`; otherwise dropping the struct calls `release`, so an unconsumed
capsule does not leak the scan.

## Wheels

[`pyproject.toml`](../../crates/raster-h3-python/pyproject.toml) uses `hatchling` with a
custom hook ([`hatch_build.py`](../../crates/raster-h3-python/hatch_build.py)) that:

1. Runs `cargo build --release --locked -p raster-h3-python -p raster-h3-cli` against the
   workspace two directories up (override with `RASTER_H3_WORKSPACE`), honouring
   `CARGO_BUILD_TARGET` or a single-arch macOS `ARCHFLAGS`.
2. Places `libraster_h3_python.{so,dylib}` / `raster_h3_python.dll` inside the `raster_h3`
   package and `raster-h3-cli` in the wheel's scripts directory (so it lands on `PATH`).
3. Tags the wheel `py3-none-<platform>`.

[`.github/workflows/wheels.yml`](../../.github/workflows/wheels.yml) runs `cibuildwheel` for
manylinux_2_28 x86_64 and aarch64, macOS arm64 and x86_64 (cross-compiled, tested under
Rosetta), and Windows x86_64, then installs each wheel and runs
[`tests/wheel_smoke.py`](../../crates/raster-h3-python/tests/wheel_smoke.py). Wheels are
uploaded as workflow artifacts; nothing is published to PyPI yet.

## Testing

```bash
cargo test -p raster-h3-python                      # Rust-side unit tests
cargo test -p raster_h3 --test it multi_engine_e2e           # spawns python3 against the built cdylib
```
