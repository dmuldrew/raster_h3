"""Partitioned raster_h3 scans on Apache Sedona (Spark), through the demo's own pipeline.

Each test plans partitions on the driver, scans every partition in its own Spark task
(`mapInArrow`), shuffles the partial rows by cell, merges them with `merge_partials`, and
checks the union against one unpartitioned scan of each raster with the demo's `verify`:
the same cells, each exactly once, equal statistics and identical WKB.
"""

from __future__ import annotations

import os
import subprocess
import sys

import pytest
import raster_h3_spark_demo as demo


def run(sedona, paths, resolutions, target_chunks):
    tasks = demo.plan(paths, target_chunks)
    cells, final, partial = demo.distributed_cells(sedona, tasks, resolutions)
    return tasks, cells, final.count(), partial.count(), cells.count()


@pytest.mark.parametrize("resolutions", [[9], [7, 8, 9]], ids=["res9", "res7-8-9"])
def test_partitioned_scan_matches_single_scan(sedona, terrain, resolutions):
    path = terrain()  # 512 x 512 px in 64 tiles
    tasks, cells, n_final, n_partial, n_cells = run(sedona, [path], resolutions, target_chunks=8)

    # 64 tiles at ~8 per partition: the planner cuts the tile grid into rectangles (3 x 3 here).
    assert len(tasks) > 4
    # Every partition boundary crosses cells, so the merge step really runs.
    assert n_partial > 0
    assert n_cells > n_final
    demo.verify(cells, [path], resolutions)


def test_verify_detects_missing_merged_cells(sedona, terrain):
    """The check bites: dropping the merged edge cells must fail `verify`."""
    path = terrain()
    tasks = demo.plan([path], 8)
    _, final, _ = demo.distributed_cells(sedona, tasks, [9])
    with pytest.raises(AssertionError, match="cells missing"):
        demo.verify(final.drop("partial_state"), [path], [9])


def test_rasters_sharing_cells_are_merged_separately(sedona, terrain):
    """Two overlapping rasters reach the same cell ids; their partial rows must not mix."""
    big, small = terrain("big.tif", side=512), terrain("small.tif", side=384)
    resolutions = [9]
    _, cells, _, n_partial, _ = run(sedona, [big, small], resolutions, target_chunks=8)

    shared = (
        cells.groupBy("h3_index").agg({"path": "count"}).where("`count(path)` = 2").count()
    )
    assert shared > 0
    assert n_partial > 0
    demo.verify(cells, [big, small], resolutions)


def test_raster_in_one_partition_needs_no_merge(sedona, terrain):
    path = terrain()
    tasks, cells, n_final, n_partial, n_cells = run(sedona, [path], [8, 9], target_chunks=10_000)

    assert len(tasks) == 1
    assert n_partial == 0
    assert n_cells == n_final
    demo.verify(cells, [path], [8, 9])


def test_sedona_reads_raster_h3_cells(sedona, terrain):
    """raster_h3's WKB loads as Sedona geometry, and Sedona's H3 functions (h3-java) agree
    with raster_h3 on every cell id: the cell of each centroid, and the polygon of each id."""
    path = terrain()
    tasks = demo.plan([path], 8)
    cells, _, _ = demo.distributed_cells(sedona, tasks, [9])
    demo.register_cells(sedona, cells, "test_cells")

    row = sedona.sql(
        """
        SELECT count(*) AS cells,
               sum(CASE WHEN ST_IsValid(geom) THEN 0 ELSE 1 END) AS invalid,
               sum(CASE WHEN ST_H3CellIDs(ST_Centroid(geom), 9, false)[0] = h3_index
                        THEN 0 ELSE 1 END) AS other_cell,
               max(ST_Distance(ST_Centroid(geom),
                               ST_Centroid(ST_H3ToGeom(array(h3_index))[0]))) AS max_gap_deg
        FROM test_cells
        """
    ).collect()[0]

    assert row["cells"] == cells.count() > 0
    assert row["invalid"] == 0
    assert row["other_cell"] == 0
    assert row["max_gap_deg"] < 1e-9


def test_demo_script_end_to_end(terrain):
    """The command-line demo, including `--verify` and its Sedona SQL queries."""
    path = terrain()
    env = dict(os.environ, SPARK_MASTER=os.environ.get("SPARK_MASTER", "local[2]"))
    result = subprocess.run(
        [sys.executable, demo.__file__, path, "--resolutions", "8,9", "--verify"],
        capture_output=True,
        text=True,
        env=env,
        timeout=600,
    )
    assert result.returncode == 0, result.stdout[-2000:] + result.stderr[-4000:]
    assert f"verified {path}" in result.stdout
    assert "centroid_gap_deg" in result.stdout
