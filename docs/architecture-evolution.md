# Architecture Evolution & Engineering Lessons Learned

[← Back to README](../README.md) · [Core Engineering](engineering.md) · [Architecture Comparison](architecture-comparison.md) · [Optimal Raster Format](optimal-raster-format.md)

This document traces the chronological evolution of `raster_h3` from an initial single-threaded DuckDB prototype (`v0.1.0`) to a multi-engine geospatial workspace (`v0.4.0`) capable of aggregating 18+ billion pixels across the Continental United States (CONUS) into 74.4 million H3 hexagons in under 9 minutes on a single workstation.

More importantly, it documents the **engineering hypotheses that failed**, the **subtle geodetic and concurrency bugs uncovered under continental-scale stress testing**, and the **systems lessons learned** along the way.

---

## Executive Timeline: The Five Architectural Eras

```mermaid
flowchart LR
    P1["Phase 1 (v0.1.0)<br/>Scanline Horizon &<br/>In-Register Runs"] --> P2["Phase 2 (v0.2.0)<br/>Heuristic Lookahead,<br/>Binary Search & PMTiles"]
    P2 --> P3["Phase 3 (v0.3.0)<br/>Option 25 Lock-Free Pools,<br/>SIMD & Multi-Res Fusion"]
    P3 --> P4["Phase 4 (Oct 2026 Audit)<br/>Exact Indexing, Certified<br/>Eviction & Spill Budgets"]
    P4 --> P5["Phase 5 (v0.4.0)<br/>Multi-Engine Workspace &<br/>Zero-Copy Arrow Streams"]
```

| Era | Key Architectural Milestone | Primary Bottleneck Solved | Key Pivot / Lesson Learned |
| :--- | :--- | :--- | :--- |
| **Phase 1 (`v0.1.0`)** | Scanline horizon streaming + Welford online stats + In-register run accumulation | Python/GDAL memory explosion & per-pixel hash table lookups | 98% of consecutive pixels fall in the same H3 cell; CPU registers beat L1 cache. |
| **Phase 2 (`v0.2.0`)** | `H3ScanlineLookahead` (jump guessing + binary search) & PMTiles v3 pyramids | Spherical trigonometry cost & WebGL tile server dependencies | Heuristic jumping looked 2.6× faster on flat grids but had fatal geodetic re-entry flaws on curved projections. |
| **Phase 3 (`v0.3.0`)** | `DecodingBufferPool`, `OrderedPrefetchQueue`, SIMD predictors, 32-way sharding | OS `mmap`/`brk` allocator storms across 100k+ chunks & DuckDB lock contention | At 15B+ pixels, memory allocator churn and mutex contention eclipse raw trigonometric math. |
| **Phase 4 (`Oct 2026`)** | Stripped out lookahead for exact indexing; certified latitude bounds; external disk spilling | Geodetic boundary misassignment, rotated/conic eviction bugs, and unbounded RAM | Branchless sequential SIMD + exact indexing is **7–12% faster** than binary search lookahead and 100% mathematically sound. |
| **Phase 5 (`v0.4.0`)** | 6-crate workspace (`core`, `duckdb`, `python`, `cli`, `datafusion`, `pg`) + Arrow PyCapsule | Engine lock-in, Python ABI coupling, and WKB geometry serialization overhead | Pure C-ABI (`ctypes`) Arrow streams allow Polars & DuckDB to saturate hardware with zero intermediate copies. |

---

## Phase 1: Foundations — Moving Scanner Front & Register Accumulation (`v0.1.0`)

### What We Built
Traditional geospatial pipelines (`rasterio` + `h3-py`, or `gdal_polygonize` + PostGIS) materialize entire 2D coordinate meshgrids in RAM or convert rasters into millions of vector polygons before intersecting them with H3 cells. On a 24 GB raster, this exhausts memory almost immediately.

Phase 1 introduced two foundational primitives that remain at the heart of `raster_h3`:
1. **North-to-South Scanline Horizon Streaming:** Reading GeoTIFFs like a document scanner so completed hexagons north of the active scan front can be sealed and emitted immediately.
2. **In-Register Run Accumulation (Option 5):** Maintaining `run_cell`, `run_sum`, `run_min`, `run_max`, and `run_count` in local scalar variables (CPU registers) across contiguous horizontal pixels sharing the same H3 cell, flushing to the hash map only when a cell boundary is crossed.

### Lesson 1: Register Accumulation Eliminates ~98% of Hash Probes
At H3 Resolution 8 or 9 on a 30-meter raster, a single hexagon spans ~15 to 35 consecutive horizontal pixels. Updating a `HashMap<u64, H3Accumulator>` on every pixel caused continuous L1/L2 cache misses and hash computation overhead. Accumulating contiguous runs in registers reduced hash table probes by **~98%** and replaced conditional min/max branches with branchless hardware instructions (`minsd`/`maxsd`).

---

## Phase 2: The Heuristic Shortcuts Era — Lookahead & Binary Search (`v0.2.0`)

### What We Tried
Profiling showed that converting $(lat, lon)$ to a 64-bit H3 index (`h3o::LatLng::to_cell`) was the largest remaining CPU cost.
* **Attempt 1 (`SpatialCoherenceCache`):** Cached the last cell's center coordinate and radius, assuming adjacent pixels within 2 meters shared the same cell.
* **Attempt 2 (`H3ScanlineLookahead` — Commit `14bd48d`):** Replaced the cache with an algorithmic jump-and-search cursor:
  1. **Jump Guess:** Remember `prev_hex_width` (in pixels) from the previous hexagon and jump ahead by that distance along the scanline.
  2. **Convexity Assumption:** If the pixel at the jump destination resolves to the same H3 cell, assume all intermediate pixels belong to that cell and skip H3 indexing for the entire span.
  3. **Binary Search (`O(log2 N)`):** If the jump overshoots into a neighbor cell, binary-search between the start and destination pixels to find the exact cell boundary.

In commit `d3531fa`, this was extended to sub-pixel super-sampling (`find_core_span`), using a local linear Jacobian to approximate sub-pixel offsets around boundary pixels while bulk-accumulating the interior core.

### What Went Wrong (Uncovered in Phase 4)
While commit `14bd48d` initially measured a **~2.6× speedup** on synthetic flat test grids, real-world testing on continental rasters (`BP_CONUS.tif` in Albers Equal-Area Conic `EPSG:5070`) and adversarial geodetic audits exposed three fundamental flaws:

1. **The Geodetic Convexity Fallacy (Cell Re-entry):**
   H3 hexagons are convex in icosahedral Face-IJK space, **not in projected raster $(x, y)$ pixel space**. Under conic (`EPSG:5070`), transverse Mercator, or polar projections, a straight horizontal pixel scanline maps to a **curved geodesic arc** on the sphere. Near hexagon vertices or pentagons, a scanline can graze a cell, exit into a neighbor for a few pixels, and re-enter the original cell.
   * *Proof:* Regression test `floating_point_reentry_is_not_skipped` (commit `ea82c10`) demonstrated a 70-pixel scanline at $\text{lat} = -46.87655^\circ$ where native indexing changed cell at column 65, re-entered at column 70, and caused endpoint lookahead to silently misassign columns 65–69.
2. **Incremental Coordinate Drift (`lon += delta_lon`):**
   Stepping longitude by repeated floating-point addition (`lon += delta_lon`) across 156,335 columns accumulated IEEE-754 rounding error, shifting boundary assignments on wide continental strips compared to evaluating the exact affine formula `gt.pixel_to_coord(col, row)`.
3. **Branch Mispredictions & Stateful Cursor Overhead:**
   The binary search loop (`search_prefix`) introduced unpredictable data-dependent branches at every hexagon boundary, stalling out-of-order CPU pipelines and preventing LLVM from auto-vectorizing the coordinate loop.

---

## Phase 3: Continental Scale & Hardware Saturation (`v0.3.0` / Options 15–25)

When testing scaled from 257-Megapixel regional rasters (Hawaii `CFL_HI.tif`, 59 MB) to 15.8–18.2 Billion-pixel continental rasters (`BP_CONUS.tif`, 24.4 GB, 101,538 chunks), new systems bottlenecks emerged that had nothing to do with H3 geometry.

### Lesson 2: At 100,000+ Chunks, Allocator Churn Eclipses Math (Option 25)
* **The Bottleneck:** Decompressing 101,538 strip chunks (each ~625 KB) or 69,866 tiles (each ~1 MB) caused worker threads to allocate and drop multi-megabyte `Vec<f32>` buffers tens of thousands of times per query. On macOS and Linux, buffers exceeding the thread-local allocator cache triggered continuous kernel `mmap`/`munmap`/`madvise` syscalls and page-table lock contention across cores.
* **The Fix (Commit `688d72c` & `2c9e179`):** Implemented `DecodingBufferPool` backed by a lock-free `crossbeam_deque::Injector<DecodingResult>` with bounded retention and retry-on-contention stealing. Buffers are allocated once and recycled across the lifetime of the scan.
* **Impact:** Delivered an immediate **+8.3% to +9.7% end-to-end speedup** and eliminated kernel system-time spikes.

### Lesson 3: Single-Hop Ring Buffers Beat Collector Threads
* **The Bottleneck:** Early async prefetching used worker threads feeding an intermediate OS collector thread via dual MPSC channels to reorder chunks by latitude. The collector thread became a scheduling bottleneck, causing context-switch jitter.
* **The Fix:** Replaced with `OrderedPrefetchQueue<T>`, a single-hop bounded ring buffer where background decompression workers deposit decoded chunks directly into their sequence slot and wake the consumer without an intermediate thread.

### Lesson 4: DuckDB Worker Starvation & Multi-Batch Refills
* **The Bottleneck:** In DuckDB's parallel table function (`init_local`), all DuckDB execution threads competed for a single `Mutex<MultiScanHorizonStreamer>` every 2,048 rows (one DuckDB `DataChunk`). On 8–16 cores, threads spent most of their time blocked on the mutex.
* **The Fix (Commit `83b1415`):** Added a `ready_batches: Mutex<VecDeque<Vec<Record>>>` queue with **8,192-row coordinated refills** and thread-local `[u8; 16]` hex formatting buffers (`RasterH3LocalData`). One thread refills four chunks at a time while peer threads format and emit DuckDB vectors in parallel without holding the streamer lock.

### Lesson 5: Custom SIMD Decompression & Byte-Plane Predictors
* Standard `flate2` (miniz_oxide) and generic LZW decoders became CPU bottlenecks on compressed COGs.
* **The Fix (Commits `0a9ca30`, `70c9c8a`, `c4e4208`):**
  - Integrated `libdeflater` for whole-chunk SIMD DEFLATE decompression.
  - Built a zero-allocation LZW decoder writing directly into pre-allocated slice memory.
  - Implemented explicit **ARM NEON (`aarch64`) and x86 SIMD vectorization** for **TIFF Predictor 2** (prefix-sum horizontal differencing) and **TIFF Predictor 3** (4-way byte-plane de-interleaving and prefix-sum reconstruction for IEEE-754 floats).

### Lesson 6: Single-Pass Multi-Resolution Fusion
* **The Fix (Commit `d299777`):** Instead of reading and decompressing a 24 GB raster three times to build H3 Resolutions 7, 8, and 9, `MultiScanHorizonStreamer` transforms each pixel coordinate **once** and steps across the minimum common sub-span ($\text{step\_end} = \min_i(\text{span\_ends}[i])$), updating all target resolution accumulators in a single pass (**1.31× faster** than separate passes).

---

## Phase 4: The Geodetic & Concurrency Audit (`Oct 2026`)

In October 2026, a comprehensive geodetic, concurrency, and resource audit tested every assumption in the engine against PROJ 9.5/9.8 reference points, rotated/sheared affine transforms, antimeridian crossings, and strict memory limits.

### Lesson 7: Stripping Out Lookahead Made the Engine Both Exact and Faster
In commit `d6a92d2` (*"Refactor exact H3 indexing and budgets"*), the binary-search lookahead cursor (`H3ScanlineLookahead`) and incremental longitude stepping were completely removed in favor of **stateless, exact per-pixel indexing from absolute affine coordinates** combined with sequential span discovery (`as_chunks` loops).

When re-benchmarked on the 257-Megapixel Hawaii dataset (`CFL_HI.tif`, Apple M1 Pro, release build, median of 3–8 runs — commit `3eff849`):

| Benchmark Case | With Lookahead (`02ff37c`) | Exact Sequential Indexing (`3eff849`) | Result |
| :--- | ---: | ---: | :---: |
| **Continuous, center, Res 8** | 3.44 s | **3.19 s** | **7% faster** |
| **Continuous, center, Res 6 + 8 + 10** | 11.78 s | **10.39 s** | **12% faster** |
| **Continuous, RGSS 4-point, Res 8** | 9.78 s | **9.53 s** | **3% faster** |
| **Categorical, center, Res 8** | 22.77 s | **21.81 s** | **4% faster** |

On CONUS (`BP_CONUS.tif`, Albers EPSG:5070), removing lookahead and pairing exact indexing with Option 25 buffer pooling cut runtime from the lookahead-era **35–40 minutes** down to **15.1–17.8 minutes (~2× faster)** while guaranteeing 100% exact per-pixel cell assignment.

### Lesson 8: The Myth of the "15 MB Flat Memory" vs. Certified Eviction & Spilling
* **The Flaw:** Early documentation claimed active memory was strictly `< 15 MB` because scanlines move North-to-South. On conic projections (like CONUS Albers `EPSG:5070` where lines of latitude curve deeply across a 4,500 km strip), rotated grids, or uncertified custom PROJ strings, a pixel on row $r + 500$ near the eastern/western edge can have a higher latitude than a pixel on row $r$ near the central meridian ($-96^\circ$). Evicting based solely on row latitude silently split hexagons into duplicate partial records!
* **The Fix (Commits `cb6abd2`, `d6a92d2`, `a05ae4b`, `b00a417`):**
  1. **Certified Latitude Envelopes:** A cell is evicted *only* when its certified south-latitude bound strictly exceeds the maximum certified north-latitude bound of all remaining chunks (`eviction_north_bound`). Analytical bounds are mathematically certified for WGS84, Web Mercator, Albers, Lambert Conformal Conic, Polar Stereographic, EASE-Grid 2.0, and Transverse Mercator/UTM (including cone-apex and pole containment checks).
  2. **Budgeted External Disk Spilling (`SpillRuns`):** When curved projections or huge continental horizons accumulate more active cells than `aggregation_budget_bytes` (default 64 MiB), the engine flushes sorted partial runs to temporary disk files and performs an external $k$-way merge at EOF.
  3. **Three Independent Memory Budgets (`a5757e0`):** Separated `aggregation_budget_bytes` (64 MiB), `decode_budget_bytes` (256 MiB across queued, in-flight, and pooled chunks), and `compaction_budget_bytes` (bounding pending parent/child trees in `HierarchicalCompactor`).

### Lesson 9: The H3 Low-Bit Sharding Trap (`SplitMix64`)
* **The Bug (Commits `5992406`, `791b4bd`):** To reduce lock contention, resolution maps were partitioned into 32 shards (`ShardedResolutionMap`). Initially, shard selection used `h3_index & 31` or standard `FxHash`. However, in the H3 bit layout, **every unused finer resolution slot (3 bits per resolution up to 15) is filled with `111_2`**. For any H3 resolution $\le 13$, the lowest 6+ bits are identical (`0b111111...`) across **every single hexagon on Earth**!
* **Impact:** All hexagons at Resolutions 0–13 collapsed into a single shard (Shard 31), leaving 31 of 32 shards completely empty and causing severe hash collisions.
* **The Fix:** Replaced the shard selector and map hasher with a `SplitMix64` finalizer (`H3BuildHasher`) that mixes all 64 bits (including base cell and high-order face/IJK digits) down to the low bits, restoring uniform 32-way distribution and $O(1)$ shard byte accounting (`067d742`).

### Lesson 10: Rayon `pool.install()` Reentrancy Trap
* **The Bug:** To avoid deadlocks when called inside an existing Rayon thread, the controller checked `if rayon::current_thread_index().is_some()` and fell back to serial iteration. Consequently, any user or benchmark invoking `raster_h3` inside `pool.install(|| ...)` silently ran **single-threaded on 1 core** regardless of pool size!
* **The Fix (Commit `2caf9d0`):** Replaced global/caller pool reliance with `shared_aggregation_pool(threads)`—a dedicated, weak-referenced pool registry (`OnceLock<Mutex<HashMap<usize, Weak<ThreadPool>>>>`) that guarantees true multi-core execution without starving or deadlocking caller thread pools.

### Lesson 11: Geodetic Rigor — Units, Prime Meridians, Datums & Mosaic Footprints
* **CRS Fast-Path Validation (`1d38204`, `ddd55d9`, `b00a417`):** Fast analytical paths previously matched `+proj=aea` or `+proj=longlat` without checking unit overrides (`+units=us-ft`), prime meridians (`+pm=paris`), or datum shifts (`+towgs84`). The parser now tokenizes and validates every parameter, applies 7-parameter Helmert datum shifts (`+towgs84` and national grids like `EPSG:27700` OSGB36 and `EPSG:28992` Amersfoort), converts geographic degrees to radians before `proj4rs` fallback, and reads Transverse Mercator scale from GeoKey `3092` (`ProjScaleAtNatOriginGeoKey`) rather than `3076`.
* **Mosaic Footprint vs. Bounding Box (`2caf9d0`, `0aa8d13`):** In multi-file mosaics, a rotated or diamond-shaped tile's axis-aligned WGS84 bounding box is much larger than its actual valid pixel footprint. Using bounding boxes for `First` or `Cutline` ownership falsely suppressed valid pixels from neighboring tiles. Ownership now verifies actual inverse-affine footprint membership and handles antimeridian-crossing ($\pm 180^\circ$) bounding boxes consistently across SQL and native paths.

---

## Phase 5: Multi-Engine Workspace & Continental Benchmarks (`v0.4.0`)

In commit `0fd67df`, the monolithic repository was split into a modular 6-crate workspace:
* [`crates/raster-h3-core`](../crates/raster-h3-core): Pure-Rust streaming engine, CRS transformers, SIMD decoders, GeoParquet & PMTiles writers.
* [`crates/raster-h3-duckdb`](../crates/raster-h3-duckdb): Loadable DuckDB extension via stable C-API (`C_STRUCT` ABI).
* [`crates/raster-h3-python`](../crates/raster-h3-python): `ctypes` C-ABI library exposing zero-copy Arrow PyCapsule streams (`__arrow_c_stream__`) and Polars IO plugins (`register_io_source`).
* [`crates/raster-h3-cli`](../crates/raster-h3-cli): Standalone CLI for GeoParquet, PMTiles, Arrow IPC, and ClickHouse `JSONEachRow` streaming.
* [`crates/raster-h3-datafusion`](../crates/raster-h3-datafusion): Apache Arrow DataFusion `TableProvider` and scalar UDFs.
* [`crates/raster-h3-pg`](../crates/raster-h3-pg): PostgreSQL C-ABI extension (`pg_tle` / native SRF).

### CONUS Level 9 Multi-Engine Benchmark (Measured October 2026)

To validate the culmination of Phases 1–5 at full continental scale, we benchmarked converting the two USFS CONUS Wildfire Burn Probability rasters into H3 Resolution 9 Parquet files (~74.44 million hexagons) across **CLI**, **Polars 2.0.0**, and **DuckDB v1.5.5** on macOS Apple Silicon (release profile, profiled via Darwin kernel `/usr/bin/time -l`):

1. **`data/BP_CONUS.tif` (24.4 GB):** Native NAD83 / Conus Albers (`EPSG:5070`), $156,335 \times 101,538$ pixels (**15.87 billion pixels**), $156,335 \times 1$ strip chunks (101,538 chunks).
2. **`data/BP_CONUS_wgs84_tiled.tif` (21.0 GB):** Reprojected WGS84 (`EPSG:4326`), $197,514 \times 92,269$ pixels (**18.22 billion pixels**), $512 \times 512$ tiled COG (69,866 chunks).

#### 1. Native Projected Raster: `BP_CONUS.tif` (Albers EPSG:5070, 24.4 GB, 15.87B pixels)

| Metric | CLI Engine (`raster-h3 parquet`) | Polars 2.0.0 (`scan_raster_h3().sink_parquet()`) | DuckDB v1.5.5 (`COPY ... TO PARQUET`) |
| :--- | :--- | :--- | :--- |
| **Output Columns** | `h3_index`, `min_value`, `max_value`, `avg_value`, `sum_value`, `pixel_count`, `geometry` (WKB) | `h3_index`, `min`, `max`, `mean` | `h3_index`, `min`, `max`, `mean` |
| **H3 Row Count** | **74,440,878** | **74,440,878** | **74,440,878** |
| **Output File Size** | **6.2 GB** (GeoParquet 1.1) | **960 MB** (~12.9 B/row) | **1.4 GB** (~18.8 B/row) |
| **Wall-Clock Time** | **1,009.92 s** (~16.8 min) | **908.05 s** (~15.1 min) *(Fastest)* | **1,067.35 s** (~17.8 min) |
| **User / Sys CPU Time** | 4,399.34 s / 605.04 s | 4,333.14 s / 608.21 s | 5,557.49 s / 738.65 s |
| **Sustained Throughput** | 15.72 Mpix/s (~73,700 hex/s) | **17.48 Mpix/s** (~82,000 hex/s) | 14.87 Mpix/s (~69,700 hex/s) |
| **Peak RSS (Memory)** | **1,918.9 MB** (~1.87 GiB) | **2,458.6 MB** (~2.40 GiB) | **4,092.0 MB** (~4.00 GiB) |
| **Peak OS Footprint** | 94.2 MB | 122.0 MB | 98.4 MB |

#### 2. Tiled WGS84 Raster: `BP_CONUS_wgs84_tiled.tif` (WGS84 EPSG:4326, 21.0 GB, 18.22B pixels)

| Metric | CLI Engine (`raster-h3 parquet`) | Polars 2.0.0 (`scan_raster_h3().sink_parquet()`) | DuckDB v1.5.5 (`COPY ... TO PARQUET`) |
| :--- | :--- | :--- | :--- |
| **Output Columns** | `h3_index`, `min_value`, `max_value`, `avg_value`, `sum_value`, `pixel_count`, `geometry` (WKB) | `h3_index`, `min`, `max`, `mean` | `h3_index`, `min`, `max`, `mean` |
| **H3 Row Count** | **74,441,152** | **74,441,152** | **74,441,152** |
| **Output File Size** | **6.2 GB** (GeoParquet 1.1) | **987 MB** (~13.3 B/row) | **1.5 GB** (~20.1 B/row) |
| **Wall-Clock Time** | **596.53 s** (~9.9 min) | **532.40 s** (~8.9 min) | **517.87 s** (~8.6 min) *(Fastest)* |
| **User / Sys CPU Time** | 2,803.40 s / 405.91 s | 2,794.64 s / 410.49 s | 2,739.90 s / 391.05 s |
| **Sustained Throughput** | 30.55 Mpix/s (~124,800 hex/s) | 34.23 Mpix/s (~139,800 hex/s) | **35.19 Mpix/s** (~143,750 hex/s) |
| **Peak RSS (Memory)** | **2,264.6 MB** (~2.21 GiB) | **4,239.8 MB** (~4.14 GiB) | **4,154.8 MB** (~4.06 GiB) |
| **Peak OS Footprint** | 110.9 MB | 156.5 MB | 133.9 MB |

#### 3. Cross-Engine Numerical Parity
All three engines produced bit-identical cell counts and identical floating-point aggregation statistics across the 74.44 million cells:
* **Albers (`BP_CONUS.tif`):** `rows = 74,440,878`, `min = 0.0`, `max = 0.1351630985736847`, `avg = 0.0030984747703537`
* **WGS84 (`BP_CONUS_wgs84_tiled.tif`):** `rows = 74,441,152`, `min = 0.0`, `max = 0.1351628750562668`, `avg = 0.0030984656577561`

---

## Summary of Golden Engineering Rules

1. **Favor Branchless Sequential Loops Over Heuristic Skipping:** On modern superscalar CPUs, exact coordinate evaluation with register-level span accumulation beats branch-heavy binary search lookahead in both speed and geodetic correctness.
2. **Recycle Every Large Buffer:** In streaming pipelines processing tens of thousands of tiles, OS virtual memory syscalls (`mmap`/`madvise`) will dominate system CPU time unless decompression buffers are pooled and recycled.
3. **Never Trust Raw H3 Low Bits for Hashing:** Because H3 pads unused child resolutions with `111_2` bits, always pass H3 indices through a 64-bit avalanche finalizer (`SplitMix64`) before modulo/bitmask sharding.
4. **Certify Eviction Envelopes Mathematically:** Never assume pixel row order equals latitude order on projected or rotated grids; evict early only when certified by rigorous analytical bounds, and spill to disk when uncertified horizons exceed memory budgets.
5. **Push Column Projections All the Way to the Scan Kernel:** Skipping WKB polygon generation when only scalar statistics (`h3_index`, `min`, `max`, `mean`) are needed reduces output Parquet file sizes by **84% (6.2 GB → 960 MB)** and cuts total runtime by **~10–15%**.
6. **Physical Raster Layout Multiplies Throughput by 2×:** Converting striped projected GeoTIFFs into $512 \times 512$ tiled WGS84 COGs doubles end-to-end throughput (from ~17 Mpix/s to ~35 Mpix/s) by keeping active hexagon accumulators resident in L1/L2 cache and eliminating transcendental projection math.

---

## Future Performance Roadmap (Post-`v0.4.0` Opportunities)

Based on the CONUS Resolution 9 benchmark telemetry (`518s–596s` on WGS84 and `908s–1,067s` on Albers) and instruction-level inspection of the `v0.4.0` hot paths, the following optimizations represent the highest-leverage next steps, ranked by **Return on Investment (Impact vs. Complexity)**.

### Priority Matrix

| Priority | Optimization | Target Workload | Expected / Measured Speedup | Effort |
| :---: | :--- | :--- | :--- | :---: |
| **P0** | **Snyder Authalic Series + `rho_sq` + Row-Hoisted `y_p` in `AlbersConicFast`** | Projected rasters (`EPSG:5070`) | **1.19×–1.26×** (-20.3% User CPU / -879s CPU on CONUS; -41.1% Albers cycle overhead) | Low (~50 LOC) |
| **P1 (withdrawn)** | **Endpoint-Only Stride Probing** | Not safe without a geometric certificate | Not applicable | Requires a correctness proof |
| **P2** | **Row-Latitude Hoisting for `Wgs84Identity` + Specialized Single-Point Loop** | Native `EPSG:4326` / `EPSG:3857` | **1.01×–1.03×** (-0.7% cycles / -3.6 cyc/px on CONUS WGS84) | Low (~80 LOC) |
| **P3** | **Parallel Column Encoding + `BYTE_STREAM_SPLIT` in CLI Parquet** | CLI Parquet export | **+10–15% speed, 6× smaller files** | Medium (~150 LOC) |
| **P4** | **Row-Shared Trigonometric Precomputation in H3 Indexing** | North-up `EPSG:4326` / `EPSG:3857` | **+8–12%** on H3 indexing math | Medium (~150 LOC) |
| **P5** | **Projected SoA Eviction Slabs (Skip AoS `OutputBuffer`)** | DuckDB / Polars / DataFusion | **+5% speed, -25% Peak RSS (~1.2 GB)** | Medium (~200 LOC) |

---

### Tier 1: High-Impact Inner-Loop Optimizations

#### 1. Replace Newton-Raphson + `.ln()` in `AlbersConicFast` with Snyder's Authalic Series & Eliminate Redundant `sqrt()` (`P0`)
* **Where:** [`crates/raster-h3-core/src/crs/transformer.rs`](../crates/raster-h3-core/src/crs/transformer.rs) (`AlbersConicFast::transform_point`)
* **The Bottleneck:** On CONUS Albers (`EPSG:5070`), coordinate reprojection accounts for the entire ~400–550 second gap versus native WGS84. Currently, `AlbersConicFast::transform_point` executes:
  1. `let rho = (x_p * x_p + y_p * y_p).sqrt();` followed immediately by `rho * rho` (`rho` is never used unsquared).
  2. Two iterations of Newton-Raphson, invoking **6 transcendental functions per pixel** (`2× sin`, `2× cos`, `2× ln`) in addition to `atan2` and `asin`.
* **The Fix:**
  - Compute `rho_sq = x_p * x_p + y_p * y_p` directly, eliminating the hardware `fsqrt` instruction per pixel.
  - Replace the 2-iteration Newton-Raphson solver with a **4-term authalic latitude series** using ellipsoid constants prepared once per walker from an immutable projection borrow, evaluated via ordinary multiply/add Chebyshev/angle-doubling polynomials in $\sin\beta = q / q_p$ and $\cos\beta = \sqrt{1 - \sin^2\beta}$.
  - This removes the iterative `sin`, `cos`, and `ln` calls. Independent PROJ 9.5.1 forward-reference points verify inverse errors below **0.01 m**, including both signs of latitude through **89.9999°**, northern/southern parallels and false offsets. This is a tested range, not a uniform accuracy guarantee arbitrarily close to the poles, where inversion is ill-conditioned in floating point.

#### 2. Endpoint-Only Stride Probing (`P1`, withdrawn)
* **Where:** [`crates/raster-h3-core/src/aggregator/multi_horizon/walker.rs`](../crates/raster-h3-core/src/aggregator/multi_horizon/walker.rs)
* **Correctness finding:** Short probes have the same re-entry problem as the retired binary-search lookahead. A five-pixel scanline can index as `A, A, B, A, A`, including at resolution 9; equal endpoints do not certify intermediate assignments. Convexity in gnomonic face space does not make a projected raster row a straight line in that space.
* **Boundary finding:** Bisecting a three-column probe also cannot infer an unchecked pixel's cell from the midpoint and endpoint. For `A, A, A, A, A, B, A, C, C`, the midpoint is `A` and the next probe agrees with the `C` endpoint, but skipping the unchecked `B` still misassigns it to `A`.
* **Current implementation:** Index every valid sample at every requested resolution, then batch only verified same-cell spans. Keep the Albers authalic series, row-invariant projection hoisting, and single-point loop specialization. Any future skipping requires a conservative geometric certificate, not endpoint equality.

#### 3. Extend Row-Invariant Latitude Hoisting to `Wgs84Identity` & Specialize the Single-Point `walk` Loop (`P2`)
* **Where:** [`crates/raster-h3-core/src/aggregator/multi_horizon/walker.rs`](../crates/raster-h3-core/src/aggregator/multi_horizon/walker.rs) (`SampleProjector` and `walk`)
* **The Bottleneck:**
  1. Before these changes, `SampleProjector` hoisted row-constant latitudes **only** for `WebMercatorFast`, leaving `Wgs84Identity` (`EPSG:4326`) to recompute them per pixel.
  2. Inside `walk()`, even when `batch == true` (single-band center sampling, covering >95% of queries) and `resolutions.len() == 1`, every valid pixel clears and pushes to a heap-backed `positions: Vec<(LatLng, f64)>`, iterates over `ctx.sampling.points`, and loops over `cells: Vec<u64>`.
* **The Fix:**
  - Hoist row latitude (and Albers row $Y$ terms) whenever `gt.d == 0.0`, including horizontally sheared grids (`gt.b != 0.0`). A single row-cache enum selects WGS84, Mercator, prepared Albers, or the general fallback.
  - Use the specialized single-point loop when no bbox or mosaic-owner filter is needed, and share exact cell/run advancement with the general batch loop for one or many resolutions. Optimized projection and scan paths remain enabled under `stream-profile`; transform counters measure the per-sample cache path, excluding row setup.

---

### Tier 2: Engine, Serialization & Trigonometric Optimizations

#### 4. Parallel Column Encoding & `BYTE_STREAM_SPLIT` in the Native CLI Parquet Writer (`P3`)
* **Where:** [`crates/raster-h3-core/src/parquet/pipeline.rs`](../crates/raster-h3-core/src/parquet/pipeline.rs) and [`crates/raster-h3-core/src/parquet/writer.rs`](../crates/raster-h3-core/src/parquet/writer.rs)
* **The Bottleneck:**
  - In the CONUS benchmarks, the CLI engine was ~65–100s slower than Polars and DuckDB and produced **6.2 GB** files vs. Polars' **960 MB** files.
  - Currently, `pipeline.rs` sorts a single `perm: Vec<usize>` on one background thread, clones each column `Vec` sequentially in `reorder_by_perm`, and ZSTD-compresses 11 columns serially on that single thread. When a row group holds 1,000,000 rows, serial column encoding stalls the `sync_channel(1)` pipeline.
* **The Fix:**
  - Parallelize column permutation and Parquet column page compression across the shared Rayon pool (`par_iter_mut` over columns), and support column-projected / `--compact` schemas with `ZSTD(1)` + `BYTE_STREAM_SPLIT` encoding for floating-point statistics columns.

#### 5. Row-Shared Trigonometric Precomputation Inside H3 Indexing (`P4`)
* **Where:** [`crates/raster-h3-core/src/aggregator/h3_scanline.rs`](../crates/raster-h3-core/src/aggregator/h3_scanline.rs)
* **The Bottleneck:**
  - Inside `h3o::LatLng::to_cell`, converting `(lat, lon)` to a 3D unit sphere vector $(x, y, z)$ computes `lat.sin_cos()` and `lon.sin_cos()` on every pixel.
  - On north-up `EPSG:4326` and `EPSG:3857` rasters, `lat` is **identical for every pixel in a row**, causing `sin(lat)` and `cos(lat)` to be recomputed thousands of times per row for the same angle.
* **The Fix:**
  - Precompute `(sin_lat, cos_lat, z)` once per row on north-up geographic/Mercator grids, or evaluate `lon.sin_cos()` across 4-pixel vectors using portable SIMD (`f64x4` NEON/AVX2).

#### 6. Direct Projected SoA Eviction Slabs (Eliminating AoS `OutputBuffer` Transpose) (`P5`)
* **Where:** [`crates/raster-h3-core/src/aggregator/multi_horizon/lifecycle.rs`](../crates/raster-h3-core/src/aggregator/multi_horizon/lifecycle.rs) and [`crates/raster-h3-core/src/columnar/mod.rs`](../crates/raster-h3-core/src/columnar/mod.rs)
* **The Bottleneck:**
  - Evicted cells from `ShardedEvictionMap` are first materialized into an Array-of-Structs (`Vec<MultiContinuousRecord>`) inside `OutputBuffer`, and then transposed into Struct-of-Arrays (`ColumnArray` vectors for Polars/Arrow or `duckdb_data_chunk` vectors for DuckDB). Across 74.4M rows on CONUS, that AoS → SoA transpose copies ~6 GB of records through L3 cache.
* **The Fix:**
  - Evict from `ShardedEvictionMap` directly into columnar SoA slabs (`ColumnBatchSlab`) with column projection pushed down into eviction (e.g., when a query only selects `h3_index, min, max, mean`, never compute or copy `stddev`, `variance`, `sum`, `ValidCount`, or `Coverage`).

---

### Measured Results: Corrected `P0` + `P2` Implementation (`perf/p0-p1-snyder-stride` at `1daa33e`)

With endpoint stride probing (`P1`) removed so that every valid sample is indexed at every requested resolution, the branch retains **`P0`** (the 4-term Snyder/Chebyshev Horner series, `rho_sq` without `sqrt`, and precomputed reciprocals in `AlbersInverse`) and **`P2`** (row-invariant projection hoisting via `RowCache` whenever `gt.d == 0.0` plus the specialized single-point batch loop).

#### 1. Hawaii Continuous & Categorical Benchmark Suite (3-Run Interleaved Median, CLI)

| Workload | Baseline (`v0.4.0`) | Corrected `P0` + `P2` (`1daa33e`) | Speedup | Runtime Change | Cell & Value Parity (`1daa33e` vs. `v0.4.0`) |
| :--- | ---: | ---: | :---: | :---: | :--- |
| **CFL Albers (`EPSG:5070`), Res 8 [IPC]** | 3.010 s | **2.387 s** | **1.26×** | **-20.7%** | — |
| **CFL Albers (`EPSG:5070`), Res 9 [IPC]** | 3.515 s | **2.922 s** | **1.20×** | **-16.9%** | — |
| **CFL Albers (`EPSG:5070`), Res 6,8,10 [IPC]** | 10.945 s | **9.772 s** | **1.12×** | **-10.7%** | — |
| **CFL WGS84 (`EPSG:4326`), Res 8 [IPC]** | 2.104 s | **2.091 s** | **1.01×** | **-0.6%** | — |
| **CFL WGS84 (`EPSG:4326`), Res 9 [IPC]** | 2.686 s | **2.627 s** | **1.02×** | **-2.2%** | — |
| **CFL Albers (`EPSG:5070`), Res 8 [Parquet]** | 3.209 s | **2.625 s** | **1.22×** | **-18.2%** | 30,958 cells (1 px of 28,373,991 shifted across a cell edge by sub-nm Chebyshev vs. NR rounding) |
| **CFL Albers (`EPSG:5070`), Res 9 [Parquet]** | 3.733 s | **3.129 s** | **1.19×** | **-16.2%** | 212,029 cells (sub-nm Chebyshev vs. NR boundary rounding only) |
| **CFL WGS84 (`EPSG:4326`), Res 8 [Parquet]** | 2.055 s | **2.063 s** | **1.00×** | **+0.4%** | 30,959 cells (**100% bit-for-bit identical**) |
| **CFL WGS84 (`EPSG:4326`), Res 9 [Parquet]** | 2.739 s | **2.663 s** | **1.03×** | **-2.8%** | 212,042 cells (**100% bit-for-bit identical**) |
| **FBFM40 Categorical Albers, Res 8 [Parquet]** | 20.787 s | **16.876 s** | **1.23×** | **-18.8%** | 273,840 rows |
| **FBFM40 Categorical WGS84, Res 8 [Parquet]** | 17.955 s | **17.855 s** | **1.01×** | **-0.6%** | 273,840 rows (**100% bit-for-bit identical**) |

#### 2. Full CONUS Resolution 9 Benchmarks (Polars 2.0 IO Plugin, `/usr/bin/time -l` Hardware Counters)

| Dataset | Metric | Baseline (`v0.4.0`) | Corrected `P0` + `P2` (`1daa33e`) | Measured Improvement |
| :--- | :--- | ---: | ---: | :--- |
| **`BP_CONUS.tif`**<br/>*(Albers `EPSG:5070`, 24.4 GB, 15.87B px)* | **User CPU Time**<br/>**Sys CPU Time**<br/>**CPU Cycles Elapsed**<br/>**Instructions Retired**<br/>**Cycles per Pixel**<br/>**Wall-Clock Time**<br/>**Normalized Wall-Clock (at 5.44× cores)**<br/>**Albers / WGS84 Wall-Clock Ratio**<br/>**H3 Row Count** | 4,333.14 s<br/>608.21 s<br/>14.81 T<br/>31.10 T<br/>933.5 cyc/px<br/>908.05 s *(at 5.44× cores)*<br/>908.05 s<br/>1.71×<br/>74,440,878 | **3,454.04 s**<br/>**595.38 s**<br/>**12.09 T**<br/>**28.40 T**<br/>**762.0 cyc/px**<br/>**909.97 s** *(at 4.45× cores)*<br/>**~744.4 s**<br/>**1.44×**<br/>**74,440,878** | **-20.3% (-879.1 s / 1.25× less User CPU)**<br/>**-2.1% (-12.8 s)**<br/>**-18.4% (-2.72 T cycles / 1.23× fewer)**<br/>**-8.7% (-2.69 T instructions)**<br/>**-171.5 cyc/px (-41.1% Albers reprojection overhead)**<br/>+0.2% raw (lower thread scheduling concurrency)<br/>**~1.22× faster (-18.0% / -163.7 s)**<br/>**Albers gap cut from +71% to +44% over WGS84**<br/>Exact match |
| **`BP_CONUS_wgs84_tiled.tif`**<br/>*(WGS84 `EPSG:4326`, 21.0 GB, 18.22B px)* | **User CPU Time**<br/>**Sys CPU Time**<br/>**CPU Cycles Elapsed**<br/>**Instructions Retired**<br/>**Cycles per Pixel**<br/>**Wall-Clock Time**<br/>**Normalized Wall-Clock (at 6.02× cores)**<br/>**H3 Row Count** | 2,794.64 s<br/>410.49 s<br/>9.56 T<br/>23.67 T<br/>524.8 cyc/px<br/>532.40 s *(at 6.02× cores)*<br/>532.40 s<br/>74,441,152 | **2,780.77 s**<br/>**406.65 s**<br/>**9.50 T**<br/>**23.93 T**<br/>**521.2 cyc/px**<br/>**631.68 s** *(at 5.05× cores)*<br/>**~529.5 s**<br/>**74,441,152** | **-0.5% (-13.9 s User CPU)**<br/>**-0.9% (-3.8 s)**<br/>**-0.7% (-64.8 B cycles)**<br/>+1.1% (+261 B instructions)<br/>**-3.6 cyc/px (-0.7%)**<br/>+18.6% raw (5.05× vs. 6.02× core scheduling)<br/>**1.01× (-0.5% / -2.9 s)**<br/>Exact match |

---

### Historical Results: Original `P0` + `P1` Implementation (`4a48ba6`, With Endpoint Stride Probing)

These reported measurements used the original endpoint-probing implementation (`4a48ba6`), which was subsequently found to misassign pixels on short scanline re-entries and removed in `c8aa1de`. They are retained as historical data showing how much of the initial speedup came from skipping ~65–70% of `LatLng::to_cell` calls versus `P0` + `P2` alone.

#### 1. Full CONUS Resolution 9 Benchmarks (Polars 2.0, `/usr/bin/time -l`)

| Dataset | Metric | Baseline (`v0.4.0`) | P0 + P1 (`perf/p0-p1-snyder-stride`) | Measured Improvement |
| :--- | :--- | ---: | ---: | :--- |
| **`BP_CONUS.tif`**<br/>*(Albers `EPSG:5070`, 24.4 GB, 15.87B px)* | **Wall-Clock Time**<br/>**User CPU Time**<br/>**Sustained Throughput**<br/>**Peak RSS**<br/>**H3 Row Count** | 908.05 s (~15.1 min)<br/>4,333.14 s<br/>17.48 Mpix/s<br/>2,458.6 MB<br/>74,440,878 | **578.76 s (~9.6 min)**<br/>**1,983.15 s**<br/>**27.42 Mpix/s**<br/>**2,307.4 MB**<br/>**74,440,878** | **1.57× faster (-36.3% / -329.3 s)**<br/>**2.18× less User CPU (-54.2%)**<br/>**+56.9% throughput**<br/>**-6.2% (-151 MB)**<br/>**Matching row count only** |
| **`BP_CONUS_wgs84_tiled.tif`**<br/>*(WGS84 `EPSG:4326`, 21.0 GB, 18.22B px)* | **Wall-Clock Time**<br/>**User CPU Time**<br/>**Sustained Throughput**<br/>**Peak RSS**<br/>**H3 Row Count** | 532.40 s (~8.9 min)<br/>2,794.64 s<br/>34.23 Mpix/s<br/>4,239.8 MB<br/>74,441,152 | **421.11 s (~7.0 min)**<br/>**1,555.86 s**<br/>**43.27 Mpix/s**<br/>**2,193.9 MB**<br/>**74,441,152** | **1.26× faster (-20.9% / -111.3 s)**<br/>**1.80× less User CPU (-44.3%)**<br/>**+26.4% throughput**<br/>**-48.3% (-2.05 GB)**<br/>**Matching row count only** |

#### 2. Hawaii Continuous & Categorical Benchmark Suite (Median of 3 Runs)

| Workload | Baseline (`v0.4.0`) | P0 + P1 (`perf/p0-p1-snyder-stride`) | Speedup | Runtime Reduction | H3 Row Parity |
| :--- | ---: | ---: | :---: | :---: | :---: |
| **CFL Albers (`EPSG:5070`), Res 8 [IPC]** | 3.312 s | **1.554 s** | **2.13×** | **-53.1%** | — |
| **CFL Albers (`EPSG:5070`), Res 9 [IPC]** | 4.069 s | **2.073 s** | **1.96×** | **-49.1%** | — |
| **CFL Albers (`EPSG:5070`), Res 6,8,10 [IPC]** | 10.941 s | **9.194 s** | **1.19×** | **-16.0%** | — |
| **CFL WGS84 (`EPSG:4326`), Res 8 [IPC]** | 2.224 s | **1.300 s** | **1.71×** | **-41.5%** | — |
| **CFL WGS84 (`EPSG:4326`), Res 9 [IPC]** | 2.636 s | **1.944 s** | **1.36×** | **-26.3%** | — |
| **CFL Albers (`EPSG:5070`), Res 8 [Parquet]** | 3.176 s | **1.474 s** | **2.15×** | **-53.6%** | 30,958 (matching row count only) |
| **CFL Albers (`EPSG:5070`), Res 9 [Parquet]** | 3.738 s | **2.237 s** | **1.67×** | **-40.1%** | 212,029 (matching row count only) |
| **CFL WGS84 (`EPSG:4326`), Res 8 [Parquet]** | 2.337 s | **1.294 s** | **1.81×** | **-44.6%** | 30,959 (matching row count only) |
| **CFL WGS84 (`EPSG:4326`), Res 9 [Parquet]** | 2.642 s | **1.925 s** | **1.37×** | **-27.1%** | 212,042 (matching row count only) |
| **FBFM40 Categorical Albers, Res 8 [Parquet]** | 22.484 s | **8.848 s** | **2.54×** | **-60.6%** | 273,840 (matching row count only) |
| **FBFM40 Categorical WGS84, Res 8 [Parquet]** | 18.214 s | **10.053 s** | **1.81×** | **-44.8%** | 273,840 (matching row count only) |
