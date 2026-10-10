# Supported Coordinate Reference Systems (CRS)

[← Back to README](../README.md) · [Optimal Raster Format](optimal-raster-format.md)

`raster_h3` automatically detects the Coordinate Reference System (CRS) embedded in your GeoTIFF file and reprojects all pixel coordinates to WGS84 (EPSG:4326) for H3 indexing. You can also override the CRS manually via the `source_crs` parameter.

## How CRS Detection Works

1. **GeoTIFF metadata**: The extension reads the `ModelTiepointTag`, `ModelPixelScaleTag`, and `GeoKeyDirectoryTag` from the TIFF header to extract the embedded projection definition.
2. **EPSG matching**: If an EPSG code is found, the transformer selects the optimal tier (Identity → Analytical → PROJ4).
3. **PROJ string fallback**: If only a PROJ.4 definition string or WKT is present (e.g. Lambert Conformal Conic, Albers Equal-Area), it is parsed and transformed via `proj4rs`.
4. **Mandatory Explicit CRS if Undetected**: If a GeoTIFF lacks embedded CRS metadata or uses an unrecognized projection code, `raster_h3` **strictly halts with an error** (`RasterH3Error::CrsNotDetected` / `UnsupportedEpsg`) instead of guessing. You must supply the CRS explicitly.
5. **Manual specification & override**: The `crs` (or `source_crs`) parameter accepts `'EPSG:XXXX'` codes or full PROJ.4 definition strings, taking strict precedence over any embedded metadata.

## Supported Projection Families

| Projection Family | Common Use Cases | Example EPSG Codes |
| :--- | :--- | :--- |
| **Geographic (lat/lon)** | Global datasets, climate grids (ERA5, PRISM) | `EPSG:4326` (WGS84), `EPSG:4269` (NAD83, see datum policy) |
| **Web Mercator** | Web tile services, Google/Bing/OSM basemaps | `EPSG:3857`, `EPSG:3785`, `EPSG:900913` |
| **UTM (Universal Transverse Mercator)** | High-resolution regional data, Sentinel-2, Landsat | `EPSG:32601`–`32660` (North), `EPSG:32701`–`32760` (South) |
| **Transverse Mercator** | National grids | `EPSG:7846`–`7859` (GDA2020 / MGA zones 46–59, see datum policy), custom PROJ strings |
| **Albers Equal-Area** | Area-preserving thematic maps, NLCD | `EPSG:5070` (CONUS), `EPSG:3338` (Alaska) |
| **Cylindrical Equal-Area** | EASE-Grid 2.0 global products | `EPSG:6933` |
| **Lambert Conformal Conic** | Continental-scale datasets | Custom PROJ strings |
| **Polar Stereographic** | Arctic/Antarctic datasets, sea ice, NSIDC | `EPSG:3413` (North), `EPSG:3031` (South) |

These EPSG codes are the complete built-in list: `proj4rs` is compiled without
an EPSG database, so `'EPSG:XXXX'` and `+init=epsg:XXXX` work only for the codes
above. Any other CRS (for example British National Grid, `EPSG:27700`, which
needs a Helmert datum shift) must be supplied as a PROJ string, and datum shifts
(`+towgs84`, `+nadgrids`) are rejected.

### Fast paths and fallback

EPSG:4326, Web Mercator and Albers use analytical fast paths only when every
PROJ parameter is understood: linear units in metres, a Greenwich prime
meridian, and no `+axis`, `+geoc`, `+lon_wrap` or similar modifiers. Anything
else is handed to `proj4rs`, which honours units (`+units=us-ft`,
`+to_meter`), axis order and geographic prime meridians. A non-Greenwich prime
meridian on a projected CRS is rejected.

### GeoTIFF definitions

Registered EPSG codes in the GeoKeys are used directly. User-defined
(`32767`) projections are reconstructed only for Transverse Mercator, Lambert
Conformal Conic (2SP) and Albers on a WGS84 or NAD83 geodetic CRS with a
Greenwich meridian, degree angular units and a metre, foot, US survey foot or
explicit linear unit. A WKT1/ESRI `PROJCS` string in `GeoAsciiParamsTag` is
accepted under the same restrictions. Incomplete or other definitions are not
guessed: the file reports no CRS and `crs := ...` is required.

### Transformation failures

A sample whose coordinates lie outside the projection's valid domain (the
inverse fails, or yields a non-finite or out-of-range latitude) is skipped,
not aborted on and not counted. Successful transforms always return finite
latitudes in [-90, 90].

## Accuracy and Validity Boundaries

- **Datum policy.** No datum shifts are applied. EPSG:4269 (NAD83), the
  GDA2020 MGA zones and the Albers fast paths are treated as WGS84 (metre-level
  offsets). Golden tests compare against PROJ using the source datum as the
  target; their 0.01 m tolerance measures inverse-projection agreement, **not**
  absolute WGS84 accuracy. A small datum displacement can move samples across
  cell boundaries at any resolution.
- **Weighting.** Counts and means are sample-weighted, not area-weighted (see
  [Super-Sampling](super-sampling.md)).
- **H3 model.** H3 indexes on a sphere using its own conventions; geodetic
  WGS84 latitudes are passed to it unchanged.
- **Compaction.** Hierarchical compaction is a logical aggregation of seven
  children into their parent. H3 parents only approximately contain their
  children, so a compacted parent is not geometrically identical to indexing
  directly at the coarse resolution.
- **Mosaic overlap.** `first` and `cutline` test each candidate tile's actual
  footprint (exact inverse projection into its pixel grid); bounding boxes are
  only a candidate filter. `first` does not fall back to later tiles where the
  first tile holds nodata.
- **Geometry export.** WKB output unwraps ordinary antimeridian cells but
  leaves pole-cell rings unchanged, so polar cells are not faithful polar-cap
  polygons for planar GIS operations. GeoParquet declares planar edges, so
  exported boundaries approximate H3's spherical edges.

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

