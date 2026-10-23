import sys
import time
from pathlib import Path
import raster_h3

if len(sys.argv) < 3:
    print("Usage: run_polars_res9.py <input_tif> <output_parquet>")
    sys.exit(1)

input_tif = sys.argv[1]
output_parquet = sys.argv[2]

print(f"Starting Polars benchmark: {input_tif} -> {output_parquet}")
t0 = time.time()
lf = raster_h3.scan_raster_h3(
    input_tif,
    resolutions=9,
    columns=["h3_index", "min", "max", "mean"]
)
lf.sink_parquet(output_parquet)
t1 = time.time()
print(f"Finished Polars benchmark in {t1 - t0:.2f}s")
