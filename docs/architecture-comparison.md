# Architectural Comparison with Other Approaches

[← Back to README](../README.md)

This document compares the architectural design and performance characteristics of `raster_h3` against alternative geospatial processing approaches (Python, PostGIS, and GDAL), examines trade-offs across execution environments (local workstations, cloud servers, and containers), and details empirical streaming benchmarks.

## Structural Trade-Off Matrix

| Dimension | Python (`rasterio` + `h3-py` + `pyproj`) | PostGIS (`raster2pgsql` + `ST_H3_Polyfill`) | GDAL CLI (`gdal_polygonize` + `ogr2ogr`) | `raster_h3` (Native DuckDB) |
| :--- | :--- | :--- | :--- | :--- |
| **Execution Environment** | Python interpreter with C-extension FFI | PostgreSQL database daemon | External CLI toolchain | **Embedded inside DuckDB query engine** |
| **Memory Architecture** | Allocates full 2D coordinate meshgrids in RAM | Subject to PostgreSQL shared buffer limits | Allocates intermediate polygon geometries | **Bounded O(Scan Front) < 15 MB RAM** |
| **Coordinate Transforms** | Evaluated per pixel independently | Evaluated per geometry | Evaluated during polygonization | **Row-constant hoisting (1 transform / row)** |
| **H3 Index Calculation** | Per-pixel C/FFI boundary crossings | Point-in-polygon spatial queries | Geometry intersection & rasterization | **Scanline lookahead + run accumulation** |
| **Intermediate Storage** | NumPy arrays or temporary scratch files | Database table storage & index bloat | Multi-gigabyte shapefiles / GeoJSON | **Zero intermediate files (direct stream)** |
| **Multi-Resolution Sync** | Separate processing passes per resolution | Separate queries with parent rollups | Separate polygonization runs | **Single-pass multi-resolution streaming** |
| **Web Tile Output** | Requires external `tippecanoe` + tile server | Requires MVT server (Martin/Tegola) | Requires tiling toolchain | **Direct in-memory PMTiles v3 export** |

## Detailed Architectural Nuances

### 1. Python Pipelines (`rasterio` + `h3-py` / `scipy` / `numpy`)
- **Coordinate Meshgrid Allocations**: `rasterio.transform.xy` and `pyproj.Transformer` allocate 2D floating-point arrays for X, Y, Lat, and Lon (40+ bytes per pixel), requiring gigabytes of RAM for large rasters.
- **Per-Pixel C/FFI Crossing Overhead**: Calling `h3.latlng_to_cell()` millions of times invokes Python C/ctypes wrapper overhead on every call, allocating individual heap objects.
- **Redundant Trigonometry**: Evaluates projection math independently on all pixels without scanline hoisting.
- **Single-Threaded GIL**: Python loops cannot fully saturate modern multi-core processors without multiprocessing IPC serialization overhead.

### 2. PostGIS & Traditional Spatial SQL
- Requires importing rasters via `raster2pgsql`, introducing database storage expansion.
- Relies on spatial polygon intersection tests rather than bitwise mathematical index transformations.
- Data serialization between database processes limits throughput.

### 3. GDAL Vector Polygonization
- `gdal_polygonize` generates intermediate vector polygon layers with topology validation before spatial binning, producing large temporary files on disk.

---

## Execution Environments: Local Native vs. Cloud vs. Containerized

`raster_h3` is architected to saturate hardware across all platforms, but throughput varies significantly depending on how the runtime interacts with the CPU vector engine, memory hierarchy, and OS kernel:

| Dimension | Local Native Workstation | High-Core Cloud Server | Desktop Container (Docker) |
| :--- | :--- | :--- | :--- |
| **Typical Setup** | Apple M-Series (M1–M4), AMD Zen 4/5, Intel Ultra | AWS Graviton3/4 (`c7g`/`c8g`), OCI Ampere Altra | Docker Desktop with mounted volume (`-v $(pwd):/data`) |
| **SIMD Execution** | Direct hardware NEON (4 pipelines/core) or 512-bit AVX-512 | Native NEON / SVE2 across server cores | Virtualized guest instructions; emulation penalty if cross-arch |
| **Memory Bandwidth** | **Ultra-low-latency unified memory** | High-bandwidth multi-channel DDR5 | Guest VM page tables + hypervisor SLAT translation |
| **File I/O Path** | Direct OS page cache (APFS / NVMe) | High-throughput EBS or direct S3 Nitro (25–50 Gbps) | Virtual filesystem bridge (VirtioFS / gRPC-FUSE) |
| **Throughput (Hawaii Res 9)** | **Highest per-core throughput** (rapid interactive turnaround) | **Linear multi-core scaling** (sub-second execution at scale) | **Reduced by I/O virtualization** (longer execution time from volume bridge) |
| **Best Use Case** | Interactive analysis, local DuckDB CLI, data exploration | Massive fleet processing, automated batch pipelines | Reproducible CI/CD, portable deployments, isolated tests |

### Key Performance Drivers:

1. **Why Local Native Outperforms Desktop Containers (~3x to 5x faster)**:
   - **Zero Hypervisor I/O Overhead**: In Docker Desktop on macOS or Windows, reading large raster chunks across host-mounted volumes incurs file-sharing translation penalties over VirtioFS or gRPC-FUSE. Native binaries stream directly from local NVMe through the OS page cache.
   - **Asymmetric Core Scheduling**: Modern workstations often combine high-performance (P) and high-efficiency (E) cores. Native schedulers (like macOS Grand Central Dispatch) pin heavy compute threads to wide P-cores. Guest Linux VMs inside Docker treat all vCPUs uniformly, which can cause Rayon worker stragglers on low-power E-cores.
   - **Direct SIMD & Unified Memory**: Modern workstation CPUs with wide SIMD units and low-latency unified memory feed the 8-lane SIMD span accumulator with zero bus stalls.

2. **When to Choose Cloud Instances**:
   - **Horizontal Scale**: While a single workstation core is faster than a cloud server core, cloud instances scale to **64 to 96 physical cores** (e.g., `c7g.16xlarge`, `c8g.24xlarge`), processing all ~9 billion pixels of CONUS in **under 2 minutes** for roughly ten cents.
   - **Cloud-Native S3 COGs**: When rasters reside in S3, EC2 instances bypass local disk entirely via 25–50 Gbps Nitro networking and parallel HTTP range-requests.
   - **Always-Free Background Cloud**: Free cloud tiers (such as Oracle Cloud’s 4-core Ampere Altra A1 with 24 GB RAM) provide a stable, zero-cost 24/7 environment that runs ~1.5x faster than desktop Docker without consuming local laptop battery.

## Real-World Performance Benchmarks: Option 25 Throughput & Scalability

To evaluate hardware-saturating performance, benchmarks were conducted on full-scale 257-Megapixel regional rasters (State of Hawaii, 30-meter resolution):
- **Continuous Surface**: `CFL_HI.tif` ($16,384 \times 16,384$ pixels = 268.4M pixels), IEEE 754 Float32, LZW compression, 268 MB on disk.
- **Categorical Surface**: `LF2024_FBFM40_HI.tif` ($16,384 \times 16,384$ pixels = 268.4M pixels), Int16, Deflate/Zlib compression, 40 LANDFIRE fuel models.

Tests were executed on an 8-core ARM64 workstation with 16 GB unified memory running the native release build:

| Benchmark Scenario | Baseline Engine | Option 25 (Lock-Free Pool + Direct Prefetch) | Relative Speedup | Sustained Processing Throughput |
| :--- | :---: | :---: | :---: | :---: |
| **Continuous Ingestion (Res 8, 1-pass)** | 2.41 s | **2.21 s** | **+8.3% faster** | **~121.5M pixels / sec** (~82,000 hex/s) |
| **Categorical Ingestion (Res 8, Wide Format)** | 3.14 s | **2.96 s** | **+5.7% faster** | **~90.7M pixels / sec** (~61,000 hex/s) |
| **Dual-Pyramid Stream (Res 7 & 8 single pass)** | 4.32 s | **3.90 s** | **+9.7% faster** | **~68.8M pixels / sec** (~115,000 hex/s combined) |
| **Shannon Landscape Entropy Calculation** | 0.38 s | **0.30 s** | **+21.1% faster** | **~894.7M pixels / sec** |
| **Active Peak RAM Usage** | < 15 MB | **< 15 MB** | **Bounded O(Scan Front)** | Strictly flat memory profile |

### Engineering Analysis:
1. **Minimized Allocation Churn**: By recycling decompression buffers via `DecodingBufferPool`, repetitive buffer allocations and OS page mappings are minimized during sustained streaming while strictly bounding retained idle memory.
2. **Context-Switch Reduction**: Connecting background decompression workers directly to the aggregator ring buffer in `OrderedPrefetchQueue<T>` eliminates intermediate collector threads, cutting thread context switches and latency jitter.
3. **Hardware Saturation**: Ingestion speeds approach the raw memory-bandwidth and hardware decompression limits of NVMe storage and modern SIMD vector engines, ensuring zero database CPU waste.
