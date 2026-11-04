# CLI Deep Dive: `raster-h3-cli`

[← Engines Overview](README.md) · [← Back to README](../../README.md)

`raster-h3-cli` ([`crates/raster-h3-cli`](../../crates/raster-h3-cli)) is a single
executable that streams raster aggregations to stdout or writes Parquet/PMTiles files. It
is meant for shell pipelines, batch jobs and engines that can run an external command,
such as ClickHouse executable table functions.

The same source ([`src/bin/raster_h3.rs`](../../crates/raster-h3-cli/src/bin/raster_h3.rs))
builds two binaries:

| Binary | Built by |
| :--- | :--- |
| `raster-h3-cli` | `cargo build --release -p raster-h3-cli`; also installed onto `PATH` by the `raster-h3` Python wheel |
| `raster-h3` | the root crate (`cargo build --release` / `cargo install --path .`) |

## Usage

```text
raster-h3-cli ipc     <input> [options]        Stream RH3IPC01 batches to stdout or -o <file>
raster-h3-cli jsonl   <input> [options]        Stream JSONEachRow (NDJSON) to stdout or -o <file>
raster-h3-cli parquet <input> -o <out> [opts]  Write GeoParquet
raster-h3-cli pmtiles <input> -o <out> [opts]  Write a PMTiles v3 archive
raster-h3-cli plan    <input> [options]        Print partition specs (JSON Lines), see Partitioned scans
raster-h3-cli merge   <part>... [options]      Combine partitioned `ipc` outputs into final cells
```

`<input>` can be a path, glob, comma-separated list or VRT (it may also be given with
`-i/--input`). Mosaic overlaps use the `cutline` rule.

| Option | Applies to | Meaning |
| :--- | :--- | :--- |
| `-r, --resolutions <LIST>` (alias `--resolution`) | all | H3 resolutions, e.g. `8` or `7,8,9` (default `8`) |
| `-c, --categorical` | all | Categorical aggregation |
| `--format <wide\|long>` | `ipc`, `jsonl` | Categorical layout (default `wide`) |
| `--crs <CRS>` (alias `--source-crs`) | all | Source CRS override, e.g. `EPSG:4326` |
| `--nodata <FLOAT>` | all | NoData override |
| `--raw-values` | all | Aggregate stored values without GDAL band scale/offset calibration |
| `--bbox <min_lon,min_lat,max_lon,max_lat>` | all | WGS84 bounding box |
| `--columns <c1,c2,…>` | `ipc`, `jsonl` | Column projection, in the given order |
| `--limit <N>` | `ipc`, `jsonl` | Maximum rows |
| `--partition <SPEC\|@FILE>` | `ipc` | Scan one partition (see [Partitioned scans](#partitioned-scans)) |
| `--partition-index <K>` | `ipc` | Line `K` of the plan file given as `--partition @FILE` |
| `-o, --output <PATH\|->` | all | Output file; `-` or omitted means stdout (required for `parquet` and `pmtiles`) |
| `-h, --help` | | Help |

Arguments are validated strictly: unknown flags, missing values, resolutions outside
`0..=15`, malformed numbers, a bbox without exactly four numbers, an unknown column name
and a second positional input are all errors. Errors go to stderr with exit status `1`.
Every subcommand also prints a warning to stderr when valid samples were dropped because
their coordinate transform failed (see [transform-failure diagnostics](core.md#transform-failure-diagnostics)).

Column names come from the core schemas (see the
[overview](README.md#output-schemas)); `--columns` with a name that does not
exist in the selected schema fails before any raster is read.

## Output Formats

### `jsonl`

One JSON object per row. Non-finite floats and nulls become `null`; `wkb` is a lowercase
hex string. Keys are emitted in alphabetical order (not `--columns` order); consumers that
match by name, such as ClickHouse `JSONEachRow`, are unaffected.

```bash
raster-h3-cli jsonl dem.tif -r 8 --columns h3_hex,mean,count --limit 3
# {"count":64.0,"h3_hex":"8828308281fffff","mean":42.0}
```

### `ipc`

`RH3IPC01`, the workspace's binary columnar stream. It is **not** Apache Arrow IPC; read it
with `raster_h3_core::columnar::ipc::read_ipc_stream` or against the layout in the
[Core deep dive](core.md#rh3ipc01-stream-and-ndjson). Use `jsonl`, or the Python Arrow
stream, when the consumer expects standard formats.

### `parquet` and `pmtiles`

`parquet` always writes GeoParquet 1.1 (WKB geometry, see [cell geometry](../sql-api-reference.md#cell-geometry), + `geo` metadata) and reports nothing
on stdout. `pmtiles` writes one zoom range per requested resolution (see
[PMTiles & Multi-Resolution](../pmtiles.md)).

```bash
raster-h3-cli parquet dem.tif -r 7,8 -o dem_h3.parquet
raster-h3-cli pmtiles landcover.tif -c -r 5,6,7,8 -o landcover.pmtiles
```

## Recipes

These mirror the [DuckDB SQL recipes](duckdb.md#sql-usage--recipes) for the options the CLI
exposes.

### Basic continuous aggregation

```bash
raster-h3-cli jsonl elevation.tif -r 8 --columns h3_index,h3_hex,mean,count,min,max,sum
```

### CRS, NoData and bounding box

```bash
raster-h3-cli jsonl temperature_global.tif -r 9 \
  --crs EPSG:4326 --nodata -9999 \
  --bbox -122.50,37.70,-122.35,37.85 \
  --columns h3_hex,mean,count
```

### Multiple resolutions in one pass

```bash
raster-h3-cli jsonl elevation.tif -r 6,7,8 --columns resolution,h3_hex,mean \
  | jq -c 'select(.resolution == 8)'
```

### Sampling the output

```bash
raster-h3-cli jsonl elevation.tif -r 8 --limit 5      # stops aggregation after 5 rows
```

### Categorical aggregation

```bash
# Wide: majority class, dominance and the JSON histogram
raster-h3-cli jsonl worldcover_2021.tif -c -r 8 \
  --columns h3_hex,majority_class,majority_fraction,unique_classes,total_count,histogram

# Histogram lookups with jq (histogram is a JSON string)
raster-h3-cli jsonl worldcover_2021.tif -c -r 8 --columns h3_hex,histogram \
  | jq -c '{h3_hex, urban: (.histogram | fromjson | .["50"])} | select(.urban != null)'

# Long: one row per (cell, class)
raster-h3-cli jsonl worldcover_2021.tif -c --format long -r 8 \
  --columns h3_hex,category,count,fraction \
  | jq -c 'select(.category == 50 and .fraction >= 0.25)'
```

### Export to GeoParquet and PMTiles

```bash
raster-h3-cli parquet elevation.tif -r 8 -o elevation_h3.parquet
raster-h3-cli pmtiles california_elevation.tif -r 6,7,8 -o california_elevation.pmtiles
raster-h3-cli pmtiles worldcover_2021.tif -c -r 5,6,7,8 -o worldcover.pmtiles
```

### Remote COG & S3 streaming

```bash
raster-h3-cli jsonl https://example.com/rasters/cog_elevation.tif -r 8 --limit 10
raster-h3-cli parquet s3://my-geospatial-bucket/landsat/scene_01.tif -r 9 -o scene_01_h3.parquet
```

### Multi-file mosaics

Quote globs so the shell does not expand them; overlaps use the `cutline` rule.

```bash
raster-h3-cli jsonl 'tiles/tile_*.tif' -r 8
raster-h3-cli jsonl tile_west.tif,tile_east.tif -c -r 8
```

### Joins

The CLI has no join of its own; stream into an engine that does. With DuckDB (no extension
needed):

```bash
raster-h3-cli jsonl california_elevation.tif -r 8 --columns h3_index,mean | duckdb -c "
  SELECT r.h3_index, r.mean, p.total_population
  FROM read_ndjson_auto('/dev/stdin') r
  JOIN 'population_h3.parquet' p USING (h3_index)
  WHERE r.mean > 500"
```

## Integrations

### ClickHouse executable table function

```sql
SELECT h3_hex, mean, count
FROM executable(
    'raster-h3-cli jsonl /data/dem.tif -r 8 --columns h3_hex,mean,count',
    JSONEachRow,
    'h3_hex String, mean Float64, count Float64'
)
ORDER BY count DESC
LIMIT 10;
```

For ClickHouse's `executable`, the command must be in the server's `user_scripts`
directory (or referenced through a wrapper script there).

### Partitioned scans

One raster or mosaic can be split across processes (`xargs -P`, GNU parallel, batch-job
arrays), each reading a disjoint set of TIFF tiles or strips. The result has exactly the
cells and values of a single scan.

```bash
# 1. Plan from metadata: one JSON spec per line, ~256 tiles/strips each.
raster-h3-cli plan dem.tif --target-chunks 256 -o plan.jsonl

# 2. Scan each partition (any process or machine that can read dem.tif).
seq 0 $(( $(wc -l < plan.jsonl) - 1 )) | xargs -P 8 -I{} \
  raster-h3-cli ipc dem.tif -r 8 --partition @plan.jsonl --partition-index {} -o parts/{}.rh3

# 3. Merge: final rows pass through, cells split across partitions are combined.
raster-h3-cli merge parts/*.rh3 -o dem_r8.rh3          # or --jsonl
```

- `plan` takes `--target-chunks <N>` (default 256), `--crs`, `--bbox` and `-o`. Plan and
  scan with the same input, `--crs` and `--bbox`: a raster with another layout, or a scan
  with another bbox, refuses the spec (`different mosaic`). Rasters that differ only in
  pixel values share a layout, so one plan serves them all.
- `--partition` takes a spec inline (one line of the plan, e.g. from
  `xargs -d '\n' -I{} … --partition '{}'`) or `@FILE`: a file with one spec, or a plan file
  with `--partition-index`.
- A partition's output is the normal `ipc` stream plus a nullable `partial_state` column:
  null rows are final cells, set rows are this partition's share of a cell that other
  partitions also reach. Only `ipc` output is accepted (that is what `merge` reads);
  `--limit`, `--format long`, `parquet` and `pmtiles` are refused with `--partition`, and
  `--columns` must keep `h3_index` and `resolution`.
- `merge` reads the partition files of one scan (same options and columns, enforced),
  writes every final row, then the merged rows. Only partial rows are kept in memory.
  Categorical input is recognised from its columns; pass `-c` when the projection has
  only shared columns (`h3_index`, `h3_hex`, `resolution`, `wkb`). The output's columns
  are the scan's, without `partial_state`, so it can be used as an unpartitioned `ipc`
  result (rows in a different order).

### Shell pipelines

```bash
raster-h3-cli jsonl dem.tif -r 8 --columns h3_index,mean | duckdb -c \
  "SELECT count(*), avg(mean) FROM read_ndjson_auto('/dev/stdin')"

raster-h3-cli jsonl dem.tif -r 8 | jq -c 'select(.count > 100)'
```

## Library API

The binary is a thin wrapper over [`src/lib.rs`](../../crates/raster-h3-cli/src/lib.rs):

| Item | Purpose |
| :--- | :--- |
| `parse_stream_args(args, default_format)` | Parse arguments into `CliStreamOptions` and an optional output path |
| `CliStreamOptions` | `input_path`, `resolutions`, `categorical`, `categorical_format`, `source_crs`, `nodata`, `apply_scale_offset` (true), `bbox`, `columns`, `batch_size` (2048), `limit`, `output_format`, `partition` |
| `stream_raster_to_writer(&opts, &mut impl Write)` | Stream IPC or JSONL to any writer; returns rows written |
| `export_raster_to_parquet(...)`, `export_raster_to_pmtiles(...)` | File exports |
| `plan_raster_partitions(input, target_chunks, crs, bbox)`, `write_plan(&plan, w)` | Plan partitions; write them as JSON Lines |
| `read_partition_spec(spec, index)` | Parse a spec, or `@FILE` (with `index` for a plan file); sets `CliStreamOptions::partition` |
| `merge_partition_files(&CliMergeOptions, w)` | Merge partition files; `parse_plan_args` / `parse_merge_args` parse the subcommands |

## Testing

```bash
cargo test -p raster-h3-cli        # argument parsing, IPC round trip, JSONL, plan/partition/merge vs one scan
cargo test -p raster_h3 --test it multi_engine_e2e # runs the built binary for all four subcommands
```
