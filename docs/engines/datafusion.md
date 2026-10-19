# DataFusion Deep Dive: `raster-h3-datafusion`

[← Engines Overview](README.md) · [← Back to README](../../README.md)

`raster-h3-datafusion` ([`crates/raster-h3-datafusion`](../../crates/raster-h3-datafusion))
exposes rasters as scannable tables and the H3 helpers as vectorized scalar functions,
using the same shapes as Apache DataFusion's `TableProvider`, `ExecutionPlan` and
`ScalarUDFImpl`.

> **Scope.** The crate does **not** depend on the `datafusion` crate. `TableProvider`,
> `ExecutionPlan` and `ScalarUDFImpl` here are this crate's own traits, modelled on
> DataFusion's, operating on `raster_h3_core::columnar` types (`ColumnSchema`,
> `RecordBatch`, `ColumnArray`). To use them inside a real DataFusion `SessionContext`, wrap
> them in DataFusion's traits and import batches through the Arrow C Stream
> (`execute_arrow_c_stream`). Its only dependencies are `raster-h3-core` and `h3o`.

```toml
[dependencies]
raster-h3-datafusion = { path = "crates/raster-h3-datafusion" }
```

## Quick Start

```rust
use raster_h3_datafusion::{
    CategoricalFormat, FilterPredicate, RasterH3SessionCatalog, TableProvider,
};

let ctx = RasterH3SessionCatalog::new(); // all scalar UDFs registered

let provider = ctx
    .h3_raster_continuous_aggregate("dem.tif", vec![7, 8])?
    .with_crs("EPSG:4326")
    .with_batch_size(8192);

let filters = [FilterPredicate::MinCount(10.0), FilterPredicate::Resolution(8)];
// projection = h3_index (0), mean (2); limit = 1000
let plan = provider.scan(Some(&[0, 2]), &filters, Some(1000))?;

for batch in plan.execute()? {
    let batch = batch?;
    println!("{} rows", batch.num_rows);
}

// Or hand the result to any Arrow C Stream consumer
let stream = plan.execute_arrow_c_stream()?;
```

## Catalog

`RasterH3SessionCatalog` is a small registry, not a query planner:

| Method | Purpose |
| :--- | :--- |
| `new()` | Catalog with every default scalar UDF registered (`register_all`). |
| `register_udf(udf)` / `udf(name)` | Register / look up a UDF by name or alias (case-insensitive). |
| `register_table(name, provider)` / `table(name)` | Register / look up a `TableProvider` by name. |
| `invoke_scalar(name, &[ColumnArray])` | Run a registered UDF on columnar arguments. |
| `h3_raster_continuous_aggregate(source, resolutions)` | Build a `ContinuousRasterTableProvider`. |
| `h3_raster_categorical_aggregate(source, resolutions)` | Build a `CategoricalRasterTableProvider`. |

`source` accepts anything `resolve_raster_sources` accepts (a path, glob, comma-separated
list or VRT). An empty `resolutions` vector defaults to `[8]`.

## Table Providers

### `ContinuousRasterTableProvider`

Schema: `ColumnSchema::continuous(&quantiles)` (see the [overview](README.md#output-schemas)).

| Builder | Maps to `MultiResolutionConfig` |
| :--- | :--- |
| `with_crs(crs)` | `custom_crs` |
| `with_nodata(v)` | `custom_nodata` |
| `with_bbox([min_lon, min_lat, max_lon, max_lat])` | `bbox` |
| `with_sampling(SamplingPattern)` | `sampling` (see [Super-Sampling](../super-sampling.md)) |
| `with_band(n)` | `band` |
| `with_spectral_formula(SpectralFormula)` | `spectral_formula` (NDVI, NDWI, NBR, EVI) |
| `with_quantiles(Vec<QuantileTarget>)` | `quantiles`; adds one nullable `Float64` column per target |
| `with_min_count`, `with_min_mean`, `with_max_mean` | value filters |
| `with_compact(bool)` | parent compaction |
| `with_overlap_rule(OverlapRule)` | mosaic overlap rule (`Cutline`, `First`, `Average`) |
| `with_batch_size(n)` | rows per `RecordBatch` (default 2048) |

### `CategoricalRasterTableProvider`

Schema: `categorical_wide()` (default) or `categorical_long()`.

| Builder | Effect |
| :--- | :--- |
| `with_format(CategoricalFormat::Wide \| Long)` | Output layout |
| `with_crs`, `with_nodata`, `with_bbox`, `with_batch_size` | As above |
| `with_remapper(Arc<CategoryRemapper>)` | Reclassify categories during ingestion |
| `with_min_count`, `with_min_majority_fraction` | Value filters |

## Pushdown

`TableProvider::scan(projection, filters, limit)` clones the provider's configuration,
narrows it with the filters and returns an `ExecutionPlan`.

| `FilterPredicate` | Continuous | Categorical | Effect |
| :--- | :---: | :---: | :--- |
| `MinCount(v)` | Exact | Exact | Keeps the stricter of `v` and any configured minimum |
| `MinMean(v)`, `MaxMean(v)` | Exact | Unsupported | Keeps the stricter bound |
| `MinMajorityFraction(v)` | Unsupported | Exact | Keeps the stricter bound |
| `Resolution(r)` | Exact | Exact | Restricts the scan to `r`; an `r` outside the configured set yields an empty scan |
| `Bbox(b)` | Inexact | Inexact | Intersects with the configured bbox; a disjoint box yields an empty scan. Antimeridian-crossing boxes keep the configured bbox, so the caller must re-apply the predicate |
| `Other(_)` | Unsupported | Unsupported | Ignored by the scan |

Filters only ever narrow the table. `supports_filters_pushdown(&filters)` returns the
classification above so a planner knows which predicates it still has to evaluate.

- **Projection**: `Some(&[indices])` projects the schema; only those columns are built.
- **Limit**: passed to the batch reader, which stops draining once the limit is reached.
- **Statistics**: `statistics()` returns `TableStatistics { num_rows, is_exact: false }`
  from `estimate_raster_cardinality`.

## Execution Plans

`ContinuousRasterExec` and `CategoricalRasterExec` implement:

| Method | Returns |
| :--- | :--- |
| `schema()` | Projected `Arc<ColumnSchema>` |
| `execute()` | `Box<dyn Iterator<Item = Result<RecordBatch>> + Send>`; each call opens a new mosaic and streamer |
| `execute_arrow_c_stream()` | `ArrowArrayStream` wrapping `execute()` |

## Scalar UDFs

`ScalarUDFImpl` has `name()`, `aliases()`, `return_type(&[ArrowDataType])` and
`invoke(&[ColumnArray]) -> ColumnArray`. Input validity propagates to the output.

| UDF | Signature(s) | Aliases |
| :--- | :--- | :--- |
| `H3ToStringUdf` | `h3_to_string(UInt64) -> Utf8` | |
| `StringToH3Udf` | `string_to_h3(Utf8) -> UInt64` | |
| `H3ToLatUdf`, `H3ToLngUdf` | `(UInt64) -> Float64` | |
| `H3GetResolutionUdf` | `h3_get_resolution(UInt64) -> Int64` | |
| `H3IsValidUdf` | `h3_is_valid(UInt64 \| Utf8) -> Boolean` | |
| `H3ToWkbUdf` | `h3_to_wkb(UInt64 \| Utf8) -> Binary` | `h3_to_geometry`, `h3_cell_to_geometry` |
| `H3CellToParentUdf` | `h3_cell_to_parent(UInt64, Int64) -> UInt64`, `(Utf8, Int64) -> Utf8` | |

```rust
use raster_h3_datafusion::{ColumnArray, RasterH3SessionCatalog};

let ctx = RasterH3SessionCatalog::new();
let cells = ColumnArray::UInt64 { values: vec![0x8828308281fffff], validity: None };
let hex = ctx.invoke_scalar("h3_to_string", &[cells])?;
```

## Testing

```bash
cargo test -p raster-h3-datafusion
```

Unit tests cover projection, limit, filter narrowing (resolution and bbox), the Arrow C
stream round trip and every UDF. `tests/it/multi_engine_e2e.rs` checks the providers
against the other engines.
