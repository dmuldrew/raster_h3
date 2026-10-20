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
| `--bbox <min_lon,min_lat,max_lon,max_lat>` | all | WGS84 bounding box |
| `--columns <c1,c2,…>` | `ipc`, `jsonl` | Column projection, in the given order |
| `--limit <N>` | `ipc`, `jsonl` | Maximum rows |
| `-o, --output <PATH\|->` | all | Output file; `-` or omitted means stdout (required for `parquet` and `pmtiles`) |
| `-h, --help` | | Help |

Arguments are validated strictly: unknown flags, missing values, resolutions outside
`0..=15`, malformed numbers, a bbox without exactly four numbers, an unknown column name
and a second positional input are all errors. Errors go to stderr with exit status `1`.

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

`parquet` always writes GeoParquet 1.1 (WKB geometry + `geo` metadata) and reports nothing
on stdout. `pmtiles` writes one zoom range per requested resolution (see
[PMTiles & Multi-Resolution](../pmtiles.md)).

```bash
raster-h3-cli parquet dem.tif -r 7,8 -o dem_h3.parquet
raster-h3-cli pmtiles landcover.tif -c -r 5,6,7,8 -o landcover.pmtiles
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
| `CliStreamOptions` | `input_path`, `resolutions`, `categorical`, `categorical_format`, `source_crs`, `nodata`, `bbox`, `columns`, `batch_size` (2048), `limit`, `output_format` |
| `stream_raster_to_writer(&opts, &mut impl Write)` | Stream IPC or JSONL to any writer; returns rows written |
| `export_raster_to_parquet(...)`, `export_raster_to_pmtiles(...)` | File exports |

## Testing

```bash
cargo test -p raster-h3-cli        # argument parsing, IPC round trip, JSONL
cargo test -p raster_h3 --test it multi_engine_e2e # runs the built binary for all four subcommands
```
