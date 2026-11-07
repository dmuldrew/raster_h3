"""`raster_h3.spark`: the Spark 4 Python data source and its partial-cell merge.

Every result is compared with one unpartitioned `RasterH3ArrowStream` scan of the same
raster and options: the same cells, each once, with equal values (statistics within 1e-9
relative, for summation order).
"""

from __future__ import annotations

import math

import pyarrow as pa
import pytest

import raster_h3

spark_ds = pytest.importorskip("raster_h3.spark")
pytest.importorskip("pyspark.sql.datasource")


@pytest.fixture(scope="module", autouse=True)
def registered(sedona):
    spark_ds.register(sedona)


def assert_matches_single_scan(cells, path, **options):
    got = cells.toArrow()
    want = pa.table(raster_h3.RasterH3ArrowStream(path, **options))
    assert sorted(got.column_names) == sorted(want.column_names)

    def rows(table):
        keys = list(zip(table["resolution"].to_pylist(), table["h3_index"].to_pylist()))
        values = [dict(zip(table.column_names, r)) for r in zip(*(table[c].to_pylist() for c in table.column_names))]
        return keys, dict(zip(keys, values))

    got_keys, got_rows = rows(got)
    want_keys, want_rows = rows(want)
    assert len(got_keys) == len(set(got_keys)), "a cell appears twice"
    assert set(got_keys) == set(want_keys), (
        f"{len(set(want_keys) - set(got_keys))} cells missing, {len(set(got_keys) - set(want_keys))} extra"
    )
    for key, expected in want_rows.items():
        for column, x in expected.items():
            y = got_rows[key][column]
            if isinstance(x, float) and isinstance(y, float) and not (math.isnan(x) and math.isnan(y)):
                assert abs(x - y) <= 1e-9 * max(1.0, abs(x)), f"{column} of {key}: {y} != {x}"
            else:
                assert x == y, f"{column} of {key}: {y!r} != {x!r}"
    return len(want_keys)


def test_load_and_merge_match_a_single_scan(sedona, terrain):
    path = terrain()
    df = sedona.read.format("raster_h3").option("resolutions", "8,9").option("target_chunks", 8).load(path)

    assert df.rdd.getNumPartitions() == len(raster_h3.plan_partitions(path, target_chunks=8)) > 4
    assert dict(df.dtypes)["h3_index"] == "bigint"
    assert df.where("partial_state IS NOT NULL").count() > 0
    cells = spark_ds.merge_partials(df)
    assert "partial_state" not in cells.columns
    assert assert_matches_single_scan(cells, path, resolutions=[8, 9]) > 0


def test_read_helper_carries_options_into_the_merge(sedona, terrain):
    """Quantiles, a projection and cell filters reach both the scan and the merge."""
    path = terrain()
    options = dict(
        resolutions=[9],
        columns=["h3_index", "resolution", "count", "mean", "p50"],
        quantiles="p50",
        min_count=20,
        max_mean=380.0,
    )
    cells = spark_ds.read(sedona, path, target_chunks=8, **options)
    assert cells.columns == ["h3_index", "resolution", "count", "mean", "p50"]
    assert assert_matches_single_scan(cells, path, **options) > 0


def test_categorical_wide(sedona, terrain):
    path = terrain("classes.tif", categorical=True)
    options = dict(resolutions=[8, 9], categorical=True, min_majority_fraction=0.6)
    cells = spark_ds.read(sedona, path, target_chunks=8, **options)
    assert assert_matches_single_scan(cells, path, **options) > 0


def test_one_partition_has_no_partial_rows(sedona, terrain):
    path = terrain()
    df = sedona.read.format("raster_h3").option("resolutions", "9").option("target_chunks", 10_000).load(path)
    assert df.rdd.getNumPartitions() == 1
    assert df.where("partial_state IS NOT NULL").count() == 0
    assert_matches_single_scan(spark_ds.merge_partials(df), path, resolutions=[9])


def test_sql_and_sedona_geometry(sedona, terrain):
    """The data source works from Spark SQL, and its WKB is Sedona geometry."""
    path = terrain()
    sedona.sql(
        f"CREATE OR REPLACE TEMPORARY VIEW raster_parts USING raster_h3 "
        f"OPTIONS (path '{path}', resolutions '9', target_chunks '8')"
    )
    cells = spark_ds.merge_partials(sedona.table("raster_parts"))
    cells.createOrReplaceTempView("raster_cells")
    row = sedona.sql(
        """
        SELECT count(*) AS n,
               sum(CASE WHEN ST_IsValid(ST_GeomFromWKB(wkb)) THEN 0 ELSE 1 END) AS invalid,
               sum(CASE WHEN ST_H3CellIDs(ST_Centroid(ST_GeomFromWKB(wkb)), 9, false)[0] = h3_index
                        THEN 0 ELSE 1 END) AS other_cell
        FROM raster_cells
        """
    ).collect()[0]
    assert row["n"] > 0 and row["invalid"] == 0 and row["other_cell"] == 0


@pytest.mark.parametrize(
    "options, message",
    [
        ({}, "needs a path"),
        ({"path": "x.tif", "colour": "red"}, "unknown raster_h3 option"),
        ({"path": "x.tif", "resolutions": "eight"}, "option resolutions"),
        ({"path": "x.tif", "categorical": "maybe"}, "option categorical"),
    ],
)
def test_bad_options_are_rejected(options, message):
    with pytest.raises(ValueError, match=message):
        spark_ds.RasterH3DataSource(options)


def test_scan_errors_reach_spark(sedona, terrain):
    path = terrain()
    with pytest.raises(Exception, match="(?i)resolution"):
        sedona.read.format("raster_h3").option("resolutions", "16").load(path).count()
