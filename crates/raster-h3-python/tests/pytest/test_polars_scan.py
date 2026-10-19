"""`scan_raster_h3` as a Polars IO source (`register_io_source`, Polars >= 1.4): projection,
predicate and `n_rows` pushdown give the same rows as an eager full scan filtered in Polars.
"""

from __future__ import annotations

import pytest

pl = pytest.importorskip("polars")
pytest.importorskip("pyarrow")
pytest.importorskip("polars.io.plugins", reason="register_io_source needs polars >= 1.4")

from conftest import RES, frame, sort  # noqa: E402

KEY = ["h3_index", "mean", "count"]
POLARS = tuple(int(x) for x in pl.__version__.split(".")[:2])


def semi(df, keys):
    """Rows of `df` whose `h3_index` appears in `keys`."""
    return df.join(keys.select("h3_index"), on="h3_index", how="semi")


@pytest.fixture
def scans(raster_h3, monkeypatch):
    """Records the arguments of every `RasterH3ArrowStream` opened after the test clears it.

    Clear it between building the LazyFrame and collecting, so only the IO source's scans count.
    """
    calls = []
    original = raster_h3.RasterH3ArrowStream

    class Recording(original):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            calls.append({"columns": self.columns, "limit": self.limit})

    monkeypatch.setattr(raster_h3, "RasterH3ArrowStream", Recording)
    return calls


def test_scan_returns_lazyframe_equal_to_stream(raster_h3, gradient_tif, eager):
    lazy = raster_h3.scan_raster_h3(gradient_tif, RES)
    assert isinstance(lazy, pl.LazyFrame)
    assert sort(lazy.collect()).equals(eager)
    schema = lazy.collect_schema() if hasattr(lazy, "collect_schema") else lazy.schema
    assert dict(schema) == dict(eager.schema)


def test_projection_pushdown_reads_only_requested_columns(raster_h3, gradient_tif, eager, scans):
    lazy = raster_h3.scan_raster_h3(gradient_tif, RES)
    scans.clear()
    out = lazy.select(["h3_index", "count"]).collect()
    assert out.columns == ["h3_index", "count"]
    assert sort(out).equals(eager.select(["h3_index", "count"]))
    assert scans, "the IO source never opened a scan"
    assert all(set(s["columns"]) == {"h3_index", "count"} for s in scans), scans


def test_columns_argument_limits_schema_and_scan(raster_h3, gradient_tif, eager, scans):
    lazy = raster_h3.scan_raster_h3(gradient_tif, RES, columns=["h3_index", "mean"])
    scans.clear()
    out = lazy.collect()
    assert out.columns == ["h3_index", "mean"]
    assert sort(out).equals(eager.select(["h3_index", "mean"]))
    assert all(set(s["columns"]) <= {"h3_index", "mean"} for s in scans), scans


@pytest.mark.parametrize(
    "predicate",
    [
        lambda: pl.col("mean") > 9.0,
        lambda: (pl.col("count") >= 10) & (pl.col("min") <= 3.0),
        pytest.param(
            lambda: pl.col("h3_hex").str.ends_with("fffff"),
            marks=pytest.mark.skipif(
                POLARS < (1, 22),
                reason="polars < 1.22 drops str predicates on IO sources (passes None)",
            ),
        ),
        lambda: pl.col("mean") < 0.0,
    ],
    ids=["mean", "count_and_min", "hex", "none_match"],
)
def test_predicate_pushdown_matches_eager_filter(raster_h3, gradient_tif, eager, predicate):
    out = raster_h3.scan_raster_h3(gradient_tif, RES).filter(predicate()).collect()
    assert sort(out).equals(eager.filter(predicate()))


def test_predicate_on_index_with_projection(raster_h3, gradient_tif, eager):
    cells = eager["h3_index"].to_list()[::3]
    out = (
        raster_h3.scan_raster_h3(gradient_tif, RES)
        .filter(pl.col("h3_index").is_in(cells))
        .select(KEY)
        .collect()
    )
    assert sort(out).equals(eager.filter(pl.col("h3_index").is_in(cells)).select(KEY))


@pytest.mark.parametrize("n", [0, 1, 5, 10_000])
def test_head_pushdown_limits_scan(raster_h3, gradient_tif, eager, scans, n):
    lazy = raster_h3.scan_raster_h3(gradient_tif, RES)
    scans.clear()
    out = lazy.head(n).collect()
    assert out.height == min(n, eager.height)
    assert out.columns == eager.columns
    # Every returned row is a real cell of the full scan, with identical values.
    assert sort(out).equals(semi(eager, out))
    pushed = [s["limit"] for s in scans]
    if n == 0:
        # Polars may answer `head(0)` from the schema without calling the source.
        assert all(lim == 0 for lim in pushed), pushed
        return
    assert pushed and all(lim is not None and lim <= n for lim in pushed), pushed


@pytest.mark.parametrize("n", [1, 3, 10_000])
def test_head_after_filter_counts_filtered_rows(raster_h3, gradient_tif, eager, n):
    predicate = pl.col("mean") > 8.0
    want = eager.filter(predicate)
    out = raster_h3.scan_raster_h3(gradient_tif, RES).filter(predicate).head(n).collect()
    assert out.height == min(n, want.height)
    assert sort(out).equals(semi(want, out))


@pytest.mark.parametrize(
    "query",
    [
        lambda lf: lf.head(0),
        lambda lf: lf.select("mean").head(0),
        lambda lf: lf.filter(pl.col("mean") > 0).head(0),
        lambda lf: lf.filter(pl.col("mean") < 0).select(KEY),
    ],
    ids=["head0", "select_head0", "filter_head0", "filter_none"],
)
def test_empty_results_keep_schema(raster_h3, gradient_tif, eager, query):
    out = query(raster_h3.scan_raster_h3(gradient_tif, RES)).collect()
    assert out.height == 0
    assert out.schema == query(eager.lazy()).collect().schema


def test_limit_argument_caps_rows(raster_h3, gradient_tif, eager):
    out = raster_h3.scan_raster_h3(gradient_tif, RES, limit=4).collect()
    assert out.height == 4
    assert sort(out).equals(semi(eager, out))
    assert raster_h3.scan_raster_h3(gradient_tif, RES, limit=0).collect().height == 0


def test_categorical_long_pushdown(raster_h3, classes_tif):
    by = ["h3_index", "category"]
    lazy = raster_h3.scan_raster_h3(classes_tif, RES, categorical=True, format="long")
    full = frame(raster_h3.RasterH3ArrowStream(classes_tif, RES, categorical=True, format="long").to_pyarrow())
    assert lazy.collect().sort(by).equals(full.sort(by))
    out = lazy.filter(pl.col("category") == 3).select(by + ["count"]).collect()
    assert out.sort(by).equals(full.filter(pl.col("category") == 3).select(by + ["count"]).sort(by))


def test_categorical_wide_pushdown(raster_h3, classes_tif):
    lazy = raster_h3.scan_raster_h3(classes_tif, RES, categorical=True)
    full = frame(raster_h3.RasterH3ArrowStream(classes_tif, RES, categorical=True).to_pyarrow())
    assert sort(lazy.collect()).equals(sort(full))
    cols = ["h3_index", "majority_class", "majority_fraction"]
    head = lazy.select(cols).head(5).collect()
    assert head.height == 5
    assert sort(head).equals(sort(semi(full, head).select(cols)))
    out = lazy.filter(pl.col("majority_fraction") < 1.0).collect()
    assert sort(out).equals(sort(full.filter(pl.col("majority_fraction") < 1.0)))


def test_scan_errors_raise(raster_h3, gradient_tif, tmp_path):
    with pytest.raises(Exception, match="(?i)missing.tif|no such file|not found"):
        raster_h3.scan_raster_h3(tmp_path / "missing.tif", RES).collect()
    with pytest.raises(pl.exceptions.ColumnNotFoundError):
        raster_h3.scan_raster_h3(gradient_tif, RES).select("nope").collect()
    with pytest.raises(Exception, match="bounding box"):
        raster_h3.scan_raster_h3(gradient_tif, RES, bbox=[5, 5, 6, 6]).collect()
