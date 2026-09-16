# Sub-Pixel Super-Sampling Guide

[← Back to README](../README.md)

When a raster pixel lies across the boundary between two or more H3 hexagons, single-point center sampling assigns 100% of the pixel's value to whichever cell contains the center point. 

With **Sub-Pixel Super-Sampling**, multiple sample offsets (dx_i, dy_i) are evaluated within each pixel's unit box [0, 1] x [0, 1] with fractional weights:

![Sub-Pixel Super-Sampling Patterns](../assets/sampling_patterns.svg)

## Sampling Preset Reference Table

| Preset Name | Points | Weighting | Geometric Rationale | Why & When to Use |
| :--- | :---: | :--- | :--- | :--- |
| **`'center'`** *(default)* | 1 | 1.0 (Center) | Centroid evaluation | **Maximum speed**: Best when raster pixels are much smaller than H3 cells (e.g. 10m Sentinel vs Res 7 cells). |
| **`'rgss'`** / `'rotated4'` ⭐ | 4 | 0.25 each | 26.6° rotated grid (arctan 0.5) | **Best overall balance**: No two points share the same X or Y axis, eliminating collinear boundary blind spots with only 4 samples. |
| **`'hex'`** / `'7point'` | 7 | 1/7 each | Inscribed regular hexagon | **H3 Geometry Alignment**: Matches the natural hexagonal symmetry of H3 cell edges with zero directional bias. |
| **`'gaussian'`** / `'psf'` | 5 | Center 0.50, Edges 0.125 | Gaussian Point Spread Function | **Optical Sensor Emulation**: Emulates real-world satellite sensor response where the pixel center is more sensitive than the corners. |
| **`'5point'`** / `'quincunx'` | 5 | 0.20 each | Center + 4 diagonal corners | **Classic Area Weighting**: Standard 5-point super-sampling. |
| **`'8rooks'`** / `'stratified8'`| 8 | 1/8 each | Latin Hypercube non-attacking rooks | **Diagonal Anti-Aliasing**: Eliminates sample clumping along diagonal hexagon edges. |
| **`'9point'`** / `'3x3'` | 9 | 1/9 each | Regular 3 × 3 grid | **Dense Uniform Coverage**: Smooth, uniform sub-pixel discretization. |
| **`'16point'`** / `'4x4'` | 16 | 1/16 each | Regular 4 × 4 grid | **Coarse → Fine Resampling**: Ideal when coarse pixels (e.g. 1km climate / ERA5 data) overlap fine H3 cells (Res 9–11). |

## Performance & Precision Trade-Off Guide

| Sampling Mode | Samples / Pixel | Relative Runtime | Boundary Precision | Recommended Use Case |
| :--- | :---: | :---: | :--- | :--- |
| **`center`** | 1 | **$1.0\times$** (Fastest) | Baseline | Fast exploratory scans, massive high-resolution rasters (10m pixels into Res 6–8 cells) |
| **`rgss`** *(Recommended)* | 4 | **$\sim 0.75\times$** | High Anti-Aliasing | Default for production analytical queries; eliminates axis-aligned blind spots |
| **`hex`** | 7 | **$\sim 0.60\times$** | True Hexagonal Symmetry | When strict hexagonal area weighting is required |
| **`gaussian`** | 5 | **$\sim 0.68\times$** | Optical PSF Emulation | Remote sensing satellite imagery where pixel centers dominate sensor response |
| **`8rooks`** | 8 | **$\sim 0.55\times$** | Full Stratified Anti-Aliasing | Highly complex boundary contours with diagonal edges |
| **`16point`** | 16 | **$\sim 0.35\times$** | Sub-Grid Reconstruction | Coarse rasters (e.g. 1km climate grids) aggregated into fine H3 cells (Res 9–11) |

### Performance Tuning Tips
1. **Match `chunk_size` to Tile Dimensions**: For tiled GeoTIFFs (e.g., $256 \times 256$ or $512 \times 512$ tiles), set `chunk_size := 512` to align DuckDB decompressor buffers with native TIFF block boundaries.
2. **Region of Interest (ROI) Pruning**: Always specify `min_lon`, `min_lat`, `max_lon`, `max_lat` when analyzing spatial subsets. Non-overlapping GeoTIFF blocks are discarded instantly before reading from disk.
3. **Multi-Resolution Single Passes**: When creating multi-zoom web layers, use `resolutions := [6, 7, 8]` or `h3_raster_to_pmtiles(...)` rather than separate SQL queries to read the underlying GeoTIFF only once.
