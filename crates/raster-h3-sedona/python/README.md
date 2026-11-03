# raster-h3-sedona (Python)

`raster_h3` rasters as [SedonaDB](https://github.com/apache/sedona-db) tables.
`RasterH3Table` implements SedonaDB's `__sedonadb_table_provider__` protocol, so
`create_data_frame` scans it directly, with projection, `LIMIT`, threshold and
spatial filter pushdown.

```python
import sedonadb
from raster_h3_sedona import RasterH3Table

sd = sedonadb.connect()
sd.create_data_frame(RasterH3Table.continuous("dem.tif", [7, 8])).to_view("dem")
sd.sql("""
    SELECT h3_index, mean
    FROM dem
    WHERE count >= 4
      AND ST_Intersects(wkb, ST_SetSRID(ST_GeomFromWKT('POINT (-122.42 37.77)'), 4326))
""").show()
```

- `RasterH3Table.continuous(source, resolutions=8, *, crs=None, nodata=None, band=None, bbox=None)`
- `RasterH3Table.categorical(source, resolutions=8, *, crs=None, nodata=None, bbox=None)`

The capsule carries SedonaDB's C extension ABI. This build targets `sedonadb` 0.5.0.

## Build and test

```bash
cd crates/raster-h3-sedona/python
python -m venv .venv && . .venv/bin/activate
pip install maturin
maturin develop --release --extras test
pytest
```
