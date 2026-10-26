# raster-h3

Aggregate GeoTIFF / Cloud-Optimized GeoTIFF rasters into [H3](https://h3geo.org) hexagonal
cells, streamed as Arrow and Polars data. A Rust engine does the work; this package bundles
it for Python along with the `raster-h3-cli` command-line tool.

```bash
pip install "raster-h3[polars]"
```

## Python / Polars

```python
import polars as pl
import raster_h3

# Lazy Polars scan with projection, predicate and row-limit pushdown
lf = raster_h3.scan_raster_h3("dem.tif", resolutions=[7, 8])
lf.filter(pl.col("count") > 10).select("h3_index", "mean").collect()

# Any Arrow PyCapsule consumer works too (pyarrow, DuckDB, ...)
stream = raster_h3.RasterH3ArrowStream("landcover.tif", 8, categorical=True)
for batch in stream.iter_batches(batch_size=50_000):
    ...

# H3 helpers and direct exports
raster_h3.h3_to_string(0x8828308281FFFFF)          # '8828308281fffff'
raster_h3.raster_to_parquet("dem.tif", "dem.parquet", resolutions=8)
raster_h3.raster_to_pmtiles("dem.tif", "dem.pmtiles", resolutions=8)
```

## Command line

```bash
raster-h3-cli jsonl dem.tif -r 8 --columns h3_index,mean
raster-h3-cli parquet dem.tif -r 7,8 -o dem.parquet
```

Run `raster-h3-cli --help` for all options.

## Wheels

Each wheel is `py3-none-<platform>` (any Python 3.9+): manylinux_2_28 x86_64 and aarch64,
macOS arm64 and x86_64, and Windows x86_64. Building from source requires a Rust toolchain.

Source and the DuckDB, DataFusion and PostgreSQL integrations:
<https://github.com/dmuldrew/raster_h3>
