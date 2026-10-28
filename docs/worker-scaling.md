# Worker Scaling

Should the serial horizon merge be parallelised? This page records the measurements
taken with the `worker_scaling` example (see
[core engine docs](engines/core.md#worker-scaling)) and the resulting decision.

## Background

Each kernel wave fills one private cell map per aggregation worker, in parallel. The
calling thread then merges every worker map into the shared per-resolution horizon
maps (`merge_window`, timed as `StreamProfile::merge_ns`). A merge that dominates wall
time caps the speedup from adding workers.

## Setup

- Commit `cf8dd5a`, release build, rustc 1.97.0.
- 4-vCPU Intel Xeon cloud VM with 15 GiB RAM. The 8-worker rows oversubscribe it, so
  they show contention rather than 8-core scaling.
- 4096 × 4096 px synthetic rasters (16.8 Mpx, 0.0005° pixels), centre sampling,
  `decode_workers` 4.
- Each figure is the median of 3 runs after one warm-up. Output was identical at every
  worker count.

```bash
cargo run --release -p raster-h3-core --example worker_scaling -- --json scaling.json
cargo run --release -p raster-h3-core --example worker_scaling -- --budget-mb 2048 \
  --only 'res 11 (fine);res 7,9,11;res 9 + quantiles;categorical res 9'
```

## Results

### Default 64 MiB aggregation budget

| Workload | Cells | Wall at 1 worker (s) | Speedup at 4 workers | Merge share at 4 workers | Spill (s) |
| :--- | ---: | ---: | ---: | ---: | ---: |
| res 9 (~30 px/cell) | 361 k | 8.1 | 3.15× | 11.7% | 0 |
| res 7 (~1,500 px/cell) | 7.5 k | 6.7 | 3.69× | 0.7% | 0 |
| res 11 (< 1 px/cell) | 15.0 M | 25.2 | 1.43× | 71.8% | 13.2 |
| res 7, 9, 11 | 15.4 M | 41.5 | 1.52× | 60.7% | 16.6 |
| res 9 + quantiles | 361 k | 13.6 | 1.82× | 48.2% | 3.3 |
| categorical res 9 | 361 k | 8.7 | 2.38× | 10.7% | 0.2 |

`merge_ns` includes spilling a resolution map that outgrows its budget share, so the
large merge shares in this table are partly disk spills.

### 2 GiB aggregation budget: little or no spilling

| Workload | Wall at 1 worker (s) | Speedup at 4 workers | Merge at 4 workers (s) | Merge share at 4 workers | Spill at 4 workers (s) |
| :--- | ---: | ---: | ---: | ---: | ---: |
| res 11 | 23.8 | 1.28× | 12.2 | 65.4% | 0 |
| res 7, 9, 11 | 38.4 | 1.87× | 12.2 | 59.2% | 10.8 |
| res 9 + quantiles | 10.1 | 2.87× | 0.8 | 22.1% | 0 |
| categorical res 9 | 8.3 | 3.03× | 0.3 | 12.1% | 0 |

### Findings

1. **The parallel kernel stage scales well.** Without spilling it reaches 3.1–3.7× on 4
   cores, with 86–91% worker utilisation. With the default budget, two workloads (the
   multi-resolution and categorical runs) reached only 2.6–2.7×.
2. **Merge time does not depend on the worker count.** It is driven by the number of
   distinct cells. At ~30 pixels per cell it is 11–12% of wall time at 4 workers; at
   ~1,500 pixels per cell it is under 1%. When cells are about as numerous as pixels
   (resolution 11, or any multi-resolution run that includes it), it is 60–70%.
3. **Most of the merge is one function.** Each cell inserted into a horizon map for the
   first time gets an eviction key from `compute_cell_south_lat`, which builds the
   cell's boundary.
   - A micro-benchmark measured this at **~584 ns per cell**, against ~98 ns for the
     hash insert itself.
   - At resolution 11 (15 M cells) that is about 8.8 s of the 11–12 s merge.
   - At resolution 9 it is about 0.2 s of the 0.3 s merge.
   - The key depends only on the cell index, so it does not need to run on the merging
     thread.
4. **Spilling is a separate, budget-driven cost.**
   - With the default 64 MiB budget, the quantile workload spends 3.3 s spilling. With
     2 GiB it spills nothing and runs 3.5 s at 4 workers instead of 7.4 s.
   - Fine and multi-resolution runs still spill at 2 GiB once several workers hold
     large worker maps.
5. **No gain beyond 4 workers on 4 cores**, as expected. A machine with at least 8
   cores is needed for real 8-worker numbers.

## Decision

Do not parallelise the merge yet. Remove the dominant serial cost first:

1. **Take the eviction-key computation off the merging thread.** Either compute
   `compute_cell_south_lat` in the parallel workers when a cell first enters a worker
   map, or replace it with a cheap conservative bound: the cell centre's latitude
   minus a per-resolution maximum cell radius. The estimate is that this cuts the
   resolution-11 merge from ~12 s to ~3 s. The bound must stay at or south of the
   true southern edge, because eviction correctness depends on it.
2. **Document the spill cost.** Note in the aggregation-budget docs that quantile and
   fine-resolution workloads spill under the 64 MiB default, and that a larger
   `aggregation_budget_bytes` avoids it.
3. **Re-run this benchmark.** Parallelise the merge itself, sharded by cell hash (the
   horizon map already has 32 shards), only if it still exceeds 20% of wall time on a
   common workload.
