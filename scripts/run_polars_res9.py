import sys
import time
import raster_h3


def main():
    if len(sys.argv) != 3:
        print("Usage: run_polars_res9.py <input_tif> <output_parquet>")
        sys.exit(1)

    input_tif, output_parquet = sys.argv[1:]
    print(f"Starting raster benchmark: {input_tif} -> {output_parquet}")
    t0 = time.perf_counter()
    scan = raster_h3.scan_raster_h3(
        input_tif,
        resolutions=9,
        columns=["h3_index", "min", "max", "mean"],
    )
    if isinstance(scan, raster_h3.RasterH3ArrowStream):
        backend = "direct exporter (Polars IO plugin unavailable; full statistics schema)"
        raster_h3.raster_to_parquet(input_tif, output_parquet, resolutions=9, geoparquet=False)
    else:
        backend = "Polars IO plugin"
        scan.sink_parquet(output_parquet)
    elapsed = time.perf_counter() - t0
    print(f"Finished raster benchmark via {backend} in {elapsed:.2f}s")


if __name__ == "__main__":
    main()
