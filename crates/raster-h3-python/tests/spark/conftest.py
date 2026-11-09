"""Fixtures for the Spark 4.1 filter-pushdown tests of `raster_h3.spark`.

They need PySpark 4.1+ (with Java), NumPy and the `raster_h3` package with its native
library, but not Apache Sedona. CI runs them in the `sedona` image with PySpark 4.1
installed beside its Spark 4.0:

    pip3 install --target /tmp/ps41 "pyspark==4.1.*"
    env -u SPARK_HOME PYTHONPATH=/tmp/ps41 python3 -m pytest crates/raster-h3-python/tests/spark

Without PySpark 4.1 the module is skipped.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

for module in ("pyspark.sql.datasource", "raster_h3.spark", "numpy"):
    pytest.importorskip(module)

import raster_h3.spark as spark_ds  # noqa: E402

if not spark_ds._PUSHDOWN:
    pytest.skip("filter pushdown needs PySpark 4.1+", allow_module_level=True)

# The tiled terrain writer from the Apache Sedona demo.
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "examples" / "sedona_spark"))
import raster_h3_spark_demo as demo  # noqa: E402


@pytest.fixture(scope="session")
def spark():
    from pyspark.sql import SparkSession

    os.environ.setdefault("PYSPARK_PYTHON", sys.executable)
    session = (
        SparkSession.builder.master(os.environ.get("SPARK_MASTER", "local[2]"))
        .appName("raster_h3 pushdown tests")
        .config("spark.sql.shuffle.partitions", "4")
        .getOrCreate()
    )
    session.sparkContext.setLogLevel("ERROR")
    yield session
    session.stop()


@pytest.fixture(scope="session")
def terrain(tmp_path_factory):
    """A 512 x 512 px float raster in 64 tiles (9 partitions of ~8 tiles)."""
    path = str(tmp_path_factory.mktemp("rasters") / "terrain.tif")
    demo.write_sample(path, side=512, tile=64)
    return path
