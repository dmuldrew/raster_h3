# Supported Coordinate Reference Systems (CRS)

[← Back to README](../README.md) · [Optimal Raster Format](optimal-raster-format.md)

`raster_h3` automatically detects the Coordinate Reference System (CRS) embedded in your GeoTIFF file and reprojects all pixel coordinates to WGS84 (EPSG:4326) for H3 indexing. You can also override the CRS manually via the `source_crs` parameter.

## How CRS Detection Works

1. **GeoTIFF metadata**: The extension reads the `ModelTiepointTag`, `ModelPixelScaleTag`, and `GeoKeyDirectoryTag` from the TIFF header to extract the embedded projection definition.
2. **EPSG matching**: If an EPSG code is found, the transformer selects the optimal tier (Identity → Analytical → PROJ4).
3. **PROJ string fallback**: If only a PROJ.4 definition string or WKT is present (e.g. Lambert Conformal Conic, Albers Equal-Area), it is parsed and transformed via `proj4rs`.
4. **Mandatory Explicit CRS if Undetected**: If a GeoTIFF lacks embedded CRS metadata or uses an unrecognized projection code, `raster_h3` **strictly halts with an error** (`RasterH3Error::CrsError`) instead of guessing. You must supply the CRS explicitly.
5. **Manual specification & override**: The `crs` (or `source_crs`) parameter accepts `'EPSG:XXXX'` codes or full PROJ.4 definition strings, taking strict precedence over any embedded metadata.

## Supported Projection Families

| Projection Family | Common Use Cases | Example EPSG Codes |
| :--- | :--- | :--- |
| **Geographic (lat/lon)** | Global datasets, climate grids (ERA5, PRISM) | `EPSG:4326` (WGS84), `EPSG:4269` (NAD83) |
| **Web Mercator** | Web tile services, Google/Bing/OSM basemaps | `EPSG:3857`, `EPSG:900913` |
| **UTM (Universal Transverse Mercator)** | High-resolution regional data, Sentinel-2, Landsat | `EPSG:32601`–`32660` (North), `EPSG:32701`–`32760` (South) |
| **Transverse Mercator** | National grid systems (British National Grid, GDA2020) | `EPSG:27700`, `EPSG:7856` |
| **Lambert Conformal Conic** | Continental-scale datasets, CONUS projections | `EPSG:5070` (NAD83 Conus Albers), custom PROJ strings |
| **Albers Equal-Area** | Area-preserving thematic maps, NLCD, MODIS composites | `EPSG:5070`, `EPSG:6933` |
| **Polar Stereographic** | Arctic/Antarctic datasets, sea ice, NSIDC | `EPSG:3413` (North), `EPSG:3031` (South) |

## Usage Examples

```sql
-- Auto-detect from GeoTIFF metadata (most common)
SELECT * FROM h3_raster_continuous_aggregate('sentinel2_utm32n.tif', resolution := 8);

-- Explicitly supply CRS for unreferenced GeoTIFFs (or override existing CRS)
SELECT * FROM h3_raster_continuous_aggregate('legacy_raster.tif', resolution := 8, crs := 'EPSG:32632');

-- Supply full PROJ string (Lambert Conformal Conic)
SELECT * FROM h3_raster_continuous_aggregate(
    'conus_climate.tif',
    resolution := 7,
    crs := '+proj=lcc +lat_1=25 +lat_2=60 +lat_0=42.5 +lon_0=-100 +datum=NAD83 +units=m'
);
```

---

## Optimal Raster Format & Ingestion Speed

While `raster_h3` supports arbitrary projected coordinate reference systems (Albers, UTM, Lambert Conformal Conic, etc.), the physical layout and projection of the source file significantly affect ingestion throughput (up to **4.5×–6× faster** with tiled WGS84 COGs).

For complete benchmarks, in-depth architectural explanations, and universal GDAL recipes for continuous and categorical rasters:

👉 **[Optimal Raster Format & Ingestion Speed Deep Dive](optimal-raster-format.md)**

