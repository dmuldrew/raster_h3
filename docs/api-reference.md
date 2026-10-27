# API Reference

[← Back to README](../README.md)

The API reference is split by interface:

| Reference | Covers |
| :--- | :--- |
| [SQL API Reference (DuckDB)](sql-api-reference.md) | `raster_h3` DuckDB extension: table functions, Parquet / PMTiles exports, scalar SQL functions |
| [Polars API Reference](polars-api-reference.md) | `raster_h3` Python package: `scan_raster_h3` / `read_raster_h3`, `RasterH3ArrowStream`, Python H3 helpers, exporters |

The other engines (PostgreSQL, DataFusion, CLI, Rust core) are documented in their
[engine deep dives](engines/README.md).
