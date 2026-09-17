# Direct Ground-Truth Multi-Resolution Spatial Pyramids

[← Back to README](../README.md)

`raster_h3` provides single-pass multi-resolution streaming via `MultiScanHorizonStreamer` and `MultiCategoricalHorizonStreamer`, enabling simultaneous extraction across multiple H3 zoom levels (e.g. resolutions 7, 8, and 9) in a **single file read**.

![Direct Pixel Containment vs Hierarchical Parent Rollup](../assets/direct_vs_hierarchical.svg)

## The "Aperture 7" Challenge & True Ground-Truth Guarantee

In the H3 Discrete Global Grid System, parent hexagons are **not** the strict geometric union of their 7 child hexagons due to an Aperture-7 angular rotation. As a result:
* **Naive Parent Rollups (`cell.parent()`)**: Suffer from boundary distortion near cell edges because child hexagons slightly overlap neighboring parent boundaries.
* **`raster_h3` Direct Multi-Resolution Streaming**: Evaluates every pixel's exact coordinate center against the true polygon boundary of every requested resolution level simultaneously.

> [!TIP]
> **100.000% Exact Numerical Identity**: Running multi-resolution extraction on `[7, 8, 9]` produces cell indices, pixel counts, means, variances, mins, and maxes that are **100% identical** down to the exact pixel compared to running three separate single-resolution scans.

```sql
-- Direct multi-resolution extraction across zoom levels 7, 8, and 9
SELECT 
    resolution,
    h3_hex,
    round(mean, 2) AS mean_elevation,
    round(stddev, 2) AS ruggedness,
    count AS pixels
FROM h3_raster_continuous_aggregate(
    'california_dem.tif',
    resolutions := [7, 8, 9]
)
ORDER BY resolution ASC, pixels DESC;
```
