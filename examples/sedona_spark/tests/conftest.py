"""Fixtures for running the Apache Sedona (Spark) demo's pipeline under pytest.

These tests need PySpark, Apache Sedona (its Python package and jars) and the `raster_h3`
package, as in the `sedona` image built from `Dockerfile.sedona`:

    docker build -f Dockerfile.sedona -t raster_h3-sedona .
    docker run --rm -v "$PWD/examples/sedona_spark:/opt/raster_h3/examples" raster_h3-sedona \\
        sh -c "pip3 install --break-system-packages pytest && python3 -m pytest -v /opt/raster_h3/examples/tests"

Outside such an environment the whole module is skipped.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

DEMO_DIR = Path(__file__).resolve().parents[1]

for module in ("pyspark", "sedona.spark", "raster_h3", "numpy"):
    pytest.importorskip(module)

# The executors' Python workers unpickle the demo's `mapInArrow` functions by module name,
# so the demo directory must be importable there as well as here.
sys.path.insert(0, str(DEMO_DIR))
os.environ["PYTHONPATH"] = os.pathsep.join(filter(None, [str(DEMO_DIR), os.environ.get("PYTHONPATH")]))

import raster_h3_spark_demo as demo  # noqa: E402


@pytest.fixture(scope="session")
def sedona():
    """One local Spark session with Sedona's functions, shared by every test."""
    spark = demo.sedona_session(os.environ.get("SPARK_MASTER", "local[2]"), shuffle_partitions=4)
    spark.sparkContext.setLogLevel("ERROR")
    yield spark
    spark.stop()


@pytest.fixture
def terrain(tmp_path):
    """Write the demo's synthetic tiled terrain; returns a factory of paths."""

    def write(name: str = "terrain.tif", side: int = 512, tile: int = 64, categorical: bool = False) -> str:
        path = str(tmp_path / name)
        demo.write_sample(path, side=side, tile=tile, categorical=categorical)
        return path

    return write
