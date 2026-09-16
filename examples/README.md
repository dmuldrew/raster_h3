# Examples

Example binaries demonstrating the `raster_h3` library capabilities. Run any example with:

```bash
cargo run --example <name> --release
```

## Production CLI Utilities

| Example | Description |
|:---|:---|
| `raster_to_pmtiles` | Convert a GeoTIFF to a multi-resolution PMTiles v3 vector hexagon archive. |
| `parquet_to_pmtiles` | Convert an H3-indexed Parquet file to a PMTiles v3 archive. |

## Benchmarks

| Example | Description |
|:---|:---|
| `benchmark_e2e` | Comprehensive 6-suite end-to-end benchmark (continuous, categorical, supersampling, ROI, multi-res, concurrency). |
| `benchmark_scaling` | Rayon multi-core worker thread scaling on synthetic rasters of varying sizes. |
| `benchmark_deflate` | SIMD-accelerated `libdeflater` vs standard `flate2` decompression. |
| `benchmark_lzw` | `weezl`-accelerated LZW decompression vs standard TIFF decoder paths. |
| `benchmark_multi_resolution` | Single-pass multi-resolution fusion vs sequential multi-pass scans. |
| `benchmark_sampling_lookahead` | Sub-pixel super-sampling patterns and scanline lookahead acceleration. |
| `benchmark_features` | Multi-band spectral indices, H3 compaction, and predicate pushdown filtering. |

## Test & Verification

| Example | Description |
|:---|:---|
| `test_hawaii_files` | Real-world benchmark on Hawaii CFL and LANDFIRE FBFM40 datasets. |
| `test_mosaic_burn_probability` | Multi-tile mosaic ingestion using USFS Burn Probability GeoTIFF tiles. |
| `verify_pushdown` | Verifies spatial bounding box predicate pushdown with chunk-level pruning statistics. |
| `generate_sample` | Generates a synthetic WGS84 GeoTIFF with a radial elevation gradient for testing. |

## Debug Utilities

The `debug/` subdirectory contains developer diagnostic tools. See [`debug/README.md`](debug/README.md) for details.
