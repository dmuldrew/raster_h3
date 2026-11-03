"""Aggregate GeoTIFFs to H3 on Spark executors with raster_h3, then query them with Sedona.

Each raster file becomes one Spark task: an executor's Python worker streams the file
through raster_h3 and returns Arrow batches through `mapInArrow`. A whole file per task
keeps every hexagon in one task, so the aggregates are exact without a shuffle.
(Splitting one large raster across tasks needs owned-region filtering in the core first.)

Inside the `sedona` compose service:

    docker compose exec sedona python3 /opt/raster_h3/examples/raster_h3_spark_demo.py
    docker compose exec sedona python3 /opt/raster_h3/examples/raster_h3_spark_demo.py \
        '/data/tiles/*.tif' --resolution 7

Set SPARK_MASTER=spark://localhost:7077 to run on the container's standalone cluster
(started by the image's entrypoint) instead of in local mode.
"""

from __future__ import annotations

import argparse
import glob
import os
from typing import Iterator

import pyarrow as pa
from sedona.spark import SedonaContext

# Spark has no unsigned integer types, so the Arrow output is narrowed to signed ones.
# H3 cell indexes never set the top bit, so UInt64 -> Int64 keeps every value.
SPARK_SCHEMA = (
    "path string, h3_index long, mean double, stddev double, count double, "
    "min double, max double, resolution short, wkb binary"
)
ARROW_SCHEMA = pa.schema(
    [
        ("path", pa.string()),
        ("h3_index", pa.int64()),
        ("mean", pa.float64()),
        ("stddev", pa.float64()),
        ("count", pa.float64()),
        ("min", pa.float64()),
        ("max", pa.float64()),
        ("resolution", pa.int16()),
        ("wkb", pa.binary()),
    ]
)
COLUMNS = [f.name for f in ARROW_SCHEMA if f.name != "path"]


def aggregate_files(resolution: int):
    """Build the `mapInArrow` function: rows of `path` in, H3 cell batches out."""

    def run(batches: Iterator[pa.RecordBatch]) -> Iterator[pa.RecordBatch]:
        import raster_h3  # imported on the executor

        for batch in batches:
            for path in batch.column("path").to_pylist():
                stream = raster_h3.RasterH3ArrowStream(path, resolution, columns=COLUMNS)
                for cells in pa.RecordBatchReader.from_stream(stream):
                    paths = pa.array([path] * cells.num_rows, pa.string())
                    columns = [paths] + [
                        cells.column(f.name).cast(f.type) for f in ARROW_SCHEMA if f.name != "path"
                    ]
                    yield pa.RecordBatch.from_arrays(columns, schema=ARROW_SCHEMA)

    return run


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "paths",
        nargs="*",
        default=[os.environ.get("RASTER_H3_SAMPLE", "/opt/raster_h3/sample_sf.tif")],
        help="GeoTIFF paths or glob patterns (visible to every executor)",
    )
    parser.add_argument("--resolution", type=int, default=8)
    args = parser.parse_args()

    paths = sorted({p for pattern in args.paths for p in (glob.glob(pattern) or [pattern])})

    config = (
        SedonaContext.builder()
        .master(os.environ.get("SPARK_MASTER", "local[*]"))
        .appName("raster_h3 on Sedona")
        .getOrCreate()
    )
    sedona = SedonaContext.create(config)

    files = sedona.createDataFrame([(p,) for p in paths], "path string").repartition(len(paths))
    cells = files.mapInArrow(aggregate_files(args.resolution), SPARK_SCHEMA)
    cells = cells.selectExpr(
        "path", "h3_index", "mean", "stddev", "count", "min", "max", "resolution",
        "ST_GeomFromWKB(wkb) AS geom",
    )
    cells.cache().createOrReplaceTempView("cells")

    print(f"Aggregated {len(paths)} file(s) at resolution {args.resolution}:")
    sedona.sql(
        "SELECT path, count(*) AS cells, round(avg(mean), 2) AS avg_mean, "
        "round(min(min), 2) AS min, round(max(max), 2) AS max FROM cells GROUP BY path"
    ).show(truncate=False)

    # raster_h3 and Sedona (h3-java) share the same 64-bit cell ids, so Sedona's own H3
    # functions join straight onto the aggregated cells.
    print("Cells in a 2 km buffer around a point, via Sedona's ST_H3CellIDs:")
    sedona.sql(
        f"""
        WITH area AS (
            SELECT explode(ST_H3CellIDs(
                ST_Buffer(ST_Centroid(ST_Envelope_Aggr(geom)), 0.02), {args.resolution}, true
            )) AS h3_index
            FROM cells
        )
        SELECT c.h3_index, round(c.mean, 2) AS mean, c.count,
               -- raster_h3's hexagon vs Sedona's polygon for the same id: ~0 when they agree
               ST_Distance(ST_Centroid(c.geom),
                           ST_Centroid(ST_H3ToGeom(array(c.h3_index))[0])) AS centroid_gap_deg
        FROM cells c JOIN area a ON c.h3_index = a.h3_index
        ORDER BY c.mean DESC
        """
    ).show(10, truncate=False)


if __name__ == "__main__":
    main()
