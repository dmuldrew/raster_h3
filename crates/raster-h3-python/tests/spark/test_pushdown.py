"""Filter pushdown into the `raster_h3` Spark data source (Spark 4.1+).

`resolution` comparisons and `partial_state IS [NOT] NULL` are pushed into the scan (they
leave Spark's plan) and give the same rows as Spark evaluating them; filters on statistics
stay in Spark's plan, because on partial rows they would see one partition's share of a
cell.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

import raster_h3
import raster_h3.spark as spark_ds

RESOLUTIONS = "7,8,9"


def load(spark, path, pushdown):
    spark_ds.register(spark, pushdown=pushdown)
    return (
        spark.read.format("raster_h3")
        .option("resolutions", RESOLUTIONS)
        .option("target_chunks", 8)
        .load(path)
    )


def plan(df) -> str:
    return df._jdf.queryExecution().optimizedPlan().toString()


def rows(df):
    """Rows grouped by (resolution, h3_index, is partial): an edge cell has one partial row
    per partition that reaches it. Scanning fewer resolutions changes floating-point
    summation order, so values are compared within 1e-9 relative and partial states (which
    encode those sums) by presence; merges of them are compared below."""
    out = {}
    for row in df.toArrow().to_pylist():
        state = row.pop("partial_state")
        out.setdefault((row["resolution"], row["h3_index"], state is not None), []).append(row)
    for group in out.values():
        group.sort(key=lambda r: (r["count"], r["mean"]))
    return out


def assert_same_rows(got, want):
    assert got.keys() == want.keys(), (
        f"{len(want.keys() - got.keys())} groups missing, {len(got.keys() - want.keys())} extra"
    )
    for key, expected_group in want.items():
        assert len(got[key]) == len(expected_group), f"{key}: {len(got[key])} rows, expected {len(expected_group)}"
        for expected, actual in zip(expected_group, got[key]):
            for column, x in expected.items():
                y = actual[column]
                if isinstance(x, float) and isinstance(y, float):
                    assert abs(x - y) <= 1e-9 * max(1.0, abs(x)), f"{column} of {key}: {y} != {x}"
                else:
                    assert x == y, f"{column} of {key}: {y!r} != {x!r}"


@pytest.mark.parametrize(
    "condition, pushed",
    [
        ("resolution = 9", True),
        ("resolution IN (7, 9)", True),
        ("resolution >= 8", True),
        ("resolution < 8 AND resolution > 6", True),
        ("partial_state IS NULL", True),
        ("partial_state IS NOT NULL AND resolution = 8", True),
        ("count >= 20", False),
        ("mean > 300 AND resolution = 9", False),  # resolution pushed, mean kept
        ("resolution = 9 OR count > 50", False),  # an OR is never split
    ],
)
def test_pushed_filters_leave_the_plan_and_keep_the_rows(spark, terrain, condition, pushed):
    expected = rows(load(spark, terrain, pushdown=False).where(condition))
    df = load(spark, terrain, pushdown=True).where(condition)
    assert ("Filter" not in plan(df)) is pushed, plan(df)
    assert_same_rows(rows(df), expected)
    assert expected  # every condition keeps some rows


def test_resolution_filter_on_a_view_merges_like_a_narrower_scan(spark, terrain):
    """Pushed through SQL too; merging the pushed scan equals a res-9-only scan."""
    spark_ds.register(spark, pushdown=True)
    spark.sql(
        f"CREATE OR REPLACE TEMPORARY VIEW push_parts USING raster_h3 "
        f"OPTIONS (path '{terrain}', resolutions '{RESOLUTIONS}', target_chunks '8')"
    )
    parts = spark.sql("SELECT * FROM push_parts WHERE resolution = 9")
    assert "Filter" not in plan(parts)
    got = spark_ds.merge_partials(parts).toArrow()
    want = pa.table(raster_h3.RasterH3ArrowStream(terrain, resolutions=[9]))
    as_rows = lambda t: {(r["resolution"], r["h3_index"], False): [r] for r in t.to_pylist()}
    assert_same_rows(as_rows(got), as_rows(want))


def test_filters_that_exclude_everything_scan_nothing(spark, terrain):
    df = load(spark, terrain, pushdown=True)
    assert df.where("resolution = 12").count() == 0
    assert df.where("partial_state IS NULL AND partial_state IS NOT NULL").count() == 0


def test_register_follows_the_session_setting(spark, terrain):
    spark.conf.set(spark_ds._PUSHDOWN_CONF, "false")
    spark_ds.register(spark)
    df = spark.read.format("raster_h3").option("resolutions", "9").load(terrain).where("resolution = 9")
    assert "Filter" in plan(df)  # not pushed, and the reader still works
    assert df.count() > 0

    spark.conf.set(spark_ds._PUSHDOWN_CONF, "true")
    spark_ds.register(spark)
    df = spark.read.format("raster_h3").option("resolutions", "9").load(terrain).where("resolution = 9")
    assert "Filter" not in plan(df)
