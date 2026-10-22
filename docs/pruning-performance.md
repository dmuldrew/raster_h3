# Pruning performance

Measured on 2026-10-22 on a local macOS arm64 machine with rustc 1.98.1,
optimized Cargo bench profile. Baseline: PR #45 commit `e28603d` plus the
benchmark fixture alone; after: this polar-bounds change. These are local
synthetic measurements, not remote-COG throughput or production guarantees.

## Workloads and results

The `pruning` Criterion group in `benches/streaming.rs` generates 16 disjoint
128×128 uncompressed Float32 GeoTIFFs, 100 m pixels, on a 4×4 grid with
250 km origin spacing. Each tile has one chunk. H3 resolution is 7, with one
aggregation worker and one decode worker. Setup of fixtures is outside
timing; opening, planning, reading, decompression, projection, ownership,
aggregation and batch consumption are inside timing. Filesystem caches are
warm. There are 10 samples, a 1-second warmup, and a requested 1-second
measurement window (Criterion extends it when necessary).

`small_bbox` uses `First` and a 0.02°×0.02° box centered on the first tile.
`disjoint_first` scans the whole mosaic under `First`. `disjoint_average`
is the full-scan control that already avoids ownership checks.

| EPSG | Workload | Before mean (ms) | After mean (ms) | Before / after |
| --- | --- | ---: | ---: | ---: |
| 3413 | `small_bbox` | 98.72 | 6.53 | 15.11× |
| 3413 | `disjoint_first` | 300.57 | 166.06 | 1.81× |
| 3413 | `disjoint_average` | 168.62 | 171.25 | 0.98× |
| 3031 | `small_bbox` | 98.44 | 6.65 | 14.80× |
| 3031 | `disjoint_first` | 296.38 | 162.55 | 1.82× |
| 3031 | `disjoint_average` | 167.03 | 164.50 | 1.02× |
| 2154 | `small_bbox` | 115.95 | 116.03 | 1.00× |
| 2154 | `disjoint_first` | 397.62 | 357.66 | 1.11× |
| 2154 | `disjoint_average` | 185.25 | 184.22 | 1.01× |
| 3035 | `small_bbox` | 33.70 | 33.46 | 1.01× |
| 3035 | `disjoint_first` | 233.79 | 232.81 | 1.00× |
| 3035 | `disjoint_average` | 108.12 | 102.76 | 1.05× |

The polar bbox plans change from 16 tiles/chunks to 1, with no overlap
checks. Polar full scans keep all 16 chunks, but `First` changes from
16 overlap-flagged chunks to zero. This explains the roughly 15× bbox
speedup and 1.8× full-scan `First` speedup. Bounds construction uses five
projection calls instead of 121.

EPSG:2154 (Lambert conformal) and EPSG:3035 (LAEA) retain all 16 tiles/chunks
and overlap flags under `First`. Their certificates have not been extended.
The shortcut for uncertified bbox checks only saves discarded planning
probes. In particular, the apparent 11% Lambert `First` change is not evidence
of better ownership filtering: that execution path is unchanged and the
baseline samples were noisy. The `Average` controls likewise show no
substantial gain; do not treat small single-run changes as guarantees.

## Reproducing

Use the same benchmark source on the baseline and changed versions. Save
Criterion's output directory across the two runs (or use two checkouts and
copy the baseline directories). Run on an otherwise idle machine:

```sh
# Baseline implementation plus the benchmark fixture
cargo bench -p raster-h3-core --bench streaming --locked -- pruning \
  --sample-size 10 --measurement-time 1 --warm-up-time 1 --save-baseline before-polar

# Updated implementation, with the saved baseline available
cargo bench -p raster-h3-core --bench streaming --locked -- pruning \
  --sample-size 10 --measurement-time 1 --warm-up-time 1 --baseline before-polar
```

The benchmark prints retained tile/chunk and overlap counts before each
case. Criterion writes estimates and confidence intervals under
`target/criterion/pruning/`. No wall-clock threshold is enforced in tests.

## Safety and remaining scope

Certification is limited to the built-in EPSG:3413 and EPSG:3031 definitions.
See [the radial/angular enclosure argument](crs-and-projection.md#bounding-boxes-and-pruning).
Pole-containing and antimeridian-crossing bounds use full longitude;
rotated/sheared rectangles may have wider bounds. Generic/custom polar,
Lambert and LAEA definitions still fail open. Horizon eviction remains
conservative for these PROJ-backed transformers.

Integration tests check dense polar enclosure (both hemispheres, rotated
and sheared grids, antimeridian crossings, poles and large radii) and compare
per-cell counts/sums with an otherwise identical uncertified transformer.
Aggregation parity covers small bbox and full scans, disjoint and overlapping
tiles, and `First`, `Cutline` and `Average` ownership rules.

Future work should benchmark compressed tiled/remote COGs and mixed-CRS
mosaics, and add separate proofs before restoring Lambert/LAEA pruning.
