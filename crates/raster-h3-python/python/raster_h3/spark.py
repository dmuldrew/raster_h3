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
does this), so merged rows have the scan's columns and pass the same filters. Spark applies
`WHERE` and `LIMIT` after the scan; use the options above to narrow it.

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


class RasterH3Reader(DataSourceReader):  # type: ignore[misc,valid-type]
    def __init__(self, config: Mapping[str, Any], specs: Sequence[str], schema: pa.Schema) -> None:
        self.config = dict(config)
        self.specs = list(specs)
        self.arrow_schema = schema

    def partitions(self) -> List[RasterH3Partition]:
        return [RasterH3Partition(i, spec) for i, spec in enumerate(self.specs)]

    def read(self, partition: RasterH3Partition) -> Iterator[pa.RecordBatch]:
        import raster_h3 as rh3  # imported on the executor

        stream = rh3.RasterH3ArrowStream(
            self.config["path"], partition=partition.spec, **_scan_kwargs(self.config)
        )
        for batch in pa.RecordBatchReader.from_stream(stream):
            yield _to_schema(batch, self.arrow_schema)


def _require_pyspark() -> None:
    if _IMPORT_ERROR is not None:
        raise ImportError(
            "raster_h3.spark needs PySpark 4.0 or later (pyspark.sql.datasource)"
        ) from _IMPORT_ERROR


def register(spark) -> None:
    """Register the `raster_h3` data source with a Spark session."""
    _require_pyspark()
    spark.dataSource.register(RasterH3DataSource)


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

    state = raster_h3.PARTIAL_STATE_COLUMN
    final = df.where(f"{state} IS NULL").drop(state)
    partial = df.where(f"{state} IS NOT NULL")
    out_schema = final.schema
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
        import raster_h3 as rh3  # imported on the executor

        # After `IS NOT NULL` Spark marks the state non-nullable, so build the table from
        # the batches' own schema.
        batches = list(batches)
        if not batches:
            return
        merged = rh3.merge_partials(pa.Table.from_batches(batches), **options)
        for batch in merged.to_batches():
            yield _to_schema(batch, arrow_out)

    # Every share of a cell lands in one task; an H3 index fixes its resolution.
    merged = partial.repartition("h3_index").mapInArrow(merge, out_schema)
    return final.unionByName(merged)


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
    "RasterH3Partition",
    "RasterH3Reader",
    "merge_partials",
    "read",
    "register",
]
