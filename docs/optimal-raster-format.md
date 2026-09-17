# Optimal Raster Format & Projection: Achieving Maximum Ingestion Speed

[← Back to README](../README.md) · [CRS & Projections](crs-and-projection.md)

While `raster_h3` can ingest arbitrary GeoTIFF files in any projected Coordinate Reference System (Albers, UTM, Lambert Conformal Conic, etc.), the physical layout and coordinate reference system of the source file drastically impacts processing throughput.

By formatting your rasters into an optimal layout, you can achieve **up to 4.5×–6× faster ingestion** while reducing storage footprints.

---

## Performance Comparison: Layout & Projection Impact

| Raster Format & CRS | Projection Math | Chunk Geometry | Relative Ingestion Speed |
| :--- | :--- | :--- | :---: |
| **Striped BigTIFF (e.g. Albers EPSG:5070)** | Heavy spherical trig (`atan2`, `sqrt`, authalic) | $156\text{k} \times 1$ strips | Baseline ($1\times$) |
| **Tiled COG (Projected CRS, e.g. Albers)** | Heavy spherical trig | $512 \times 512$ square tiles | **$\approx 2.5\times$ faster** |
| **Tiled COG in Native WGS84 (EPSG:4326)** | **Zero trig** (Single addition: $lng \mathrel{+}= \Delta lng$) | $512 \times 512$ square tiles | **$\approx 4.5\times - 6\times$ faster** |

---

## Why Tiled WGS84 Cloud-Optimized GeoTIFFs (COG) Are Optimal

1. **Zero Trigonometry ("Affine Cruise Control")**:
   In native WGS84 (`EPSG:4326`), pixel coordinates map directly to $(lng, lat)$ degrees with simple addition. All complex authalic trigonometric calculations (`atan2`, `sqrt`, series expansions) vanish, converting the coordinate step into a single CPU cycle.
2. **Dense L1/L2 Cache Locality**:
   A $512 \times 512$ square tile represents a localized geographic block ($\approx 15\,\text{km} \times 15\,\text{km}$). All 262,144 pixels map to a tight cluster of neighboring hexagons that fit in the CPU's high-speed L1/L2 cache, rather than scattering updates across $4,500\,\text{km}$ of continental longitude as a single scanline strip would.
3. **Sparse Ocean & Boundary Tile Pruning**:
   In a tiled COG, ocean and empty boundary blocks consume 0 bytes on disk and are skipped in $0\,\text{ms}$ with zero CPU decompression overhead.
4. **Zstandard (`ZSTD`) Acceleration**:
   Decompresses $3\times - 5\times$ faster than legacy DEFLATE/Zip while matching or beating its compression ratio.

---

## Universal GDAL Conversion Recipes

You can convert any arbitrary raster (regardless of original CRS or striped format) into an optimal **WGS84 Tiled COG** in a single pass using `gdalwarp`:

### 1. For Continuous Surfaces (Elevation, Fire Behavior, Temperature, Climate)

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

### 2. For Categorical Classifications (Land Cover, Fuel Models, Soil Types, Zoning)

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

---

## GDAL Parameter Breakdown

* **`-t_srs EPSG:4326`**: Reprojects the coordinate grid to WGS84 geographic degrees, unlocking zero-trig stepping.
* **`-r bilinear` vs. `-r near`**: Uses smooth bilinear interpolation for continuous numerical data; preserves discrete integer class IDs via nearest-neighbor for categorical grids.
* **`-of COG`**: Targets GDAL's native Cloud-Optimized GeoTIFF driver with optimized header placement and IFD organization.
* **`-co BLOCKSIZE=512`**: Configures $512 \times 512$ tile geometry for optimal L2 cache residency during hexagonal aggregation.
* **`-co COMPRESS=ZSTD`**: Applies modern high-throughput Zstandard compression.
* **`-co PREDICTOR=3`**: Enables floating-point delta prediction (byte-diffing) for 32-bit floats (`PREDICTOR=2` for integer categories).
* **`-multi` & `-wo NUM_THREADS=ALL_CPUS`**: Multi-threads the coordinate warping engine across all host CPU cores.
* **`-wm 2048`**: Allocates a 2 GB RAM buffer to eliminate disk swapping during reprojection.

---

## Why the Difference for Categorical vs. Continuous Data?

There are two critical reasons why categorical rasters require different GDAL flags:

### 1. Interpolation Artifacts (`-r near` vs. `-r bilinear`)
- **Continuous Surfaces (Elevation, Temperature, Flame Length)**: Values represent smooth physical fields. Bilinear interpolation (`-r bilinear`) smoothly blends pixel values across reprojected coordinate grids without jagged stair-stepping.
- **Categorical Classifications (Land Cover, Fuel Models, Soil Type)**: Pixel values are discrete integer labels (e.g. `101 = Grass`, `161 = Timber`). If you accidentally use `-r bilinear` or `-r cubic` on a categorical raster, the warping engine calculates mathematical weighted averages along class borders (e.g. averaging Grass `101` and Timber `161` into `131 = Shrub` or non-existent corrupt IDs). **You must use `-r near` (Nearest Neighbor) or `-r mode` (Majority Class)** to ensure every reprojected pixel remains an authentic source category code.

### 2. TIFF Compression Predictors (`PREDICTOR=2` vs. `PREDICTOR=3`)
- **`PREDICTOR=2` (Horizontal Differencing)**: Designed for **integers** (8-bit, 16-bit, 32-bit classification codes). It replaces raw values with differences between adjacent horizontal pixels (`current - previous`). In categorical maps with contiguous parcels of the same class, this generates long runs of zeros that compress dramatically.
- **`PREDICTOR=3` (Floating-Point Differencing)**: Designed specifically for **IEEE 754 32-bit/64-bit floats**. Floating-point numbers have exponent and mantissa bits that fluctuate rapidly, rendering standard horizontal differencing ineffective. `PREDICTOR=3` splits the 4 bytes of each float into 4 separate byte planes (sign/exponent, high mantissa, mid mantissa, low mantissa) before differencing, cutting continuous float file sizes in half.

---

## Summary Checklist for Production Pipelines

| Feature | Recommended Setting | Rationale |
| :--- | :--- | :--- |
| **Container Format** | Cloud-Optimized GeoTIFF (`-of COG`) | Decouples metadata headers from payload blocks; enables remote streaming |
| **Chunk Layout** | Tiled $512 \times 512$ (`-co BLOCKSIZE=512`) | L1/L2 cache locality and sparse tile skipping |
| **Projection** | WGS84 (`-t_srs EPSG:4326`) | Eliminates spherical trigonometry in favor of 1-cycle affine arithmetic |
| **Compression** | Zstandard (`-co COMPRESS=ZSTD`) | High decompression throughput with excellent compression ratio |
| **Predictor (Float)**| Floating-point differencing (`-co PREDICTOR=3`) | Byte-plane splitting for IEEE 754 floats |
| **Predictor (Int)**  | Horizontal differencing (`-co PREDICTOR=2`) | High run-length zero sequences on categorical parcels |
| **Resampling** | `bilinear` (continuous) / `near` (categorical) | Prevents invalid synthetic class generation |
