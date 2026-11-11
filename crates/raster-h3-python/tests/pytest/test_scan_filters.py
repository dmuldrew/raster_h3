"""Quantile columns, category remapping, cell filters (`min_count`, `min_mean`, `max_mean`,
`min_majority_fraction`) and the pushdown of Polars predicates into those filters.

Pushdown is checked two ways: the rows must equal a full scan filtered in Polars, and the
scan the IO source opens must carry the expected bounds.
"""

from __future__ import annotations

from collections import defaultdict

import pytest

pl = pytest.importorskip("polars")
pytest.importorskip("pyarrow")
pytest.importorskip("polars.io.plugins", reason="register_io_source needs polars >= 1.4")

from conftest import RES, class_value, pixel_cells, sort  # noqa: E402

MULTI = [8, RES]
POLARS = tuple(int(x) for x in pl.__version__.split(".")[:2])
# Before Polars 1.22, `scan_raster_h3` keeps filters out of the scan (those versions drop
# some filters they hand to IO sources), so nothing is pushed and Polars filters instead.
PUSHDOWN = POLARS >= (1, 22)


@pytest.fixture
def scans(raster_h3, monkeypatch):
    """Resolutions and options of every `RasterH3ArrowStream` the IO source opens; clear it
    between building the LazyFrame and collecting."""
    calls = []
    original = raster_h3.RasterH3ArrowStream

    class Recording(original):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            calls.append({"resolutions": self.resolutions, "options": dict(self.options)})

    monkeypatch.setattr(raster_h3, "RasterH3ArrowStream", Recording)
    return calls


def assert_frames_close(got, expected):
    """Equal rows, floats compared to 1e-12: narrowing resolutions or filtering in the
    engine changes floating-point summation order, not values."""
    got, expected = sort_cells(got), sort_cells(expected)
    assert got.columns == expected.columns
    assert got.height == expected.height
    for name in expected.columns:
        a, b = expected[name].to_list(), got[name].to_list()
        if expected[name].dtype == pl.Float64:
            assert b == pytest.approx(a, rel=1e-12, abs=1e-12, nan_ok=True), name
        else:
            assert b == a, name


def sort_cells(df):
    keys = [k for k in ("resolution", "h3_index", "category") if k in df.columns]
    return df.sort(keys)


# -----------------------------------------------------------------------------
# quantiles
# -----------------------------------------------------------------------------


def test_quantile_columns_follow_the_base_schema(raster_h3, gradient_tif):
    lf = raster_h3.scan_raster_h3(gradient_tif, RES, quantiles="p10,median,iqr")
    schema = lf.collect_schema() if hasattr(lf, "collect_schema") else lf.schema
    assert list(schema)[-4:] == ["wkb", "p10", "median", "iqr"]
    df = lf.collect()
    assert list(df.columns) == list(schema)
    assert all(schema[name] == pl.Float64 for name in ("p10", "median", "iqr"))
    assert df.filter(
        (pl.col("p10") < pl.col("min"))
        | (pl.col("median") > pl.col("max"))
        | (pl.col("p10") > pl.col("median"))
        | (pl.col("iqr") < 0)
    ).is_empty()


def test_single_pixel_cells_have_exact_quantiles(raster_h3, gradient_tif):
    df = raster_h3.scan_raster_h3(gradient_tif, 13, quantiles="p50").collect()
    single = df.filter(pl.col("count") == 1)
    assert single.height > 0
    assert single["p50"].to_list() == single["mean"].to_list()


@pytest.mark.parametrize(
    "spec, names",
    [
        ("box", ["p25", "p50", "p75", "iqr"]),
        (["p95", 0.05], ["p95", "p05"]),
        ("p50,p50,median", ["p50", "median"]),
        ([], []),
        ("none", []),
    ],
)
def test_quantile_specs(raster_h3, gradient_tif, spec, names):
    df = raster_h3.read_raster_h3(gradient_tif, RES, quantiles=spec)
    assert df.columns[df.columns.index("wkb") + 1 :] == names
    lazy = raster_h3.scan_raster_h3(gradient_tif, RES, quantiles=spec).collect()
    assert lazy.columns == df.columns


def test_quantile_projection(raster_h3, gradient_tif):
    full = raster_h3.read_raster_h3(gradient_tif, RES, quantiles="p90")
    lf = raster_h3.scan_raster_h3(gradient_tif, RES, quantiles="p90")
    assert sort(lf.select("h3_index", "p90").collect()).equals(sort(full.select("h3_index", "p90")))
    assert sort(lf.select("h3_index", "mean").collect()).equals(sort(full.select("h3_index", "mean")))
    stream = raster_h3.RasterH3ArrowStream(gradient_tif, RES, quantiles="p90", columns=["p90", "h3_index"])
    assert stream.to_polars().columns == ["p90", "h3_index"]


def test_arrow_schema_carries_quantile_columns(raster_h3, gradient_tif):
    pa = pytest.importorskip("pyarrow")
    stream = raster_h3.RasterH3ArrowStream(gradient_tif, RES, quantiles="q1,q3")
    assert pa.schema(stream).names[-2:] == ["q1", "q3"]
    assert pa.table(stream).schema.names == pa.schema(stream).names


@pytest.mark.parametrize(
    "kwargs, error, match",
    [
        ({"quantiles": "p150"}, RuntimeError, "p150"),
        ({"quantiles": "p50", "categorical": True}, RuntimeError, "continuous scans only"),
        ({"quantiles": [True]}, TypeError, "quantiles entries must be str or float"),
    ],
)
def test_quantile_errors(raster_h3, gradient_tif, kwargs, error, match):
    with pytest.raises(error, match=match):
        raster_h3.RasterH3ArrowStream(gradient_tif, RES, **kwargs).to_dict()


# -----------------------------------------------------------------------------
# remap
# -----------------------------------------------------------------------------


def remapped_counts(mapping):
    """Per cell, the sample count of each class after `mapping` (None drops the class)."""
    acc = defaultdict(lambda: defaultdict(float))
    for (c, r), cell in pixel_cells(48, 48, RES).items():
        target = mapping(class_value(c, r))
        if target is not None:
            acc[cell][target] += 1.0
    return {cell: dict(classes) for cell, classes in acc.items()}


@pytest.mark.parametrize(
    "remap",
    [
        {range(1, 3): 10, 3: 30, "else": None},
        "1..2: 10, 3: 30, else: null",
        "{1: 10, 2: 10, 3: 30, default: nodata}",
    ],
    ids=["mapping", "range-string", "braces"],
)
def test_remap_merges_and_drops_classes(raster_h3, classes_tif, remap):
    df = raster_h3.scan_raster_h3(
        classes_tif, RES, categorical=True, format="long", remap=remap
    ).collect()
    got = defaultdict(dict)
    for row in df.iter_rows(named=True):
        got[row["h3_index"]][row["category"]] = row["count"]
    expected = remapped_counts(lambda v: {1: 10, 2: 10, 3: 30}.get(v))
    assert dict(got) == expected


def test_remap_passes_unmatched_classes_through(raster_h3, classes_tif):
    df = raster_h3.read_raster_h3(classes_tif, RES, categorical=True, format="long", remap={5: 4})
    assert set(df["category"].unique().to_list()) == {1, 2, 3, 4}


@pytest.mark.parametrize(
    "kwargs, error, match",
    [
        ({"remap": {1: 2}}, RuntimeError, "categorical scans only"),
        ({"remap": "1 2", "categorical": True}, RuntimeError, "missing ':' separator"),
        ({"remap": {1.5: 2}, "categorical": True}, TypeError, "remap keys must be int"),
        ({"remap": {1: "a"}, "categorical": True}, TypeError, "remap targets must be int or None"),
        ({"remap": {range(1, 9, 2): 1}, "categorical": True}, ValueError, "step 1"),
        ({"remap": {}, "categorical": True}, ValueError, "empty"),
    ],
)
def test_remap_errors(raster_h3, gradient_tif, kwargs, error, match):
    with pytest.raises(error, match=match):
        raster_h3.RasterH3ArrowStream(gradient_tif, RES, **kwargs).to_dict()


# -----------------------------------------------------------------------------
# Explicit filters
# -----------------------------------------------------------------------------


@pytest.mark.parametrize(
    "kwargs, expr",
    [
        ({"min_count": 20}, pl.col("count") >= 20),
        ({"min_mean": 8.0}, pl.col("mean") >= 8.0),
        ({"max_mean": 8.0}, pl.col("mean") <= 8.0),
        ({"min_count": 10, "min_mean": 6, "max_mean": 11}, pl.col("count").ge(10) & pl.col("mean").is_between(6, 11)),
    ],
)
def test_continuous_filters_are_inclusive(raster_h3, gradient_tif, kwargs, expr):
    full = raster_h3.read_raster_h3(gradient_tif, RES)
    got = raster_h3.read_raster_h3(gradient_tif, RES, **kwargs)
    expected = full.filter(expr)
    assert 0 < expected.height < full.height
    assert sort(got).equals(sort(expected))


def test_categorical_min_count_uses_total_count(raster_h3, classes_tif):
    full = raster_h3.read_raster_h3(classes_tif, RES, categorical=True)
    got = raster_h3.read_raster_h3(classes_tif, RES, categorical=True, min_count=20)
    assert sort(got).equals(sort(full.filter(pl.col("total_count") >= 20)))


@pytest.mark.parametrize(
    "kwargs, error, match",
    [
        ({"min_mean": 1.0, "categorical": True}, RuntimeError, "min_mean applies to continuous scans only"),
        ({"max_mean": 1.0, "categorical": True}, RuntimeError, "max_mean applies to continuous scans only"),
        ({"min_count": float("inf")}, ValueError, "min_count must be finite"),
        ({"min_mean": "3"}, TypeError, "min_mean must be a number"),
    ],
)
def test_filter_errors(raster_h3, gradient_tif, kwargs, error, match):
    with pytest.raises(error, match=match):
        raster_h3.RasterH3ArrowStream(gradient_tif, RES, **kwargs).to_dict()


# -----------------------------------------------------------------------------
# Predicate pushdown
# -----------------------------------------------------------------------------


@pytest.mark.parametrize(
    "expr, pushed",
    [
        ((pl.col("count") > 10) & (pl.col("mean") <= 9), {"min_count": 10.0, "max_mean": 9.0}),
        (pl.col("mean").is_between(5, 8), {"min_mean": 5.0, "max_mean": 8.0}),
        (9 > pl.col("mean"), {"max_mean": 9.0}),
        (pl.col("mean") == 7.5, {"min_mean": 7.5, "max_mean": 7.5}),
        ((pl.col("count") >= 4) & (pl.col("count") >= 12), {"min_count": 12.0}),
        # OR, NOT-equal and comparisons on other columns push nothing.
        ((pl.col("count") > 10) | (pl.col("mean") < 3), {}),
        (pl.col("count") != 3, {}),
        (pl.col("sum") > 100, {}),
        # Polars before 1.22 drops this filter if the IO source sees it (a strict cast
        # cannot be handed down); `scan_raster_h3` keeps it in Polars on those versions.
        (pl.col("count").cast(pl.Int64) > 10, {}),
    ],
)
def test_continuous_predicates(raster_h3, gradient_tif, scans, expr, pushed):
    lf = raster_h3.scan_raster_h3(gradient_tif, RES)
    expected = lf.collect().filter(expr)
    scans.clear()
    got = lf.filter(expr).collect()
    assert sort(got).equals(sort(expected))
    assert scans, "the IO source never opened a scan"
    want = pushed if PUSHDOWN else {}
    assert all(s["options"] == want for s in scans), scans


def test_pushdown_combines_with_explicit_filters(raster_h3, gradient_tif, scans):
    lf = raster_h3.scan_raster_h3(gradient_tif, RES, min_count=15, max_mean=10)
    expected = raster_h3.read_raster_h3(gradient_tif, RES, min_count=15, max_mean=10).filter(
        (pl.col("count") > 5) & (pl.col("mean") < 12) & (pl.col("mean") > 6)
    )
    scans.clear()
    got = lf.filter((pl.col("count") > 5) & (pl.col("mean") < 12) & (pl.col("mean") > 6)).collect()
    assert sort(got).equals(sort(expected))
    assert {s["options"]["min_count"] for s in scans} == {15}
    assert {s["options"]["max_mean"] for s in scans} == {10}
    assert {s["options"].get("min_mean") for s in scans} == ({6.0} if PUSHDOWN else {None})


@pytest.mark.parametrize(
    "expr, resolutions",
    [
        (pl.col("resolution") == RES, [RES]),
        (pl.col("resolution") < RES, [8]),
        (pl.col("resolution") >= 8, MULTI),
        (pl.col("resolution").is_between(RES, 15), [RES]),
        ((pl.col("resolution") == 8) & (pl.col("count") > 50), [8]),
    ],
)
def test_resolution_predicates_narrow_the_scan(raster_h3, gradient_tif, scans, expr, resolutions):
    lf = raster_h3.scan_raster_h3(gradient_tif, MULTI)
    expected = lf.collect().filter(expr)
    scans.clear()
    got = lf.filter(expr).collect()
    assert_frames_close(got, expected)
    want = resolutions if PUSHDOWN else MULTI
    assert scans and all(s["resolutions"] == want for s in scans), scans


def test_resolution_predicate_rejecting_every_resolution(raster_h3, gradient_tif, scans):
    lf = raster_h3.scan_raster_h3(gradient_tif, MULTI)
    scans.clear()
    got = lf.filter(pl.col("resolution") == 12).collect()
    assert got.is_empty()
    assert dict(got.schema) == dict(lf.collect_schema() if hasattr(lf, "collect_schema") else lf.schema)
    if PUSHDOWN:
        assert scans == []


@pytest.mark.parametrize(
    "expr, pushed",
    [
        (pl.col("majority_fraction") > 0.5, {"min_majority_fraction": 0.5}),
        (pl.col("majority_fraction") >= 1.5, {"min_majority_fraction": 1.0}),
        (pl.col("majority_fraction") > -1, {}),
        (pl.col("total_count") >= 20, {"min_count": 20.0}),
        # `count` and `mean` are not columns of a categorical scan's cells.
        (pl.col("unique_classes") > 1, {}),
    ],
)
def test_categorical_wide_predicates(raster_h3, classes_tif, scans, expr, pushed):
    lf = raster_h3.scan_raster_h3(classes_tif, RES, categorical=True)
    expected = lf.collect().filter(expr)
    scans.clear()
    got = lf.filter(expr).collect()
    assert sort(got).equals(sort(expected))
    want = pushed if PUSHDOWN else {}
    assert all(s["options"] == want for s in scans), scans


def test_categorical_long_count_is_per_class(raster_h3, classes_tif, scans):
    """In the long layout `count` is one class's share of a cell, so it bounds nothing."""
    lf = raster_h3.scan_raster_h3(classes_tif, RES, categorical=True, format="long")
    expr = (pl.col("count") > 8) & (pl.col("total_count") > 10)
    expected = lf.collect().filter(expr)
    scans.clear()
    got = lf.filter(expr).collect()
    assert sort_cells(got).equals(sort_cells(expected))
    want = {"min_count": 10.0} if PUSHDOWN else {}
    assert all(s["options"] == want for s in scans), scans


def test_pushdown_with_row_limit(raster_h3, gradient_tif):
    lf = raster_h3.scan_raster_h3(gradient_tif, RES)
    expr = pl.col("mean") > 9
    got = lf.filter(expr).head(5).collect()
    assert got.height == 5
    assert got.filter(~expr).is_empty()


@pytest.mark.parametrize("resolutions", [[16], [8, 16], [], "8,x"])
def test_resolution_pushdown_keeps_invalid_resolutions_an_error(raster_h3, gradient_tif, resolutions):
    """A resolution filter must not hide an invalid `resolutions` argument."""
    lf = raster_h3.scan_raster_h3(gradient_tif, resolutions)
    # Polars wraps the native error in a ComputeError in some versions, not in others.
    with pytest.raises(Exception, match="Invalid resolutions"):
        lf.filter(pl.col("resolution") == 8).collect()


def test_pushdown_without_native_scan_options(raster_h3, gradient_tif, scans, monkeypatch):
    """With a native library predating the `_v2` entry points, filters are applied by Polars
    only: no options are pushed, so the scan does not fail for lack of them."""
    monkeypatch.setattr(raster_h3, "_native_has_scan_options", lambda: False)
    lf = raster_h3.scan_raster_h3(gradient_tif, MULTI)
    expr = (pl.col("count") > 10) & (pl.col("mean") < 9) & (pl.col("resolution") == RES)
    expected = lf.collect().filter(expr)
    scans.clear()
    got = lf.filter(expr).collect()
    assert_frames_close(got, expected)
    # Narrowing the resolutions needs no new native entry point, so it still happens.
    want = [RES] if PUSHDOWN else MULTI
    assert scans and all(s["options"] == {} and s["resolutions"] == want for s in scans), scans
