# Supported Coordinate Reference Systems (CRS)

[← Back to README](../README.md)

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

## Optimal Raster Format & Projection: Achieving Maximum Ingestion Speed

While `raster_h3` can ingest arbitrary GeoTIFF files in any projected CRS (Albers, UTM, Lambert Conformal Conic), the physical layout and coordinate reference system of the source file drastically impacts processing throughput:

| Raster Format & CRS | Projection Math | Chunk Geometry | Relative Ingestion Speed |
| :--- | :--- | :--- | :---: |
| **Striped BigTIFF (e.g. Albers EPSG:5070)** | Heavy spherical trig (`atan2`, `sqrt`, authalic) | $156\text{k} \times 1$ strips | Baseline ($1\times$) |
| **Tiled COG (Projected CRS, e.g. Albers)** | Heavy spherical trig | $512 \times 512$ square tiles | **$\approx 2.5\times$ faster** |
| **Tiled COG in Native WGS84 (EPSG:4326)** | **Zero trig** (Single addition: $lng \mathrel{+}= \Delta lng$) | $512 \times 512$ square tiles | **$\approx 4.5\times - 6\times$ faster** |

### Why Tiled WGS84 Cloud-Optimized GeoTIFFs (COG) Are Optimal:
1. **Zero Trigonometry ("Affine Cruise Control")**: In WGS84 (`EPSG:4326`), pixel coordinates map directly to $(lng, lat)$ degrees with simple addition. All complex authalic trigonometric calculations (`atan2`, `sqrt`, series expansions) vanish, converting the coordinate step into a single CPU cycle.
2. **Dense L1/L2 Cache Locality**: A $512 \times 512$ square tile represents a localized geographic block ($\approx 15\,\text{km} \times 15\,\text{km}$). All 262,144 pixels map to a tight cluster of neighboring hexagons that fit in the CPU's high-speed L1/L2 cache, rather than scattering updates across $4,500\,\text{km}$ of continental longitude.
3. **Sparse Ocean & Boundary Tile Pruning**: In a tiled COG, ocean and empty boundary blocks consume 0 bytes on disk and are skipped in $0\,\text{ms}$ with zero CPU decompression overhead.
4. **Zstandard (`ZSTD`) Acceleration**: Decompresses $3\times - 5\times$ faster than legacy DEFLATE/Zip while matching or beating its compression ratio.

---

## Universal GDAL Conversion Recipe

You can convert any arbitrary raster (regardless of original CRS or striped format) into an optimal **WGS84 Tiled COG** in a single pass using `gdalwarp`:

### 1. For Continuous Surfaces (Elevation, Fire Behavior, Temperature, Climate):
```bash
gdalwarp input_raster.tif output_wgs84_cog.tif \
  -t_srs EPSG:4326 \
  -r bilinear \
  -of COG \
  -co BLOCKSIZE=512 \
  -co COMPRESS=ZSTD \
  -co PREDICTOR=3 \
  -co NUM_THREADS=ALL_CPUS \
  -multi \
  -wo NUM_THREADS=ALL_CPUS \
  -wm 2048
```

### 2. For Categorical Classifications (Land Cover, Fuel Models, Soil Types, Zoning):
```bash
gdalwarp input_categorical.tif output_categorical_wgs84_cog.tif \
  -t_srs EPSG:4326 \
  -r near \
  -of COG \
  -co BLOCKSIZE=512 \
  -co COMPRESS=ZSTD \
  -co PREDICTOR=2 \
  -co NUM_THREADS=ALL_CPUS \
  -multi \
  -wo NUM_THREADS=ALL_CPUS \
  -wm 2048
```

### GDAL Parameter Breakdown:
* **`-t_srs EPSG:4326`**: Reprojects coordinate grid to WGS84 geographic degrees.
* **`-r bilinear` vs. `-r near`**: Uses smooth bilinear interpolation for continuous numerical data; preserves discrete integer class IDs via nearest-neighbor for categorical grids.
* **`-of COG`**: Targets GDAL's native Cloud-Optimized GeoTIFF driver with optimized header placement.
* **`-co BLOCKSIZE=512`**: Configures $512 \times 512$ tile geometry for optimal L2 cache residency.
* **`-co COMPRESS=ZSTD`**: Applies modern high-throughput Zstandard compression.
* **`-co PREDICTOR=3`**: Enables floating-point delta prediction (byte-diffing) for 32-bit floats (`PREDICTOR=2` for integer categories).
* **`-multi` & `-wo NUM_THREADS=ALL_CPUS`**: Multi-threads the coordinate warping engine across all host CPU cores.
* **`-wm 2048`**: Allocates a 2 GB RAM buffer to eliminate disk swapping during reprojection.

### Why the Difference for Categorical vs. Continuous Data?

There are two critical reasons why categorical rasters require different GDAL flags:

1. **Interpolation Artifacts (`-r near` vs. `-r bilinear`)**:
   - **Continuous Surfaces (Elevation, Temperature, Flame Length)**: Values represent smooth physical fields. Bilinear interpolation (`-r bilinear`) smoothly blends pixel values across reprojected coordinate grids without jagged stair-stepping.
   - **Categorical Classifications (Land Cover, Fuel Models, Soil Type)**: Pixel values are discrete integer labels (e.g. `101 = Grass`, `161 = Timber`). If you accidentally use `-r bilinear` or `-r cubic` on a categorical raster, the warping engine calculates mathematical weighted averages along class borders (e.g. averaging Grass `101` and Timber `161` into `131 = Shrub` or non-existent corrupt IDs). **You must use `-r near` (Nearest Neighbor) or `-r mode` (Majority Class)** to ensure every reprojected pixel remains an authentic source category code.

2. **TIFF Compression Predictors (`PREDICTOR=2` vs. `PREDICTOR=3`)**:
   - **`PREDICTOR=2` (Horizontal Differencing)**: Designed for **integers** (8-bit, 16-bit, 32-bit classification codes). It replaces raw values with differences between adjacent horizontal pixels (`current - previous`). In categorical maps with contiguous parcels of the same class, this generates long runs of zeros that compress dramatically.
   - **`PREDICTOR=3` (Floating-Point Differencing)**: Designed specifically for **IEEE 754 32-bit/64-bit floats**. Floating-point numbers have exponent and mantissa bits that fluctuate rapidly, rendering standard horizontal differencing ineffective. `PREDICTOR=3` splits the 4 bytes of each float into 4 separate byte planes (sign/exponent, high mantissa, mid mantissa, low mantissa) before differencing, cutting continuous float file sizes in half.
