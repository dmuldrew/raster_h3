"""
raster_h3: High-performance Python, Polars, and Arrow PyCapsule bindings for GeoTIFF/COG to H3 aggregation.

Provides:
- `RasterH3ArrowStream`: Zero-copy Arrow PyCapsule producer (`__arrow_c_stream__`, `__arrow_c_schema__`)
  compatible with `polars.from_arrow()`, `pyarrow.table()`, `duckdb`, `daft`, `lonboard`, and `pydeck`.
- `scan_raster_h3()` / `read_raster_h3()`: Polars IO plugin (`register_io_source`) with column projection
  and row-limit pushdown.
- Scalar and vectorized H3 helpers (`h3_to_string`, `string_to_h3`, `h3_to_lat`, `h3_to_lng`,
  `h3_get_resolution`, `h3_is_valid`, `h3_to_wkb`, `h3_cell_to_parent`, `h3_polygon_to_cells`).
- Direct file exporters (`raster_to_parquet`, `raster_to_pmtiles`).
- Partitioned scans for distributed engines such as Spark (`plan_partitions`,
  `RasterH3ArrowStream(partition=...)`, `merge_partials`).
"""

from __future__ import annotations

import ctypes
import json
import math
import os
import sys
import warnings
from collections.abc import Mapping
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple, Union

# -----------------------------------------------------------------------------
# Dynamic Library Discovery & C-ABI Signatures
# -----------------------------------------------------------------------------

def _find_native_library() -> str:
    env_override = os.environ.get("RASTER_H3_PYTHON_LIB")
    if env_override and os.path.exists(env_override):
        return env_override

    if sys.platform == "darwin":
        names = ["libraster_h3_python.dylib"]
    elif sys.platform == "win32":
        names = ["raster_h3_python.dll", "libraster_h3_python.dll"]
    else:
        names = ["libraster_h3_python.so"]

    pkg_dir = Path(__file__).resolve().parent
    # Search package directory and workspace target/{release,debug} directories
    search_dirs = [
        pkg_dir,
        pkg_dir.parent.parent.parent.parent / "target" / "release",
        pkg_dir.parent.parent.parent.parent / "target" / "debug",
    ]
    for d in search_dirs:
        for name in names:
            cand = d / name
            if cand.exists():
                return str(cand)

    raise RuntimeError(
        "Could not locate libraster_h3_python shared library. "
        "Run `cargo build -p raster-h3-python` or set RASTER_H3_PYTHON_LIB."
    )


_LIB: Optional[ctypes.CDLL] = None


def _get_lib() -> ctypes.CDLL:
    global _LIB
    if _LIB is not None:
        return _LIB

    lib = ctypes.CDLL(_find_native_library())

    # Schema & Stream C-ABI
    lib.raster_h3_py_create_arrow_schema.argtypes = [
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.raster_h3_py_create_arrow_schema.restype = ctypes.c_void_p

    lib.raster_h3_py_free_arrow_schema.argtypes = [ctypes.c_void_p]
    lib.raster_h3_py_free_arrow_schema.restype = None

    lib.raster_h3_py_create_arrow_stream.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_double,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int64,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.raster_h3_py_create_arrow_stream.restype = ctypes.c_void_p

    lib.raster_h3_py_free_arrow_stream.argtypes = [ctypes.c_void_p]
    lib.raster_h3_py_free_arrow_stream.restype = None

    lib.raster_h3_py_open_batch_reader.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_double,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int64,
        ctypes.c_int64,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.raster_h3_py_open_batch_reader.restype = ctypes.c_void_p

    lib.raster_h3_py_batch_reader_next.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.raster_h3_py_batch_reader_next.restype = ctypes.c_void_p

    lib.raster_h3_py_free_batch_reader.argtypes = [ctypes.c_void_p]
    lib.raster_h3_py_free_batch_reader.restype = None

    lib.raster_h3_py_scan_json.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_double,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int64,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.raster_h3_py_scan_json.restype = ctypes.c_void_p

    lib.raster_h3_py_free_string.argtypes = [ctypes.c_void_p]
    lib.raster_h3_py_free_string.restype = None

    # `_ex` scan variants (newer libraries) add `strict_transforms`; the JSON scan
    # also reports dropped samples, and batch readers expose a running count.
    scan_args = list(lib.raster_h3_py_scan_json.argtypes[:-1])
    err_ptr = [ctypes.POINTER(ctypes.c_void_p)]
    if hasattr(lib, "raster_h3_py_create_arrow_stream_ex"):
        lib.raster_h3_py_create_arrow_stream_ex.argtypes = scan_args + [ctypes.c_int] + err_ptr
        lib.raster_h3_py_create_arrow_stream_ex.restype = ctypes.c_void_p
        lib.raster_h3_py_open_batch_reader_ex.argtypes = (
            scan_args + [ctypes.c_int64, ctypes.c_int] + err_ptr
        )
        lib.raster_h3_py_open_batch_reader_ex.restype = ctypes.c_void_p
        lib.raster_h3_py_scan_json_ex.argtypes = (
            scan_args + [ctypes.c_int, ctypes.POINTER(ctypes.c_uint64)] + err_ptr
        )
        lib.raster_h3_py_scan_json_ex.restype = ctypes.c_void_p
        lib.raster_h3_py_batch_reader_transform_failures.argtypes = [ctypes.c_void_p]
        lib.raster_h3_py_batch_reader_transform_failures.restype = ctypes.c_uint64

        # Partitioned scans (newer libraries).
        if hasattr(lib, "raster_h3_py_merge_partials"):
            lib.raster_h3_py_plan_partitions.argtypes = [
                ctypes.c_char_p,
                ctypes.c_char_p,
                ctypes.c_char_p,
                ctypes.c_int64,
            ] + err_ptr
            lib.raster_h3_py_plan_partitions.restype = ctypes.c_void_p
            lib.raster_h3_py_create_arrow_schema_partitioned.argtypes = list(
                lib.raster_h3_py_create_arrow_schema.argtypes
            )
            lib.raster_h3_py_create_arrow_schema_partitioned.restype = ctypes.c_void_p
            lib.raster_h3_py_create_arrow_stream_partitioned.argtypes = (
                scan_args + [ctypes.c_int, ctypes.c_char_p] + err_ptr
            )
            lib.raster_h3_py_create_arrow_stream_partitioned.restype = ctypes.c_void_p
            lib.raster_h3_py_open_batch_reader_partitioned.argtypes = (
                scan_args + [ctypes.c_int64, ctypes.c_int, ctypes.c_char_p] + err_ptr
            )
            lib.raster_h3_py_open_batch_reader_partitioned.restype = ctypes.c_void_p
            lib.raster_h3_py_merge_partials.argtypes = [
                ctypes.c_void_p,
                ctypes.c_int,
                ctypes.c_char_p,
            ] + err_ptr
            lib.raster_h3_py_merge_partials.restype = ctypes.c_void_p

    # `_v2` scan variants (newer libraries) add a partition argument (NULL for a whole
    # scan) and an options JSON object, which carries band, sampling and the other options
    # the positional arguments have no slot for.
    if hasattr(lib, "raster_h3_py_create_arrow_stream_v2"):
        lib.raster_h3_py_create_arrow_stream_v2.argtypes = (
            scan_args + [ctypes.c_int, ctypes.c_char_p, ctypes.c_char_p] + err_ptr
        )
        lib.raster_h3_py_create_arrow_stream_v2.restype = ctypes.c_void_p
        lib.raster_h3_py_open_batch_reader_v2.argtypes = (
            scan_args + [ctypes.c_int64, ctypes.c_int, ctypes.c_char_p, ctypes.c_char_p] + err_ptr
        )
        lib.raster_h3_py_open_batch_reader_v2.restype = ctypes.c_void_p
        lib.raster_h3_py_scan_json_v2.argtypes = (
            scan_args
            + [ctypes.c_int, ctypes.c_char_p, ctypes.POINTER(ctypes.c_uint64)]
            + err_ptr
        )
        lib.raster_h3_py_scan_json_v2.restype = ctypes.c_void_p
        lib.raster_h3_py_plan_partitions_v2.argtypes = [
            ctypes.c_char_p,
            ctypes.c_char_p,
            ctypes.c_char_p,
            ctypes.c_int64,
            ctypes.c_char_p,
        ] + err_ptr
        lib.raster_h3_py_plan_partitions_v2.restype = ctypes.c_void_p

    if hasattr(lib, "raster_h3_py_merge_partials_v2"):
        lib.raster_h3_py_merge_partials_v2.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_char_p,
        ] + err_ptr
        lib.raster_h3_py_merge_partials_v2.restype = ctypes.c_void_p
        lib.raster_h3_py_create_arrow_schema_v2.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
        ] + err_ptr
        lib.raster_h3_py_create_arrow_schema_v2.restype = ctypes.c_void_p
        lib.raster_h3_py_quantile_columns.argtypes = [ctypes.c_char_p] + err_ptr
        lib.raster_h3_py_quantile_columns.restype = ctypes.c_void_p

    if hasattr(lib, "raster_h3_py_parquet_to_pmtiles"):
        lib.raster_h3_py_parquet_to_pmtiles.argtypes = [
            ctypes.c_char_p,
            ctypes.c_char_p,
            ctypes.c_char_p,
        ] + err_ptr
        lib.raster_h3_py_parquet_to_pmtiles.restype = ctypes.c_int64

    # Scalar C-ABI
    lib.raster_h3_py_h3_to_string.argtypes = [ctypes.c_uint64]
    lib.raster_h3_py_h3_to_string.restype = ctypes.c_void_p

    lib.raster_h3_py_string_to_h3.argtypes = [ctypes.c_char_p]
    lib.raster_h3_py_string_to_h3.restype = ctypes.c_uint64

    lib.raster_h3_py_h3_to_lat.argtypes = [ctypes.c_uint64]
    lib.raster_h3_py_h3_to_lat.restype = ctypes.c_double

    lib.raster_h3_py_h3_to_lng.argtypes = [ctypes.c_uint64]
    lib.raster_h3_py_h3_to_lng.restype = ctypes.c_double

    lib.raster_h3_py_h3_get_resolution.argtypes = [ctypes.c_uint64]
    lib.raster_h3_py_h3_get_resolution.restype = ctypes.c_int64

    lib.raster_h3_py_h3_is_valid_u64.argtypes = [ctypes.c_uint64]
    lib.raster_h3_py_h3_is_valid_u64.restype = ctypes.c_int

    lib.raster_h3_py_h3_is_valid_str.argtypes = [ctypes.c_char_p]
    lib.raster_h3_py_h3_is_valid_str.restype = ctypes.c_int

    lib.raster_h3_py_h3_cell_to_parent_u64.argtypes = [ctypes.c_uint64, ctypes.c_int64]
    lib.raster_h3_py_h3_cell_to_parent_u64.restype = ctypes.c_uint64

    lib.raster_h3_py_h3_cell_to_parent_str.argtypes = [ctypes.c_char_p, ctypes.c_int64]
    lib.raster_h3_py_h3_cell_to_parent_str.restype = ctypes.c_void_p

    lib.raster_h3_py_h3_to_wkb.argtypes = [
        ctypes.c_uint64,
        ctypes.POINTER(ctypes.c_uint8),
        ctypes.c_size_t,
    ]
    lib.raster_h3_py_h3_to_wkb.restype = ctypes.c_size_t

    lib.raster_h3_py_wkb_buf_len.argtypes = []
    lib.raster_h3_py_wkb_buf_len.restype = ctypes.c_size_t

    lib.raster_h3_py_h3_polygon_to_cells.argtypes = [
        ctypes.c_char_p,
        ctypes.c_size_t,
        ctypes.c_int64,
        ctypes.c_char_p,
        ctypes.POINTER(ctypes.POINTER(ctypes.c_uint64)),
        ctypes.POINTER(ctypes.c_size_t),
    ]
    lib.raster_h3_py_h3_polygon_to_cells.restype = ctypes.c_int

    lib.raster_h3_py_free_cells.argtypes = [ctypes.POINTER(ctypes.c_uint64), ctypes.c_size_t]
    lib.raster_h3_py_free_cells.restype = None

    # Exporters. The `_with_stats` variants (newer libraries) also report the
    # transform-failure count; the original symbols keep their signatures.
    parquet_args = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
    ]
    pmtiles_args = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
    ]
    err_arg = [ctypes.POINTER(ctypes.c_void_p)]
    stats_args = [ctypes.POINTER(ctypes.c_uint64)] + err_arg
    for name, args in (
        ("raster_h3_py_export_parquet", parquet_args),
        ("raster_h3_py_export_pmtiles", pmtiles_args),
    ):
        fn = getattr(lib, name)
        fn.argtypes = args + err_arg
        fn.restype = ctypes.c_int64
        if hasattr(lib, name + "_with_stats"):
            fn = getattr(lib, name + "_with_stats")
            fn.argtypes = args + stats_args
            fn.restype = ctypes.c_int64

    _LIB = lib
    return lib


# -----------------------------------------------------------------------------
# PyCapsule Destructors for Arrow C Data / Stream Interface
# -----------------------------------------------------------------------------

_PYCAPSULE_DESTRUCTOR = ctypes.CFUNCTYPE(None, ctypes.c_void_p)

ctypes.pythonapi.PyCapsule_New.argtypes = [
    ctypes.c_void_p,
    ctypes.c_char_p,
    _PYCAPSULE_DESTRUCTOR,
]
ctypes.pythonapi.PyCapsule_New.restype = ctypes.py_object

ctypes.pythonapi.PyCapsule_GetName.argtypes = [ctypes.c_void_p]
ctypes.pythonapi.PyCapsule_GetName.restype = ctypes.c_void_p

ctypes.pythonapi.PyCapsule_GetPointer.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
ctypes.pythonapi.PyCapsule_GetPointer.restype = ctypes.c_void_p


# `PyCapsule_GetPointer` taking the capsule as a Python object (the prototype above takes
# raw pointers, as the destructors receive them).
_capsule_object_pointer = ctypes.PYFUNCTYPE(ctypes.c_void_p, ctypes.py_object, ctypes.c_char_p)(
    ("PyCapsule_GetPointer", ctypes.pythonapi)
)


def _capsule_pointer(capsule_ptr: int) -> Optional[int]:
    # Consumers may rename a capsule once they take ownership (e.g. to
    # `used_arrow_array_stream`), so look it up by whatever name it has now.
    # The outer struct is still ours to free; a consumed payload has its
    # `release` set to NULL, which the native free skips.
    name = ctypes.pythonapi.PyCapsule_GetName(capsule_ptr)
    return ctypes.pythonapi.PyCapsule_GetPointer(capsule_ptr, name)


@_PYCAPSULE_DESTRUCTOR
def _schema_capsule_destructor(capsule_ptr: int) -> None:
    if capsule_ptr and _LIB is not None:
        ptr = _capsule_pointer(capsule_ptr)
        if ptr:
            _LIB.raster_h3_py_free_arrow_schema(ptr)


@_PYCAPSULE_DESTRUCTOR
def _stream_capsule_destructor(capsule_ptr: int) -> None:
    if capsule_ptr and _LIB is not None:
        ptr = _capsule_pointer(capsule_ptr)
        if ptr:
            _LIB.raster_h3_py_free_arrow_stream(ptr)


def _raise_if_err(err_ptr: ctypes.c_void_p) -> None:
    if err_ptr.value:
        lib = _get_lib()
        msg = ctypes.string_at(err_ptr.value).decode("utf-8", errors="replace")
        lib.raster_h3_py_free_string(err_ptr.value)
        raise RuntimeError(msg)


def _encode_resolutions(resolutions: Union[int, str, Sequence[int]]) -> bytes:
    if isinstance(resolutions, int):
        return str(resolutions).encode("utf-8")
    if isinstance(resolutions, str):
        return resolutions.encode("utf-8")
    return ",".join(str(r) for r in resolutions).encode("utf-8")


def _encode_bbox(bbox: Optional[Union[str, Sequence[float]]]) -> Optional[bytes]:
    if bbox is None:
        return None
    if isinstance(bbox, str):
        return bbox.encode("utf-8")
    return ",".join(str(x) for x in bbox).encode("utf-8")


def _encode_columns(columns: Optional[Sequence[str]]) -> Optional[bytes]:
    if not columns:
        return None
    return ",".join(columns).encode("utf-8")


#: Scan options sent to the `_v2` entry points as one JSON object, beyond the original
#: positional C-ABI arguments. Every one defaults to `None` (unset).
_SCAN_OPTIONS = (
    "band",
    "sampling",
    "overlap_rule",
    "spectral_formula",
    "nir_band",
    "red_band",
    "green_band",
    "blue_band",
    "swir_band",
    "quantiles",
    "remap",
    "min_count",
    "min_mean",
    "max_mean",
    "min_majority_fraction",
)

_INT_OPTIONS = ("band", "nir_band", "red_band", "green_band", "blue_band", "swir_band")
_STR_OPTIONS = ("sampling", "overlap_rule", "spectral_formula")
_FLOAT_OPTIONS = ("min_count", "min_mean", "max_mean", "min_majority_fraction")


def _encode_quantiles(value: Union[str, Sequence[Union[str, float]]]) -> str:
    """`"p50,p90"`, `["p50", "iqr"]` or `[0.5, 0.9]` as DuckDB's `quantiles :=` string."""
    if isinstance(value, str):
        return value
    items = list(value)
    for item in items:
        if isinstance(item, bool) or not isinstance(item, (str, int, float)):
            raise TypeError(
                f"quantiles entries must be str or float, got {type(item).__name__}"
            )
    # An empty sequence asks for no quantile columns.
    return ",".join(str(item) for item in items) or "none"


def _encode_remap(value: Union[str, Mapping[Any, Optional[int]]]) -> str:
    """A remap spec string, or a mapping as DuckDB's `remap :=` string.

    Keys are class codes (`int`), `range`s of codes (step 1), or `"else"` / `"default"` for
    every other class; values are the new class code, or `None` to drop the class.
    """
    if isinstance(value, str):
        return value
    if not isinstance(value, Mapping):
        raise TypeError(f"remap must be a str or a mapping, got {type(value).__name__}")
    entries = []
    for key, target in value.items():
        if isinstance(key, bool):
            raise TypeError("remap keys must be int, range or 'else'")
        if isinstance(key, int):
            src = str(key)
        elif isinstance(key, range):
            if key.step != 1 or len(key) == 0:
                raise ValueError(f"remap range keys must be non-empty with step 1, got {key!r}")
            src = f"{key.start}..={key.stop - 1}"
        elif isinstance(key, str) and key.strip().lower() in ("else", "default", "*", "_"):
            src = key.strip()
        else:
            raise TypeError(f"remap keys must be int, range or 'else', got {key!r}")
        if target is None:
            dst = "null"
        elif isinstance(target, int) and not isinstance(target, bool):
            dst = str(target)
        else:
            raise TypeError(f"remap targets must be int or None, got {target!r}")
        entries.append(f"{src}: {dst}")
    if not entries:
        raise ValueError("remap mapping is empty")
    return ", ".join(entries)


def _encode_scan_options(options: Mapping[str, Any]) -> Optional[bytes]:
    """The options JSON of the `_v2` entry points, or None when every option is unset.

    Types are checked here; names, ranges and which scans accept an option are checked by
    the native side.
    """
    opts: Dict[str, Any] = {}
    for key, value in options.items():
        if value is None:
            continue
        if key in _INT_OPTIONS:
            if isinstance(value, bool) or not isinstance(value, int):
                raise TypeError(f"{key} must be an int, got {type(value).__name__}")
            opts[key] = value
        elif key in _STR_OPTIONS:
            if not isinstance(value, str):
                raise TypeError(f"{key} must be a str, got {type(value).__name__}")
            opts[key] = value
        elif key in _FLOAT_OPTIONS:
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                raise TypeError(f"{key} must be a number, got {type(value).__name__}")
            number = float(value)
            # JSON has no NaN or infinity, so these are checked here rather than natively.
            if not math.isfinite(number):
                raise ValueError(f"{key} must be finite, got {number}")
            if key == "min_majority_fraction" and not 0.0 <= number <= 1.0:
                raise ValueError(f"min_majority_fraction must be between 0 and 1, got {number}")
            opts[key] = number
        elif key == "quantiles":
            opts[key] = _encode_quantiles(value)
        elif key == "remap":
            opts[key] = _encode_remap(value)
        else:
            raise TypeError(f"unknown scan option {key!r}")
    return json.dumps(opts).encode("utf-8") if opts else None


class _OwnedArrowStream:
    """Owns one native `ArrowArrayStream*` and hands it out once via `__arrow_c_stream__`."""

    def __init__(self, ptr: int) -> None:
        self._ptr: Optional[int] = ptr

    def __arrow_c_stream__(self, requested_schema: Any = None) -> Any:
        ptr, self._ptr = self._ptr, None
        if not ptr:
            raise RuntimeError("Arrow stream was already consumed")
        return ctypes.pythonapi.PyCapsule_New(
            ptr, b"arrow_array_stream", _stream_capsule_destructor
        )

    def __del__(self) -> None:
        if self._ptr and _LIB is not None:
            _LIB.raster_h3_py_free_arrow_stream(self._ptr)
            self._ptr = None


def _polars_from_stream(obj: Any) -> Any:
    """Build a Polars DataFrame from an object exposing `__arrow_c_stream__`."""
    import polars as pl  # type: ignore

    try:
        result = pl.from_arrow(obj)
    except TypeError:
        # Polars predating Arrow PyCapsule support rejects the object before reading it.
        import pyarrow as pa  # type: ignore

        result = pl.from_arrow(pa.table(obj))
    if isinstance(result, pl.Series):
        # Polars 2 reads a struct-typed Arrow stream as one struct Series.
        result = result.struct.unnest()
    return result


# -----------------------------------------------------------------------------
# Arrow PyCapsule Stream Object
# -----------------------------------------------------------------------------

class RasterH3ArrowStream:
    """
    Lazy Arrow C Stream producer implementing the Arrow PyCapsule Protocol
    (`__arrow_c_schema__` and `__arrow_c_stream__`).

    Can be passed directly to:
    - `polars.from_arrow(stream)`
    - `pyarrow.table(stream)`
    - `duckdb.from_arrow(stream)`
    - `lonboard.viz(stream)`
    """

    def __init__(
        self,
        path: Union[str, Path],
        resolutions: Union[int, str, Sequence[int]] = 8,
        *,
        categorical: bool = False,
        format: str = "wide",
        crs: Optional[str] = None,
        nodata: Optional[float] = None,
        bbox: Optional[Union[str, Sequence[float]]] = None,
        columns: Optional[Sequence[str]] = None,
        limit: Optional[int] = None,
        strict_transforms: bool = False,
        partition: Optional[str] = None,
        band: Optional[int] = None,
        sampling: Optional[str] = None,
        overlap_rule: Optional[str] = None,
        spectral_formula: Optional[str] = None,
        nir_band: Optional[int] = None,
        red_band: Optional[int] = None,
        green_band: Optional[int] = None,
        blue_band: Optional[int] = None,
        swir_band: Optional[int] = None,
        quantiles: Optional[Union[str, Sequence[Union[str, float]]]] = None,
        remap: Optional[Union[str, Mapping[Any, Optional[int]]]] = None,
        min_count: Optional[float] = None,
        min_mean: Optional[float] = None,
        max_mean: Optional[float] = None,
        min_majority_fraction: Optional[float] = None,
    ) -> None:
        self.path = str(path)
        self.resolutions = resolutions
        self.categorical = categorical
        self.format = format
        self.crs = crs
        self.nodata = nodata
        self.bbox = bbox
        self.columns = list(columns) if columns is not None else None
        self.limit = limit
        self.strict_transforms = strict_transforms
        # A partition spec from `plan_partitions`: scan only that part of the raster. The
        # output gains a `partial_state` column; see `merge_partials`.
        self.partition = partition
        # Options beyond the original positional C-ABI arguments, sent as one JSON object
        # to the `_v2` entry points. Encoding here rejects wrongly typed values early; the
        # native side validates names and ranges.
        _locals = locals()
        #: The set scan options (see `_SCAN_OPTIONS`), by name.
        self.options: Dict[str, Any] = {
            k: _locals[k] for k in _SCAN_OPTIONS if _locals[k] is not None
        }
        self._options = _encode_scan_options(self.options)

    def _has_v2(self, lib: ctypes.CDLL) -> bool:
        """Whether to call the `_v2` scan variants; the extra scan options require them."""
        if hasattr(lib, "raster_h3_py_create_arrow_stream_v2"):
            return True
        if self._options is not None:
            raise RuntimeError(
                f"the scan options {sorted(self.options)} require a newer raster_h3 native "
                "library"
            )
        return False

    def _partition_arg(self, lib: ctypes.CDLL) -> bytes:
        if not hasattr(lib, "raster_h3_py_merge_partials"):
            raise RuntimeError("partitioned scans require a newer raster_h3 native library")
        assert self.partition is not None
        return self.partition.encode("utf-8")

    def _scan_args(self) -> Tuple[Any, ...]:
        return (
            self.path.encode("utf-8"),
            _encode_resolutions(self.resolutions),
            1 if self.categorical else 0,
            self.format.encode("utf-8"),
            self.crs.encode("utf-8") if self.crs else None,
            1 if self.nodata is not None else 0,
            float(self.nodata) if self.nodata is not None else 0.0,
            _encode_bbox(self.bbox),
            _encode_columns(self.columns),
            int(self.limit) if self.limit is not None else -1,
        )

    def _has_ex(self, lib: ctypes.CDLL) -> bool:
        """Whether to call the `_ex` scan variants; `strict_transforms` requires them."""
        if hasattr(lib, "raster_h3_py_scan_json_ex"):
            return True
        if self.strict_transforms:
            raise RuntimeError(
                "strict_transforms requires a newer raster_h3 native library"
            )
        return False

    def __arrow_c_schema__(self) -> Any:
        lib = _get_lib()
        err = ctypes.c_void_p(0)
        if hasattr(lib, "raster_h3_py_create_arrow_schema_v2"):
            if self.partition is not None:
                self._partition_arg(lib)
            ptr = lib.raster_h3_py_create_arrow_schema_v2(
                1 if self.categorical else 0,
                self.format.encode("utf-8"),
                _encode_columns(self.columns),
                1 if self.partition is not None else 0,
                self._options,
                ctypes.byref(err),
            )
            _raise_if_err(err)
            if not ptr:
                raise RuntimeError("Failed to create ArrowSchema")
            return ctypes.pythonapi.PyCapsule_New(
                ptr, b"arrow_schema", _schema_capsule_destructor
            )
        self._has_v2(lib)  # raises when options need a newer library
        create = lib.raster_h3_py_create_arrow_schema
        if self.partition is not None:
            self._partition_arg(lib)
            create = lib.raster_h3_py_create_arrow_schema_partitioned
        ptr = create(
            1 if self.categorical else 0,
            self.format.encode("utf-8"),
            _encode_columns(self.columns),
            ctypes.byref(err),
        )
        _raise_if_err(err)
        if not ptr:
            raise RuntimeError("Failed to create ArrowSchema")
        return ctypes.pythonapi.PyCapsule_New(
            ptr, b"arrow_schema", _schema_capsule_destructor
        )

    def __arrow_c_stream__(self, requested_schema: Any = None) -> Any:
        lib = _get_lib()
        err = ctypes.c_void_p(0)
        if self._has_v2(lib):
            ptr = lib.raster_h3_py_create_arrow_stream_v2(
                *self._scan_args(),
                1 if self.strict_transforms else 0,
                self._partition_arg(lib) if self.partition is not None else None,
                self._options,
                ctypes.byref(err),
            )
        elif self.partition is not None:
            ptr = lib.raster_h3_py_create_arrow_stream_partitioned(
                *self._scan_args(),
                1 if self.strict_transforms else 0,
                self._partition_arg(lib),
                ctypes.byref(err),
            )
        elif self._has_ex(lib):
            ptr = lib.raster_h3_py_create_arrow_stream_ex(
                *self._scan_args(), 1 if self.strict_transforms else 0, ctypes.byref(err)
            )
        else:
            ptr = lib.raster_h3_py_create_arrow_stream(*self._scan_args(), ctypes.byref(err))
        _raise_if_err(err)
        if not ptr:
            raise RuntimeError("Failed to create ArrowArrayStream")
        return ctypes.pythonapi.PyCapsule_New(
            ptr, b"arrow_array_stream", _stream_capsule_destructor
        )

    def to_dict(self) -> Dict[str, List[Any]]:
        """Materialize the scan into a Python dictionary of column lists."""
        if self.partition is not None:
            # The JSON path has no partial-state column; go through Arrow (needs pyarrow),
            # batch by batch so the transform-failure count is reported like other scans.
            return self._partition_table(stacklevel=4).to_pydict()
        lib = _get_lib()
        err = ctypes.c_void_p(0)
        failures = ctypes.c_uint64(0)
        if self._has_v2(lib):
            json_ptr = lib.raster_h3_py_scan_json_v2(
                *self._scan_args(),
                1 if self.strict_transforms else 0,
                self._options,
                ctypes.byref(failures),
                ctypes.byref(err),
            )
        elif self._has_ex(lib):
            json_ptr = lib.raster_h3_py_scan_json_ex(
                *self._scan_args(),
                1 if self.strict_transforms else 0,
                ctypes.byref(failures),
                ctypes.byref(err),
            )
        else:
            json_ptr = lib.raster_h3_py_scan_json(*self._scan_args(), ctypes.byref(err))
        _raise_if_err(err)
        if not json_ptr:
            raise RuntimeError("Failed to execute scan_json")
        try:
            raw = ctypes.string_at(json_ptr).decode("utf-8")
            result = json.loads(raw)
        finally:
            lib.raster_h3_py_free_string(json_ptr)
        # Warn only once the native result is freed: the warning may be raised as an error.
        _warn_transform_failures(failures.value, stacklevel=3)
        return result

    def _partition_table(self, stacklevel: int) -> Any:
        """A partitioned scan as a PyArrow Table, warning about dropped samples at the end."""
        import pyarrow as pa  # type: ignore

        lib = _get_lib()
        reader = self._open_reader(lib, None)
        tables = []
        try:
            while True:
                err = ctypes.c_void_p(0)
                ptr = lib.raster_h3_py_batch_reader_next(reader, ctypes.byref(err))
                _raise_if_err(err)
                if not ptr:
                    failures = lib.raster_h3_py_batch_reader_transform_failures(reader)
                    break
                tables.append(pa.table(_OwnedArrowStream(ptr)))
        finally:
            lib.raster_h3_py_free_batch_reader(reader)
        table = pa.concat_tables(tables) if tables else pa.schema(self).empty_table()
        _warn_transform_failures(failures, stacklevel=stacklevel)
        return table

    def to_polars(self) -> Any:
        """Convert to a Polars DataFrame via the Arrow PyCapsule stream.

        Scan errors propagate; the scan is never re-run through another path.
        """
        return _polars_from_stream(self)

    def iter_batches(self, batch_size: Optional[int] = None) -> Iterable[Any]:
        """Yield the scan incrementally as Polars DataFrames of up to `batch_size` rows."""
        lib = _get_lib()
        has_ex = self._has_ex(lib)
        reader = self._open_reader(lib, batch_size)
        try:
            while True:
                err = ctypes.c_void_p(0)
                ptr = lib.raster_h3_py_batch_reader_next(reader, ctypes.byref(err))
                _raise_if_err(err)
                if not ptr:
                    if has_ex:
                        _warn_transform_failures(
                            lib.raster_h3_py_batch_reader_transform_failures(reader),
                            stacklevel=3,
                        )
                    return
                yield _polars_from_stream(_OwnedArrowStream(ptr))
        finally:
            lib.raster_h3_py_free_batch_reader(reader)

    def _open_reader(self, lib: ctypes.CDLL, batch_size: Optional[int]) -> int:
        """Open a native batch reader (free with `raster_h3_py_free_batch_reader`)."""
        err = ctypes.c_void_p(0)
        size = int(batch_size) if batch_size else 0
        strict = 1 if self.strict_transforms else 0
        if self._has_v2(lib):
            reader = lib.raster_h3_py_open_batch_reader_v2(
                *self._scan_args(),
                size,
                strict,
                self._partition_arg(lib) if self.partition is not None else None,
                self._options,
                ctypes.byref(err),
            )
        elif self.partition is not None:
            reader = lib.raster_h3_py_open_batch_reader_partitioned(
                *self._scan_args(), size, strict, self._partition_arg(lib), ctypes.byref(err)
            )
        elif self._has_ex(lib):
            reader = lib.raster_h3_py_open_batch_reader_ex(
                *self._scan_args(), size, strict, ctypes.byref(err)
            )
        else:
            reader = lib.raster_h3_py_open_batch_reader(
                *self._scan_args(), size, ctypes.byref(err)
            )
        _raise_if_err(err)
        if not reader:
            raise RuntimeError("Failed to open raster_h3 batch reader")
        return reader

    def to_pyarrow(self) -> Any:
        """Convert to a PyArrow Table via zero-copy `__arrow_c_stream__`."""
        import pyarrow as pa  # type: ignore

        return pa.table(self)


# -----------------------------------------------------------------------------
# Polars IO Plugin (`scan_raster_h3` and `read_raster_h3`)
# -----------------------------------------------------------------------------

def scan_raster_h3(
    path: Union[str, Path],
    resolutions: Union[int, str, Sequence[int]] = 8,
    *,
    categorical: bool = False,
    format: str = "wide",
    crs: Optional[str] = None,
    nodata: Optional[float] = None,
    bbox: Optional[Union[str, Sequence[float]]] = None,
    columns: Optional[Sequence[str]] = None,
    limit: Optional[int] = None,
    strict_transforms: bool = False,
    band: Optional[int] = None,
    sampling: Optional[str] = None,
    overlap_rule: Optional[str] = None,
    spectral_formula: Optional[str] = None,
    nir_band: Optional[int] = None,
    red_band: Optional[int] = None,
    green_band: Optional[int] = None,
    blue_band: Optional[int] = None,
    swir_band: Optional[int] = None,
    quantiles: Optional[Union[str, Sequence[Union[str, float]]]] = None,
    remap: Optional[Union[str, Mapping[Any, Optional[int]]]] = None,
    min_count: Optional[float] = None,
    min_mean: Optional[float] = None,
    max_mean: Optional[float] = None,
    min_majority_fraction: Optional[float] = None,
) -> Any:
    """
    Scan a GeoTIFF / COG into H3 hexagonal cells.

    If `polars` is installed with `register_io_source`, returns a Polars `LazyFrame`
    with column projection (`with_columns`) and row-slice (`n_rows`) pushdown.
    Otherwise returns a `RasterH3ArrowStream` implementing `__arrow_c_stream__`.

    Valid samples whose coordinate transform fails are skipped with a
    `RuntimeWarning` once the scan completes; `strict_transforms=True` raises instead.

    Further options, matching the DuckDB table functions' named parameters:

    - `band`: 1-indexed band to aggregate (default 1).
    - `sampling`: sub-pixel sampling pattern, one of `center` (default), `5point`,
      `gaussian`, `rgss`, `hex`, `8rooks`, `9point`, `16point`.
    - `overlap_rule`: how overlapping mosaic tiles are resolved: `cutline` (default),
      `first` or `average`.
    - `min_majority_fraction` (categorical): drop cells whose majority class covers less
      than this fraction of their samples.
    - `spectral_formula` (continuous): aggregate `ndvi`, `ndwi`, `nbr` or `evi` computed
      per pixel from several bands, chosen with `nir_band` (default 4), `red_band` (3),
      `green_band` (2), `blue_band` (1) and `swir_band` (6).
    - `quantiles` (continuous): streaming quantile columns, e.g. `"p50,p90"`,
      `["median", "iqr"]`, `[0.05, 0.95]` or a preset (`"default"`, `"box"`, `"tails"`,
      `"deciles"`). Each target adds a nullable `Float64` column named after it.
    - `remap` (categorical): class remapping applied to every pixel, as DuckDB's
      `remap :=` string or a mapping such as `{range(11, 20): 1, 21: 2, "else": None}`
      (`None` drops the class).
    - `min_count`, `min_mean` / `max_mean` (continuous) and `min_majority_fraction`
      (categorical): drop cells outside these bounds inside the engine.

    A Polars `filter` on `count` (`total_count` for categorical scans), `mean`,
    `majority_fraction` or `resolution`, combined with `&`, is also pushed into the scan
    as these bounds; Polars still applies the full predicate afterwards.
    """
    _locals = locals()
    extra = {k: _locals[k] for k in _SCAN_OPTIONS}
    stream = RasterH3ArrowStream(
        path=path,
        resolutions=resolutions,
        categorical=categorical,
        format=format,
        crs=crs,
        nodata=nodata,
        bbox=bbox,
        columns=columns,
        limit=limit,
        strict_transforms=strict_transforms,
        **extra,
    )
    try:
        import polars as pl  # type: ignore
        from polars.io.plugins import register_io_source  # type: ignore

        # Build Polars schema from field names
        if not categorical:
            pl_schema = {
                "h3_index": pl.UInt64,
                "h3_hex": pl.String,
                "mean": pl.Float64,
                "stddev": pl.Float64,
                "count": pl.Float64,
                "min": pl.Float64,
                "max": pl.Float64,
                "sum": pl.Float64,
                "resolution": pl.UInt8,
                "wkb": pl.Binary,
            }
        elif format.lower() == "long":
            pl_schema = {
                "h3_index": pl.UInt64,
                "h3_hex": pl.String,
                "category": pl.Int64,
                "count": pl.Float64,
                "fraction": pl.Float64,
                "total_count": pl.Float64,
                "resolution": pl.UInt8,
                "shannon_entropy": pl.Float64,
                "entropy": pl.Float64,
                "distinct_classes": pl.Int64,
                "unique_classes": pl.Int64,
                "wkb": pl.Binary,
            }
        else:
            pl_schema = {
                "h3_index": pl.UInt64,
                "h3_hex": pl.String,
                "majority_class": pl.Int64,
                "majority_fraction": pl.Float64,
                "majority_count": pl.Float64,
                "unique_classes": pl.Int64,
                "total_count": pl.Float64,
                "histogram": pl.String,
                "resolution": pl.UInt8,
                "shannon_entropy": pl.Float64,
                "entropy": pl.Float64,
                "distinct_classes": pl.Int64,
                "wkb": pl.Binary,
            }

        if not categorical and quantiles is not None:
            for name in _quantile_columns(quantiles):
                pl_schema[name] = pl.Float64

        if columns:
            pl_schema = {k: pl_schema[k] for k in columns if k in pl_schema}

        def _source_gen(
            with_columns: Optional[List[str]],
            predicate: Optional[Any],
            n_rows: Optional[int],
            batch_size: Optional[int],
        ) -> Iterable[Any]:
            empty = True
            for df in _scan_batches(with_columns, predicate, n_rows, batch_size):
                empty = False
                yield df
            if empty:
                # Polars before 1.28 panics on an IO source that yields no frames.
                names = with_columns if with_columns is not None else list(pl_schema)
                yield pl.DataFrame(schema={k: pl_schema[k] for k in names})

        def _scan_batches(
            with_columns: Optional[List[str]],
            predicate: Optional[Any],
            n_rows: Optional[int],
            batch_size: Optional[int],
        ) -> Iterable[Any]:
            pushed_cols = with_columns if with_columns is not None else columns
            # `n_rows` can only be pushed into the scan when no predicate runs after it;
            # otherwise it caps the rows that survive the filter.
            if predicate is None:
                # Narrow, never widen, the caller's explicit `limit`.
                scan_limit = (
                    limit if n_rows is None else n_rows if limit is None else min(limit, n_rows)
                )
                remaining = None
            else:
                scan_limit = limit
                remaining = n_rows
            options = dict(extra)
            scan_resolutions: Union[int, str, Sequence[int]] = resolutions
            if predicate is not None:
                pushed = _pushdown_bounds(predicate, categorical, format)
                if _native_has_scan_options():
                    options = _narrow_options(options, pushed)
                allowed = pushed.get("resolutions")
                if allowed is not None:
                    configured = _resolution_list(resolutions)
                    if configured is not None:
                        kept = [r for r in configured if r in allowed]
                        if not kept:
                            return  # the predicate rejects every scanned resolution
                        scan_resolutions = kept
            s = RasterH3ArrowStream(
                path=path,
                resolutions=scan_resolutions,
                categorical=categorical,
                format=format,
                crs=crs,
                nodata=nodata,
                bbox=bbox,
                columns=pushed_cols,
                limit=scan_limit,
                strict_transforms=strict_transforms,
                **options,
            )
            if remaining is not None and remaining <= 0:
                return
            for df in s.iter_batches(batch_size):
                if predicate is not None:
                    df = df.filter(predicate)
                if remaining is not None:
                    df = df.head(remaining)
                    remaining -= df.height
                yield df
                if remaining is not None and remaining <= 0:
                    return

        # Positional: the parameter is `callable` before Polars 1.25 and `io_source` after.
        return register_io_source(_source_gen, schema=pl_schema)
    except ImportError:
        return stream


def _quantile_columns(quantiles: Union[str, Sequence[Union[str, float]]]) -> List[str]:
    """Output column names of a `quantiles` option, from the native parser."""
    lib = _get_lib()
    if not hasattr(lib, "raster_h3_py_quantile_columns"):
        raise RuntimeError("quantiles require a newer raster_h3 native library")
    err = ctypes.c_void_p(0)
    ptr = lib.raster_h3_py_quantile_columns(
        _encode_quantiles(quantiles).encode("utf-8"), ctypes.byref(err)
    )
    _raise_if_err(err)
    if not ptr:
        raise RuntimeError("Failed to parse quantiles")
    try:
        return list(json.loads(ctypes.string_at(ptr).decode("utf-8")))
    finally:
        lib.raster_h3_py_free_string(ptr)


def _resolution_list(resolutions: Union[int, str, Sequence[int]]) -> Optional[List[int]]:
    """The scan's resolutions as ints when they are valid (non-empty, each 0 to 15; repeats
    are dropped, as the native parser does), else None. Pushdown narrows only a valid set, so an invalid one still
    reaches the native scan and raises its usual error instead of being filtered away."""
    try:
        if isinstance(resolutions, bool):
            return None
        if isinstance(resolutions, int):
            values = [resolutions]
        elif isinstance(resolutions, str):
            values = [int(t) for t in resolutions.replace(",", " ").split()]
        else:
            values = [int(r) for r in resolutions]
    except (TypeError, ValueError):
        return None
    if not values or any(not 0 <= r <= 15 for r in values):
        return None
    return sorted(set(values))


def _native_has_scan_options() -> bool:
    """Whether the native library takes scan options (the `_v2` entry points). Pushdown
    adds filter options only then: with an older library the predicate is simply applied
    by Polars, as before."""
    return hasattr(_get_lib(), "raster_h3_py_create_arrow_stream_v2")


_FLIPPED = {"Gt": "Lt", "GtEq": "LtEq", "Lt": "Gt", "LtEq": "GtEq", "Eq": "Eq", "NotEq": "NotEq"}
_COMPARE = {
    "Gt": lambda a, b: a > b,
    "GtEq": lambda a, b: a >= b,
    "Lt": lambda a, b: a < b,
    "LtEq": lambda a, b: a <= b,
    "Eq": lambda a, b: a == b,
    "NotEq": lambda a, b: a != b,
}


def _literal_number(node: Any) -> Optional[float]:
    """The number in a serialized Polars literal (`{"Dyn": {"Int": 10}}`,
    `{"Scalar": {"UInt8": 4}}`, ...), or None for anything else."""
    for _ in range(8):
        if isinstance(node, bool):
            return None
        if isinstance(node, (int, float)):
            number = float(node)
            return number if math.isfinite(number) else None
        if not isinstance(node, dict):
            return None
        if "value" in node:
            node = node["value"]
        elif len(node) == 1:
            node = next(iter(node.values()))
        else:
            return None
    return None


def _comparisons(node: Any) -> Iterable[Tuple[str, str, float]]:
    """`(column, op, number)` for every `column <op> literal` joined by AND at the top of a
    serialized Polars expression. Other terms (OR, casts, functions) yield nothing, which is
    safe: they only make the pushed bounds looser than the predicate."""
    if not isinstance(node, dict):
        return
    if "Function" in node:
        # `col.is_between(lo, hi)`: whatever its `closed`, the inclusive bounds keep a superset.
        func = node["Function"]
        if not isinstance(func, dict) or "IsBetween" not in json.dumps(func.get("function")):
            return
        args = func.get("input")
        if (
            isinstance(args, list)
            and len(args) == 3
            and isinstance(args[0], dict)
            and set(args[0]) == {"Column"}
            and isinstance(args[0]["Column"], str)
            and all(isinstance(a, dict) and set(a) == {"Literal"} for a in args[1:])
        ):
            lo, hi = (_literal_number(a["Literal"]) for a in args[1:])
            if lo is not None:
                yield args[0]["Column"], "GtEq", lo
            if hi is not None:
                yield args[0]["Column"], "LtEq", hi
        return
    if "BinaryExpr" not in node:
        return
    expr = node["BinaryExpr"]
    if not isinstance(expr, dict):
        return
    op, left, right = expr.get("op"), expr.get("left"), expr.get("right")
    if op in ("And", "LogicalAnd"):
        yield from _comparisons(left)
        yield from _comparisons(right)
        return
    if op not in _COMPARE:
        return
    for col_side, lit_side, cmp in ((left, right, op), (right, left, _FLIPPED[op])):
        if isinstance(col_side, dict) and set(col_side) == {"Column"}:
            if isinstance(lit_side, dict) and set(lit_side) == {"Literal"}:
                number = _literal_number(lit_side["Literal"])
                if number is not None and isinstance(col_side["Column"], str):
                    yield col_side["Column"], cmp, number
            return


def _pushdown_bounds(predicate: Any, categorical: bool, format: str) -> Dict[str, Any]:
    """Scan bounds implied by a Polars predicate: `min_count`, `min_mean`, `max_mean`,
    `min_majority_fraction` and the set of allowed `resolutions`.

    Every bound keeps a superset of the rows the predicate keeps (the engine's filters are
    inclusive: `count >= min_count`, ...), so applying the predicate afterwards gives the
    exact result. An expression this cannot read pushes nothing.
    """
    try:
        tree = json.loads(predicate.meta.serialize(format="json"))
    except Exception:  # older Polars, or an expression that cannot be serialized
        return {}
    count_col = "total_count" if categorical else "count"
    lower: Dict[str, float] = {}
    upper: Dict[str, float] = {}
    resolutions: Optional[set] = None
    for column, op, value in _comparisons(tree):
        if column == "resolution":
            allowed = {r for r in range(16) if _COMPARE[op](r, value)}
            resolutions = allowed if resolutions is None else resolutions & allowed
            continue
        if column == count_col:
            key_lo, key_hi = "min_count", None
        elif column == "mean" and not categorical:
            key_lo, key_hi = "min_mean", "max_mean"
        elif column == "majority_fraction" and categorical and format.lower() != "long":
            if value < 0.0:
                continue  # every fraction passes; nothing to push
            key_lo, key_hi = "min_majority_fraction", None
            value = min(value, 1.0)  # `> 1.5` keeps nothing; `>= 1` keeps a superset
        else:
            continue
        if op in ("Gt", "GtEq", "Eq"):
            lower[key_lo] = max(lower.get(key_lo, value), value)
        if key_hi is not None and op in ("Lt", "LtEq", "Eq"):
            upper[key_hi] = min(upper.get(key_hi, value), value)
    bounds: Dict[str, Any] = {**lower, **upper}
    if resolutions is not None:
        bounds["resolutions"] = resolutions
    return bounds


def _narrow_options(options: Dict[str, Any], pushed: Mapping[str, Any]) -> Dict[str, Any]:
    """`options` with pushed bounds combined into the caller's: lower bounds take the max,
    upper bounds the min, so pushdown only ever narrows the scan."""
    out = dict(options)
    for key in ("min_count", "min_mean", "min_majority_fraction"):
        if key in pushed:
            out[key] = pushed[key] if out.get(key) is None else max(out[key], pushed[key])
    if "max_mean" in pushed:
        out["max_mean"] = (
            pushed["max_mean"] if out.get("max_mean") is None else min(out["max_mean"], pushed["max_mean"])
        )
    return out


def read_raster_h3(
    path: Union[str, Path],
    resolutions: Union[int, str, Sequence[int]] = 8,
    **kwargs: Any,
) -> Any:
    """Eagerly read a GeoTIFF / COG into a Polars DataFrame (or `RasterH3ArrowStream` if Polars is not installed)."""
    stream = RasterH3ArrowStream(path=path, resolutions=resolutions, **kwargs)
    try:
        return stream.to_polars()
    except ImportError:
        return stream


# -----------------------------------------------------------------------------
# Partitioned scans (Spark and other distributed engines)
# -----------------------------------------------------------------------------

#: Column of a partitioned scan holding a boundary cell's encoded partial state.
PARTIAL_STATE_COLUMN = "partial_state"


def plan_partitions(
    path: Union[str, Path],
    *,
    target_chunks: int = 256,
    crs: Optional[str] = None,
    bbox: Optional[Union[str, Sequence[float]]] = None,
    overlap_rule: Optional[str] = None,
) -> List[str]:
    """
    Split a raster (or mosaic) scan into independent partitions of about
    `target_chunks` TIFF tiles or strips each, reading only metadata.

    Returns one JSON string per partition. Scan each with
    `RasterH3ArrowStream(path, ..., partition=spec)` using the same `path`, `crs`, `bbox`
    and `overlap_rule` (a raster with another layout, or another bbox, refuses the spec); every TIFF
    chunk belongs to exactly one partition, so no pixel is read twice.

    Each partitioned scan returns the normal columns plus `partial_state`:

    - rows where `partial_state` is null are final cells;
    - rows where it is set are cells that other partitions also hold samples of. Collect
      them from all partitions (grouped by `h3_index`) and pass them to `merge_partials`.

    Larger partitions mean less per-task overhead and fewer partial rows; a few hundred
    chunks per partition is a good start.
    """
    lib = _get_lib()
    if not hasattr(lib, "raster_h3_py_plan_partitions"):
        raise RuntimeError("plan_partitions requires a newer raster_h3 native library")
    err = ctypes.c_void_p(0)
    args = (
        str(path).encode("utf-8"),
        crs.encode("utf-8") if crs else None,
        _encode_bbox(bbox),
        int(target_chunks),
    )
    options = _encode_scan_options({"overlap_rule": overlap_rule})
    if hasattr(lib, "raster_h3_py_plan_partitions_v2"):
        ptr = lib.raster_h3_py_plan_partitions_v2(*args, options, ctypes.byref(err))
    elif options is not None:
        raise RuntimeError("overlap_rule requires a newer raster_h3 native library")
    else:
        ptr = lib.raster_h3_py_plan_partitions(*args, ctypes.byref(err))
    _raise_if_err(err)
    if not ptr:
        raise RuntimeError("Failed to plan partitions")
    try:
        plan = json.loads(ctypes.string_at(ptr).decode("utf-8"))
    finally:
        lib.raster_h3_py_free_string(ptr)
    return [json.dumps(p, separators=(",", ":")) for p in plan]


def _as_pyarrow_table(data: Any) -> Any:
    import pyarrow as pa  # type: ignore

    if isinstance(data, pa.Table):
        return data
    if isinstance(data, pa.RecordBatch):
        return pa.Table.from_batches([data])
    if isinstance(data, (list, tuple)):
        if data and all(isinstance(b, pa.RecordBatch) for b in data):
            return pa.Table.from_batches(list(data))
        if data and all(isinstance(t, pa.Table) for t in data):
            return pa.concat_tables(list(data))
        if not data:
            return None
    if hasattr(data, "to_arrow") and not hasattr(data, "__arrow_c_stream__"):
        return data.to_arrow()  # older Polars
    return pa.table(data)


def merge_partials(
    partials: Any,
    *,
    categorical: bool = False,
    columns: Optional[Sequence[str]] = None,
    quantiles: Optional[Union[str, Sequence[Union[str, float]]]] = None,
    min_count: Optional[float] = None,
    min_mean: Optional[float] = None,
    max_mean: Optional[float] = None,
    min_majority_fraction: Optional[float] = None,
) -> Any:
    """
    Merge the partial rows of every partition of a partitioned scan into final rows.

    `partials` holds the rows whose `partial_state` is set, from all partitions (every
    partial row of a cell must be included, in any order). It can be a PyArrow Table,
    RecordBatch or list of them, or anything exposing `__arrow_c_stream__` (e.g. a Polars
    DataFrame); only the `resolution`, `h3_index` and `partial_state` columns are read,
    and integer types are cast as needed (e.g. Spark's signed `h3_index`).

    `categorical`, `columns`, `quantiles` and the filters (`min_count`, `min_mean`,
    `max_mean`, `min_majority_fraction`) must match the scans'. Returns a PyArrow Table with the
    scan's output columns (without `partial_state`), sorted by resolution and h3_index.
    Requires pyarrow.

    With Spark, for example::

        rows = cells.filter("partial_state IS NOT NULL")
        merged = rows.repartition("h3_index").mapInArrow(
            lambda batches: merge_partials(list(batches)).to_batches(), schema
        )
    """
    import pyarrow as pa  # type: ignore

    lib = _get_lib()
    if not hasattr(lib, "raster_h3_py_merge_partials"):
        raise RuntimeError("merge_partials requires a newer raster_h3 native library")
    options = _encode_scan_options(
        dict(
            quantiles=quantiles,
            min_count=min_count,
            min_mean=min_mean,
            max_mean=max_mean,
            min_majority_fraction=min_majority_fraction,
        )
    )
    if options is not None and not hasattr(lib, "raster_h3_py_merge_partials_v2"):
        raise RuntimeError(
            "quantiles and filters in merge_partials require a newer raster_h3 native library"
        )
    table = _as_pyarrow_table(partials)
    keys = {"resolution": pa.uint8(), "h3_index": pa.uint64(), PARTIAL_STATE_COLUMN: pa.binary()}
    if table is None:
        table = pa.table({name: pa.array([], type=t) for name, t in keys.items()})
    missing = [name for name in keys if name not in table.column_names]
    if missing:
        raise ValueError(f"partial rows need the columns {list(keys)}; missing {missing}")
    state = table.column(PARTIAL_STATE_COLUMN)
    if state.null_count:
        raise ValueError(
            f"{state.null_count} rows have no {PARTIAL_STATE_COLUMN}: pass only partial rows "
            "(rows without a state are already final)"
        )
    narrow = pa.table({name: table.column(name).cast(t) for name, t in keys.items()}).combine_chunks()
    capsule = narrow.__arrow_c_stream__()
    stream = _capsule_object_pointer(capsule, b"arrow_array_stream")
    err = ctypes.c_void_p(0)
    if hasattr(lib, "raster_h3_py_merge_partials_v2"):
        out = lib.raster_h3_py_merge_partials_v2(
            stream,
            1 if categorical else 0,
            _encode_columns(columns),
            options,
            ctypes.byref(err),
        )
    else:
        out = lib.raster_h3_py_merge_partials(
            stream,
            1 if categorical else 0,
            _encode_columns(columns),
            ctypes.byref(err),
        )
    del capsule  # the native side moved the stream out; releasing the capsule is a no-op
    _raise_if_err(err)
    if not out:
        raise RuntimeError("Failed to merge partial cells")
    return pa.table(_OwnedArrowStream(out))


# -----------------------------------------------------------------------------
# H3 Scalar Functions & Direct Exporters
# -----------------------------------------------------------------------------

def h3_to_string(cell: int) -> Optional[str]:
    lib = _get_lib()
    ptr = lib.raster_h3_py_h3_to_string(int(cell))
    if not ptr:
        return None
    try:
        return ctypes.string_at(ptr).decode("utf-8")
    finally:
        lib.raster_h3_py_free_string(ptr)


def string_to_h3(hex_str: str) -> int:
    return int(_get_lib().raster_h3_py_string_to_h3(hex_str.encode("utf-8")))


def h3_to_lat(cell: int) -> float:
    return float(_get_lib().raster_h3_py_h3_to_lat(int(cell)))


def h3_to_lng(cell: int) -> float:
    return float(_get_lib().raster_h3_py_h3_to_lng(int(cell)))


def h3_get_resolution(cell: int) -> int:
    return int(_get_lib().raster_h3_py_h3_get_resolution(int(cell)))


def h3_is_valid(cell_or_hex: Union[int, str]) -> bool:
    lib = _get_lib()
    if isinstance(cell_or_hex, str):
        return bool(lib.raster_h3_py_h3_is_valid_str(cell_or_hex.encode("utf-8")))
    return bool(lib.raster_h3_py_h3_is_valid_u64(int(cell_or_hex)))


def h3_to_wkb(cell_or_hex: Union[int, str]) -> Optional[bytes]:
    """OGC WKB for a cell: a Polygon, or a MultiPolygon for cells crossing 180°."""
    cell = string_to_h3(cell_or_hex) if isinstance(cell_or_hex, str) else int(cell_or_hex)
    if cell == 0:
        return None
    lib = _get_lib()
    # The native library reports the capacity that fits every cell
    # (raster-h3-core's WKB_BUF_LEN), so the two cannot drift apart.
    cap = lib.raster_h3_py_wkb_buf_len()
    buf = (ctypes.c_uint8 * cap)()
    written = lib.raster_h3_py_h3_to_wkb(cell, buf, cap)
    if written == 0:
        return None
    return bytes(buf[:written])


def h3_cell_to_parent(
    cell_or_hex: Union[int, str], parent_res: int
) -> Optional[Union[int, str]]:
    lib = _get_lib()
    if isinstance(cell_or_hex, str):
        ptr = lib.raster_h3_py_h3_cell_to_parent_str(
            cell_or_hex.encode("utf-8"), int(parent_res)
        )
        if not ptr:
            return None
        try:
            return ctypes.string_at(ptr).decode("utf-8")
        finally:
            lib.raster_h3_py_free_string(ptr)
    res = int(lib.raster_h3_py_h3_cell_to_parent_u64(int(cell_or_hex), int(parent_res)))
    return res if res != 0 else None


def h3_polygon_to_cells(
    wkb: bytes, resolution: int, containment: str = "contains_centroid"
) -> Optional[List[int]]:
    """Fill a WKB POLYGON / MULTIPOLYGON (WGS84 lon/lat degrees) with H3 cells.

    Returns the cells sorted ascending and deduplicated, or ``None`` if ``wkb`` is not a
    valid polygonal WKB or ``resolution`` is outside 0..15. ``containment`` is one of
    ``contains_centroid`` (default, alias ``center``), ``contains_boundary`` (``full``),
    ``intersects_boundary`` or ``covers`` (``overlapping``); an unknown name, or a polygon
    that would exceed 10,000,000 cells, raises ``ValueError``.
    """
    resolution = int(resolution)
    if not 0 <= resolution <= 15:
        return None
    lib = _get_lib()
    data = bytes(wkb)
    out_cells = ctypes.POINTER(ctypes.c_uint64)()
    out_len = ctypes.c_size_t(0)
    status = lib.raster_h3_py_h3_polygon_to_cells(
        data,
        len(data),
        resolution,
        containment.encode("utf-8"),
        ctypes.byref(out_cells),
        ctypes.byref(out_len),
    )
    if status == 0:
        try:
            return [int(out_cells[i]) for i in range(out_len.value)]
        finally:
            lib.raster_h3_py_free_cells(out_cells, out_len.value)
    if status == 2:
        raise ValueError(f"unknown containment mode {containment!r}")
    if status == 3:
        raise ValueError(
            "polygon would produce more than 10,000,000 cells; "
            "use a coarser resolution or split the polygon"
        )
    return None


def _run_export(name: str, args: Tuple[Any, ...]) -> int:
    """Call a native exporter, preferring its `_with_stats` variant when available."""
    lib = _get_lib()
    err = ctypes.c_void_p(0)
    if hasattr(lib, name + "_with_stats"):
        failures = ctypes.c_uint64(0)
        count = getattr(lib, name + "_with_stats")(
            *args, ctypes.byref(failures), ctypes.byref(err)
        )
        _raise_if_err(err)
        _warn_transform_failures(failures.value)
    else:
        count = getattr(lib, name)(*args, ctypes.byref(err))
        _raise_if_err(err)
    return int(count)


def _warn_transform_failures(failures: int, stacklevel: int = 4) -> None:
    """Warn when valid samples were dropped by failed coordinate transforms."""
    if failures:
        warnings.warn(
            f"raster_h3: {failures} valid sample(s) were dropped because their coordinate "
            "transform failed or fell outside the valid latitude/longitude range; check crs "
            "(strict_transforms=True raises instead)",
            RuntimeWarning,
            stacklevel=stacklevel,
        )


def raster_to_parquet(
    input_path: Union[str, Path],
    output_path: Union[str, Path],
    resolutions: Union[int, str, Sequence[int]] = 8,
    *,
    categorical: bool = False,
    crs: Optional[str] = None,
    geoparquet: bool = True,
) -> int:
    args = (
        str(input_path).encode("utf-8"),
        str(output_path).encode("utf-8"),
        _encode_resolutions(resolutions),
        1 if categorical else 0,
        crs.encode("utf-8") if crs else None,
        1 if geoparquet else 0,
    )
    return _run_export("raster_h3_py_export_parquet", args)


def raster_to_pmtiles(
    input_path: Union[str, Path],
    output_path: Union[str, Path],
    resolutions: Union[int, str, Sequence[int]] = 8,
    *,
    categorical: bool = False,
    crs: Optional[str] = None,
) -> int:
    args = (
        str(input_path).encode("utf-8"),
        str(output_path).encode("utf-8"),
        _encode_resolutions(resolutions),
        1 if categorical else 0,
        crs.encode("utf-8") if crs else None,
    )
    return _run_export("raster_h3_py_export_pmtiles", args)


def parquet_to_pmtiles(
    parquet_path: Union[str, Path],
    output_path: Union[str, Path],
    *,
    h3_column: Optional[str] = None,
) -> int:
    """
    Transcode an H3-indexed Parquet file into a PMTiles v3 vector archive, one hexagon
    feature per row with the other columns as properties (DuckDB's `h3_parquet_to_pmtiles`).

    `h3_column` names the H3 column (integer or hex string, matched case-insensitively);
    by default the first of `h3_index`, `h3_hex`, `h3`, `cell` or `hex` is used. Returns
    the number of hexagon features written.
    """
    lib = _get_lib()
    if not hasattr(lib, "raster_h3_py_parquet_to_pmtiles"):
        raise RuntimeError("parquet_to_pmtiles requires a newer raster_h3 native library")
    err = ctypes.c_void_p(0)
    count = lib.raster_h3_py_parquet_to_pmtiles(
        str(parquet_path).encode("utf-8"),
        str(output_path).encode("utf-8"),
        h3_column.encode("utf-8") if h3_column else None,
        ctypes.byref(err),
    )
    _raise_if_err(err)
    return int(count)


__all__ = [
    "PARTIAL_STATE_COLUMN",
    "RasterH3ArrowStream",
    "merge_partials",
    "parquet_to_pmtiles",
    "plan_partitions",
    "scan_raster_h3",
    "read_raster_h3",
    "h3_to_string",
    "string_to_h3",
    "h3_to_lat",
    "h3_to_lng",
    "h3_get_resolution",
    "h3_is_valid",
    "h3_to_wkb",
    "h3_cell_to_parent",
    "h3_polygon_to_cells",
    "raster_to_parquet",
    "raster_to_pmtiles",
]
