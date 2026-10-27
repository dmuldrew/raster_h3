# Polars API Reference

[← Back to README](../README.md) · [← SQL API Reference](sql-api-reference.md)

This document is the complete reference for the `raster_h3` Python package
([Python & Polars deep dive](engines/python-polars.md)): the Polars IO source
(`scan_raster_h3`, `read_raster_h3`), the Arrow PyCapsule stream (`RasterH3ArrowStream`),
the direct Parquet and PMTiles v3 exporters, and the H3 helper functions. For the DuckDB
extension, see the [SQL API Reference](sql-api-reference.md).

Statistic definitions, [cell geometry](sql-api-reference.md#cell-geometry) and
[band calibration](sql-api-reference.md#band-calibration-and-validity) are shared by every
engine and documented once in the SQL reference.

```python
import polars as pl
import raster_h3

lf = raster_h3.scan_raster_h3("elevation.tif", 8)          # pl.LazyFrame
df = lf.filter(pl.col("count") > 10).select("h3_index", "mean").collect()
```

Polars and PyArrow are optional dependencies. Without Polars, the scan functions return a
[`RasterH3ArrowStream`](#rasterh3arrowstream) that any Arrow PyCapsule consumer can read.

## Function summary

| Function | Returns | Purpose |
| :--- | :--- | :--- |
| [`scan_raster_h3(path, resolutions=8, **opts)`](#scan_raster_h3) | `pl.LazyFrame` | Lazy scan with projection and row-limit pushdown |
| [`read_raster_h3(path, resolutions=8, **opts)`](#read_raster_h3) | `pl.DataFrame` | Eager scan |
| [`RasterH3ArrowStream(path, resolutions=8, **opts)`](#rasterh3arrowstream) | stream object | Arrow PyCapsule producer, batch iterator, PyArrow / dict conversion |
| [`raster_to_parquet(input_path, output_path, resolutions=8, ...)`](#raster_to_parquet) | `int` | Stream a raster straight to Parquet / GeoParquet 1.1 |
| [`raster_to_pmtiles(input_path, output_path, resolutions=8, ...)`](#raster_to_pmtiles) | `int` | Stream a raster straight to a PMTiles v3 vector archive |
| [H3 helpers](#h3-helper-functions) | scalars | `h3_to_string`, `string_to_h3`, `h3_to_lat`, `h3_to_lng`, `h3_get_resolution`, `h3_is_valid`, `h3_to_wkb`, `h3_cell_to_parent`, `h3_polygon_to_cells` |

---

## Scan options

`scan_raster_h3`, `read_raster_h3` and `RasterH3ArrowStream` take the same arguments.
`path` and `resolutions` may be positional; everything else is keyword-only.

### Positional parameters

| Parameter | Type | Required | Default | Description |
| :--- | :--- | :---: | :--- | :--- |
| `path` | `str \| pathlib.Path` | **Yes** | — | GeoTIFF / Cloud-Optimized GeoTIFF. Also accepts an `http(s)://` or `s3://` URL, a glob (`"tiles/*.tif"`), a comma-separated list of files, or a VRT; multiple files are read as one mosaic. |
| `resolutions` | `int \| str \| Sequence[int]` | No | `8` | One or more H3 resolutions (0 to 15), aggregated in a single pass: `8`, `"7,8"`, `"7 8"` or `[7, 8]`. An empty string or empty sequence means the default, `8`. |

### Keyword parameters

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `categorical` | `bool` | `False` | Aggregate class codes (majority, histogram, entropy) instead of continuous statistics. |
| `format` | `str` | `"wide"` | Categorical layout: `"wide"` (one row per cell) or `"long"` (one row per cell and class). Case-insensitive; an empty string means `"wide"`. Validated in both modes, so an invalid value raises even when `categorical=False`, but only used for categorical scans. |
| `crs` | `str \| None` | `None` (auto) | Source CRS, e.g. `"EPSG:4326"`, `"EPSG:32610"`. Required if the raster has no embedded CRS; otherwise overrides it. |
| `nodata` | `float \| None` | `None` (auto) | NoData sentinel to exclude, matched against stored values. Overrides the file's `GDAL_NODATA` tag. |
| `bbox` | `str \| Sequence[float] \| None` | `None` | Region of interest as `(min_lon, min_lat, max_lon, max_lat)` or `"min_lon,min_lat,max_lon,max_lat"`. Exactly four numbers. Chunks outside the box are not read. |
| `columns` | `Sequence[str] \| None` | `None` (all) | Output columns, in this order. Projection is pushed into the engine, so unrequested columns (for example `wkb`, `h3_hex`) are never built. Names must come from the [output schema](#output-schemas) for the chosen mode. |
| `limit` | `int \| None` | `None` | Maximum rows to produce. |

### Fixed behaviour

The Python API always uses these settings. Use the [SQL API](sql-api-reference.md) (for
example through the `duckdb` Python package) or the Rust APIs when you need to change them.

| Setting | Python behaviour | SQL equivalent |
| :--- | :--- | :--- |
| Band | Band 1 | `band` |
| Sub-pixel sampling | `'center'` (one sample per pixel) | `sampling` |
| Mosaic overlaps | `'cutline'` | `overlap_rule` |
| Scale / offset | GDAL `SCALE`/`OFFSET` always applied to continuous values | `scale_offset` |
| Spectral indices | Not available | `formula`, `nir_band`, `red_band`, ... |
| Quantiles | Not available | `quantiles` / `percentiles` |
| Category remapping | Not available | `remap` |
| Output filters | Use Polars `filter` instead | `min_count`, `min_mean`, `max_mean`, `min_majority_fraction` |
| Single-cell pushdown | Use `bbox` or a Polars filter instead | `h3_cell`, `h3_hex` |
| Compaction | Not available | `compact` |
| Geometry column | `wkb` (`Binary`) only | `geom := true` (`GEOMETRY`) |
| Threads, decode memory | Engine defaults | `workers`, `decode_workers`, `fetch_workers`, `decode_budget_mb` |

### Validation

Invalid options raise `RuntimeError` with the engine's message; they are never silently
ignored. The only defaulting is for empty values: `resolutions=""` or `[]` means `8`, and
`format=""` means `"wide"`.

| Input | Message (prefixed `Invalid parameter:`) |
| :--- | :--- |
| A resolution outside 0–15, or an unparsable `resolutions` string | `Invalid resolutions '16': expected H3 resolutions between 0 and 15, e.g. 7,8` |
| `format` other than `wide` / `long` | `Invalid format 'tall': expected 'wide' or 'long'` |
| `bbox` without exactly four numbers | `Invalid bbox '1,2,3': expected four numbers min_lon,min_lat,max_lon,max_lat` |
| Unknown name in `columns` | `Column 'nope' not found in raster_h3 schema` |
| Missing file | `Input file path does not exist: nope.tif` |

`read_raster_h3` and the `RasterH3ArrowStream` methods raise these directly.
`scan_raster_h3` is lazy, so they surface when the frame is collected, wrapped by Polars in
a `polars.exceptions.ComputeError`.

---

## `scan_raster_h3`

```python
scan_raster_h3(
    path, resolutions=8, *,
    categorical=False, format="wide", crs=None, nodata=None,
    bbox=None, columns=None, limit=None,
) -> pl.LazyFrame | RasterH3ArrowStream
```

Returns a Polars `LazyFrame` backed by an IO source
(`polars.io.plugins.register_io_source`). The schema is known without reading the raster,
so `collect_schema()` is free. Nothing is read until the frame is collected, sunk or
streamed, and each collection runs a new scan.

If Polars is not installed, or is too old to provide `register_io_source`, it returns the
[`RasterH3ArrowStream`](#rasterh3arrowstream) for the same arguments instead.

### Query optimisation

| Polars hint | Handling |
| :--- | :--- |
| Projection (`select`, column pruning) | Pushed into the engine as `columns`; only the selected columns are built. |
| Slice (`head`, `limit`, `n_rows`) with no filter | Pushed into the engine as `limit` (combined with your own `limit` by taking the smaller). |
| Slice after a filter | Applied after the filter, so it caps the rows that survive the filter. |
| Predicate (`filter`) | Evaluated by Polars on each batch inside the source as it streams. It does not prune raster reads; use `bbox` for that. |
| Batch size | Passed to [`iter_batches`](#methods). |

### Example

```python
lf = raster_h3.scan_raster_h3(
    "temperature.tif",
    [7, 8],
    crs="EPSG:4326",
    nodata=-9999.0,
    bbox=(-122.50, 37.70, -122.35, 37.85),
)
lf.collect_schema()   # no raster I/O

hot = (
    lf.filter((pl.col("resolution") == 8) & (pl.col("mean") > 30))
      .select("h3_index", "h3_hex", "mean", "count")
      .sort("mean", descending=True)
      .collect()
)
```

---

## `read_raster_h3`

```python
read_raster_h3(path, resolutions=8, **kwargs) -> pl.DataFrame | RasterH3ArrowStream
```

Eager form of `scan_raster_h3`, equivalent to
`RasterH3ArrowStream(path, resolutions, **kwargs).to_polars()`. `kwargs` are the
[keyword parameters](#keyword-parameters) above. Returns the `RasterH3ArrowStream` if Polars
is not installed.

```python
df = raster_h3.read_raster_h3("landcover.tif", 8, categorical=True, format="long")
```

---

## `RasterH3ArrowStream`

```python
RasterH3ArrowStream(
    path, resolutions=8, *,
    categorical=False, format="wide", crs=None, nodata=None,
    bbox=None, columns=None, limit=None,
)
```

A lazy description of a scan that implements the
[Arrow PyCapsule interface](https://arrow.apache.org/docs/format/CDataInterface/PyCapsuleInterface.html).
Construction does no I/O and no validation. Every method call starts a new scan, so one
object can be consumed several times.

### Methods

| Method | Returns | Behaviour |
| :--- | :--- | :--- |
| `__arrow_c_schema__()` | `PyCapsule` (`"arrow_schema"`) | Output schema after `columns` projection. Does not open the raster. |
| `__arrow_c_stream__(requested_schema=None)` | `PyCapsule` (`"arrow_array_stream"`) | Streaming record batches, produced as the consumer pulls them. `requested_schema` is ignored. |
| `to_polars()` | `pl.DataFrame` | Reads the whole stream into Polars. Scan errors propagate. |
| `to_pyarrow()` | `pyarrow.Table` | `pyarrow.table(self)`. Requires PyArrow. |
| `iter_batches(batch_size=None)` | `Iterator[pl.DataFrame]` | Yields DataFrames of up to `batch_size` rows (default 2048) from one native reader, so memory stays bounded by the batch. |
| `to_dict()` | `dict[str, list]` | Whole result as column lists, keys in alphabetical order. Needs neither Polars nor PyArrow; intended for small results. |

The constructor arguments are kept as attributes of the same names (`path` as `str`,
`columns` as `list`).

### Consumers

Any PyCapsule-aware library can read the stream directly:

```python
stream = raster_h3.RasterH3ArrowStream("dem.tif", 8, columns=["h3_index", "mean", "wkb"])

pl.from_arrow(stream)          # Polars
pa.table(stream)               # PyArrow
duckdb.from_arrow(stream)      # DuckDB relation

for batch in stream.iter_batches(batch_size=50_000):
    process(batch)             # pl.DataFrame
```

---

## Output schemas

Column order matches the tables below unless `columns` sets another order. Every column is
non-null except `stddev`. `h3_index` is `UInt64`, so it joins directly with other
H3-indexed frames and with DuckDB `UBIGINT`.

### Continuous (`categorical=False`)

| Column | Polars dtype | Nullable | Description |
| :--- | :--- | :---: | :--- |
| `h3_index` | `UInt64` | | 64-bit H3 cell index. |
| `h3_hex` | `String` | | 15-character lowercase hex form, e.g. `"8828308281fffff"`. |
| `mean` | `Float64` | | Weighted mean of pixel values in the cell. |
| `stddev` | `Float64` | ✓ | Single-pass Welford sample standard deviation; `0.0` for one sample, null if it cannot be computed. |
| `count` | `Float64` | | Weighted number of pixels contributing to the cell. |
| `min` | `Float64` | | Minimum pixel value in the cell. |
| `max` | `Float64` | | Maximum pixel value in the cell. |
| `sum` | `Float64` | | Sum of weighted pixel values. |
| `resolution` | `UInt8` | | H3 resolution (0 to 15) of the cell. |
| `wkb` | `Binary` | | OGC 2D WKB cell geometry in WGS84; see [cell geometry](sql-api-reference.md#cell-geometry). |

See [Aggregation statistics](sql-api-reference.md#aggregation-statistics-mathematical-definitions--geospatial-use-cases)
for the formulas and when to use each statistic.

### Categorical, wide (`categorical=True`, `format="wide"`)

| Column | Polars dtype | Description |
| :--- | :--- | :--- |
| `h3_index` | `UInt64` | 64-bit H3 cell index. |
| `h3_hex` | `String` | Lowercase hex cell index. |
| `majority_class` | `Int64` | Most frequent class code in the cell. |
| `majority_fraction` | `Float64` | Share (0.0 to 1.0) of the cell held by the majority class. |
| `majority_count` | `Float64` | Weighted pixel count of the majority class. |
| `unique_classes` | `Int64` | Number of distinct classes in the cell. |
| `total_count` | `Float64` | Weighted count of all non-NoData pixels in the cell. |
| `histogram` | `String` | JSON object `{"class": fraction, ...}`. Read one class with `pl.col("histogram").str.json_path_match("$['10']")`. |
| `resolution` | `UInt8` | H3 resolution of the cell. |
| `shannon_entropy` | `Float64` | Shannon diversity, $-\sum p_i \ln p_i$. |
| `entropy` | `Float64` | Same value as `shannon_entropy`. |
| `distinct_classes` | `Int64` | Same value as `unique_classes`. |
| `wkb` | `Binary` | OGC 2D WKB cell geometry. |

### Categorical, long (`categorical=True`, `format="long"`)

| Column | Polars dtype | Description |
| :--- | :--- | :--- |
| `h3_index` | `UInt64` | 64-bit H3 cell index. |
| `h3_hex` | `String` | Lowercase hex cell index. |
| `category` | `Int64` | Class code present in the cell. |
| `count` | `Float64` | Weighted pixel count of this class in the cell. |
| `fraction` | `Float64` | Share (0.0 to 1.0) of the cell held by this class. |
| `total_count` | `Float64` | Weighted count of all non-NoData pixels in the cell. |
| `resolution` | `UInt8` | H3 resolution of the cell. |
| `shannon_entropy` | `Float64` | Shannon diversity of the whole cell (repeated on each of its rows). |
| `entropy` | `Float64` | Same value as `shannon_entropy`. |
| `distinct_classes` | `Int64` | Number of distinct classes in the whole cell. |
| `unique_classes` | `Int64` | Same value as `distinct_classes`. |
| `wkb` | `Binary` | OGC 2D WKB cell geometry. |

### Differences from the SQL schemas

- No `geom` column; convert `wkb` with `shapely.from_wkb`, `geopandas.GeoSeries.from_wkb`
  or DuckDB's `ST_GeomFromWKB`.
- No quantile columns, since quantiles cannot be requested from Python.
- Integer types use Polars names: `UBIGINT` → `UInt64`, `BIGINT` → `Int64`,
  `UTINYINT` → `UInt8`.

---

## `raster_to_parquet`

```python
raster_to_parquet(
    input_path, output_path, resolutions=8, *,
    categorical=False, crs=None, geoparquet=True,
) -> int
```

Streams a raster straight to a Parquet file without building a DataFrame, so memory does
not grow with the number of cells. Returns the number of rows written (all resolutions
together).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `input_path` | `str \| Path` | — | Raster path, URL, glob, comma-separated list or VRT, as for [`path`](#positional-parameters). |
| `output_path` | `str \| Path` | — | Destination `.parquet` file; overwritten if it exists. |
| `resolutions` | `int \| str \| Sequence[int]` | `8` | One or more H3 resolutions, written to the same file. |
| `categorical` | `bool` | `False` | Write categorical instead of continuous columns. |
| `crs` | `str \| None` | `None` (auto) | Source CRS override. |
| `geoparquet` | `bool` | `True` | Add a `geometry` WKB column and OGC GeoParquet 1.1 `geo` metadata (CRS `OGC:CRS84`). |

The file uses Snappy compression and the compact exporter layout, whose column names differ
from the scan schema:

| Mode | Columns |
| :--- | :--- |
| Continuous | `h3_index` (Int64), `geometry`\*, `min_value`, `max_value`, `sum_value`, `avg_value`, `pixel_count` |
| Categorical | `h3_index` (Int64), `geometry`\*, `majority`, `majority_fraction`, `pixel_count`, `distinct_classes`, `entropy` |

\* Only when `geoparquet=True`.

`h3_index` is stored as signed `Int64` (the same 64 bits); cast it to `UInt64` before
joining to scan output.

> **Reading the file back with Polars.** Polars' native Parquet reader (checked with
> Polars 1.36) rejects files written by the bundled `parquet` 60 crate (this exporter and the CLI) with
> `File out of specification: Invalid thrift`, while PyArrow reads them. Until that is
> fixed, read through PyArrow:
>
> ```python
> cells = (pl.read_parquet("elevation_h3.parquet", use_pyarrow=True)
>            .with_columns(pl.col("h3_index").cast(pl.UInt64)))
> ```

To write the scan schema itself, or a projection of it, use Polars, which writes files its
own reader accepts:

```python
(raster_h3.scan_raster_h3("elevation.tif", 8, columns=["h3_index", "mean", "count"])
    .sink_parquet("elevation_h3_slim.parquet"))
```

---

## `raster_to_pmtiles`

```python
raster_to_pmtiles(
    input_path, output_path, resolutions=8, *,
    categorical=False, crs=None,
) -> int
```

Builds a single-file PMTiles v3 vector archive of H3 cells. See [PMTiles export](pmtiles.md)
for how H3 resolutions map to map zoom levels. Returns the number of cells packaged across
all resolutions.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `input_path` | `str \| Path` | — | Raster path, URL, glob, comma-separated list or VRT. |
| `output_path` | `str \| Path` | — | Destination `.pmtiles` file. |
| `resolutions` | `int \| str \| Sequence[int]` | `8` | H3 resolutions to include, e.g. `[5, 6, 7, 8]`. |
| `categorical` | `bool` | `False` | Encode categorical instead of continuous properties. |
| `crs` | `str \| None` | `None` (auto) | Source CRS override. |

### Exporter errors and warnings

Both exporters raise `RuntimeError` with the engine's message on failure (missing input,
invalid `resolutions`, unknown CRS, write errors). If valid pixels were dropped because
their coordinate transform failed or fell outside the valid latitude/longitude range, they
still write the file and emit a `RuntimeWarning`:

```
raster_h3: N valid sample(s) were dropped because their coordinate transform failed or
fell outside the valid latitude/longitude range; check crs
```

This usually means `crs` is wrong; see
[transform-failure diagnostics](engines/core.md#transform-failure-diagnostics). Turn it into
an error with `warnings.simplefilter("error", RuntimeWarning)`.

---

## H3 helper functions

The helpers take and return single Python values and share their implementation with every
other engine. Invalid input returns a sentinel rather than raising.

| Function | Signature | Returns | On invalid input |
| :--- | :--- | :--- | :--- |
| `h3_to_string` | `(cell: int)` | `str \| None` | `None` for `0` |
| `string_to_h3` | `(hex_str: str)` | `int` | `0` |
| `h3_to_lat` | `(cell: int)` | `float` | `nan` |
| `h3_to_lng` | `(cell: int)` | `float` | `nan` |
| `h3_get_resolution` | `(cell: int)` | `int` | `-1` |
| `h3_is_valid` | `(cell_or_hex: int \| str)` | `bool` | `False` |
| `h3_to_wkb` | `(cell_or_hex: int \| str)` | `bytes \| None` | `None` |
| `h3_cell_to_parent` | `(cell_or_hex: int \| str, parent_res: int)` | `int \| str \| None` (same kind as the input) | `None`, also when `parent_res` is finer than the cell |
| `h3_polygon_to_cells` | `(wkb: bytes, resolution: int, containment: str = "contains_centroid")` | `list[int] \| None` | See [below](#h3_polygon_to_cells) |

- `h3_to_lat` / `h3_to_lng` give the cell centroid in WGS84 degrees.
- `h3_is_valid` checks the mode, base cell (0 to 121), resolution, direction digits and
  unused-digit padding.
- `h3_to_wkb` returns 125 bytes for a hexagon `Polygon`, a `MultiPolygon` for cells crossing
  the antimeridian and a polar-cap `Polygon` for the two pole cells; see
  [cell geometry](sql-api-reference.md#cell-geometry).

```python
cell = raster_h3.string_to_h3("8828308281fffff")   # 613196570331971583
raster_h3.h3_to_string(cell)                        # '8828308281fffff'
raster_h3.h3_get_resolution(cell)                   # 8
raster_h3.h3_cell_to_parent(cell, 6)                # 604189371209351167
raster_h3.h3_cell_to_parent("8828308281fffff", 6)   # '86283082fffffff'
raster_h3.h3_is_valid("not a cell")                 # False
```

### Applying helpers to columns

The helpers are not Polars expressions. Apply them to a column with `map_elements`, or
avoid them where the scan already provides the value (`h3_hex`, `resolution`, `wkb`):

```python
df = raster_h3.read_raster_h3("elevation.tif", 8, columns=["h3_index", "mean"])
df = df.with_columns(
    pl.col("h3_index").map_elements(raster_h3.h3_to_lat, return_dtype=pl.Float64).alias("lat"),
    pl.col("h3_index").map_elements(raster_h3.h3_to_lng, return_dtype=pl.Float64).alias("lng"),
    pl.col("h3_index")
      .map_elements(lambda c: raster_h3.h3_cell_to_parent(c, 6), return_dtype=pl.UInt64)
      .alias("parent_6"),
)
```

### `h3_polygon_to_cells`

```python
h3_polygon_to_cells(wkb: bytes, resolution: int, containment: str = "contains_centroid")
    -> list[int] | None
```

Returns every H3 cell at `resolution` that belongs to the polygon, sorted ascending with
duplicates removed. It wraps h3o's
[`ToCells`](https://docs.rs/h3o/0.6/h3o/geom/trait.ToCells.html) polyfill.

| Parameter | Type | Description |
| :--- | :--- | :--- |
| `wkb` | `bytes` | A `POLYGON` or `MULTIPOLYGON` in WGS84 longitude/latitude, as ISO/OGC WKB (2D, Z, M, ZM) or PostGIS EWKB in either byte order, e.g. `shapely.to_wkb(geom)`. Z/M values are ignored and the SRID is not checked, so reproject to EPSG:4326 first. Holes are respected. |
| `resolution` | `int` | H3 resolution `0` (coarsest) to `15` (finest). |
| `containment` | `str` | Which cells belong to the polygon (below). Case-insensitive; `-` and `_` are interchangeable. |

| Mode | Alias | A cell is returned when… |
| :--- | :--- | :--- |
| `contains_centroid` (default) | `center` | its centroid lies inside the polygon |
| `contains_boundary` | `full` | its whole boundary lies inside the polygon |
| `intersects_boundary` | — | any part of it overlaps the polygon |
| `covers` | `overlapping` | same as `intersects_boundary`, including polygons smaller than a cell |

The [SQL reference](sql-api-reference.md#containment-modes) compares the modes' coverage and
how they share cells between adjacent polygons.

| Input | Result |
| :--- | :--- |
| `wkb` is not a valid polygonal WKB (e.g. a `POINT`), or has non-finite coordinates | `None` |
| `resolution` outside `0..=15` | `None` |
| An empty polygon | `[]` |
| Unknown `containment` | `ValueError: unknown containment mode 'bogus'` |
| More than 10,000,000 cells estimated | `ValueError`; use a coarser resolution or split the polygon |

```python
import shapely

aoi = shapely.box(-122.47, 37.73, -122.37, 37.81)
cells = raster_h3.h3_polygon_to_cells(shapely.to_wkb(aoi), 8)

# Keep only the raster cells inside the area of interest
inside = (
    raster_h3.scan_raster_h3("elevation.tif", 8, bbox=aoi.bounds)
    .filter(pl.col("h3_index").is_in(cells))
    .collect()
)

# One row per (region, cell) from a GeoDataFrame
regions = pl.DataFrame({
    "name": gdf["name"],
    "h3_index": [raster_h3.h3_polygon_to_cells(shapely.to_wkb(g), 7, "covers") for g in gdf.geometry],
}).explode("h3_index").with_columns(pl.col("h3_index").cast(pl.UInt64))
```

---

## Errors at a glance

| Exception | Raised by | Cause |
| :--- | :--- | :--- |
| `RuntimeError` | `read_raster_h3`, `RasterH3ArrowStream` methods, exporters | Invalid option, missing file, unreadable raster, missing or unsupported CRS (`No CRS detected`, `Unsupported EPSG code`), write failure |
| `polars.exceptions.ComputeError` | `LazyFrame.collect()` / `sink_*` on a `scan_raster_h3` frame | The same `RuntimeError`, raised inside the Polars IO source |
| `ValueError` | `h3_polygon_to_cells` | Unknown containment mode, or too many cells |
| `RuntimeWarning` | `raster_to_parquet`, `raster_to_pmtiles` | Valid pixels dropped by failed coordinate transforms |
| `RuntimeError: Could not locate libraster_h3_python shared library` | First call of any function | Native library not found; see [Install](engines/python-polars.md#install) |
