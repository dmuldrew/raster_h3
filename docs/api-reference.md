# Complete API Reference

[← Back to README](../README.md)

This document provides the complete API reference for the DuckDB H3 Raster Hexification extension, including table functions for continuous and categorical raster aggregation, direct Parquet and PMTiles v3 vector export, and scalar utility functions.

## Continuous Rasters: `h3_raster_continuous_aggregate(file_path, [resolution], ...)`
*(Alias: `h3_raster_continuous`)*

### Positional Parameters
| Parameter | Type | Required | Default | Description |
| :--- | :--- | :---: | :--- | :--- |
| `file_path` | `VARCHAR` | **Yes** | — | Path to the GeoTIFF / Cloud-Optimized GeoTIFF file. |
| `resolution` | `BIGINT` | No | `8` | Target H3 grid resolution level (0 to 15). |

### Named Parameters
| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `resolution` | `BIGINT` | `8` | Target H3 grid resolution level (0 to 15). |
| `resolutions` | `VARCHAR` | `None` | Comma/space-delimited multiple H3 resolutions (e.g. `'7,8'` or `'7, 8, 9'`) in a single pass. |
| `min_resolution` | `BIGINT` | `None` | Minimum H3 resolution for multi-resolution pyramid range. |
| `max_resolution` | `BIGINT` | `None` | Maximum H3 resolution for multi-resolution pyramid range. |
| `band` | `BIGINT` | `1` | 1-indexed band to extract and aggregate. |
| `source_crs` | `VARCHAR` | `None` (auto) | Raster Coordinate Reference System (e.g. `'EPSG:4326'`, `'EPSG:3857'`, `'EPSG:32633'`). Required if raster lacks embedded CRS metadata; otherwise acts as override. Alias: `crs`. |
| `nodata` | `DOUBLE` | `None` (auto) | Custom NoData sentinel value to exclude from aggregations. |
| `chunk_size` | `BIGINT` | `512` | Strip/tile buffer window size in rows. |
| `sampling` | `VARCHAR` | `'center'` | Sub-pixel super-sampling preset (`'center'`, `'rgss'`, `'hex'`, `'gaussian'`, `'5point'`, `'8rooks'`, `'9point'`, `'16point'`). |
| `min_lon`, `min_lat`, `max_lon`, `max_lat` | `DOUBLE` | `None` | Region of Interest (ROI) bounding box coordinates for chunk pruning. |
| `bbox` | `VARCHAR` | `None` | Bounding box as a single string: `'min_lon,min_lat,max_lon,max_lat'` (alternative to individual coordinate parameters). |
| `h3_cell` | `BIGINT` | `None` | Single H3 cell index for predicate pushdown (only process chunks intersecting this cell). |
| `h3_hex` | `VARCHAR` | `None` | Single H3 hex string for predicate pushdown (alternative to `h3_cell`). |
| `compact` | `BOOLEAN` | `false` | Compact output format (omits `h3_hex` VARCHAR column for reduced memory). |
| `overlap_rule` | `VARCHAR` | `'cutline'` | Mosaic tile overlap resolution: `'cutline'` (Voronoi bisector), `'first'` (painter's precedence), `'average'` (blend). |
| `workers` / `threads` | `BIGINT` | `auto` | Number of background decompression worker threads. |
| `formula` | `VARCHAR` | `None` | Spectral index formula: `'ndvi'`, `'ndwi'`, `'nbr'`, `'evi'`. Requires multi-band raster. |
| `nir_band`, `red_band`, `green_band`, `blue_band`, `swir_band` | `BIGINT` | `auto` | 1-indexed band assignments for spectral index formulas. |
| `min_count` | `DOUBLE` | `None` | Minimum weighted pixel count threshold — cells below this are excluded from output. |
| `min_mean` | `DOUBLE` | `None` | Minimum mean value filter — cells with mean below this are excluded. |
| `max_mean` | `DOUBLE` | `None` | Maximum mean value filter — cells with mean above this are excluded. |
| `geom` | `BOOLEAN` | `false` | Emit a `geometry` column with 125-byte OGC WKB 2D Polygon hexagons (enables DuckDB Spatial interop). |
| `quantiles` | `VARCHAR` | `None` | Streaming quantile targets: `'p50,p90,p99'`, `'iqr'`, `'deciles'`, `'quartiles'`. Adds percentile columns to output. |
| `percentiles` | `VARCHAR` | `None` | Alias for `quantiles`. |

### Output Schema
| Column Name | Logical Type | Description |
| :--- | :--- | :--- |
| `h3_index` | `UBIGINT` | Native 64-bit unsigned integer H3 cell index (fast for joins). |
| `h3_hex` | `VARCHAR` | 15/16-character lowercase hexadecimal representation (e.g. `'8828308281fffff'`). |
| `mean` | `DOUBLE` | Weighted arithmetic mean of pixel values in the cell. |
| `stddev` | `DOUBLE` | Single-pass Welford sample standard deviation of pixel values. |
| `count` | `DOUBLE` | Weighted count of pixels contributing to the cell. |
| `min` | `DOUBLE` | Minimum pixel value observed within the cell. |
| `max` | `DOUBLE` | Maximum pixel value observed within the cell. |
| `sum` | `DOUBLE` | Sum of all weighted pixel values in the cell. |
| `resolution` | `UTINYINT` | H3 resolution level (0 to 15) of the cell. |
| `wkb` | `BLOB` | 125-byte OGC standard 2D Polygon Well-Known Binary (WKB) representation. |
| `geom` | `GEOMETRY` | Native DuckDB Spatial Polygon geometry (emitted when `geom := true`). |

---

## Aggregation Statistics: Mathematical Definitions & Geospatial Use Cases

| Statistic | Mathematical Formula | Geospatial Analytics Use Case | Why & When to Use |
| :--- | :--- | :--- | :--- |
| **`mean`** | sum(w_i * x_i) / sum(w_i) | **Continuous Surfaces**: Average elevation, mean surface temperature, average NDVI / vegetation health. | Primary metric for summarizing continuous physical phenomena across a geographic area. |
| **`stddev`** | sqrt(M2 / (sum(w_i) - 1)) | **Spatial Heterogeneity & Terrain Ruggedness**: Terrain roughness (TRI), micro-climate variability, canopy height variation. | Quantifies internal cell diversity. High `stddev` in a DEM indicates steep terrain; low `stddev` indicates flat plains. |
| **`count`** | sum(w_i) | **Coverage Completeness & QC**: Area weighting verification, boundary completeness, filtering out clipped edge cells. | In single-point sampling, returns integer count of pixels in cell. In super-sampling, returns fractional area coverage. |
| **`min`** | min(x_i) | **Extreme Lows**: Valley floor elevation, minimum winter temperature, lowest water table level. | Evaluated via branchless hardware `minsd`/`fminnm` instructions with zero branch penalties. |
| **`max`** | max(x_i) | **Extreme Peaks**: Mountain ridge summits, peak heatwave index, maximum building height. | Evaluated via branchless hardware `maxsd`/`fmaxnm` instructions. |
| **`sum`** | sum(w_i * x_i) | **Cumulative Physical Quantities**: Total precipitation volume, solar radiation flux, biomass carbon stock. | Used whenever raster pixel values represent density or rate per unit area that integrates across the hexagon. |

---

## Categorical Rasters: `h3_raster_categorical_aggregate(file_path, [resolution], ...)`
*(Alias: `h3_raster_categorical`)*

### Named Parameters
| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `resolution` | `BIGINT` | `8` | Target H3 grid resolution level (0 to 15). |
| `resolutions` | `VARCHAR` | `None` | Comma/space-delimited multiple H3 resolutions (e.g. `'7,8'` or `'7, 8, 9'`) in a single pass. |
| `min_resolution` | `BIGINT` | `None` | Minimum H3 resolution for multi-resolution pyramid range. |
| `max_resolution` | `BIGINT` | `None` | Maximum H3 resolution for multi-resolution pyramid range. |
| `format` | `VARCHAR` | `'wide'` | Output layout: `'wide'` (majority + histogram) or `'long'` (normalized rows). |
| `band` | `BIGINT` | `1` | 1-indexed band to extract and aggregate. |
| `source_crs` | `VARCHAR` | `None` (auto) | Raster Coordinate Reference System (e.g. `'EPSG:4326'`, `'EPSG:3857'`). Required if raster lacks embedded CRS metadata; otherwise acts as override. Alias: `crs`. |
| `nodata` | `DOUBLE` | `None` (auto) | Custom NoData sentinel value. |
| `chunk_size` | `BIGINT` | `512` | Strip/tile buffer window size in rows. |
| `sampling` | `VARCHAR` | `'center'` | Sub-pixel super-sampling preset (`'center'`, `'rgss'`, `'hex'`, etc.). |
| `min_lon`, `min_lat`, `max_lon`, `max_lat` | `DOUBLE` | `None` | Bounding box coordinates for spatial Region of Interest (ROI) chunk pruning. |
| `bbox` | `VARCHAR` | `None` | Bounding box as a single string: `'min_lon,min_lat,max_lon,max_lat'`. |
| `h3_cell` | `BIGINT` | `None` | Single H3 cell index for predicate pushdown. |
| `h3_hex` | `VARCHAR` | `None` | Single H3 hex string for predicate pushdown. |
| `compact` | `BOOLEAN` | `false` | Compact output format (omits `h3_hex` VARCHAR column). |
| `overlap_rule` | `VARCHAR` | `'cutline'` | Mosaic tile overlap resolution: `'cutline'`, `'first'`, `'average'`. |
| `workers` / `threads` | `BIGINT` | `auto` | Number of background decompression worker threads. |
| `min_count` | `DOUBLE` | `None` | Minimum weighted pixel count threshold for output. |
| `min_majority_fraction` | `DOUBLE` | `None` | Minimum majority class fraction — cells below this threshold are excluded. |
| `remap` | `VARCHAR` | `None` | Category remapping rules: exact (`'10=Forest,20=Urban'`), range (`'20-29=Urban'`), wildcard (`'*=Other'`). |
| `geom` | `BOOLEAN` | `false` | Emit a `geometry` column with OGC WKB 2D Polygon hexagons. |

### Wide Format Output Schema (`format := 'wide'`, Default)
| Column Name | Logical Type | Description |
| :--- | :--- | :--- |
| `h3_index` | `UBIGINT` | Native 64-bit unsigned integer H3 cell index. |
| `h3_hex` | `VARCHAR` | 15/16-character lowercase hexadecimal representation. |
| `majority_class` | `BIGINT` | Most frequent category ID in the hexagon. |
| `majority_fraction` | `DOUBLE` | Fraction (0.0 to 1.0) of the hexagon occupied by majority class. |
| `majority_count` | `DOUBLE` | Weighted pixel count of the majority category. |
| `unique_classes` | `BIGINT` | Number of distinct categories present in the hexagon (richness). |
| `total_count` | `DOUBLE` | Total non-nodata pixels in the hexagon. |
| `histogram` | `VARCHAR` | JSON map of `{category_id: fraction, ...}`. |
| `resolution` | `UTINYINT` | H3 resolution level (0 to 15) of the cell. |
| `shannon_entropy` | `DOUBLE` | Shannon-Wiener entropy index ($-\sum p_i \ln p_i$), measuring landscape diversity. |
| `entropy` | `DOUBLE` | Alias for `shannon_entropy`. |
| `distinct_classes`| `BIGINT` | Number of distinct categories present in the hexagon (alias for `unique_classes`). |
| `wkb` | `BLOB` | 125-byte OGC standard 2D Polygon Well-Known Binary (WKB) representation. |
| `geom` | `GEOMETRY` | Native DuckDB Spatial Polygon geometry (emitted when `geom := true`). |

### Long Format Output Schema (`format := 'long'`)
| Column Name | Logical Type | Description |
| :--- | :--- | :--- |
| `h3_index` | `UBIGINT` | Native 64-bit unsigned integer H3 cell index. |
| `h3_hex` | `VARCHAR` | 15/16-character lowercase hexadecimal representation. |
| `category` | `BIGINT` | Category ID present in this hexagon. |
| `count` | `DOUBLE` | Weighted pixel count for this category. |
| `fraction` | `DOUBLE` | Proportion (0.0 to 1.0) of this category in the hexagon. |
| `total_count` | `DOUBLE` | Total pixels in the hexagon across all categories. |
| `resolution` | `UTINYINT` | H3 resolution level (0 to 15) of the cell. |
| `shannon_entropy` | `DOUBLE` | Shannon-Wiener entropy index of the parent hexagon. |
| `entropy` | `DOUBLE` | Alias for `shannon_entropy`. |
| `distinct_classes`| `BIGINT` | Number of distinct categories in the parent hexagon. |
| `unique_classes`  | `BIGINT` | Alias for `distinct_classes`. |
| `wkb` | `BLOB` | 125-byte OGC standard 2D Polygon Well-Known Binary (WKB) representation. |
| `geom` | `GEOMETRY` | Native DuckDB Spatial Polygon geometry (emitted when `geom := true`). |

---

## Direct Parquet Export: `h3_raster_to_parquet(file_path, output_parquet, ...)`

Stream a GeoTIFF directly into a native Parquet file in a single command, with optional OGC GeoParquet 1.1 metadata.

```sql
SELECT * FROM h3_raster_to_parquet(
    'elevation.tif',
    'elevation_h3.parquet',
    resolution := 8,
    sampling := 'rgss',
    geoparquet := true,
    compression := 'zstd'
);
```

### Positional Parameters
| Parameter | Type | Required | Default | Description |
| :--- | :--- | :---: | :--- | :--- |
| `file_path` | `VARCHAR` | **Yes** | — | Input GeoTIFF / Cloud-Optimized GeoTIFF file path or URL. |
| `output_parquet` | `VARCHAR` | **Yes** | — | Destination path for the output `.parquet` file. |

### Named Parameters
| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `resolution` | `BIGINT` | `8` | Target H3 grid resolution level (0 to 15). |
| `sampling` | `VARCHAR` | `'center'` | Sub-pixel super-sampling preset. |
| `band` | `BIGINT` | `1` | 1-indexed band to extract and aggregate. |
| `nodata` | `DOUBLE` | `None` (auto) | Custom NoData sentinel value. |
| `categorical` | `BOOLEAN` | `false` | Whether to aggregate categorical raster classes instead of continuous stats. |
| `compact` | `BOOLEAN` | `false` | Compact output format (fewer columns). |
| `compression` | `VARCHAR` | `'snappy'` | Parquet compression codec: `'snappy'`, `'zstd'`, `'gzip'`, `'lz4'`, `'brotli'`, `'none'`. |
| `row_group_size` | `BIGINT` | `122880` | Maximum rows per Parquet row group. |
| `min_lon`, `min_lat`, `max_lon`, `max_lat` | `DOUBLE` | `None` | Spatial bounding box for chunk pruning. |
| `bbox` | `VARCHAR` | `None` | Bounding box as a single string: `'min_lon,min_lat,max_lon,max_lat'`. |
| `h3_cell` | `BIGINT` | `None` | Single H3 cell predicate pushdown filter. |
| `h3_hex` | `VARCHAR` | `None` | Single H3 hex string predicate pushdown filter. |
| `geoparquet` | `BOOLEAN` | `false` | Emit OGC GeoParquet 1.1 metadata in Parquet FileMetaData with PROJJSON `OGC:CRS84` datum. |
| `geom` | `BOOLEAN` | `false` | Emit a `geometry` column with 125-byte OGC WKB 2D Polygon hexagons. |
| `source_crs` / `crs` | `VARCHAR` | `None` (auto) | CRS override (e.g. `'EPSG:4326'`). Required if raster lacks embedded CRS. |

### Output Schema (1 Summary Row)
| Column Name | Logical Type | Description |
| :--- | :--- | :--- |
| `total_hexagons` | `BIGINT` | Total H3 hexagons written to the Parquet file. |
| `parquet_size_bytes` | `BIGINT` | File size of the generated `.parquet` file in bytes. |
| `elapsed_ms` | `DOUBLE` | Total end-to-end execution time in milliseconds. |
| `hexagons_per_sec` | `DOUBLE` | Throughput rate in hexagons processed per second. |
| `output_path` | `VARCHAR` | Path to the created `.parquet` file. |
| `status` | `VARCHAR` | Execution status (`'SUCCESS'` or error message). |

---

## PMTiles v3 Export: `h3_raster_to_pmtiles(file_path, output_pmtiles, ...)`

### Positional Parameters
| Parameter | Type | Required | Default | Description |
| :--- | :--- | :---: | :--- | :--- |
| `file_path` | `VARCHAR` | **Yes** | — | Input GeoTIFF file path. |
| `output_pmtiles` | `VARCHAR` | **Yes** | — | Destination path for the single-file `.pmtiles` archive. |

### Named Parameters
| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `resolution` | `BIGINT` | `8` | Target H3 resolution for single-zoom export. |
| `min_resolution` | `BIGINT` | `None` | Minimum H3 resolution for multi-zoom pyramid export. |
| `max_resolution` | `BIGINT` | `None` | Maximum H3 resolution for multi-zoom pyramid export. |
| `sampling` | `VARCHAR` | `'center'` | Sub-pixel super-sampling preset (`'center'`, `'rgss'`, `'hex'`, `'gaussian'`, etc.). |
| `band` | `BIGINT` | `1` | 1-indexed band to extract and aggregate. |
| `nodata` | `DOUBLE` | `None` (auto) | Custom NoData sentinel value. |
| `categorical` | `BOOLEAN` | `false` | Whether to aggregate categorical raster classes instead of continuous stats. |
| `properties` | `VARCHAR` | `None` (all) | Selective property whitelist (e.g. `'mean,count'` or `'majority,entropy'`). Reduces MVT protobuf encoding overhead and shrinks `.pmtiles` archive size by 15–30%. |

### Output Schema (1 Summary Row)
| Column Name | Logical Type | Description |
| :--- | :--- | :--- |
| `total_hexagons` | `BIGINT` | Total H3 hexagons aggregated and packaged across all zoom levels. |
| `pmtiles_size_bytes` | `BIGINT` | Total file size of the generated `.pmtiles` archive in bytes. |
| `min_zoom` | `BIGINT` | Minimum Web Mercator tile zoom level in the archive. |
| `max_zoom` | `BIGINT` | Maximum Web Mercator tile zoom level in the archive. |
| `elapsed_ms` | `DOUBLE` | Total end-to-end execution time in milliseconds. |
| `output_path` | `VARCHAR` | Path to the created `.pmtiles` file. |
| `status` | `VARCHAR` | Execution status (`'SUCCESS'` or error message). |

---

## Parquet PMTiles v3 Export: `h3_parquet_to_pmtiles(parquet_path, output_pmtiles, ...)`

Convert any H3-indexed Parquet dataset directly into an optimized PMTiles v3 vector archive from SQL.

```sql
SELECT * FROM h3_parquet_to_pmtiles(
    'census_h3.parquet',
    'census_h3.pmtiles',
    h3_column := 'h3_index'
);
```

### Positional Parameters
| Parameter | Type | Required | Default | Description |
| :--- | :--- | :---: | :--- | :--- |
| `parquet_path` | `VARCHAR` | **Yes** | — | Input Parquet file path containing H3 indices and properties. |
| `output_pmtiles` | `VARCHAR` | **Yes** | — | Destination path for the single-file `.pmtiles` archive. |

### Named Parameters
| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `h3_column` | `VARCHAR` | `None` (auto-detect) | Name of the H3 index column (supports BIGINT/UBIGINT integer or hex VARCHAR). Alias: `h3_col`. |

### Output Schema (1 Summary Row)
Returns the same 7-column summary schema as `h3_raster_to_pmtiles` (`total_hexagons`, `pmtiles_size_bytes`, `min_zoom`, `max_zoom`, `elapsed_ms`, `output_path`, `status`).

---

## Scalar Helper Functions

| Function | Signature | Return Type | Description |
| :--- | :--- | :--- | :--- |
| `h3_to_string` | `(UBIGINT)` | `VARCHAR` | Zero-allocation hexadecimal string formatter. |
| `string_to_h3` | `(VARCHAR)` | `UBIGINT` | Fast ASCII hexadecimal to 64-bit integer parser. |
| `h3_to_lat` | `(UBIGINT)` | `DOUBLE` | Centroid latitude in WGS84 decimal degrees. |
| `h3_to_lng` | `(UBIGINT)` | `DOUBLE` | Centroid longitude in WGS84 decimal degrees. |
| `h3_get_resolution` | `(UBIGINT)` | `BIGINT` | Single-cycle bitshift extraction of H3 resolution level (0 to 15). |
| `h3_is_valid` | `(UBIGINT)` / `(VARCHAR)` | `BOOLEAN` | Validates mode, base cell range (0 to 121), resolution (0 to 15), directional digits, and padding. |
| `h3_to_wkb` | `(UBIGINT)` / `(VARCHAR)` | `BLOB` | Emits 125-byte OGC standard 2D Polygon Well-Known Binary (WKB) representation for hexagons (109 bytes for pentagons). |
| `h3_to_geometry` | `(UBIGINT)` / `(VARCHAR)` | `GEOMETRY` | Converts H3 index directly to native DuckDB Spatial polygon geometry (compatible with `ST_Area`, `ST_Intersects`, etc.). |
| `h3_cell_to_geometry` | `(UBIGINT)` / `(VARCHAR)` | `GEOMETRY` | Standard DuckDB spatial compatibility alias for `h3_to_geometry`. |
| `h3_cell_to_parent` | `(UBIGINT, BIGINT)` / `(VARCHAR, BIGINT)` | `UBIGINT` / `VARCHAR` | Truncates H3 cell to coarser parent resolution level. Returns null if requested resolution is finer. |
| `raster_h3_version` | `()` | `VARCHAR` | Returns the compiled extension version string (e.g. `'0.2.0'`). |
