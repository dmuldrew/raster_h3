"""
raster_h3: High-performance Python, Polars, and Arrow PyCapsule bindings for GeoTIFF/COG to H3 aggregation.

Provides:
- `RasterH3ArrowStream`: Zero-copy Arrow PyCapsule producer (`__arrow_c_stream__`, `__arrow_c_schema__`)
  compatible with `polars.from_arrow()`, `pyarrow.table()`, `duckdb`, `daft`, `lonboard`, and `pydeck`.
- `scan_raster_h3()` / `read_raster_h3()`: Polars IO plugin (`register_io_source`) with column projection
  and row-limit pushdown.
- Scalar and vectorized H3 helpers (`h3_to_string`, `string_to_h3`, `h3_to_lat`, `h3_to_lng`,
  `h3_get_resolution`, `h3_is_valid`, `h3_to_wkb`, `h3_cell_to_parent`).
- Direct file exporters (`raster_to_parquet`, `raster_to_pmtiles`).
"""

from __future__ import annotations

import ctypes
import json
import os
import sys
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

    # Exporters
    lib.raster_h3_py_export_parquet.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.raster_h3_py_export_parquet.restype = ctypes.c_int64

    lib.raster_h3_py_export_pmtiles.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    lib.raster_h3_py_export_pmtiles.restype = ctypes.c_int64

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

ctypes.pythonapi.PyCapsule_GetPointer.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
ctypes.pythonapi.PyCapsule_GetPointer.restype = ctypes.c_void_p


@_PYCAPSULE_DESTRUCTOR
def _schema_capsule_destructor(capsule_ptr: int) -> None:
    if capsule_ptr and _LIB is not None:
        ptr = ctypes.pythonapi.PyCapsule_GetPointer(capsule_ptr, b"arrow_schema")
        if ptr:
            _LIB.raster_h3_py_free_arrow_schema(ptr)


@_PYCAPSULE_DESTRUCTOR
def _stream_capsule_destructor(capsule_ptr: int) -> None:
    if capsule_ptr and _LIB is not None:
        ptr = ctypes.pythonapi.PyCapsule_GetPointer(capsule_ptr, b"arrow_array_stream")
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

    def __arrow_c_schema__(self) -> Any:
        lib = _get_lib()
        err = ctypes.c_void_p(0)
        ptr = lib.raster_h3_py_create_arrow_schema(
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
        ptr = lib.raster_h3_py_create_arrow_stream(
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
            ctypes.byref(err),
        )
        _raise_if_err(err)
        if not ptr:
            raise RuntimeError("Failed to create ArrowArrayStream")
        return ctypes.pythonapi.PyCapsule_New(
            ptr, b"arrow_array_stream", _stream_capsule_destructor
        )

    def to_dict(self) -> Dict[str, List[Any]]:
        """Materialize the scan into a Python dictionary of column lists."""
        lib = _get_lib()
        err = ctypes.c_void_p(0)
        json_ptr = lib.raster_h3_py_scan_json(
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
            ctypes.byref(err),
        )
        _raise_if_err(err)
        if not json_ptr:
            raise RuntimeError("Failed to execute scan_json")
        try:
            raw = ctypes.string_at(json_ptr).decode("utf-8")
            return json.loads(raw)
        finally:
            lib.raster_h3_py_free_string(json_ptr)

    def to_polars(self) -> Any:
        """Convert to a Polars DataFrame via the Arrow PyCapsule stream.

        Scan errors propagate; the scan is never re-run through another path.
        """
        return _polars_from_stream(self)

    def iter_batches(self, batch_size: Optional[int] = None) -> Iterable[Any]:
        """Yield the scan incrementally as Polars DataFrames of up to `batch_size` rows."""
        lib = _get_lib()
        err = ctypes.c_void_p(0)
        reader = lib.raster_h3_py_open_batch_reader(
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
            int(batch_size) if batch_size else 0,
            ctypes.byref(err),
        )
        _raise_if_err(err)
        if not reader:
            raise RuntimeError("Failed to open raster_h3 batch reader")
        try:
            while True:
                err = ctypes.c_void_p(0)
                ptr = lib.raster_h3_py_batch_reader_next(reader, ctypes.byref(err))
                _raise_if_err(err)
                if not ptr:
                    return
                yield _polars_from_stream(_OwnedArrowStream(ptr))
        finally:
            lib.raster_h3_py_free_batch_reader(reader)

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
) -> Any:
    """
    Scan a GeoTIFF / COG into H3 hexagonal cells.

    If `polars` is installed with `register_io_source`, returns a Polars `LazyFrame`
    with column projection (`with_columns`) and row-slice (`n_rows`) pushdown.
    Otherwise returns a `RasterH3ArrowStream` implementing `__arrow_c_stream__`.
    """
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

        if columns:
            pl_schema = {k: pl_schema[k] for k in columns if k in pl_schema}

        def _source_gen(
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
            s = RasterH3ArrowStream(
                path=path,
                resolutions=resolutions,
                categorical=categorical,
                format=format,
                crs=crs,
                nodata=nodata,
                bbox=bbox,
                columns=pushed_cols,
                limit=scan_limit,
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

        return register_io_source(io_source=_source_gen, schema=pl_schema)
    except ImportError:
        return stream


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


_WKB_BUF_LEN = 192


def h3_to_wkb(cell_or_hex: Union[int, str]) -> Optional[bytes]:
    cell = string_to_h3(cell_or_hex) if isinstance(cell_or_hex, str) else int(cell_or_hex)
    if cell == 0:
        return None
    # Matches WKB_BUF_LEN in raster-h3-core: odd-resolution pentagons have up to
    # 10 boundary vertices (189 bytes of WKB).
    buf = (ctypes.c_uint8 * _WKB_BUF_LEN)()
    written = _get_lib().raster_h3_py_h3_to_wkb(cell, buf, _WKB_BUF_LEN)
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


def raster_to_parquet(
    input_path: Union[str, Path],
    output_path: Union[str, Path],
    resolutions: Union[int, str, Sequence[int]] = 8,
    *,
    categorical: bool = False,
    crs: Optional[str] = None,
    geoparquet: bool = True,
) -> int:
    lib = _get_lib()
    err = ctypes.c_void_p(0)
    count = lib.raster_h3_py_export_parquet(
        str(input_path).encode("utf-8"),
        str(output_path).encode("utf-8"),
        _encode_resolutions(resolutions),
        1 if categorical else 0,
        crs.encode("utf-8") if crs else None,
        1 if geoparquet else 0,
        ctypes.byref(err),
    )
    _raise_if_err(err)
    return int(count)


def raster_to_pmtiles(
    input_path: Union[str, Path],
    output_path: Union[str, Path],
    resolutions: Union[int, str, Sequence[int]] = 8,
    *,
    categorical: bool = False,
    crs: Optional[str] = None,
) -> int:
    lib = _get_lib()
    err = ctypes.c_void_p(0)
    count = lib.raster_h3_py_export_pmtiles(
        str(input_path).encode("utf-8"),
        str(output_path).encode("utf-8"),
        _encode_resolutions(resolutions),
        1 if categorical else 0,
        crs.encode("utf-8") if crs else None,
        ctypes.byref(err),
    )
    _raise_if_err(err)
    return int(count)


__all__ = [
    "RasterH3ArrowStream",
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
    "raster_to_parquet",
    "raster_to_pmtiles",
]
