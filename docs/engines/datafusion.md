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
> (`execute_arrow_c_stream`), or use [`raster-h3-sedona`](#using-with-apache-datafusion-and-sedonadb),
> which does exactly that. Its only dependencies are `raster-h3-core` and `h3o`.

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
| `with_scale_offset(b)` | `apply_scale_offset` (default `true`) |
| `with_bbox([min_lon, min_lat, max_lon, max_lat])` | `bbox` |
| `with_sampling(SamplingPattern)` | `sampling` (see [Super-Sampling](../super-sampling.md)) |
| `with_band(n)` | `band` |
| `with_spectral_formula(SpectralFormula)` | `spectral_formula` (NDVI, NDWI, NBR, EVI) |
| `with_quantiles(Vec<QuantileTarget>)` | `quantiles`; adds one nullable `Float64` column per target |
| `with_min_count`, `with_min_mean`, `with_max_mean` | value filters |
| `with_compact(bool)` | parent compaction |
| `with_overlap_rule(OverlapRule)` | mosaic overlap rule (`Cutline`, `First`, `Average`) |
| `with_batch_size(n)` | rows per `RecordBatch` (default 2048) |
| `with_strict_transforms(b)` | `strict_transforms`: fail the scan instead of skipping samples whose coordinate transform fails |
| `with_transform_failure_counter(Arc<AtomicU64>)` | `transform_failure_counter`: receives the number of skipped samples |

### `CategoricalRasterTableProvider`

Schema: `categorical_wide()` (default) or `categorical_long()`.

| Builder | Effect |
| :--- | :--- |
| `with_format(CategoricalFormat::Wide \| Long)` | Output layout |
| `with_crs`, `with_nodata`, `with_bbox`, `with_batch_size`, `with_strict_transforms`, `with_transform_failure_counter` | As above |
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
| `H3ToWkbUdf` | `h3_to_wkb(UInt64 \| Utf8) -> Binary` ([cell geometry](../sql-api-reference.md#cell-geometry)) | `h3_to_geometry`, `h3_cell_to_geometry` |
| `H3CellToParentUdf` | `h3_cell_to_parent(UInt64, Int64) -> UInt64`, `(Utf8, Int64) -> Utf8` | |

```rust
use raster_h3_datafusion::{ColumnArray, RasterH3SessionCatalog};

let ctx = RasterH3SessionCatalog::new();
let cells = ColumnArray::UInt64 { values: vec![0x8828308281fffff], validity: None };
let hex = ctx.invoke_scalar("h3_to_string", &[cells])?;
```

## Recipes

These mirror the [DuckDB SQL recipes](duckdb.md#sql-usage--recipes) as Rust builder calls.
The option types (`SamplingPattern`, `SpectralFormula`, `QuantileTarget`, `OverlapRule`,
`CategoryRemapper`) live in `raster-h3-core`, so add it as a dependency next to
`raster-h3-datafusion`. The snippets assume a function returning
`raster_h3_core::error::Result<()>`.

```rust
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{QuantileTarget, SpectralFormula};
use raster_h3_core::aggregator::remap::CategoryRemapper;
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::raster::mosaic::OverlapRule;
use raster_h3_datafusion::{
    CategoricalFormat, CategoricalRasterTableProvider, ColumnArray, ContinuousRasterTableProvider,
    FilterPredicate, RasterH3SessionCatalog, TableProvider,
};
```

### Basic continuous aggregation

```rust
let provider = ContinuousRasterTableProvider::try_new("elevation.tif", vec![8])?;
let plan = provider.scan(None, &[], None)?;
for batch in plan.execute()? {
    let batch = batch?;
    if let Some(ColumnArray::Float64 { values, .. }) = batch.column_by_name("mean") {
        println!("{} cells, first mean {:?}", batch.num_rows, values.first());
    }
}
```

### CRS, NoData, bounding box and sampling

```rust
let provider = ContinuousRasterTableProvider::try_new("temperature_global.tif", vec![9])?
    .with_crs("EPSG:4326")
    .with_nodata(-9999.0)
    .with_bbox([-122.50, 37.70, -122.35, 37.85])
    .with_sampling(SamplingPattern::parse("rgss"));
```

### Multiple resolutions and quantiles in one pass

```rust
let provider = ContinuousRasterTableProvider::try_new("elevation.tif", vec![7, 8, 9])?
    .with_quantiles(QuantileTarget::parse_list("p50,p95,iqr")?); // adds p50, p95, iqr columns

// Push a resolution filter down: only resolution 8 is aggregated
let res8 = provider.scan(None, &[FilterPredicate::Resolution(8)], None)?;
```

### Value filters and pushdown

```rust
let filters = [FilterPredicate::MinCount(10.0), FilterPredicate::MinMean(500.0)];
let status = provider.supports_filters_pushdown(&filters); // [Exact, Exact]
let schema = provider.schema();
let projection = [schema.index_of("h3_index").unwrap(), schema.index_of("mean").unwrap()];
let plan = provider.scan(Some(&projection), &filters, Some(1000))?;
```

### Spectral indices and band selection

```rust
let ndvi = ContinuousRasterTableProvider::try_new("sentinel2_l2a.tif", vec![9])?
    .with_spectral_formula(SpectralFormula::Ndvi { nir_band: 8, red_band: 4 });

let band3 = ContinuousRasterTableProvider::try_new("multiband.tif", vec![8])?.with_band(3);
```

### Mosaics and remote COGs

```rust
let mosaic = ContinuousRasterTableProvider::try_new("tiles/tile_*.tif", vec![8])?
    .with_overlap_rule(OverlapRule::First);
let remote = ContinuousRasterTableProvider::try_new("https://example.com/rasters/cog_elevation.tif", vec![8])?;
```

### Categorical aggregation and remapping

```rust
// Wide (default): majority class, fractions, histogram JSON
let wide = CategoricalRasterTableProvider::try_new("worldcover_2021.tif", vec![8])?
    .with_min_majority_fraction(0.5);

// Long: one row per (cell, class), with classes reclassified during ingestion
let long = CategoricalRasterTableProvider::try_new("nlcd_landcover_2021.tif", vec![8])?
    .with_format(CategoricalFormat::Long)
    .with_remapper(Arc::new(CategoryRemapper::parse("41..43: 1, 21..24: 2, else: null")?));
```

### Scalar UDFs over a batch column

```rust
let ctx = RasterH3SessionCatalog::new();
let plan = provider.scan(Some(&[0]), &[], Some(10))?; // h3_index only
for batch in plan.execute()? {
    let cells = batch?.columns.remove(0);
    let parents = ctx.invoke_scalar("h3_cell_to_parent", &[
        cells.clone(),
        ColumnArray::Int64 { values: vec![6; cells.len()], validity: None },
    ])?;
    let lat = ctx.invoke_scalar("h3_to_lat", &[cells])?;
}
```

### Registering tables in the catalog

```rust
let mut ctx = RasterH3SessionCatalog::new();
ctx.register_table("dem", Arc::new(ContinuousRasterTableProvider::try_new("dem.tif", vec![8])?));
let dem = ctx.table("dem").expect("registered");
let plan = dem.scan(None, &[], None)?;
```

### Exports

The DataFusion crate has no file writers. For Parquet/GeoParquet or PMTiles, use the core
exporters (`H3ParquetWriter`, `H3PmtilesTiler`, see [Core](core.md#file-exporters)) with the
same `MultiResolutionConfig`, or write the Arrow stream with any Arrow-based writer.

## Using with Apache DataFusion and SedonaDB

[`crates/raster-h3-sedona`](../../crates/raster-h3-sedona) is the real DataFusion adapter.
It depends on `datafusion`/`arrow` at the versions [SedonaDB](https://github.com/apache/sedona-db)
pins (currently DataFusion `=54.1.0`, arrow `58`), so the same objects register into a plain
`SessionContext` or into `SedonaContext::ctx`:

```rust
let ctx = sedona::context::SedonaContext::new_local_interactive().await?;
raster_h3_sedona::register_all(&ctx.ctx); // or a plain datafusion SessionContext

ctx.sql(
    "SELECT h3_index, mean FROM h3_raster_continuous_aggregate('dem.tif', 8)
     WHERE count >= 4
       AND ST_Intersects(wkb, ST_SetSRID(ST_GeomFromWKT('POINT (-122.42 37.77)'), 4326))",
).await?.show().await?;
```

| Piece | What it does |
| :--- | :--- |
| `RasterH3Table` | `datafusion::catalog::TableProvider` over a configured `Continuous`/`CategoricalRasterTableProvider`. Aggregation runs on DataFusion's blocking pool and streams through the Arrow C Stream into a `StreamingTableExec` without copying. Projection (including empty, for `COUNT(*)`) and `LIMIT` are pushed down. |
| Filter pushdown | `count`/`total_count >= x`, `mean >= x`, `mean <= x`, `mean BETWEEN a AND b`, `majority_fraction >= x` and `resolution = n` are `Exact`; the strict forms (`>`, `<`) push the inclusive bound as `Inexact` and DataFusion re-applies them. Anything else is `Unsupported`. |
| Table functions | `h3_raster_continuous_aggregate(source [, resolution ...] [, crs])` and `h3_raster_categorical_aggregate(...)` (wide format). Use `RasterH3Table` with a configured provider for other options. |
| Scalar UDFs | All eight H3 functions and their aliases, with SQL integer/string literals coerced to the core types. |
| Geometry | The `wkb` column and `h3_to_wkb` results are `geoarrow.wkb` fields with CRS `OGC:CRS84` (planar edges, as in the GeoParquet export), so Sedona's `ST_*` functions accept them directly. |

The crate is a standalone Cargo workspace (listed in the root `exclude`), so DataFusion
never enters the root `Cargo.lock` or the `--workspace` CI jobs. Not yet covered: `bbox`
pushdown from `ST_Intersects(wkb, <constant>)`, and registering from SedonaDB's Python API
(which takes a `sedonadb_table_provider` capsule; `sedona_extension::ExportedTableProvider`
can wrap `RasterH3Table` for that).

## Testing

```bash
cargo test -p raster-h3-datafusion
# DataFusion / SedonaDB adapter (standalone workspace)
cargo test --manifest-path crates/raster-h3-sedona/Cargo.toml
cargo test --manifest-path crates/raster-h3-sedona/Cargo.toml --features sedona-e2e  # inside a real SedonaContext
```

Unit tests cover projection, limit, filter narrowing (resolution and bbox), the Arrow C
stream round trip and every UDF. `tests/it/multi_engine_e2e.rs` checks the providers
against the other engines.
