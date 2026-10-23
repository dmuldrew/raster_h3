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
| **Transverse Mercator** | National grids | `EPSG:27700` (British National Grid), `EPSG:31466`–`31469` (DHDN / Gauss–Krüger), `EPSG:23028`–`23038` (ED50 / UTM), `EPSG:25828`–`25838` (ETRS89 / UTM), `EPSG:7846`–`7859` (GDA2020 / MGA) |
| **Oblique Stereographic** | Netherlands | `EPSG:28992` (Amersfoort / RD New) |
| **Albers Equal-Area** | Area-preserving thematic maps, NLCD | `EPSG:5070` (CONUS), `EPSG:3338` (Alaska) |
| **Lambert Azimuthal Equal-Area** | Pan-European statistics | `EPSG:3035` (LAEA Europe) |
| **Cylindrical Equal-Area** | EASE-Grid 2.0 global products | `EPSG:6933` |
| **Lambert Conformal Conic** | National and continental datasets | `EPSG:2154` (Lambert-93), custom PROJ strings |
| **Polar Stereographic** | Arctic/Antarctic datasets, sea ice, NSIDC | `EPSG:3413` (North), `EPSG:3031` (South) |

These EPSG codes are the complete built-in list: `proj4rs` is compiled without
an EPSG database, so `'EPSG:XXXX'` and `+init=epsg:XXXX` work only for the codes
above. Any other CRS must be supplied as a PROJ string; see the datum policy
below for which datum shifts are applied.

### Fast paths and fallback

EPSG:4326, Web Mercator and Albers use analytical fast paths only when every
PROJ parameter is understood: linear units in metres, a Greenwich prime
meridian, a zero datum shift, and no `+axis`, `+geoc`, `+lon_wrap` or similar
modifiers. Anything
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

### Bounding boxes and pruning

Tiles and chunks are pruned against a query `bbox`, and mosaic overlap is
pre-filtered, using WGS84 bounds of each pixel rectangle
(`CrsTransformer::transform_rect_bounds`). Data dropped by such a test never
reaches the exact per-sample check, so the bounds used for rejection must
enclose the rectangle's true image. `CrsTransformer::bounds_certified` reports
whether they do, and callers prune only with
`CrsTransformer::rejection_bounds`, which returns the whole globe for
uncertified bounds:

| CRS | Bounds | Pruning |
| :--- | :--- | :--- |
| EPSG:4326, Web Mercator, Albers fast path | Analytic | Yes |
| EPSG:3413 / EPSG:3031 (built-in definitions) | Analytic radial/angular enclosure | Yes |
| Transverse Mercator: `utm`, `etmerc`, exact `tmerc` on a named ellipsoid | Certified (see below) | Yes, within 2,500 km of the central meridian (divided by `k0`) |
| Other `proj4rs` projections, spherical/approximate `tmerc`, custom ellipsoid sizes, `+towgs84` beyond the limits below | Sampled estimate | No: every tile and chunk is read and checked exactly |

Transverse Mercator bounds rest on the projection being conformal with point
scale at least `k0`: a segment of length `L` maps to a ground path no longer
than `L / k0`, so latitude and longitude change by at most that length over
the radii of curvature (6.3e6 m is used, below every named ellipsoid). The
rectangle is sampled on an 11×11 grid and widened by these amounts. For a
512 px × 30 m chunk the widening is about 0.01–0.02°. Explicit `+towgs84`
shifts are certified up to 1.5 km translation, 10″ rotation and 50 ppm scale,
which covers every named datum. Certification fails open for duplicate parameters,
quoted values, non-lowercase keys, or whitespace around `=`; projection still
uses proj4rs normally. Scale follows the backend's effective parameters:
proj4rs 0.1.10 uses `+k0` before `+k` and ignores `+k_0`. This rule also applies
when several scale aliases occur together. UTM retains its fixed scale.

For the built-in WGS84 north/south polar stereographic definitions
(EPSG:3413 and EPSG:3031), the inverse latitude is monotonic in distance
from the projected origin, and longitude is the angle around that origin.
We enclose the affine pixel rectangle in a source-axis rectangle, then
project its four corners and its closest point to the origin. These five
points include both radial extrema and the angular extrema. Rectangles
containing the origin, or whose longitude bounds cross the antimeridian,
get full longitude. A 1e-7 degree margin exceeds the inverse iteration's
1e-10 radian stopping tolerance. See the [PROJ polar stereographic definition](https://proj.org/en/stable/operations/projections/stere.html)
and proj4rs 0.1.10's `stere::Projection::e_inverse` for the radial/atan2 form.
Rotated and sheared grids may get wider bounds because of the source-axis
enclosure; pruning still never relies on sampled interior extrema.

Only the built-in EPSG constructors enable this polar certificate. Custom
stereographic strings, shifted datums, LCC and LAEA remain uncertified.
Uncertified bbox tests now return immediately instead of computing an
estimate they would discard. This saves planning work, but does not
restore tile/chunk pruning or bypass ownership checks for those CRSs.
See [pruning benchmarks](pruning-performance.md) for measured scope and limits.

If any probe of a rectangle fails to project (part of it lies outside the
projection's domain), its bounds are the whole globe. GeoTIFFs whose affine
transform is non-finite or singular are rejected when opened.

## Datum Shifts

Coordinates end up on WGS84 in one of three ways:

| Treatment | Applies to | Accuracy relative to WGS84 |
| :--- | :--- | :--- |
| **Helmert shift** (`+towgs84`, 3 or 7 parameters) | Any PROJ string with `+towgs84=...`, named datums that imply one (`+datum=OSGB36`, `potsdam`, `ire65`, ...), and the built-in codes below | The accuracy of the chosen parameters, typically 1–10 m |
| **Treated as WGS84** (no shift) | WGS84, NAD83 (`EPSG:4269`, `5070`, `3338`), ETRS89 (`2154`, `3035`, `25828`–`25838`), GDA2020 (`7846`–`7859`), and `+towgs84=0,0,0` | Metre-level for NAD83; sub-metre and growing with time for ETRS89 and GDA2020. This matches PROJ's default for these datums. |
| **Rejected** | Grid-based datums: `+nadgrids=...` (other than `@null`) and `+datum=NAD27`; unrecognised `+datum` names | Grid files are not bundled; supply a `+towgs84` approximation explicitly if one is acceptable |

Built-in codes with a Helmert shift, chosen from the EPSG registry:

| EPSG | CRS | Transformation | Published accuracy |
| :--- | :--- | :--- | :--- |
| `27700` | OSGB36 / British National Grid | OSGB36 to WGS 84 (6), EPSG:1314 | 2 m |
| `28992` | Amersfoort / RD New | Amersfoort to WGS 84 (4), EPSG:4833 | 1 m |
| `31466`–`31469` | DHDN / 3-degree Gauss–Krüger | DHDN to WGS 84 (2), EPSG:1777 | 3 m (former West Germany) |
| `23028`–`23038` | ED50 / UTM | ED50 to WGS 84 (1), EPSG:1133 | 10 m (Europe-wide mean) |

PROJ itself may prefer a different transformation for these CRSs, for example
a national grid (OSTN15 for OSGB36) or a regional ED50 Helmert. The tests in
`crates/raster-h3-core/tests/it/datum_shifts.rs` check agreement with PROJ for the same parameters
(≤ 0.01 m) and with PROJ's preferred transformation (within the published
accuracy above).

Shifts are applied in PROJ's order (axis normalization, then prime meridian,
then datum) with zero ellipsoidal height; the horizontal effect of ignoring
height is negligible. Time-dependent (epoch-aware) transformations and grid
shifts are not supported.

## Accuracy and Validity Boundaries

- **Datum policy.** See [Datum Shifts](#datum-shifts). Golden tests for the
  unshifted paths compare against PROJ using the source datum as the target;
  their 0.01 m tolerance measures projection agreement, **not** absolute WGS84
  accuracy. Any datum displacement can move samples across cell boundaries at
  any resolution.
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

