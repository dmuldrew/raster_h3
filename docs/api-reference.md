# API Reference

[← Back to README](../README.md)

The API reference is split by interface:

| Reference | Covers |
| :--- | :--- |
| [SQL API Reference (DuckDB)](sql-api-reference.md) | `raster_h3` DuckDB extension: table functions, Parquet / PMTiles exports, scalar SQL functions |
| [Polars API Reference](polars-api-reference.md) | `raster_h3` Python package: `scan_raster_h3` / `read_raster_h3`, `RasterH3ArrowStream`, Python H3 helpers, exporters |

The other engines (PostgreSQL, DataFusion, CLI, Rust core) are documented in their
[engine deep dives](engines/README.md).

## Moved sections

This page used to hold the DuckDB SQL reference. Links to its sections land here;
follow the matching row to the section's new location.

| Old anchor | New location |
| :--- | :--- |
| <a id="continuous-rasters-h3_raster_continuous_aggregatefile_path-resolution-"></a>`#continuous-rasters-h3_raster_continuous_aggregatefile_path-resolution-` | [Continuous Rasters: `h3_raster_continuous_aggregate(file_path, [resolution], ...)`](sql-api-reference.md#continuous-rasters-h3_raster_continuous_aggregatefile_path-resolution-) |
| <a id="positional-parameters"></a>`#positional-parameters` | [Positional Parameters](sql-api-reference.md#positional-parameters) |
| <a id="named-parameters"></a>`#named-parameters` | [Named Parameters](sql-api-reference.md#named-parameters) |
| <a id="output-schema"></a>`#output-schema` | [Output Schema](sql-api-reference.md#output-schema) |
| <a id="aggregation-statistics-mathematical-definitions--geospatial-use-cases"></a>`#aggregation-statistics-mathematical-definitions--geospatial-use-cases` | [Aggregation Statistics: Mathematical Definitions & Geospatial Use Cases](sql-api-reference.md#aggregation-statistics-mathematical-definitions--geospatial-use-cases) |
| <a id="cell-geometry"></a>`#cell-geometry` | [Cell geometry](sql-api-reference.md#cell-geometry) |
| <a id="band-calibration-and-validity"></a>`#band-calibration-and-validity` | [Band calibration and validity](sql-api-reference.md#band-calibration-and-validity) |
| <a id="categorical-rasters-h3_raster_categorical_aggregatefile_path-resolution-"></a>`#categorical-rasters-h3_raster_categorical_aggregatefile_path-resolution-` | [Categorical Rasters: `h3_raster_categorical_aggregate(file_path, [resolution], ...)`](sql-api-reference.md#categorical-rasters-h3_raster_categorical_aggregatefile_path-resolution-) |
| <a id="named-parameters-1"></a>`#named-parameters-1` | [Named Parameters](sql-api-reference.md#named-parameters-1) |
| <a id="wide-format-output-schema-format--wide-default"></a>`#wide-format-output-schema-format--wide-default` | [Wide Format Output Schema (`format := 'wide'`, Default)](sql-api-reference.md#wide-format-output-schema-format--wide-default) |
| <a id="long-format-output-schema-format--long"></a>`#long-format-output-schema-format--long` | [Long Format Output Schema (`format := 'long'`)](sql-api-reference.md#long-format-output-schema-format--long) |
| <a id="direct-parquet-export-h3_raster_to_parquetfile_path-output_parquet-"></a>`#direct-parquet-export-h3_raster_to_parquetfile_path-output_parquet-` | [Direct Parquet Export: `h3_raster_to_parquet(file_path, output_parquet, ...)`](sql-api-reference.md#direct-parquet-export-h3_raster_to_parquetfile_path-output_parquet-) |
| <a id="positional-parameters-1"></a>`#positional-parameters-1` | [Positional Parameters](sql-api-reference.md#positional-parameters-1) |
| <a id="named-parameters-2"></a>`#named-parameters-2` | [Named Parameters](sql-api-reference.md#named-parameters-2) |
| <a id="output-schema-1-summary-row"></a>`#output-schema-1-summary-row` | [Output Schema (1 Summary Row)](sql-api-reference.md#output-schema-1-summary-row) |
| <a id="pmtiles-v3-export-h3_raster_to_pmtilesfile_path-output_pmtiles-"></a>`#pmtiles-v3-export-h3_raster_to_pmtilesfile_path-output_pmtiles-` | [PMTiles v3 Export: `h3_raster_to_pmtiles(file_path, output_pmtiles, ...)`](sql-api-reference.md#pmtiles-v3-export-h3_raster_to_pmtilesfile_path-output_pmtiles-) |
| <a id="positional-parameters-2"></a>`#positional-parameters-2` | [Positional Parameters](sql-api-reference.md#positional-parameters-2) |
| <a id="named-parameters-3"></a>`#named-parameters-3` | [Named Parameters](sql-api-reference.md#named-parameters-3) |
| <a id="output-schema-1-summary-row-1"></a>`#output-schema-1-summary-row-1` | [Output Schema (1 Summary Row)](sql-api-reference.md#output-schema-1-summary-row-1) |
| <a id="parquet-pmtiles-v3-export-h3_parquet_to_pmtilesparquet_path-output_pmtiles-"></a>`#parquet-pmtiles-v3-export-h3_parquet_to_pmtilesparquet_path-output_pmtiles-` | [Parquet PMTiles v3 Export: `h3_parquet_to_pmtiles(parquet_path, output_pmtiles, ...)`](sql-api-reference.md#parquet-pmtiles-v3-export-h3_parquet_to_pmtilesparquet_path-output_pmtiles-) |
| <a id="positional-parameters-3"></a>`#positional-parameters-3` | [Positional Parameters](sql-api-reference.md#positional-parameters-3) |
| <a id="named-parameters-4"></a>`#named-parameters-4` | [Named Parameters](sql-api-reference.md#named-parameters-4) |
| <a id="output-schema-1-summary-row-2"></a>`#output-schema-1-summary-row-2` | [Output Schema (1 Summary Row)](sql-api-reference.md#output-schema-1-summary-row-2) |
| <a id="scalar-helper-functions"></a>`#scalar-helper-functions` | [Scalar Helper Functions](sql-api-reference.md#scalar-helper-functions) |
| <a id="h3_polygon_to_cells"></a>`#h3_polygon_to_cells` | [`h3_polygon_to_cells`](sql-api-reference.md#h3_polygon_to_cells) |
| <a id="containment-modes"></a>`#containment-modes` | [Containment modes](sql-api-reference.md#containment-modes) |
| <a id="nulls-invalid-input-and-limits"></a>`#nulls-invalid-input-and-limits` | [NULLs, invalid input and limits](sql-api-reference.md#nulls-invalid-input-and-limits) |
| <a id="examples"></a>`#examples` | [Examples](sql-api-reference.md#examples) |
