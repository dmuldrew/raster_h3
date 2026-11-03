"""Aggregate rasters to H3 on Spark with raster_h3's planned partitions, then query with Sedona.

The driver plans each raster into partitions from metadata only
(`raster_h3.plan_partitions`). Every partition becomes one Spark task that scans a disjoint
set of TIFF tiles or strips on an executor (`mapInArrow`), so no pixel is read twice:

1. Cells that only one partition can reach come out final (`partial_state` is null).
2. Cells on a partition edge come out as partial states. They are shuffled by `h3_index`
   and merged with `raster_h3.merge_partials`, a small fraction of the output.

The union is exactly what one scan of each raster produces (`--verify` checks this).

Inside the `sedona` compose service:

    docker compose exec sedona python3 /opt/raster_h3/examples/raster_h3_spark_demo.py --verify
    docker compose exec sedona python3 /opt/raster_h3/examples/raster_h3_spark_demo.py \\
        '/data/tiles/*.tif' --resolutions 7 --target-chunks 256

Without paths, a tiled synthetic raster is generated (2048 x 2048 px in 64 tiles of 256 px).
Real rasters must be tiled or striped (COGs are tiled) for a file to split into more than one
partition; aim for a few hundred chunks per partition. Set SPARK_MASTER=spark://localhost:7077
to run on the container's standalone cluster instead of in local mode.
"""

from __future__ import annotations

import argparse
import glob
import os
import struct
import tempfile
from typing import Iterator, List

import pyarrow as pa
import pyarrow.compute as pc
from sedona.spark import SedonaContext

# Spark has no unsigned integer types, so the Arrow output is narrowed to signed ones.
# H3 cell indexes never set the top bit, so UInt64 -> Int64 keeps every value.
ARROW_SCHEMA = pa.schema(
    [
        ("path", pa.string()),
        ("h3_index", pa.int64()),
        ("resolution", pa.int16()),
        ("mean", pa.float64()),
        ("stddev", pa.float64()),
        ("count", pa.float64()),
        ("min", pa.float64()),
        ("max", pa.float64()),
        ("wkb", pa.binary()),
        ("partial_state", pa.binary()),
    ]
)
SPARK_SCHEMA = (
    "path string, h3_index long, resolution short, mean double, stddev double, count double, "
    "min double, max double, wkb binary, partial_state binary"
)
# Columns requested from raster_h3; `partial_state` is added by partitioned scans.
COLUMNS = ["h3_index", "resolution", "mean", "stddev", "count", "min", "max", "wkb"]


def to_spark(cells: pa.RecordBatch, path: str) -> pa.RecordBatch:
    """raster_h3 output (unsigned keys, optional `partial_state`) in the Spark schema."""
    n = cells.num_rows
    names = cells.schema.names
    columns = []
    for field in ARROW_SCHEMA:
        if field.name == "path":
            columns.append(pa.array([path] * n, pa.string()))
        elif field.name in names:
            columns.append(cells.column(names.index(field.name)).cast(field.type))
        else:
            columns.append(pa.nulls(n, field.type))  # merged rows carry no partial state
    return pa.RecordBatch.from_arrays(columns, schema=ARROW_SCHEMA)


def scan_partitions(resolutions: List[int]):
    """`mapInArrow`: rows of (path, spec) in, that partition's cells out."""

    def run(batches: Iterator[pa.RecordBatch]) -> Iterator[pa.RecordBatch]:
        import raster_h3  # imported on the executor

        for batch in batches:
            for path, spec in zip(batch.column("path").to_pylist(), batch.column("spec").to_pylist()):
                stream = raster_h3.RasterH3ArrowStream(path, resolutions, columns=COLUMNS, partition=spec)
                for cells in pa.RecordBatchReader.from_stream(stream):
                    yield to_spark(cells, path)

    return run


def merge_boundary_cells(batches: Iterator[pa.RecordBatch]) -> Iterator[pa.RecordBatch]:
    """`mapInArrow` after a shuffle by (path, h3_index): partial rows in, final cells out.

    Every partial row of a cell lands in the same Spark partition, so each group can be
    merged on its own. Rasters are merged separately: they are independent scans.
    """
    import raster_h3

    table = pa.Table.from_batches(list(batches), schema=ARROW_SCHEMA)
    if table.num_rows == 0:
        return
    for path in pc.unique(table.column("path")).to_pylist():
        rows = table.filter(pc.equal(table.column("path"), path))
        for merged in raster_h3.merge_partials(rows, columns=COLUMNS).to_batches():
            yield to_spark(merged, path)


def write_sample(path: str, side: int = 2048, tile: int = 256) -> None:
    """A tiled float32 GeoTIFF (EPSG:4326) of synthetic terrain near San Francisco."""
    import numpy as np

    y, x = np.mgrid[0:side, 0:side].astype(np.float32)
    z = 300 + 120 * np.sin(x / 97) * np.cos(y / 131) + 40 * np.sin((x + y) / 23)
    z = z.astype("<f4")
    across = side // tile
    tiles = [
        z[r * tile : (r + 1) * tile, c * tile : (c + 1) * tile].tobytes()
        for r in range(across)
        for c in range(across)
    ]
    pixel, origin = 0.0002, (-122.60, 37.90)
    blobs = [
        struct.pack("<3d", pixel, pixel, 0.0),
        struct.pack("<6d", 0, 0, 0, origin[0], origin[1], 0),
        struct.pack("<12H", 1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326),
        struct.pack(f"<{len(tiles)}I", *(len(t) for t in tiles)),
        b"\0" * 4 * len(tiles),  # tile offsets, filled in below
    ]
    entries = [  # (tag, type, count, value or blob index); SHORT=3, LONG=4, DOUBLE=12
        (256, 4, 1, side), (257, 4, 1, side), (258, 3, 1, 32), (259, 3, 1, 1), (262, 3, 1, 1),
        (277, 3, 1, 1), (284, 3, 1, 1), (322, 4, 1, tile), (323, 4, 1, tile),
        (324, 4, len(tiles), 4), (325, 4, len(tiles), 3), (339, 3, 1, 3),
        (33550, 12, 3, 0), (33922, 12, 6, 1), (34735, 3, 12, 2),
    ]  # fmt: skip
    offset = 8 + 2 + 12 * len(entries) + 4
    blob_at = []
    for blob in blobs:
        blob_at.append(offset)
        offset += len(blob)
    tile_at = []
    for t in tiles:
        tile_at.append(offset)
        offset += len(t)
    blobs[4] = struct.pack(f"<{len(tiles)}I", *tile_at)
    out = bytearray(b"II*\x00" + struct.pack("<IH", 8, len(entries)))
    for tag, typ, count, val in entries:
        if tag in (324, 325, 33550, 33922, 34735):
            val = blob_at[val]
        if typ == 3 and count == 1:
            out += struct.pack("<HHIHH", tag, typ, count, val, 0)
        else:
            out += struct.pack("<HHII", tag, typ, count, val)
    out += struct.pack("<I", 0)
    for chunk in blobs + tiles:
        out += chunk
    with open(path, "wb") as f:
        f.write(out)


def verify(cells, paths: List[str], resolutions: List[int]) -> None:
    """Compare the distributed result with one unpartitioned scan of each raster."""
    import raster_h3

    got = cells.toArrow()

    def means(table):
        keys = zip(table["resolution"].to_pylist(), table["h3_index"].to_pylist())
        return dict(zip(keys, table["mean"].to_pylist()))

    for path in paths:
        mine = got.filter(pc.equal(got.column("path"), path))
        want = pa.table(raster_h3.RasterH3ArrowStream(path, resolutions, columns=COLUMNS))
        a, b = means(want), means(mine)
        assert len(b) == mine.num_rows, f"{path}: a cell appears twice"
        assert a.keys() == b.keys(), (
            f"{path}: {len(a.keys() - b.keys())} cells missing, {len(b.keys() - a.keys())} extra"
        )
        worst = max(abs(a[k] - b[k]) for k in a)
        assert worst < 1e-9 * max(1.0, max(abs(v) for v in a.values())), f"{path}: means differ by {worst}"
        print(f"verified {path}: {len(a)} cells identical to a single scan (max |mean difference| {worst:.1e})")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("paths", nargs="*", help="GeoTIFF paths or glob patterns (visible to every executor)")
    parser.add_argument("--resolutions", default="8,9", help="comma-separated H3 resolutions")
    parser.add_argument("--target-chunks", type=int, default=8, help="TIFF tiles or strips per partition")
    parser.add_argument("--verify", action="store_true", help="compare with an unpartitioned scan")
    args = parser.parse_args()
    resolutions = [int(r) for r in args.resolutions.split(",")]

    import raster_h3

    paths = sorted({p for pattern in args.paths for p in (glob.glob(pattern) or [pattern])})
    if not paths:
        sample = os.path.join(tempfile.gettempdir(), "raster_h3_demo_terrain.tif")
        if not os.path.exists(sample):
            write_sample(sample)
        paths = [sample]

    # 1. Plan on the driver: metadata only.
    tasks = [(p, spec) for p in paths for spec in raster_h3.plan_partitions(p, target_chunks=args.target_chunks)]
    print(f"{len(paths)} raster(s) planned into {len(tasks)} partitions of ~{args.target_chunks} chunks")

    config = (
        SedonaContext.builder()
        .master(os.environ.get("SPARK_MASTER", "local[*]"))
        .appName("raster_h3 partitioned scan on Sedona")
        .config("spark.sql.shuffle.partitions", str(max(1, len(tasks))))
        .getOrCreate()
    )
    sedona = SedonaContext.create(config)

    # 2. Scan: one task per partition.
    specs = sedona.createDataFrame(tasks, "path string, spec string").repartition(len(tasks))
    scanned = specs.mapInArrow(scan_partitions(resolutions), SPARK_SCHEMA).cache()

    # 3. Merge: final cells pass through; boundary cells are shuffled by cell and merged.
    final = scanned.where("partial_state IS NULL")
    partial = scanned.where("partial_state IS NOT NULL")
    merged = partial.repartition("path", "h3_index").mapInArrow(merge_boundary_cells, SPARK_SCHEMA)
    cells = final.unionByName(merged).drop("partial_state").cache()

    n_final, n_partial, n_cells = final.count(), partial.count(), cells.count()
    print(
        f"{n_cells} cells: {n_final} final from their partition, {n_cells - n_final} merged "
        f"from {n_partial} partial rows ({100 * (n_cells - n_final) / max(n_cells, 1):.1f}% of cells)"
    )
    if args.verify:
        verify(cells, paths, resolutions)

    # 4. Query with Sedona.
    cells.selectExpr("*", "ST_GeomFromWKB(wkb) AS geom").drop("wkb").createOrReplaceTempView("cells")
    sedona.sql(
        "SELECT path, resolution, count(*) AS cells, round(avg(mean), 2) AS avg_mean, "
        "round(min(min), 2) AS min, round(max(max), 2) AS max "
        "FROM cells GROUP BY path, resolution ORDER BY path, resolution"
    ).show(truncate=False)

    # raster_h3 and Sedona (h3-java) share 64-bit cell ids, so Sedona's own H3 functions join
    # straight onto the aggregated cells.
    finest = max(resolutions)
    print(f"Resolution-{finest} cells within ~2 km of the centre, via Sedona's ST_H3CellIDs:")
    sedona.sql(
        f"""
        WITH area AS (
            SELECT explode(ST_H3CellIDs(
                ST_Buffer(ST_Centroid(ST_Envelope_Aggr(geom)), 0.02), {finest}, true
            )) AS h3_index
            FROM cells WHERE resolution = {finest}
        )
        SELECT c.h3_index, round(c.mean, 2) AS mean, c.count,
               -- raster_h3's hexagon vs Sedona's polygon for the same id: ~0 when they agree
               ST_Distance(ST_Centroid(c.geom),
                           ST_Centroid(ST_H3ToGeom(array(c.h3_index))[0])) AS centroid_gap_deg
        FROM cells c JOIN area a ON c.h3_index = a.h3_index
        WHERE c.resolution = {finest}
        ORDER BY c.mean DESC
        """
    ).show(10, truncate=False)


if __name__ == "__main__":
    main()
