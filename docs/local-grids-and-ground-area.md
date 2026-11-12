# Local datum grids and ground-area weighting

Status: implementation proposal; neither feature is implemented by this document.

## Constraints

Keep the default deployment pure Rust. Start with explicitly supplied local NTv2 horizontal-shift grids. Do not add a system PROJ dependency or download grids implicitly. Existing pixel-weighted output remains the default.

## Local grid transformations

The first supported operation is an explicit PROJ definition with a required local NTv2 grid and a WGS84 target. Do not infer a national transformation solely from an EPSG code or silently substitute identity/Helmert transformations. Document each grid's source datum, target datum, geographic extent, units, convention and licence. A horizontal grid is not a vertical or epoch transformation.

Verify the grid API of the locked proj4rs 0.1.10 before selecting reuse or a dependency update. Upstream main exposes experimental grid loading, but that does not establish the pinned version's capabilities. Changes to a process-global grid registry must account for concurrent scans, bounded memory and embedding in databases. Prefer immutable, per-transform grid ownership; avoid replacing another library user's global loader.

Required behavior: validate file headers, byte order, node counts, finite shifts, extents and spacing; bound allocation; resolve nested subgrids deterministically; implement bilinear interpolation with documented edge ownership; convert NTv2 positive-west longitude and arc-second shifts explicitly; reject points outside coverage; bound inverse iterations and report non-convergence. The grid's target datum must be represented correctly in the transformation chain.

CRS pruning/horizon optimizations must fail open until their bounds include grid displacement. Grid-induced errors follow the stream's existing transform-failure policy and strict mode. Required missing or malformed grids fail before scanning.

Tests: synthetic constant and varying shifts, both byte orders, nested grids, boundaries, axis/prime-meridian handling, inverse convergence, concurrent independent grids, malformed/truncated files and allocation limits. Add independently generated PROJ golden coordinates and full raster-to-H3 checks. Check results under pruning, spills and partition merges. Network access must not be needed by tests.

## Opt-in ground-area statistics

Expose a weighting option with pixel weighting as the default and an explicit ground-area mode. Preserve pixel contribution count separately from retained ground area in square metres. Do not relabel square metres as a pixel count or silently change existing min_count semantics.

In ground-area mode, retain per-cell sum(A_i * value_i), sum(A_i), and sum(A_i^2), where A_i is the retained area contribution of one source pixel to that cell. Mean divides the area-weighted sum by retained area. Category fractions use retained class area divided by retained total area. Min/max retain their observation meaning. State explicitly that area-weighted sums have value-times-square-metre units and are not automatically totals for extensive raster quantities.

Keep the reliability-weighted variance contract, merging all contributions from one pixel to the same cell before squaring its weight. Pixel counts, area weights, quantile weights and categorical weights must remain consistent with nodata, alpha, bbox filtering and mosaic ownership. Define an explicit area-weighted sum field instead of overloading an existing pixel sum without an API migration.

First milestone: exact ellipsoidal area for north-up WGS84 geographic rectangles, whose edges follow meridians and parallels. Handle longitude wrapping, poles and invalid latitude extents explicitly. Validate with independent ellipsoidal area integration; a geodesic polygon using only corner vertices is a different footprint and is not an exact oracle for parallel edges.

Reject unsupported CRS/grid geometries in the first milestone. Projected, rotated and sheared footprints require edge densification or integration with a documented convergence tolerance; a four-corner polygon alone does not establish area accuracy. General-CRS support requires a separate tested milestone.

Fixed-point supersampling apportions pixel area approximately and does not compute exact raster/hexagon overlap. Document this approximation; preserve total area for an unclipped pixel. Gaussian sensor weights are not uniform area quadrature, so ground-area mode must initially accept only explicitly validated uniform quadrature patterns. Never silently treat all existing sampling presets as physical area fractions.

Extend accumulator, spill and partial-state schemas together. Version distributed states and reject incompatible versions. Expose units and weighting mode through CLI, SQL, Python, DataFusion/Sedona and output metadata, with filter semantics documented.

Tests: analytic WGS84 equator/high-latitude/polar rectangles, antimeridian equivalence, independent ellipsoidal reference areas, constant-field invariance, area conservation, nonconstant weighted means and categorical fractions, invalid/unsupported geometries, sampling convergence, nodata/alpha/mosaic clipping, spill and partition round trips, and adapter/output metadata parity.

## Implementation sequence

1. Verify dependency capabilities and implement/test an immutable local NTv2 transform API.
2. Wire explicit grid operations through CRS overrides, error policy and conservative pruning.
3. Implement/test WGS84 rectangle areas and weighted-state/output schema extensions.
4. Wire area mode through the walker and every adapter; validate conservation and distributed merges.
5. Add general projected/rotated footprint integration only with independently checked error bounds.

Each implementation PR must run the relevant Rust tests, formatting, Clippy and existing cross-engine CI. Benchmark grid interpolation and area-mode throughput with fixed inputs before changing optimized paths.
