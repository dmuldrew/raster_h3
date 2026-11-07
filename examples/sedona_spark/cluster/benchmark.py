"""Time a partitioned raster_h3 scan on a Spark cluster: the data source plus the merge.

    python3 benchmark.py --workers 3 [--side 6144] [--tile 256] [--resolutions 9]
                         [--target-chunks 16] [--repeat 3] [--path raster.tif]

Without `--path`, a tiled synthetic float32 raster is written to RASTER_H3_SHARED_DIR (or
/tmp), where every executor must be able to read it. Prints one JSON line: the cluster's
workers and cores, the raster, the partition count and the median wall time of
`raster_h3.spark.read(...).count()` over the repeats (after one warm-up run).
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import raster_h3_spark_demo as demo  # noqa: E402


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--workers", type=int, help="worker count, recorded in the output")
    parser.add_argument("--path", help="raster to scan (visible to every executor)")
    parser.add_argument("--side", type=int, default=6144, help="synthetic raster side in pixels")
    parser.add_argument("--tile", type=int, default=256, help="synthetic raster tile size")
    parser.add_argument("--resolutions", default="9")
    parser.add_argument("--target-chunks", type=int, default=16)
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args()

    import raster_h3
    import raster_h3.spark

    path = args.path
    if path is None:
        shared = Path(os.environ.get("RASTER_H3_SHARED_DIR", "/tmp"))
        path = str(shared / f"benchmark_{args.side}_{args.tile}.tif")
        if not os.path.exists(path):
            demo.write_sample(path, side=args.side, tile=args.tile)
    resolutions = [int(r) for r in args.resolutions.split(",")]
    partitions = len(raster_h3.plan_partitions(path, target_chunks=args.target_chunks))

    spark = demo.sedona_session(os.environ.get("SPARK_MASTER", "local[*]"), shuffle_partitions=partitions)
    spark.sparkContext.setLogLevel("ERROR")
    status = spark.sparkContext._jsc.sc().getExecutorMemoryStatus()
    executors = status.size() - 1  # minus the driver
    cores = spark.sparkContext.defaultParallelism

    def run() -> tuple:
        start = time.perf_counter()
        cells = raster_h3.spark.read(
            spark, path, resolutions=resolutions, target_chunks=args.target_chunks
        ).count()
        return time.perf_counter() - start, cells

    run()  # warm-up: Python workers, JIT, file cache
    times, cells = [], 0
    for _ in range(args.repeat):
        seconds, cells = run()
        times.append(seconds)
    print(
        json.dumps(
            {
                "workers": args.workers,
                "executors": executors,
                "cores": cores,
                "pixels": args.side * args.side if args.path is None else None,
                "path": path,
                "resolutions": resolutions,
                "partitions": partitions,
                "cells": cells,
                "median_s": round(statistics.median(times), 3),
                "runs_s": [round(t, 3) for t in times],
            }
        ),
        flush=True,
    )
    spark.stop()


if __name__ == "__main__":
    main()
