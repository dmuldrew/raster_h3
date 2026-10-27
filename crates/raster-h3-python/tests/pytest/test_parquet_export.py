"""`raster_to_parquet` output read back with Polars' native Parquet reader (not
`use_pyarrow=True`): parquet-rs 60 footers made it fail with "Invalid thrift: bad data".
"""

from __future__ import annotations

import pytest

pl = pytest.importorskip("polars")

from conftest import RES  # noqa: E402


@pytest.mark.parametrize("geoparquet", [True, False])
@pytest.mark.parametrize("categorical", [False, True])
def test_native_polars_reads_exporter_output(
    raster_h3, gradient_tif, classes_tif, tmp_path, geoparquet, categorical
):
    tif = classes_tif if categorical else gradient_tif
    out = tmp_path / "cells.parquet"
    written = raster_h3.raster_to_parquet(
        tif, out, RES, categorical=categorical, geoparquet=geoparquet
    )
    assert written > 0

    df = pl.read_parquet(out)
    assert df.height == written
    assert ("geometry" in df.columns) == geoparquet
    assert df["h3_index"].n_unique() == written

    lazy = pl.scan_parquet(out).select(pl.len()).collect().item()
    assert lazy == written
