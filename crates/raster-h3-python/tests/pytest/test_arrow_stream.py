"""`RasterH3ArrowStream` (Arrow PyCapsule protocol) read by real PyArrow and Polars:
values against a direct per-pixel H3 reference, projection and limit, batches,
categorical wide/long, and errors raised as Python exceptions.
"""

from __future__ import annotations

import pytest

pl = pytest.importorskip("polars")
pa = pytest.importorskip("pyarrow")

from conftest import RES, frame, sort  # noqa: E402

KEY = ["h3_index", "mean", "count"]


def semi(df, keys):
    """Rows of `df` whose `h3_index` appears in `keys`."""
    return df.join(keys.select("h3_index"), on="h3_index", how="semi")


def test_values_match_direct_h3_reference(eager, gradient_expected):
    assert eager.height == len(gradient_expected)
    assert eager["resolution"].unique().to_list() == [RES]
    for row in eager.iter_rows(named=True):
        want = gradient_expected[row["h3_index"]]
        assert row["count"] == want["count"]
        assert row["sum"] == want["sum"]
        assert row["min"] == want["min"] and row["max"] == want["max"]
        assert row["mean"] == pytest.approx(want["mean"], rel=1e-12)
    assert eager["h3_hex"].to_list() == [format(c, "x") for c in eager["h3_index"]]


def test_polars_from_arrow_equals_pyarrow_table(raster_h3, gradient_tif, eager):
    stream = raster_h3.RasterH3ArrowStream(gradient_tif, RES)
    try:
        df = frame(stream)
    except TypeError:
        pytest.skip(f"polars {pl.__version__} from_arrow does not accept __arrow_c_stream__")
    assert sort(df).equals(eager)
    # Every `__arrow_c_stream__` call is a fresh scan.
    assert sort(frame(stream)).equals(eager)


def test_to_polars_and_read_raster_h3_equal_pyarrow_table(raster_h3, gradient_tif, eager):
    assert sort(raster_h3.RasterH3ArrowStream(gradient_tif, RES).to_polars()).equals(eager)
    assert sort(raster_h3.read_raster_h3(gradient_tif, RES)).equals(eager)


def test_pyarrow_table_types(raster_h3, gradient_tif):
    table = raster_h3.RasterH3ArrowStream(gradient_tif, RES).to_pyarrow()
    assert table.schema.field("h3_index").type == pa.uint64()
    assert table.schema.field("h3_hex").type in (pa.string(), pa.large_string())
    assert table.schema.field("count").type == pa.float64()
    assert table.schema.field("wkb").type in (pa.binary(), pa.large_binary())


def test_arrow_schema_capsule_matches_stream(raster_h3, gradient_tif):
    stream = raster_h3.RasterH3ArrowStream(gradient_tif, RES, columns=KEY)
    schema = pa.schema(stream)
    assert schema.names == KEY
    assert schema.equals(pa.table(stream).schema)


def test_projection_and_limit(raster_h3, gradient_tif, eager):
    table = pa.table(raster_h3.RasterH3ArrowStream(gradient_tif, RES, columns=KEY))
    assert table.column_names == KEY
    assert sort(frame(table)).equals(eager.select(KEY))

    table = pa.table(raster_h3.RasterH3ArrowStream(gradient_tif, RES, columns=KEY, limit=3))
    assert table.column_names == KEY and table.num_rows == 3
    out = frame(table)
    assert sort(out).equals(semi(eager, out).select(KEY))


def test_iter_batches_concatenate_to_full_scan(raster_h3, gradient_tif, eager):
    batches = list(raster_h3.RasterH3ArrowStream(gradient_tif, RES).iter_batches(batch_size=7))
    assert len(batches) > 1 and all(b.height <= 7 for b in batches)
    assert sort(pl.concat(batches)).equals(eager)


def test_categorical_long_matches_reference(raster_h3, classes_tif, classes_expected):
    stream = raster_h3.RasterH3ArrowStream(classes_tif, RES, categorical=True, format="long")
    out = frame(pa.table(stream))
    got = {(r["h3_index"], r["category"]): r for r in out.iter_rows(named=True)}
    want = {
        (cell, cls): (n, sum(classes.values()))
        for cell, classes in classes_expected.items()
        for cls, n in classes.items()
    }
    assert set(got) == set(want)
    for key, (n, total) in want.items():
        assert got[key]["count"] == n
        assert got[key]["total_count"] == total
        assert got[key]["fraction"] == pytest.approx(n / total)


def test_categorical_wide_matches_reference(raster_h3, classes_tif, classes_expected):
    out = frame(pa.table(raster_h3.RasterH3ArrowStream(classes_tif, RES, categorical=True)))
    assert out.height == len(classes_expected)
    for row in out.iter_rows(named=True):
        classes = classes_expected[row["h3_index"]]
        total = sum(classes.values())
        top = max(classes.values())
        assert row["majority_count"] == top
        assert classes[row["majority_class"]] == top
        assert row["unique_classes"] == len(classes)
        assert row["total_count"] == total
        assert row["majority_fraction"] == pytest.approx(top / total)


def test_missing_file_raises(raster_h3, tmp_path):
    missing = tmp_path / "missing.tif"
    with pytest.raises(RuntimeError):
        pa.table(raster_h3.RasterH3ArrowStream(missing, RES))
    with pytest.raises(RuntimeError):
        raster_h3.RasterH3ArrowStream(missing, RES).to_polars()
    with pytest.raises(RuntimeError):
        list(raster_h3.RasterH3ArrowStream(missing, RES).iter_batches())


def test_unknown_column_raises(raster_h3, gradient_tif):
    with pytest.raises(RuntimeError, match="nope"):
        pa.table(raster_h3.RasterH3ArrowStream(gradient_tif, RES, columns=["h3_index", "nope"]))
    with pytest.raises(RuntimeError, match="nope"):
        pa.schema(raster_h3.RasterH3ArrowStream(gradient_tif, RES, columns=["nope"]))


def test_bad_input_raises(raster_h3, gradient_tif, tmp_path):
    not_tiff = tmp_path / "not_a.tif"
    not_tiff.write_bytes(b"definitely not a TIFF")
    with pytest.raises(RuntimeError):
        pa.table(raster_h3.RasterH3ArrowStream(not_tiff, RES))
    with pytest.raises(RuntimeError, match="bounding box"):
        pa.table(raster_h3.RasterH3ArrowStream(gradient_tif, RES, bbox=[5, 5, 6, 6]))
    for bbox in ["1,2,3", "x,y,z,w", [1, 2, 3]]:
        with pytest.raises(RuntimeError, match="Invalid bbox"):
            pa.table(raster_h3.RasterH3ArrowStream(gradient_tif, RES, bbox=bbox))
    for res in [99, "abc", [8, 16], "", []]:
        with pytest.raises(RuntimeError, match="Invalid resolutions"):
            pa.table(raster_h3.RasterH3ArrowStream(gradient_tif, res))
    with pytest.raises(RuntimeError, match="Invalid format"):
        pa.table(
            raster_h3.RasterH3ArrowStream(gradient_tif, RES, categorical=True, format="tall")
        )
