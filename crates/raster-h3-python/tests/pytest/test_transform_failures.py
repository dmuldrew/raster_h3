"""Samples dropped by failed coordinate transforms: lenient scans warn once the scan
completes, and `strict_transforms=True` raises instead of returning partial results.
"""

from __future__ import annotations

import warnings

import pytest

from conftest import write_geotiff

pl = pytest.importorskip("polars")
pytest.importorskip("pyarrow")


@pytest.fixture(scope="module")
def past_pole_tif(tmp_path_factory):
    # 10 x 20 one-degree pixels from 95N: the top 5 rows (50 pixels) lie beyond the pole.
    path = tmp_path_factory.mktemp("rasters") / "past_pole.tif"
    write_geotiff(path, 10, 20, lambda c, r: 3.0, origin=(0.0, 95.0), pixel=1.0)
    return path


def test_lenient_scans_warn_with_the_count(raster_h3, past_pole_tif):
    stream = raster_h3.RasterH3ArrowStream(past_pole_tif, 3)
    with pytest.warns(RuntimeWarning, match=r"50 valid sample\(s\) were dropped"):
        frames = list(stream.iter_batches())
    assert sum(df.height for df in frames) > 0

    with pytest.warns(RuntimeWarning, match=r"50 valid sample\(s\)"):
        assert stream.to_dict()["h3_index"]

    lazy = raster_h3.scan_raster_h3(past_pole_tif, 3)
    if isinstance(lazy, pl.LazyFrame):
        with pytest.warns(RuntimeWarning, match=r"50 valid sample\(s\)"):
            assert lazy.collect().height > 0


def test_warnings_as_errors_still_free_the_result(raster_h3, past_pole_tif, monkeypatch):
    lib = raster_h3._get_lib()
    freed = []
    real_free = lib.raster_h3_py_free_string

    def recording_free(ptr):
        freed.append(ptr)
        real_free(ptr)

    monkeypatch.setattr(lib, "raster_h3_py_free_string", recording_free)
    with warnings.catch_warnings():
        warnings.simplefilter("error", RuntimeWarning)
        with pytest.raises(RuntimeWarning, match=r"50 valid sample\(s\)"):
            raster_h3.RasterH3ArrowStream(past_pole_tif, 3).to_dict()
    assert len(freed) == 1


def test_valid_rasters_do_not_warn(raster_h3, gradient_tif):
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        list(raster_h3.RasterH3ArrowStream(gradient_tif, 9).iter_batches())
        raster_h3.RasterH3ArrowStream(gradient_tif, 9, strict_transforms=True).to_polars()


def test_strict_scans_raise(raster_h3, past_pole_tif):
    stream = raster_h3.RasterH3ArrowStream(past_pole_tif, 3, strict_transforms=True)
    with pytest.raises(Exception, match="strict_transforms"):
        list(stream.iter_batches())
    with pytest.raises(Exception, match="strict_transforms"):
        stream.to_dict()
    with pytest.raises(Exception, match="strict_transforms"):
        stream.to_polars()
    with pytest.raises(Exception, match="strict_transforms"):
        raster_h3.read_raster_h3(past_pole_tif, 3, strict_transforms=True)

    lazy = raster_h3.scan_raster_h3(past_pole_tif, 3, strict_transforms=True)
    if isinstance(lazy, pl.LazyFrame):
        with pytest.raises(Exception, match="strict_transforms"):
            lazy.collect()
