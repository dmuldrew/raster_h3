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

## The Recommended Compromise: Why RGSS is the Optimal Default

For production workflows, **`'rgss'` (Rotated Grid Super-Sampling, 4-point)** provides the ideal compromise between boundary accuracy and throughput.

### 1. Geometric Efficiency
Standard axis-aligned grids (such as a regular $2 \times 2$ grid or 5-point quincunx) suffer from collinear blind spots: when a hexagonal boundary cuts parallel to an axis, multiple sample points fall on the same side of the cut line, degrading anti-aliasing efficiency. 

RGSS rotates the sub-pixel grid by $\arctan(0.5) \approx 26.57^\circ$:
$$\left( \frac{3}{8}, \frac{1}{8} \right), \quad \left( \frac{7}{8}, \frac{3}{8} \right), \quad \left( \frac{1}{8}, \frac{5}{8} \right), \quad \left( \frac{5}{8}, \frac{7}{8} \right)$$

Because **no two points share the same horizontal ($X$) or vertical ($Y$) coordinate**, RGSS provides 4 distinct 1D projection slices across any arbitrary hexagon edge with only 4 evaluation points.

### 2. Algorithmic Synergy with Core Span Lookahead
The scanline walker separates raster rows into:
- **Core Interior Spans**: Pixels whose full sub-pixel bounding envelope $[0.125, 0.875] \times [0.125, 0.875]$ is guaranteed to lie inside the active H3 hexagon. These pixels are accumulated in bulk using single-pass SIMD vectorization at baseline speed with zero coordinate transformations or H3 lookups.
- **Boundary Perimeter Pixels**: Only pixels straddling the hexagon edge execute exact spherical H3 cell resolution.

Because the RGSS envelope is compact and interior pixels bypass individual sample indexing, **a $4\times$ increase in sampling density incurs only a $\sim 2.1\times$ runtime difference rather than a $4\times$ penalty**.

### 3. Selection Heuristic

```
Is raster pixel resolution significantly smaller than H3 cell? (e.g. 10m pixels into Res 7/8)
 ├── YES ──> Use 'center' (Maximum throughput; boundary partial-pixel area error is < 0.5%)
 └── NO
      ├── Are raster pixels LARGER than H3 cells? (e.g. 1km climate grids into Res 9+)
      │    └── YES ──> Use '16point' (Prevents blocky spatial quantization across hexagons)
      └── Standard Production & Analytical Queries
           └── YES ──> Use 'rgss' (Optimal balance: eliminates collinear aliasing with minimal overhead)
```

## Performance Tuning Tips
1. **Match `chunk_size` to Tile Dimensions**: For tiled GeoTIFFs (e.g., $256 \times 256$ or $512 \times 512$ tiles), set `chunk_size := 512` to align DuckDB decompressor buffers with native TIFF block boundaries.
2. **Region of Interest (ROI) Pruning**: Always specify `min_lon`, `min_lat`, `max_lon`, `max_lat` when analyzing spatial subsets. Non-overlapping GeoTIFF blocks are discarded instantly before reading from disk.
3. **Multi-Resolution Single Passes**: When creating multi-zoom web layers, use `resolutions := [6, 7, 8]` or `h3_raster_to_pmtiles(...)` rather than separate SQL queries to read the underlying GeoTIFF only once.

