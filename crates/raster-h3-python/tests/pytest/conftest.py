"""Fixtures for the pytest suite that runs the in-tree `raster_h3` package against real
Polars and PyArrow.

The package is imported from `crates/raster-h3-python/python` and loads the native library
from `RASTER_H3_PYTHON_LIB` or, failing that, `target/{release,debug}` (build it with
`cargo build -p raster-h3-python --lib`). Rasters are written by a minimal TIFF writer, and
the expected per-cell values are computed independently by indexing every pixel centre with
the `h3` package.
"""

from __future__ import annotations

import struct
import sys
from collections import defaultdict
from pathlib import Path
from typing import Callable, Dict, Tuple

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "python"))

# Near San Francisco; 0.0005° pixels give a few dozen resolution-9 cells per raster.
ORIGIN = (-122.45, 37.80)
PIXEL = 0.0005
RES = 9


def write_geotiff(
    path: Path,
    width: int,
    height: int,
    value: Callable[[int, int], float],
    *,
    dtype: str = "f32",
    origin: Tuple[float, float] = ORIGIN,
    pixel: float = PIXEL,
) -> None:
    """Uncompressed little-endian single-band GeoTIFF (EPSG:4326 GeoKeys), one strip.

    `dtype` is `"f32"` (IEEE float) or `"u8"`; `value(col, row)` gives each pixel.
    """
    fmt, bits, sample_format = {"f32": ("f", 32, 3), "u8": ("B", 8, 1)}[dtype]
    values = [value(c, r) for r in range(height) for c in range(width)]
    data = struct.pack(f"<{len(values)}{fmt}", *values)
    scale = struct.pack("<3d", pixel, pixel, 0.0)
    tiepoint = struct.pack("<6d", 0.0, 0.0, 0.0, origin[0], origin[1], 0.0)
    geokeys = struct.pack("<12H", 1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326)
    blobs = [scale, tiepoint, geokeys]
    # (tag, type, count, value-or-blob-index); SHORT=3, LONG=4, DOUBLE=12
    entries = [
        (256, 3, 1, width), (257, 3, 1, height), (258, 3, 1, bits), (259, 3, 1, 1),
        (262, 3, 1, 1), (273, 4, 1, "data"), (277, 3, 1, 1), (278, 3, 1, height),
        (279, 4, 1, len(data)), (339, 3, 1, sample_format),
        (33550, 12, 3, 0), (33922, 12, 6, 1), (34735, 3, 12, 2),
    ]
    offset = 8 + 2 + 12 * len(entries) + 4
    blob_offsets = []
    for blob in blobs:
        blob_offsets.append(offset)
        offset += len(blob)
    data_offset = offset

    out = bytearray(b"II*\x00" + struct.pack("<I", 8) + struct.pack("<H", len(entries)))
    for tag, typ, count, val in entries:
        if val == "data":
            val = data_offset
        elif tag in (33550, 33922, 34735):
            val = blob_offsets[val]
        if typ == 3 and count == 1:
            out += struct.pack("<HHIHH", tag, typ, count, val, 0)
        else:
            out += struct.pack("<HHII", tag, typ, count, val)
    out += struct.pack("<I", 0)
    for blob in blobs:
        out += blob
    out += data
    path.write_bytes(bytes(out))


def gradient_value(c: int, r: int) -> float:
    # Integers keep float32 sums exact.
    return float((c + 3 * r) % 17 + 1)


def class_value(c: int, r: int) -> int:
    return (c // 4 + r // 4) % 5 + 1


def pixel_cells(width: int, height: int, res: int) -> Dict[Tuple[int, int], int]:
    """H3 cell of each pixel centre, indexed directly with the `h3` package."""
    h3 = pytest.importorskip("h3")
    cells = {}
    for r in range(height):
        for c in range(width):
            lng = ORIGIN[0] + PIXEL * (c + 0.5)
            lat = ORIGIN[1] - PIXEL * (r + 0.5)
            cells[(c, r)] = h3.str_to_int(h3.latlng_to_cell(lat, lng, res))
    return cells


def frame(obj):
    """`pl.from_arrow` result as a DataFrame (Polars 2 reads a struct stream as a Series)."""
    import polars as pl

    out = pl.from_arrow(obj)
    return out.struct.unnest() if isinstance(out, pl.Series) else out


def sort(df):
    return df.sort("h3_index")


@pytest.fixture(scope="session")
def raster_h3():
    import raster_h3

    raster_h3._get_lib()
    return raster_h3


@pytest.fixture(scope="session")
def gradient_tif(tmp_path_factory) -> Path:
    path = tmp_path_factory.mktemp("rasters") / "gradient.tif"
    write_geotiff(path, 64, 48, gradient_value)
    return path


@pytest.fixture(scope="session")
def classes_tif(tmp_path_factory) -> Path:
    path = tmp_path_factory.mktemp("rasters") / "classes.tif"
    write_geotiff(path, 48, 48, class_value, dtype="u8")
    return path


@pytest.fixture(scope="session")
def eager(raster_h3, gradient_tif):
    """Full continuous scan of `gradient_tif`, read through `pyarrow.table`, sorted by cell."""
    pa = pytest.importorskip("pyarrow")
    pytest.importorskip("polars")
    return sort(frame(pa.table(raster_h3.RasterH3ArrowStream(gradient_tif, RES))))


@pytest.fixture(scope="session")
def gradient_expected() -> Dict[int, Dict[str, float]]:
    """Per-cell count, sum, mean, min and max of `gradient_tif`."""
    acc: Dict[int, list] = defaultdict(list)
    for (c, r), cell in pixel_cells(64, 48, RES).items():
        acc[cell].append(gradient_value(c, r))
    return {
        cell: {
            "count": float(len(v)),
            "sum": sum(v),
            "mean": sum(v) / len(v),
            "min": min(v),
            "max": max(v),
        }
        for cell, v in acc.items()
    }


@pytest.fixture(scope="session")
def classes_expected() -> Dict[int, Dict[int, float]]:
    """Per-cell pixel count of each class in `classes_tif`."""
    acc: Dict[int, Dict[int, float]] = defaultdict(lambda: defaultdict(float))
    for (c, r), cell in pixel_cells(48, 48, RES).items():
        acc[cell][class_value(c, r)] += 1.0
    return {cell: dict(classes) for cell, classes in acc.items()}
