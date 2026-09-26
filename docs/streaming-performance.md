# Streaming performance measurements

Run the reproducible synthetic benchmark outside a Rayon worker:

```sh
RAYON_NUM_THREADS=8 cargo run --release --example benchmark_streaming -- 512 3 > timing.jsonl
RAYON_NUM_THREADS=8 cargo run --release --features stream-profile --example benchmark_streaming -- 512 3 > profile.jsonl
```

The arguments are square image width and repetitions. Repeat with thread counts
1, 4 and 8, and larger images for scaling measurements. The benchmark generates
uncompressed Float32 TIFFs with 64-row strips, CRS 4326/3857/32610, resolutions
7/10, center/RGSS sampling, and 256 KiB/64 MiB aggregation budgets. Coordinates
are near California; projected and geographic fixtures cover different extents.
Compare configurations within a CRS, rather than treating the CRSs as equivalent
workloads. File generation is outside the timed region. Each run includes stream
construction, fetching and insertion into the verification BTreeMap. This is an
end-to-end benchmark with verification overhead, not an isolated kernel speed.

The benchmark asserts exact cell/count/sum parity across repetitions and budgets,
no duplicate output keys, and total sample weight/value conservation. It uses
integer sample values and binary-exact RGSS weights so these comparisons do not
require tolerances. It is not an independent geodetic reference; the integration
suite supplies exhaustive per-sample reference comparisons.

## Counters and their limits

`stream.metrics` contains worker job/map-set counts, estimated peak worker
storage, decoded bytes, prefetch wait, worker/merge/spill durations, and optional
hot-path counters. `stream.spill_bytes_written()` includes temporary intermediate
merge outputs; `spill_run_count()` counts initial runs only.

With `stream-profile`, worker counters include H3 indexing calls, time inside
those calls, generic coordinate-transform calls/time, and bytes copied by the
compatibility window adapter. Inline WGS84/Mercator row arithmetic is not counted
as a generic transform call. Geometric horizon setup is not counted as worker H3
indexing. Per-call timers add overhead: use the build without the feature for
throughput and the instrumented build for attribution. Hot-path counters are zero
when the feature is disabled. Coarse job timings remain available in both builds.

Worker time sums concurrent worker durations and can exceed wall time. H3 and
transform times are included in worker time; spill time during active merging is
included in merge time. EOF run consolidation is included in spill time, but
final record deserialization/compaction is not. Do not add these columns together
as disjoint wall-clock stages. The separate synchronous decode pass is a
warm-cache baseline, not the decoder's time during streaming. Prefetch wait is
consumer wait time, not decoding CPU time.

Worker map-set counts describe reusable map containers, not allocator calls.
Peak worker and active-map bytes include estimated hash-table capacity and owned
accumulator state. They are measured separately and are not a whole-process RSS
limit. Decoder/prefetch buffers, output verification records and temporary sort
storage are additional memory. Use an OS profiler for RSS and allocation counts.

## Allocation changes

Built-in continuous and categorical kernels borrow native typed slices from the
decoded chunk. Jobs combine full rows when those rows fit the worker allowance;
otherwise they use horizontal row segments. Bounds retain global pixel offsets,
while the view retains the cropped decoded chunk's physical stride, including
interleaved bands. Each worker retains its maps between jobs and transfers
accumulators by draining them. Job/result vectors are reused within a batch.
External kernels implementing only `process_chunk` remain supported through a
copying default `process_window`; overriding it opts into borrowed storage.

The conservative worker allowance, spill behavior, and serialized merging remain.
This removes pixel-window copies and repeated map-container construction; it does
not remove per-pixel H3 indexing or establish a particular throughput target.

## Lookahead status

Exponential/binary endpoint lookahead remains disabled. The regression with an
H3 cell leave-and-reenter along a constant-latitude row demonstrates why endpoint
agreement cannot certify a prefix. A spherical polygon containment test with an
arbitrary inward margin would still require a justified error bound relating it
to the floating-point H3 indexing implementation. No such bound has been
established here, so there is no newly enabled geometric certificate or no-op
configuration switch claiming otherwise. Center spans and subpixel cores retain
exact intermediate-sample checks.

The remaining optimization requires a whole-prefix certificate, including every
actual sampling offset, conservative arithmetic error handling, supported face
and projection domains, and exact fallback whenever containment is inconclusive.
A sampled or empirical margin is insufficient for the requested soundness claim.

## Local comparison, 2026-09-26

[Raw measurements](benchmarks/streaming-2026-09-26.json) compare commit `aa70286`
with the borrowed-window implementation using the same benchmark, 512×512 images,
10 Rayon threads (at most 8 aggregation workers), and uninstrumented release
builds. Three repetitions were followed by five repetitions in reversed
before/after order; the table reports medians across all eight. Compilation was
finished before the retained timing runs. The baseline benchmark omitted fields
for counters that did not exist in that commit.

Selected 64 MiB budget results, in Mpx/s:

| CRS | Resolution | Sampling | Before | After |
| --- | ---: | --- | ---: | ---: |
| WGS84 | 7 | Center | 16.99 | 23.43 |
| WGS84 | 7 | RGSS | 4.07 | 4.65 |
| Web Mercator | 10 | Center | 11.06 | 14.51 |
| UTM 10N | 7 | Center | 11.45 | 13.05 |
| UTM 10N | 10 | RGSS | 2.36 | 2.10 |

These small synthetic runs show substantial variance. They support an allocation
improvement for some workloads, not a universal speedup: the UTM resolution-10
RGSS case regressed by about 11%. Tiny-budget runs remain dominated by job/spill
overhead. A separate instrumented 256×256 matrix confirmed zero copied pixel
bytes in built-in kernels and map reuse across jobs. Center sampling still uses
one H3 index call per pixel; RGSS uses roughly five. Restoring a sound geometric
certificate, and measuring representative production TIFFs, remain separate work.

Validation: 387 existing tests passed with `stream-profile` (the two large Hawaii
integration tests excluded), plus the new interleaved-window test. The targeted
budget/multiband suites passed again after the final counter changes. The default
build also passed `cargo check --lib`.
