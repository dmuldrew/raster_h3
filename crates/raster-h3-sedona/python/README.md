# raster-h3-sedona (Python)

`raster_h3` rasters as [SedonaDB](https://github.com/apache/sedona-db) tables.
`RasterH3Table` implements SedonaDB's `__sedonadb_table_provider__` protocol, so
`create_data_frame` scans it directly, with projection, `LIMIT`, threshold and
spatial filter pushdown. `scalar_functions()` exports the raster_h3 SQL functions
(`h3_polygon_to_cells`, `h3_to_wkb`, `h3_cell_to_parent`, ...) as native SedonaDB kernels.
`write_pmtiles()` writes query results (or any Arrow stream) as a PMTiles v3 archive of
H3 hexagons.

```python
import sedonadb
import raster_h3_sedona
from raster_h3_sedona import RasterH3Table

sd = sedonadb.connect()
sd.register(raster_h3_sedona.scalar_functions())
sd.create_data_frame(RasterH3Table.continuous("dem.tif", [7, 8])).to_view("dem")
sd.sql("""
    SELECT h3_index, mean
    FROM dem
    WHERE count >= 4
      AND ST_Intersects(wkb, ST_SetSRID(ST_GeomFromWKT('POINT (-122.42 37.77)'), 4326))
""").show()

# Polyfill a polygon and join on the shared H3 index
sd.sql("""
    SELECT d.h3_index, d.mean
    FROM (SELECT unnest(h3_polygon_to_cells(
              ST_SetSRID(ST_GeomFromWKT('POLYGON ((-122.45 37.75, -122.40 37.75, -122.40 37.80, -122.45 37.75))'), 4326),
              8, 'covers')) AS h3_index) AS aoi
    JOIN dem AS d ON d.h3_index = aoi.h3_index
""").show()

# Tile any query: one hexagon feature per row, the other columns as properties
raster_h3_sedona.write_pmtiles(sd.sql("SELECT h3_index, mean FROM dem WHERE count >= 4"), "dem.pmtiles")
```

- `RasterH3Table.continuous(source, resolutions=8, *, crs=None, nodata=None, band=None, bbox=None)`
- `RasterH3Table.categorical(source, resolutions=8, *, crs=None, nodata=None, bbox=None)`
- `scalar_functions()`: every function under its name and aliases; pass it to
  `sd.register`, or iterate and register single `ScalarFunction`s
- `write_pmtiles(data, path, *, h3_column=None, min_zoom=None, max_zoom=None, memory_budget_mb=None)`:
  `data` is anything exposing `__arrow_c_stream__` (SedonaDB DataFrame, PyArrow table, Polars
  DataFrame). The H3 column is `h3_column`, else `h3_index` / `h3_hex` / `h3` / `cell` / `hex`,
  else the first column (unsigned or signed 64-bit, or hex strings); null cells are skipped and
  invalid ones raise `ValueError`. Binary columns such as `wkb` are not written as properties.
  Returns the number of features written. In Rust and DataFusion SQL the same writer is
  `COPY (query) TO 'cells.pmtiles'`.

The capsule carries SedonaDB's C extension ABI. This build targets `sedonadb` 0.5.0.

## Build and test

```bash
cd crates/raster-h3-sedona/python
python -m venv .venv && . .venv/bin/activate
pip install maturin
maturin develop --release --extras test
pytest
```
