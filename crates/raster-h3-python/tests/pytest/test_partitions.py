"""Partitioned scans (`plan_partitions`, `RasterH3ArrowStream(partition=...)`,
`merge_partials`) produce exactly the cells and values of one unpartitioned scan.
"""

from __future__ import annotations

import json

import pytest

pa = pytest.importorskip("pyarrow")
pc = pytest.importorskip("pyarrow.compute")

from conftest import class_value, gradient_value, write_geotiff  # noqa: E402

RESOLUTIONS = [8, 9]


@pytest.fixture(scope="module")
def striped_gradient(tmp_path_factory):
    path = tmp_path_factory.mktemp("partitions") / "gradient.tif"
    write_geotiff(path, 160, 120, gradient_value, rows_per_strip=8)  # 15 strips
    return path


@pytest.fixture(scope="module")
def striped_classes(tmp_path_factory):
    path = tmp_path_factory.mktemp("partitions") / "classes.tif"
    write_geotiff(path, 160, 120, class_value, dtype="u8", rows_per_strip=8)
    return path


def sort(table):
    return table.sort_by([("resolution", "ascending"), ("h3_index", "ascending")])


def run_partitioned(raster_h3, path, target_chunks, **kwargs):
    """Scan every partition, merge the partial rows, and return all final rows."""
    plan = raster_h3.plan_partitions(path, target_chunks=target_chunks)
    finals, partials = [], []
    for spec in plan:
        table = pa.table(raster_h3.RasterH3ArrowStream(path, RESOLUTIONS, partition=spec, **kwargs))
        assert table.column_names[-1] == raster_h3.PARTIAL_STATE_COLUMN
        is_partial = pc.is_valid(table.column(raster_h3.PARTIAL_STATE_COLUMN))
        finals.append(table.filter(pc.invert(is_partial)).drop([raster_h3.PARTIAL_STATE_COLUMN]))
        partials.append(table.filter(is_partial))
    merged = raster_h3.merge_partials(
        partials,
        categorical=kwargs.get("categorical", False),
        columns=kwargs.get("columns"),
    )
    combined = sort(pa.concat_tables(finals + [merged]))
    return combined, len(plan), sum(p.num_rows for p in partials)


def assert_tables_match(single, combined):
    assert combined.column_names == single.column_names
    assert combined.num_rows == single.num_rows
    keys = list(zip(single["resolution"].to_pylist(), single["h3_index"].to_pylist()))
    assert len(set(keys)) == len(keys)
    assert list(zip(combined["resolution"].to_pylist(), combined["h3_index"].to_pylist())) == keys
    for name in single.column_names:
        a, b = single[name].to_pylist(), combined[name].to_pylist()
        if pa.types.is_floating(single.schema.field(name).type):
            assert b == pytest.approx(a, rel=1e-9, abs=1e-12, nan_ok=True), name
        elif name == "histogram":
            assert [json.loads(x) for x in b] == [json.loads(x) for x in a], name
        else:
            assert b == a, name


@pytest.mark.parametrize("target_chunks", [1, 4, 1000])
def test_continuous_partitions_match_single_scan(raster_h3, striped_gradient, target_chunks):
    single = sort(pa.table(raster_h3.RasterH3ArrowStream(striped_gradient, RESOLUTIONS)))
    combined, partitions, partials = run_partitioned(raster_h3, striped_gradient, target_chunks)
    assert_tables_match(single, combined)
    assert (partials > 0) == (partitions > 1)


def test_categorical_partitions_match_single_scan(raster_h3, striped_classes):
    single = sort(pa.table(raster_h3.RasterH3ArrowStream(striped_classes, RESOLUTIONS, categorical=True)))
    combined, partitions, _ = run_partitioned(raster_h3, striped_classes, 3, categorical=True)
    assert partitions > 1
    assert_tables_match(single, combined)


def test_projection_carries_through_the_merge(raster_h3, striped_gradient):
    columns = ["h3_index", "resolution", "mean", "count"]
    single = sort(pa.table(raster_h3.RasterH3ArrowStream(striped_gradient, RESOLUTIONS, columns=columns)))
    combined, _, _ = run_partitioned(raster_h3, striped_gradient, 4, columns=columns)
    assert_tables_match(single, combined)


def test_merge_accepts_spark_style_types_and_inputs(raster_h3, striped_gradient):
    """Spark has no unsigned types: h3_index arrives as int64 and resolution as int16."""
    spec = raster_h3.plan_partitions(striped_gradient, target_chunks=4)[1]
    table = pa.table(raster_h3.RasterH3ArrowStream(striped_gradient, RESOLUTIONS, partition=spec))
    partial = table.filter(pc.is_valid(table.column("partial_state")))
    assert partial.num_rows > 0
    spark_like = pa.table(
        {
            "h3_index": partial["h3_index"].cast(pa.int64()),
            "resolution": partial["resolution"].cast(pa.int16()),
            "partial_state": partial["partial_state"].cast(pa.large_binary()),
        }
    )
    expected = raster_h3.merge_partials(partial)
    for data in (spark_like, spark_like.to_batches(), spark_like.to_batches()[0]):
        assert raster_h3.merge_partials(data).equals(expected)
    assert raster_h3.merge_partials([]).num_rows == 0


def test_batches_and_dict_output(raster_h3, striped_gradient):
    pytest.importorskip("polars")
    spec = raster_h3.plan_partitions(striped_gradient, target_chunks=4)[0]
    stream = raster_h3.RasterH3ArrowStream(striped_gradient, RESOLUTIONS, partition=spec)
    rows = sum(df.height for df in stream.iter_batches(64))
    as_dict = stream.to_dict()
    assert rows == len(as_dict["h3_index"]) == pa.table(stream).num_rows
    assert "partial_state" in as_dict
    assert pa.schema(stream).names[-1] == "partial_state"


def test_misuse_raises(raster_h3, striped_gradient, striped_classes, tmp_path):
    spec = raster_h3.plan_partitions(striped_gradient, target_chunks=4)[0]
    stream = raster_h3.RasterH3ArrowStream
    with pytest.raises(RuntimeError, match="limit"):
        pa.table(stream(striped_gradient, 9, partition=spec, limit=5))
    with pytest.raises(RuntimeError, match="h3_index"):
        pa.table(stream(striped_gradient, 9, partition=spec, columns=["mean"]))
    with pytest.raises(RuntimeError, match="wide"):
        pa.table(stream(striped_classes, 9, partition=spec, categorical=True, format="long"))
    # The spec identifies the chunk layout: a raster with another layout refuses it.
    other = tmp_path / "other.tif"
    write_geotiff(other, 160, 121, gradient_value, rows_per_strip=8)
    with pytest.raises(RuntimeError, match="different mosaic"):
        pa.table(stream(other, 9, partition=spec))
    with pytest.raises(RuntimeError, match="target_chunks"):
        raster_h3.plan_partitions(striped_gradient, target_chunks=0)

    table = pa.table(stream(striped_gradient, 9, partition=spec))
    with pytest.raises(ValueError, match="only partial rows"):
        raster_h3.merge_partials(table)  # includes final rows
    with pytest.raises(ValueError, match="missing"):
        raster_h3.merge_partials(table.drop(["partial_state"]))
