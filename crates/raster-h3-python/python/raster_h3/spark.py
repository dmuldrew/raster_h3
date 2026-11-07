"""raster_h3 as an Apache Spark data source (Spark 4.0+ Python Data Source API).

    import raster_h3.spark

    raster_h3.spark.register(spark)
    df = (spark.read.format("raster_h3")
          .option("resolutions", "8,9")
          .option("target_chunks", 256)
          .load("dem.tif"))
    cells = raster_h3.spark.merge_partials(df)

    # or both steps at once:
    cells = raster_h3.spark.read(spark, "dem.tif", resolutions=[8, 9])

    -- or in SQL (register() also registers the raster_h3_merge table function):
    CREATE TEMPORARY VIEW dem_parts USING raster_h3 OPTIONS (path 'dem.tif', resolutions '8,9');
    SELECT * FROM raster_h3_merge(TABLE(dem_parts));

Each Spark partition scans one partition planned by `raster_h3.plan_partitions` (a disjoint
set of TIFF tiles or strips), so executors read one raster in parallel and no pixel is read
twice. The data source returns the scan's columns with Spark's signed types plus
`partial_state`: rows where it is null are final cells, the others are shares of cells that
several partitions reach. `merge_partials` shuffles those by `h3_index` and merges them, so
the result equals one unpartitioned scan (up to floating-point summation order).

Options (all optional except the path), with the names and meanings of
`raster_h3.RasterH3ArrowStream`:

    resolutions          "8" or "7,8,9" (default 8)
    target_chunks        TIFF tiles or strips per partition (default 256)
    categorical          "true" for categorical aggregation (wide format)
    columns              comma-separated output columns (keep h3_index and resolution)
    crs, nodata, bbox    CRS override, NoData override, "minx,miny,maxx,maxy" in WGS84
    band, sampling, overlap_rule, spectral_formula, nir_band, red_band, green_band,
    blue_band, swir_band, quantiles, remap, min_count, min_mean, max_mean,
    min_majority_fraction, strict_transforms

Pass the same `categorical`, `columns`, `quantiles` and filters to `merge_partials` (`read`
does this), so merged rows have the scan's columns and pass the same filters.

With filter pushdown (Spark 4.1+, `register(spark, pushdown=True)` or
`spark.sql.python.filterPushdown.enabled` set before `register`), `WHERE` filters on `resolution` (`=`, `IN`, `<`, `<=`, `>`, `>=`) and on
`partial_state IS [NOT] NULL` are pushed into the scan: only matching resolutions are
indexed, and only final or only partial rows are produced. Filters on statistics (`count`,
`mean`, ...) are left to Spark: before the merge they would see a partition's share of an
edge cell, and after the merge Spark cannot push them through it, so narrow the scan with
the options above (`min_count`, ...) instead. `LIMIT` and column selection apply after the
scan; use `columns` to project.

Every executor needs this package (with its native library) and must be able to open the
path: a shared file system or an `http(s)://` / `s3://` URL.
"""

from __future__ import annotations

from typing import Any, Dict, Iterator, List, Mapping, Optional, Sequence

import pyarrow as pa

import raster_h3

SHORT_NAME = "raster_h3"

# Spark option name -> parser of its string value. Spark lower-cases option keys.
_INT = int
_FLOAT = float
_STR = str


def _bool(value: str) -> bool:
    lowered = str(value).strip().lower()
    if lowered in ("true", "1", "yes"):
        return True
    if lowered in ("false", "0", "no"):
        return False
    raise ValueError(f"expected true or false, got {value!r}")


def _int_list(value: str) -> List[int]:
    return [int(v) for v in str(value).split(",") if v.strip()]


def _str_list(value: str) -> List[str]:
    return [v.strip() for v in str(value).split(",") if v.strip()]


_SCAN_OPTIONS = {
    "resolutions": _int_list,
    "categorical": _bool,
    "crs": _STR,
    "nodata": _FLOAT,
    "bbox": _STR,
    "columns": _str_list,
    "strict_transforms": _bool,
    "band": _INT,
    "sampling": _STR,
    "overlap_rule": _STR,
    "spectral_formula": _STR,
    "nir_band": _INT,
    "red_band": _INT,
    "green_band": _INT,
    "blue_band": _INT,
    "swir_band": _INT,
    "quantiles": _STR,
    "remap": _STR,
    "min_count": _FLOAT,
    "min_mean": _FLOAT,
    "max_mean": _FLOAT,
    "min_majority_fraction": _FLOAT,
}
_PLAN_OPTIONS = ("crs", "bbox", "overlap_rule")
_MERGE_OPTIONS = (
    "categorical",
    "columns",
    "quantiles",
    "min_count",
    "min_mean",
    "max_mean",
    "min_majority_fraction",
)
_OTHER_OPTIONS = ("path", "target_chunks")

# Spark has no unsigned integers. H3 cell ids never set the top bit, so UInt64 -> Int64 is
# lossless; the smaller unsigned types widen.
_SIGNED = {
    pa.uint8(): pa.int16(),
    pa.uint16(): pa.int32(),
    pa.uint32(): pa.int64(),
    pa.uint64(): pa.int64(),
    pa.large_binary(): pa.binary(),
    pa.large_string(): pa.string(),
}


def _signed_schema(schema: pa.Schema) -> pa.Schema:
    return pa.schema(
        [pa.field(f.name, _SIGNED.get(f.type, f.type), nullable=True) for f in schema]
    )


def _to_schema(batch: pa.RecordBatch, schema: pa.Schema) -> pa.RecordBatch:
    """Cast `batch` to `schema`, adding a null column for any field it lacks."""
    names = batch.schema.names
    columns = []
    for field in schema:
        if field.name in names:
            columns.append(batch.column(names.index(field.name)).cast(field.type))
        else:
            columns.append(pa.nulls(batch.num_rows, field.type))
    return pa.RecordBatch.from_arrays(columns, schema=schema)


def _parse_options(options: Mapping[str, Any]) -> Dict[str, Any]:
    """Spark's string options as typed `RasterH3ArrowStream` arguments."""
    lowered = {str(k).lower(): v for k, v in options.items()}
    unknown = sorted(set(lowered) - set(_SCAN_OPTIONS) - set(_OTHER_OPTIONS))
    if unknown:
        raise ValueError(f"unknown raster_h3 option(s): {', '.join(unknown)}")
    if "path" not in lowered:
        raise ValueError("raster_h3 needs a path: spark.read.format('raster_h3').load(path)")
    parsed: Dict[str, Any] = {"path": str(lowered["path"])}
    parsed["target_chunks"] = int(lowered.get("target_chunks", 256))
    for name, parse in _SCAN_OPTIONS.items():
        if name in lowered and lowered[name] is not None:
            try:
                parsed[name] = parse(lowered[name])
            except ValueError as exc:
                raise ValueError(f"raster_h3 option {name}: {exc}") from None
    parsed.setdefault("resolutions", [8])
    return parsed


def _scan_kwargs(config: Mapping[str, Any]) -> Dict[str, Any]:
    return {k: v for k, v in config.items() if k in _SCAN_OPTIONS}


try:
    from pyspark.sql.datasource import DataSource, DataSourceReader, InputPartition
except ImportError as _exc:  # pragma: no cover - exercised only without PySpark 4
    _IMPORT_ERROR: Optional[ImportError] = _exc
    DataSource = DataSourceReader = InputPartition = object  # type: ignore[misc,assignment]
else:
    _IMPORT_ERROR = None

try:  # Spark 4.1+: filter pushdown
    from pyspark.sql.datasource import (
        EqualTo,
        GreaterThan,
        GreaterThanOrEqual,
        In,
        IsNotNull,
        IsNull,
        LessThan,
        LessThanOrEqual,
    )
except ImportError:
    _PUSHDOWN = False
else:
    _PUSHDOWN = True


class RasterH3Partition(InputPartition):  # type: ignore[misc,valid-type]
    """One `plan_partitions` spec: the TIFF chunks one Spark task scans."""

    def __init__(self, index: int, spec: str) -> None:
        if _IMPORT_ERROR is None:
            super().__init__(index)
        self.index = index
        self.spec = spec


class RasterH3DataSource(DataSource):  # type: ignore[misc,valid-type]
    """`spark.read.format("raster_h3")`: see the module documentation for options."""

    @classmethod
    def name(cls) -> str:
        return SHORT_NAME

    def __init__(self, options: Mapping[str, Any]) -> None:
        super().__init__(options)
        self.config = _parse_options(options)
        self._specs: Optional[List[str]] = None
        self._arrow_schema: Optional[pa.Schema] = None

    def specs(self) -> List[str]:
        """The partition specs, planned once from the raster's metadata."""
        if self._specs is None:
            plan = {k: self.config[k] for k in _PLAN_OPTIONS if k in self.config}
            self._specs = raster_h3.plan_partitions(
                self.config["path"], target_chunks=self.config["target_chunks"], **plan
            )
        return self._specs

    def arrow_schema(self) -> pa.Schema:
        """The partitioned scan's schema in Spark's signed types (with `partial_state`)."""
        if self._arrow_schema is None:
            stream = raster_h3.RasterH3ArrowStream(
                self.config["path"], partition=self.specs()[0], **_scan_kwargs(self.config)
            )
            self._arrow_schema = _signed_schema(pa.schema(stream))
        return self._arrow_schema

    def schema(self):
        from pyspark.sql.pandas.types import from_arrow_schema

        return from_arrow_schema(self.arrow_schema())

    def reader(self, schema) -> "RasterH3Reader":
        return RasterH3Reader(self.config, self.specs(), self.arrow_schema())


def _resolution_filter(flt: Any) -> Optional[Any]:
    """A predicate on one resolution value for a pushable `resolution` filter, else None."""
    if getattr(flt, "attribute", None) != ("resolution",):
        return None
    if isinstance(flt, IsNotNull):
        return lambda r: True  # every row has a resolution
    if not hasattr(flt, "value"):
        return None
    value = flt.value
    if isinstance(flt, EqualTo):
        return lambda r: r == value
    if isinstance(flt, In):
        values = set(value)
        return lambda r: r in values
    if isinstance(flt, GreaterThan):
        return lambda r: r > value
    if isinstance(flt, GreaterThanOrEqual):
        return lambda r: r >= value
    if isinstance(flt, LessThan):
        return lambda r: r < value
    if isinstance(flt, LessThanOrEqual):
        return lambda r: r <= value
    return None


class RasterH3Reader(DataSourceReader):  # type: ignore[misc,valid-type]
    def __init__(self, config: Mapping[str, Any], specs: Sequence[str], schema: pa.Schema) -> None:
        self.config = dict(config)
        self.specs = list(specs)
        self.arrow_schema = schema
        # Set by pushFilters (Spark 4.1+): "final" or "partial" rows only, "none", or both (None).
        self.rows: Optional[str] = None

    def partitions(self) -> List[RasterH3Partition]:
        return [RasterH3Partition(i, spec) for i, spec in enumerate(self.specs)]

    def read(self, partition: RasterH3Partition) -> Iterator[pa.RecordBatch]:
        import pyarrow.compute as pc

        import raster_h3 as rh3  # imported on the executor

        if not self.config["resolutions"] or self.rows == "none":
            return  # the pushed filters exclude every row
        stream = rh3.RasterH3ArrowStream(
            self.config["path"], partition=partition.spec, **_scan_kwargs(self.config)
        )
        for batch in pa.RecordBatchReader.from_stream(stream):
            if self.rows is not None:
                state = batch.column(batch.schema.get_field_index(rh3.PARTIAL_STATE_COLUMN))
                mask = pc.is_null(state) if self.rows == "final" else pc.is_valid(state)
                batch = batch.filter(mask)
                if batch.num_rows == 0:
                    continue
            yield _to_schema(batch, self.arrow_schema)


class RasterH3PushdownReader(RasterH3Reader):
    """`RasterH3Reader` with Spark 4.1 filter pushdown.

    Kept separate because Spark refuses a reader that defines `pushFilters` while
    `spark.sql.python.filterPushdown.enabled` is false; `register` picks the reader.
    """

    def pushFilters(self, filters: List[Any]) -> List[Any]:
        """Push `resolution` comparisons and `partial_state IS [NOT] NULL`; keep the rest.

        Both are exact for every row the scan produces, final or partial, so Spark need not
        evaluate them again. Filters on statistics are returned: on partial rows they would
        see only one partition's share of a cell.
        """
        resolutions = list(self.config["resolutions"])
        state = (raster_h3.PARTIAL_STATE_COLUMN,)
        wanted = {"final", "partial"}
        remaining = []
        for flt in filters:
            keep = _resolution_filter(flt)
            if keep is not None:
                resolutions = [r for r in resolutions if keep(r)]
            elif isinstance(flt, IsNull) and flt.attribute == state:
                wanted &= {"final"}
            elif isinstance(flt, IsNotNull) and flt.attribute == state:
                wanted &= {"partial"}
            else:
                remaining.append(flt)
        self.config["resolutions"] = resolutions
        self.rows = None if len(wanted) == 2 else (wanted.pop() if wanted else "none")
        return remaining


class RasterH3PushdownDataSource(RasterH3DataSource):
    """`raster_h3` with filter pushdown (Spark 4.1+, filter pushdown enabled)."""

    def reader(self, schema) -> "RasterH3PushdownReader":
        return RasterH3PushdownReader(self.config, self.specs(), self.arrow_schema())


def _require_pyspark() -> None:
    if _IMPORT_ERROR is not None:
        raise ImportError(
            "raster_h3.spark needs PySpark 4.0 or later (pyspark.sql.datasource)"
        ) from _IMPORT_ERROR


_PUSHDOWN_CONF = "spark.sql.python.filterPushdown.enabled"


def register(spark, pushdown: Optional[bool] = None) -> None:
    """Register the `raster_h3` data source and the `raster_h3_merge` SQL table function.

    `pushdown` selects filter pushdown (Spark 4.1+). `None` follows the session's
    `spark.sql.python.filterPushdown.enabled` (false by default); `True` sets it and uses
    pushdown; `False` registers the source without it. Register again after changing that
    setting: Spark rejects a pushdown reader while it is false.
    """
    _require_pyspark()
    from pyspark.sql.functions import udtf

    if pushdown is None:
        pushdown = _PUSHDOWN and str(spark.conf.get(_PUSHDOWN_CONF, "false")).lower() == "true"
    elif pushdown:
        if not _PUSHDOWN:
            raise RuntimeError("filter pushdown needs Spark 4.1 or later")
        spark.conf.set(_PUSHDOWN_CONF, "true")
    spark.dataSource.register(RasterH3PushdownDataSource if pushdown else RasterH3DataSource)
    spark.udtf.register("raster_h3_merge", udtf(RasterH3Merge))


def merge_partials(
    df,
    *,
    categorical: bool = False,
    columns: Optional[Sequence[str]] = None,
    quantiles: Optional[str] = None,
    min_count: Optional[float] = None,
    min_mean: Optional[float] = None,
    max_mean: Optional[float] = None,
    min_majority_fraction: Optional[float] = None,
):
    """Final cells of a `raster_h3` DataFrame: merge the rows that have a `partial_state`.

    Pass the scan's `categorical`, `columns`, `quantiles` and filters. Merge each raster's
    DataFrame on its own: partial states of different rasters must not be combined.
    Returns a DataFrame with the scan's columns, without `partial_state`.
    """
    _require_pyspark()
    from pyspark.sql.pandas.types import to_arrow_schema

    from pyspark.sql.types import StructType

    state = raster_h3.PARTIAL_STATE_COLUMN
    out_schema = StructType([f for f in df.schema.fields if f.name != state])
    arrow_out = to_arrow_schema(out_schema)
    options = {
        "categorical": categorical,
        "columns": list(columns) if columns is not None else None,
        "quantiles": quantiles,
        "min_count": min_count,
        "min_mean": min_mean,
        "max_mean": max_mean,
        "min_majority_fraction": min_majority_fraction,
    }
    options = {k: v for k, v in options.items() if v is not None and v is not False}

    def merge(batches: Iterator[pa.RecordBatch]) -> Iterator[pa.RecordBatch]:
        import pyarrow.compute as pc

        import raster_h3 as rh3  # imported on the executor

        partial = []
        for batch in batches:
            is_final = pc.is_null(batch.column(batch.schema.get_field_index(state)))
            final = batch.filter(is_final)
            if final.num_rows:
                yield _to_schema(final, arrow_out)
            shares = batch.filter(pc.invert(is_final))
            if shares.num_rows:
                partial.append(shares)
        if partial:
            merged = rh3.merge_partials(pa.Table.from_batches(partial), **options)
            for batch in merged.to_batches():
                yield _to_schema(batch, arrow_out)

    # One scan, one shuffle: every share of a cell lands in the same task (an H3 index fixes
    # its resolution); final rows pass through. Splitting final and partial rows into two
    # branches instead would scan the raster twice unless the DataFrame were cached.
    return df.repartition("h3_index").mapInArrow(merge, out_schema)


_CATEGORICAL_COLUMNS = ("majority_class", "majority_fraction", "total_count", "histogram")


class RasterH3Merge:
    """`raster_h3_merge(TABLE(t) [, 'name=value, ...'])`: the SQL form of `merge_partials`.

    `t` is a `raster_h3` scan (with `partial_state`). Final rows pass through; partial rows
    are merged per cell. The options are those of `merge_partials` (`quantiles`,
    `min_count`, `min_mean`, `max_mean`, `min_majority_fraction`, `categorical`), plus
    `buckets` (default 64), the number of groups the rows are hashed into by `h3_index`.
    Categorical input is recognised from its columns.
    """

    @staticmethod
    def _options(text: Optional[str]) -> Dict[str, str]:
        options: Dict[str, str] = {}
        for item in (text or "").replace(";", ",").split(","):
            if item.strip():
                name, sep, value = item.partition("=")
                if not sep:
                    raise ValueError(f"raster_h3_merge option {item.strip()!r} is not name=value")
                options[name.strip().lower()] = value.strip()
        unknown = sorted(set(options) - set(_MERGE_OPTIONS) - {"buckets"} - {"columns"})
        if unknown or "columns" in options:
            raise ValueError(f"unknown raster_h3_merge option(s): {', '.join(unknown or ['columns'])}")
        return options

    @staticmethod
    def analyze(table, options=None):
        from pyspark.sql.types import StructType
        from pyspark.sql.udtf import AnalyzeResult, PartitioningColumn

        state = raster_h3.PARTIAL_STATE_COLUMN
        if not getattr(table, "isTable", False):
            raise ValueError("raster_h3_merge takes a table: raster_h3_merge(TABLE(t))")
        names = [f.name for f in table.dataType.fields]
        missing = [c for c in ("h3_index", "resolution", state) if c not in names]
        if missing:
            raise ValueError(f"raster_h3_merge needs a raster_h3 scan; missing column(s): {', '.join(missing)}")
        parsed = RasterH3Merge._options(options.value if options is not None else None)
        buckets = int(parsed.get("buckets", 64))
        if buckets < 1:
            raise ValueError("raster_h3_merge option buckets must be at least 1")
        return AnalyzeResult(
            schema=StructType([f for f in table.dataType.fields if f.name != state]),
            partitionBy=[PartitioningColumn(f"pmod(xxhash64(h3_index), {buckets})")],
        )

    def __init__(self) -> None:
        self.names: Optional[List[str]] = None
        self.merge_options: Dict[str, Any] = {}
        self.partial: Dict[str, list] = {"resolution": [], "h3_index": [], "partial_state": []}

    def _setup(self, row: Any, options: Optional[str]) -> None:
        state = raster_h3.PARTIAL_STATE_COLUMN
        self.names = [n for n in row.__fields__ if n != state]
        parsed = self._options(options)
        parsed.pop("buckets", None)
        typed = _parse_options({**parsed, "path": "-"})
        self.merge_options = {k: typed[k] for k in _MERGE_OPTIONS if k in typed}
        self.merge_options["columns"] = self.names
        if "categorical" not in typed and any(c in self.names for c in _CATEGORICAL_COLUMNS):
            self.merge_options["categorical"] = True

    def eval(self, row: Any, options: Optional[str] = None) -> Iterator[tuple]:
        if self.names is None:
            self._setup(row, options)
        state = row[raster_h3.PARTIAL_STATE_COLUMN]
        if state is None:
            yield tuple(row[n] for n in self.names)
        else:
            self.partial["resolution"].append(row["resolution"])
            self.partial["h3_index"].append(row["h3_index"])
            self.partial["partial_state"].append(bytes(state))

    def terminate(self) -> Iterator[tuple]:
        if not self.partial["h3_index"]:
            return
        table = pa.table(
            {
                "resolution": pa.array(self.partial["resolution"], pa.int16()),
                "h3_index": pa.array(self.partial["h3_index"], pa.int64()),
                "partial_state": pa.array(self.partial["partial_state"], pa.binary()),
            }
        )
        options = {k: v for k, v in self.merge_options.items() if v is not False}
        merged = raster_h3.merge_partials(table, **options)
        columns = [merged.column(n).to_pylist() for n in self.names]
        yield from zip(*columns)


def _option_string(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (list, tuple)):
        return ",".join(str(v) for v in value)
    return str(value)


def read(spark, path: str, **options: Any):
    """Load `path` with the `raster_h3` data source and merge its partial cells.

    `options` are the data source options as Python values (`resolutions=[8, 9]`,
    `target_chunks=256`, `quantiles="p50"`, ...). Returns the final cells.
    """
    register(spark)
    reader = spark.read.format(SHORT_NAME)
    for name, value in options.items():
        if value is not None:
            reader = reader.option(name, _option_string(value))
    df = reader.load(str(path))
    parsed = _parse_options({**{k: _option_string(v) for k, v in options.items() if v is not None}, "path": path})
    return merge_partials(df, **{k: parsed[k] for k in _MERGE_OPTIONS if k in parsed})


__all__ = [
    "SHORT_NAME",
    "RasterH3DataSource",
    "RasterH3Merge",
    "RasterH3Partition",
    "RasterH3PushdownDataSource",
    "RasterH3PushdownReader",
    "RasterH3Reader",
    "merge_partials",
    "read",
    "register",
]
