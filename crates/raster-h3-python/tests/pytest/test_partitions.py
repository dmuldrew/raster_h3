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
MERGE_OPTIONS = ("quantiles", "min_count", "min_mean", "max_mean", "min_majority_fraction")


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
    # The merge must see the scans' cell options; the pixel options (remap, band, ...) are
    # already applied to the partial states.
    merge_options = {k: kwargs[k] for k in MERGE_OPTIONS if k in kwargs}
    merged = raster_h3.merge_partials(
        partials,
        categorical=kwargs.get("categorical", False),
        columns=kwargs.get("columns"),
        **merge_options,
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


def test_merge_accepts_the_scans_columns_including_partial_state(raster_h3, striped_gradient):
    columns = ["h3_index", "resolution", "mean", "partial_state"]
    spec = raster_h3.plan_partitions(striped_gradient, target_chunks=4)[1]
    table = pa.table(raster_h3.RasterH3ArrowStream(striped_gradient, RESOLUTIONS, columns=columns, partition=spec))
    assert table.column_names == columns
    partial = table.filter(pc.is_valid(table.column("partial_state")))
    merged = raster_h3.merge_partials(partial, columns=columns)
    assert merged.column_names == ["h3_index", "resolution", "mean"]
    assert merged.num_rows > 0


def test_partitioned_to_dict_warns_about_dropped_samples(raster_h3, tmp_path):
    # 10 x 20 one-degree pixels from 95N: the top 5 rows (50 pixels) lie beyond the pole.
    path = tmp_path / "past_pole.tif"
    write_geotiff(path, 10, 20, lambda c, r: 3.0, origin=(0.0, 95.0), pixel=1.0, rows_per_strip=4)
    spec = raster_h3.plan_partitions(path, target_chunks=100)[0]
    stream = raster_h3.RasterH3ArrowStream(path, 3, partition=spec)
    with pytest.warns(RuntimeWarning, match=r"50 valid sample\(s\) were dropped"):
        result = stream.to_dict()
    assert len(result["h3_index"]) > 0 and "partial_state" in result


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


@pytest.mark.parametrize(
    "options",
    [
        {"quantiles": "p10,median,iqr"},
        {"min_count": 30.0, "min_mean": 6.0, "max_mean": 12.0},
        {"quantiles": [0.9], "min_count": 20.0, "columns": ["h3_index", "resolution", "p90"]},
    ],
    ids=["quantiles", "filters", "quantiles-filters-projection"],
)
def test_continuous_options_survive_the_merge(raster_h3, striped_gradient, options):
    single = sort(pa.table(raster_h3.RasterH3ArrowStream(striped_gradient, RESOLUTIONS, **options)))
    combined, partitions, partials = run_partitioned(raster_h3, striped_gradient, 4, **options)
    assert partitions > 1 and partials > 0
    assert_tables_match(single, combined)


@pytest.mark.parametrize(
    "options",
    [{"min_majority_fraction": 0.6}, {"remap": {1: 10, 2: 10, "else": None}}, {"min_count": 40.0}],
    ids=["majority", "remap", "min_count"],
)
def test_categorical_options_survive_the_merge(raster_h3, striped_classes, options):
    single = sort(
        pa.table(
            raster_h3.RasterH3ArrowStream(striped_classes, RESOLUTIONS, categorical=True, **options)
        )
    )
    combined, partitions, partials = run_partitioned(
        raster_h3, striped_classes, 3, categorical=True, **options
    )
    assert partitions > 1 and partials > 0
    assert_tables_match(single, combined)


def test_merge_without_the_scans_filters_keeps_merged_cells(raster_h3, striped_gradient):
    """The filters are applied to merged cells only when passed to `merge_partials` too."""
    plan = raster_h3.plan_partitions(striped_gradient, target_chunks=4)
    partials = []
    for spec in plan:
        table = pa.table(
            raster_h3.RasterH3ArrowStream(striped_gradient, RESOLUTIONS, partition=spec, min_count=1e9)
        )
        partials.append(table.filter(pc.is_valid(table.column(raster_h3.PARTIAL_STATE_COLUMN))))
        # No interior cell has a billion samples.
        assert table.filter(pc.is_null(table.column(raster_h3.PARTIAL_STATE_COLUMN))).num_rows == 0
    assert raster_h3.merge_partials(partials, min_count=1e9).num_rows == 0
    assert raster_h3.merge_partials(partials).num_rows > 0
