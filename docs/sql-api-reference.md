# SQL API Reference (DuckDB)

[← Back to README](../README.md) · [Polars API Reference →](polars-api-reference.md)

This document is the complete **SQL** reference for the `raster_h3` DuckDB extension ([DuckDB deep dive](engines/duckdb.md)), including table functions for continuous and categorical raster aggregation, direct Parquet and PMTiles v3 vector export, and scalar utility functions. For the Python package (Polars `LazyFrame`s, Arrow streams and Python helpers) see the [Polars API Reference](polars-api-reference.md).

The other engines accept a subset of these parameters; see their deep dives under [Engines](engines/README.md). Statistic definitions and output column meanings apply to every engine.

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
| `scale_offset` | `BOOLEAN` | `true` | Convert stored values to physical values with each band's GDAL `SCALE`/`OFFSET` before statistics and spectral indices. `false` aggregates raw stored values. See [Band calibration and validity](#band-calibration-and-validity). |
| `chunk_size` | `BIGINT` | `512` | Strip/tile buffer window size in rows. |
| `sampling` | `VARCHAR` | `'center'` | Sub-pixel super-sampling preset (`'center'`, `'rgss'`, `'hex'`, `'gaussian'`, `'5point'`, `'8rooks'`, `'9point'`, `'16point'`). |
| `min_lon`, `min_lat`, `max_lon`, `max_lat` | `DOUBLE` | `None` | Region of Interest (ROI) bounding box coordinates for chunk pruning. |
| `bbox` | `VARCHAR` | `None` | Bounding box as a single string: `'min_lon,min_lat,max_lon,max_lat'` (alternative to individual coordinate parameters). |
| `h3_cell` | `BIGINT` | `None` | Single H3 cell index for predicate pushdown (only process chunks intersecting this cell). |
| `h3_hex` | `VARCHAR` | `None` | Single H3 hex string for predicate pushdown (alternative to `h3_cell`). |
| `compact` | `BOOLEAN` | `false` | Compact output format (omits `h3_hex` VARCHAR) and merge complete sets of seven H3 children into a parent. Cannot be combined with adjacent requested resolutions, which would duplicate parent cells. |
| `overlap_rule` | `VARCHAR` | `'cutline'` | Mosaic tile overlap resolution: `'cutline'` (among tiles whose footprint contains the point, the one with the nearest centre pixel by great-circle distance), `'first'` (first tile in input order whose footprint contains the point; its nodata is not filled from later tiles), `'average'` (accumulate every observation, so denser or finer overlapping grids carry more weight). |
| `workers` / `threads` | `BIGINT` | ambient | Aggregation (kernel) threads. When set, the query runs kernel windows on a dedicated pool of this size, shared by concurrent queries that request the same size. When unset, windows use the caller's Rayon pool, capped at 8 per wave. |
| `decode_workers` | `BIGINT` | `4` | Chunk decompression threads. |
| `fetch_workers` | `BIGINT` | `8` | Concurrent HTTP range requests for remote tiles. |
| `decode_budget_mb` | `BIGINT` | `256` | Memory for decoded chunks held at once (decoding, queued, being aggregated, idle for reuse), in MiB. Decoding waits for room; a single chunk larger than the budget is decoded only when no other is held. |
| `compaction_budget_mb` | `BIGINT` | `None` | Opt-in limit on pending `compact` sibling groups, in MiB. Past it, the groups nearest completion are emitted as individual children. Samples and totals are unchanged, but which groups compact can vary between runs. Unset keeps compaction deterministic and its memory unbounded. |
| `formula` | `VARCHAR` | `None` | Spectral index formula: `'ndvi'`, `'ndwi'`, `'nbr'`, `'evi'`. Requires multi-band raster. |
| `nir_band`, `red_band`, `green_band`, `blue_band`, `swir_band` | `BIGINT` | `auto` | 1-indexed band assignments for spectral index formulas. |
| `min_count` | `DOUBLE` | `None` | Minimum weighted pixel count threshold — cells below this are excluded from output. |
| `min_mean` | `DOUBLE` | `None` | Minimum mean value filter — cells with mean below this are excluded. |
| `max_mean` | `DOUBLE` | `None` | Maximum mean value filter — cells with mean above this are excluded. |
| `geom` | `BOOLEAN` | `false` | Emit a `geometry` column of the cell geometries (enables DuckDB Spatial interop); see [cell geometry](#cell-geometry). |
| `quantiles` | `VARCHAR` | `None` | Streaming quantile targets: percentiles such as `'p50,p90,p99.5'` or `'0.05'`, aliases `median`/`q1`/`q2`/`q3`/`iqr`, or presets `'default'`, `'box'` (p25, p50, p75, iqr), `'tails'`, `'deciles'`. Percentiles must be finite and strictly between 0 and 100 (decimals between 0 and 1). Adds one nullable column per target; per-cell sketches are only built when the query selects or filters on one of these columns. |
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
| `wkb` | `BLOB` | OGC 2D Well-Known Binary (WKB) cell geometry; see [cell geometry](#cell-geometry). |
| `geom` | `GEOMETRY` | Native DuckDB Spatial Polygon geometry (emitted when `geom := true`). |

---

## Aggregation Statistics: Mathematical Definitions & Geospatial Use Cases

| Statistic | Mathematical Formula | Geospatial Analytics Use Case | Why & When to Use |
| :--- | :--- | :--- | :--- |
| **`mean`** | sum(w_i * x_i) / sum(w_i) | **Continuous Surfaces**: Average elevation, mean surface temperature, average NDVI / vegetation health. | Primary metric for summarizing continuous physical phenomena across a geographic area. |
| **`stddev`** | sqrt(M2 / (sum(w_i) - 1)) | **Spatial Heterogeneity & Terrain Ruggedness**: Terrain roughness (TRI), micro-climate variability, canopy height variation. | Quantifies internal cell diversity. High `stddev` in a DEM indicates steep terrain; low `stddev` indicates flat plains. |
| **`count`** | sum(w_i) | **Coverage Completeness & QC**: Area weighting verification, boundary completeness, filtering out clipped edge cells. | In single-point sampling, returns integer count of pixels in cell. In super-sampling, returns fractional area coverage. |
| **`min`** | min(x_i) | **Extreme Lows**: Valley floor elevation, minimum winter temperature, lowest water table level. | Use to detect extremes that a mean hides, such as a single deep sink or a localized cold pocket. |
| **`max`** | max(x_i) | **Extreme Peaks**: Mountain ridge summits, peak heatwave index, maximum building height. | Use to detect isolated peaks that a mean smooths away, such as a lone tower or a hot spot. |
| **`sum`** | sum(w_i * x_i) | **Cumulative Physical Quantities**: Total precipitation volume, solar radiation flux, biomass carbon stock. | Used whenever raster pixel values represent density or rate per unit area that integrates across the hexagon. |

### Cell geometry

Every engine writes H3 cells (`wkb`, `geom`, `h3_to_wkb`, GeoParquet `geometry`) as OGC
2D WKB in WGS84 longitude/latitude, valid for planar GIS tools:

- Ordinary cells are a `Polygon` of H3's 5–10 vertices plus the closing point (125 bytes
  for a hexagon, 109 for a pentagon, up to 189 at odd resolutions), counter-clockwise.
- A cell crossing the antimeridian is a `MultiPolygon` of its eastern and western parts,
  cut at ±180°. Longitudes always stay within [-180, 180].
- The two cells containing a pole are polar-cap `Polygon`s: the cell ring unrolled from
  -180° to 180° and closed along both meridians and ±90°.
- Edges are straight lines in longitude/latitude, approximating H3's great-circle edges;
  GeoParquet metadata declares `"edges": "planar"` and `geometry_types`
  `["Polygon", "MultiPolygon"]`. Its `bbox` is the extent of the cell geometries actually
  written (wider than the raster, since cells extend past its edges); it spans -180 to 180
  when a written cell crosses the antimeridian.

PMTiles vector tiles use the same rules in Web Mercator tile coordinates: every exterior ring
has positive area as MVT 2.1 requires, a cell crossing the antimeridian is drawn on the
tile's side of the seam (and on both sides in the z0 world tile), and the two pole cells are
polar caps closed along the map's top or bottom edge. Cells lying wholly beyond Web
Mercator's ±85.05° limit flatten onto that edge.

### Band calibration and validity

Every engine reads these from the GeoTIFF and applies them the same way:

- **Scale/offset.** GDAL stores per-band `SCALE` and `OFFSET` in the `GDAL_METADATA` tag
  (42112). Continuous statistics (`mean`, `min`, `max`, `stddev`, `sum`, quantiles) and
  spectral indices use the physical value `stored * scale + offset`, computed per band and,
  in a mosaic, per file. A negative scale swaps `min` and `max`. A scale that is zero or not
  finite, an offset that is not finite, or an item for a band the file does not have is an
  error when the file is opened. Non-numeric scale/offset values are also rejected.
  DuckDB's `scale_offset := false`, the CLI's `--raw-values`, and DataFusion's
  `with_scale_offset(false)` aggregate stored values instead, but do not bypass metadata
  validation: invalid calibration metadata is rejected in all modes, including categorical
  aggregation.
- **NoData.** The `GDAL_NODATA` tag and the `nodata` override are matched against
  *stored* values, before scaling, as GDAL does.
- **Alpha.** A sample declared as associated or unassociated alpha by the TIFF
  `ExtraSamples` tag makes every pixel whose alpha is `0` transparent: it contributes to no
  statistic, continuous or categorical. Extra samples declared as unspecified (for
  example a fourth NIR band) are ordinary data.
- **Not read.** Internal or external mask bands (`.msk`, `NewSubfileType` mask IFDs) and
  per-band NoData values other than the single `GDAL_NODATA` sentinel are not read yet.
- **Categorical classes** are never scaled.

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
| `scale_offset` | `BOOLEAN` | `true` | Accepted for symmetry and ignored: class codes are never scaled. |
| `chunk_size` | `BIGINT` | `512` | Strip/tile buffer window size in rows. |
| `sampling` | `VARCHAR` | `'center'` | Sub-pixel super-sampling preset (`'center'`, `'rgss'`, `'hex'`, etc.). |
| `min_lon`, `min_lat`, `max_lon`, `max_lat` | `DOUBLE` | `None` | Bounding box coordinates for spatial Region of Interest (ROI) chunk pruning. |
| `bbox` | `VARCHAR` | `None` | Bounding box as a single string: `'min_lon,min_lat,max_lon,max_lat'`. |
| `h3_cell` | `BIGINT` | `None` | Single H3 cell index for predicate pushdown. |
| `h3_hex` | `VARCHAR` | `None` | Single H3 hex string for predicate pushdown. |
| `compact` | `BOOLEAN` | `false` | Compact output format (omits `h3_hex` VARCHAR) and merge complete sets of seven H3 children into a parent. Cannot be combined with adjacent requested resolutions. |
| `overlap_rule` | `VARCHAR` | `'cutline'` | Mosaic tile overlap resolution: `'cutline'`, `'first'`, `'average'` (see above). |
| `workers` / `threads` | `BIGINT` | ambient | Aggregation (kernel) threads. When set, the query runs kernel windows on a dedicated pool of this size, shared by concurrent queries that request the same size. When unset, windows use the caller's Rayon pool, capped at 8 per wave. |
| `decode_workers` | `BIGINT` | `4` | Chunk decompression threads. |
| `fetch_workers` | `BIGINT` | `8` | Concurrent HTTP range requests for remote tiles. |
| `decode_budget_mb` | `BIGINT` | `256` | Memory for decoded chunks held at once (decoding, queued, being aggregated, idle for reuse), in MiB. Decoding waits for room; a single chunk larger than the budget is decoded only when no other is held. |
| `compaction_budget_mb` | `BIGINT` | `None` | Opt-in limit on pending `compact` sibling groups, in MiB. Past it, the groups nearest completion are emitted as individual children. Samples and totals are unchanged, but which groups compact can vary between runs. Unset keeps compaction deterministic and its memory unbounded. |
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
| `wkb` | `BLOB` | OGC 2D Well-Known Binary (WKB) cell geometry; see [cell geometry](#cell-geometry). |
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
| `wkb` | `BLOB` | OGC 2D Well-Known Binary (WKB) cell geometry; see [cell geometry](#cell-geometry). |
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
| `scale_offset` | `BOOLEAN` | `true` | Calibrate continuous values with GDAL band `SCALE`/`OFFSET`; ignored for categorical exports. |
| `categorical` | `BOOLEAN` | `false` | Whether to aggregate categorical raster classes instead of continuous stats. |
| `compact` | `BOOLEAN` | `false` | Compact output format (fewer columns). |
| `compression` | `VARCHAR` | `'snappy'` | Parquet compression codec: `'snappy'`, `'zstd'`, `'gzip'`, `'lz4'`, `'brotli'`, `'none'`. |
| `row_group_size` | `BIGINT` | `122880` | Maximum rows per Parquet row group. |
| `min_lon`, `min_lat`, `max_lon`, `max_lat` | `DOUBLE` | `None` | Spatial bounding box for chunk pruning. |
| `bbox` | `VARCHAR` | `None` | Bounding box as a single string: `'min_lon,min_lat,max_lon,max_lat'`. |
| `h3_cell` | `BIGINT` | `None` | Single H3 cell predicate pushdown filter. |
| `h3_hex` | `VARCHAR` | `None` | Single H3 hex string predicate pushdown filter. |
| `geoparquet` | `BOOLEAN` | `false` | Emit OGC GeoParquet 1.1 metadata in Parquet FileMetaData with PROJJSON `OGC:CRS84` datum. |
| `geom` | `BOOLEAN` | `false` | Emit a `geometry` column of the cell geometries; see [cell geometry](#cell-geometry). |
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
| `transform_failures` | `BIGINT` | Valid sample points dropped because their coordinate transform failed or fell outside the valid latitude/longitude range (0 when the CRS is right); see [transform-failure diagnostics](engines/core.md#transform-failure-diagnostics). |

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
| `scale_offset` | `BOOLEAN` | `true` | Calibrate continuous values with GDAL band `SCALE`/`OFFSET`; ignored for categorical exports. |
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
| `transform_failures` | `BIGINT` | Valid sample points dropped by failed coordinate transforms; see [transform-failure diagnostics](engines/core.md#transform-failure-diagnostics). |

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
| `h3_column` | `VARCHAR` | `None` (auto-detect) | Name of the H3 index column (supports BIGINT/UBIGINT integer or hex VARCHAR; matching is case-insensitive). An explicitly named column that is not found is an error listing available columns. Alias: `h3_col`. |

### Output Schema (1 Summary Row)
Returns the same 8-column summary schema as `h3_raster_to_pmtiles` (`total_hexagons`, `pmtiles_size_bytes`, `min_zoom`, `max_zoom`, `elapsed_ms`, `output_path`, `status`, `transform_failures`); `transform_failures` is always 0 because Parquet input is already indexed.

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
| `h3_to_wkb` | `(UBIGINT)` / `(VARCHAR)` | `BLOB` | Emits the cell's OGC 2D Well-Known Binary (WKB) geometry; see [cell geometry](#cell-geometry). |
| `h3_to_geometry` | `(UBIGINT)` / `(VARCHAR)` | `GEOMETRY` | Converts H3 index directly to native DuckDB Spatial polygon geometry (compatible with `ST_Area`, `ST_Intersects`, etc.). |
| `h3_cell_to_geometry` | `(UBIGINT)` / `(VARCHAR)` | `GEOMETRY` | Standard DuckDB spatial compatibility alias for `h3_to_geometry`. |
| `h3_cell_to_parent` | `(UBIGINT, BIGINT)` / `(VARCHAR, BIGINT)` | `UBIGINT` / `VARCHAR` | Truncates H3 cell to coarser parent resolution level. Returns null if requested resolution is finer. |
| `h3_polygon_to_cells` | `(BLOB, BIGINT)` / `(BLOB, BIGINT, VARCHAR)`; also `GEOMETRY` on DuckDB ≥ 1.5 | `UBIGINT[]` | Fills a polygon or multipolygon with H3 cells using h3o's polyfill. See [below](#h3_polygon_to_cells). |
| `raster_h3_version` | `()` | `VARCHAR` | Returns the compiled extension version string (e.g. `'0.2.0'`). |

### `h3_polygon_to_cells`

```sql
h3_polygon_to_cells(geometry, resolution [, containment]) -> UBIGINT[]
```

Returns every H3 cell at `resolution` that belongs to the polygon, as a list sorted
ascending with duplicates removed. It wraps h3o's
[`ToCells`](https://docs.rs/h3o/0.6/h3o/geom/trait.ToCells.html) polyfill (the h3o
counterpart of H3's `polygonToCells`).

| Parameter | Type | Description |
| :--- | :--- | :--- |
| `geometry` | `BLOB` (WKB) or `GEOMETRY` | A `POLYGON` or `MULTIPOLYGON` in WGS84 longitude/latitude degrees (x = longitude, y = latitude). The `BLOB` form accepts ISO/OGC WKB (2D, Z, M, ZM) and PostGIS EWKB (including an embedded SRID) in either byte order. Z/M values are ignored and the SRID is not checked, so reproject other CRSs to EPSG:4326 first. Holes (interior rings) are respected. The `GEOMETRY` overload is registered on DuckDB 1.5+, where `GEOMETRY` is a built-in type. On older versions with the spatial extension, pass `ST_AsWKB(geom)`. |
| `resolution` | `BIGINT` | H3 resolution `0` (coarsest) to `15` (finest). |
| `containment` | `VARCHAR`, optional | How to decide whether a cell belongs to the polygon. Defaults to `'contains_centroid'`. Case-insensitive; `-` and `_` are interchangeable. |

#### Containment modes

| Mode | Alias (H3 C API name) | A cell is returned when… | Coverage | Shared cells between adjacent polygons |
| :--- | :--- | :--- | :--- | :--- |
| `contains_centroid` (default) | `center` | its centroid lies inside the polygon | Can overshoot the edge and leave slivers uncovered | None. Each cell goes to exactly one of two polygons that touch without overlapping |
| `contains_boundary` | `full` | its whole boundary lies inside the polygon | Never overshoots; leaves the most area uncovered | None |
| `intersects_boundary` | — | any part of it overlaps the polygon | Covers the polygon completely and overshoots the edge | Possible |
| `covers` | `overlapping` | same as `intersects_boundary` | Complete, even for polygons smaller than a cell | Possible |

The result sets are nested: `contains_boundary` ⊆ `contains_centroid` ⊆ `intersects_boundary` ⊆
`covers`. Since h3o 0.11, `intersects_boundary` tests overlap with the whole cell rather than with
its edges, so a polygon that fits inside one cell returns that cell under both `intersects_boundary`
and `covers` (and an empty list under `contains_boundary`); the two modes are kept as distinct
names for compatibility. H3's `CONTAINMENT_OVERLAPPING_BBOX` mode has no h3o equivalent and is
not supported.

#### NULLs, invalid input and limits

| Input | Result |
| :--- | :--- |
| Any argument `NULL` | `NULL` |
| `geometry` is not a valid WKB `POLYGON`/`MULTIPOLYGON` (e.g. a `POINT`), or has non-finite coordinates | `NULL` |
| `resolution` outside `0..=15` | `NULL` |
| `POLYGON EMPTY` / `MULTIPOLYGON EMPTY` | `[]` |
| Unknown `containment` name | Query error that lists the accepted names |
| h3o's pre-tiling estimate exceeds 10,000,000 cells for a row | Query error. Use a coarser resolution or split the polygon (for example with `ST_Subdivide`-style tiling) |

#### Examples

```sql
-- Cells whose centroid falls in a bounding box (DuckDB 1.5+ GEOMETRY literal)
SELECT unnest(h3_polygon_to_cells(
    'POLYGON((-122.47 37.73, -122.37 37.73, -122.37 37.81, -122.47 37.81, -122.47 37.73))'::GEOMETRY,
    8)) AS h3_index;

-- Full coverage of each county, one row per (county, cell), via the spatial extension
SELECT name, unnest(h3_polygon_to_cells(ST_AsWKB(geom), 7, 'covers')) AS h3_index
FROM counties;

-- Join raster aggregates to the cells inside an area of interest
SELECT r.*
FROM h3_raster_continuous_aggregate('elevation.tif', 8) r
WHERE r.h3_index IN (SELECT unnest(h3_polygon_to_cells(ST_AsWKB(aoi.geom), 8)) FROM aoi);
```
