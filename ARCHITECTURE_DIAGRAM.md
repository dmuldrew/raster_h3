# raster_h3 Architecture Diagram

## High-Level System Architecture

```mermaid
graph TB
    subgraph "User Layer"
        SQL[SQL Query]
        CLI[CLI Tools]
    end

    subgraph "DuckDB Extension Layer"
        Entry[raster_h3_init<br/>Extension Entry Point]
        TableFunc[Table Functions<br/>h3_raster_continuous_aggregate<br/>h3_raster_categorical_aggregate<br/>h3_raster_to_pmtiles<br/>h3_raster_to_parquet]
        ScalarFunc[Scalar Functions<br/>h3_to_string, h3_to_lat, etc.]
    end

    subgraph "FFI Layer"
        FFI[DuckDB C-FFI Bindings<br/>duckdb_c.rs, spatial_detect.rs]
    end

    subgraph "Processing Pipeline"
        Raster[GeoTIFF I/O<br/>Local & Cloud Streaming]
        CRS[CRS Transform<br/>WGS84, Web Mercator, Albers, Proj4]
        Aggregator[Multi-Resolution<br/>Horizon Streaming Engine]
        Export[Export Formats<br/>Parquet, PMTiles v3]
    end

    subgraph "Data Sources"
        Local[Local GeoTIFF]
        Cloud[Cloud COG<br/>HTTP/S3]
        Mosaic[Mosaic<br/>Multi-file]
    end

    subgraph "Outputs"
        DuckDBTable[DuckDB Table]
        ParquetFile[GeoParquet 1.1]
        PMTilesFile[PMTiles v3<br/>Vector Tiles]
    end

    SQL --> Entry
    CLI --> Entry
    Entry --> TableFunc
    Entry --> ScalarFunc
    TableFunc --> FFI
    ScalarFunc --> FFI
    FFI --> Raster
    FFI --> Aggregator
    FFI --> Export

    Local --> Raster
    Cloud --> Raster
    Mosaic --> Raster

    Raster --> CRS
    CRS --> Aggregator
    Aggregator --> Export
    Aggregator --> DuckDBTable
    Export --> ParquetFile
    Export --> PMTilesFile

    style Entry fill:#e1f5ff
    style Aggregator fill:#fff4e1
    style Export fill:#e1ffe1
```

## Detailed Module Architecture

```mermaid
graph TB
    subgraph "src/lib.rs - Extension Entry"
        Init[raster_h3_init<br/>Register Functions]
        CAPI[raster_h3_init_c_api<br/>v1.2+ C API]
        Version[raster_h3_version<br/>v0.0.1]
    end

    subgraph "src/functions/ - SQL Interface"
        TF[table_function.rs<br/>Continuous Aggregate]
        CTF[categorical_table_function.rs<br/>Categorical Aggregate]
        PTF[pmtiles_table_function.rs<br/>PMTiles Export]
        PQT[parquet_table_function.rs<br/>Parquet Export]
        SF[scalar.rs<br/>Helper Functions]
        BU[bind_utils/<br/>Parameter Binding]
    end

    subgraph "src/ffi/ - DuckDB Integration"
        DC[duckdb_c.rs<br/>C API Declarations]
        SD[spatial_detect.rs<br/>Version Detection]
    end

    subgraph "src/raster/ - GeoTIFF I/O"
        GT[geotiff.rs<br/>Streaming Reader]
        GTX[geotransform.rs<br/>Affine Transforms]
        MD[metadata.rs<br/>GeoKey Extraction]
        PR[predictor.rs<br/>TIFF Prediction]
        MOS[mosaic.rs<br/>Multi-file Mosaic]
        PF[prefetch.rs<br/>Local Prefetch Queue]
        RPF[remote_prefetch.rs<br/>Remote COG Prefetch]
        HR[http_range.rs<br/>HTTP Range Requests]
    end

    subgraph "src/crs/ - Projections"
        CRSMod[mod.rs<br/>CRS Detection]
        TRANS[transformer.rs<br/>3-Tier Pipeline<br/>Wgs84Identity<br/>WebMercatorFast<br/>AlbersConicFast<br/>Proj4 Fallback]
    end

    subgraph "src/aggregator/ - Statistical Engine"
        ACC[accumulator.rs<br/>Welford Mean/Variance]
        CAT[categorical.rs<br/>Class Histograms]
        QUANT[quantiles.rs<br/>DDSketch Quantiles]
        NOD[nodata.rs<br/>NoData Handling]
        REM[remap.rs<br/>Category Remapping]
        SAMP[sampling.rs<br/>Sub-pixel Patterns]
        SIMD[simd.rs<br/>Vectorized Spans]
        HS[h3_scanline.rs<br/>H3 Indexing]
        HST[horizon_streamer.rs<br/>Eviction Queue]
    end

    subgraph "src/aggregator/multi_horizon/ - Multi-Resolution Engine"
        CTRL[controller.rs<br/>Central Orchestrator]
        CFG[config.rs<br/>Query Configuration]
        LIFE[lifecycle.rs<br/>State Machine]
        SHARD[sharded_map.rs<br/>32-way Lock-free Maps]
        COMP[compaction.rs<br/>7→1 Hierarchical]
        CONT[continuous.rs<br/>Continuous Processing]
        CSTR[continuous_streamer.rs<br/>Continuous Streamer]
        CATM[categorical.rs<br/>Categorical Processing]
        CSTRM[categorical_streamer.rs<br/>Categorical Streamer]
        SPEC[spectral.rs<br/>NDVI/NDWI/NBR/EVI]
        WALK[walker.rs<br/>Pixel-by-Pixel Loop]
        COORD[coordinates.rs<br/>Coordinate Transforms]
        SPILL[spill.rs<br/>Disk Spilling]
    end

    subgraph "src/parquet/ - Parquet Export"
        PMOD[mod.rs]
        GM[geoparquet_metadata.rs<br/>OGC GeoParquet 1.1]
        PIPE[pipeline.rs<br/>Streaming Pipeline]
        WR[writer.rs<br/>Row Group Buffers]
    end

    subgraph "src/pmtiles/ - PMTiles Export"
        PMOD[mod.rs]
        PW[writer.rs<br/>PMTiles v3 Writer]
        MVT[mvt.rs<br/>MVT Encoding]
        PYR[pyramid.rs<br/>Tile Pyramid Math]
        FEAT[features.rs<br/>Feature Definitions]
        TIL[tiler.rs<br/>Multi-zoom Tiler]
    end

    subgraph "src/transcode/ - Format Conversion"
        TMOD[mod.rs]
        PT[parquet_tiler.rs<br/>Parquet→PMTiles]
    end

    subgraph "src/encoding/ - Serialization"
        EMOD[mod.rs]
        FH[fast_hex.rs<br/>Hex Formatting]
        WKB[wkb.rs<br/>WKB Geometry]
    end

    subgraph "src/error.rs"
        ERR[RasterH3Error<br/>Unified Error Types]
    end

    Init --> TF
    Init --> CTF
    Init --> PTF
    Init --> PQT
    Init --> SF

    TF --> BU
    CTF --> BU
    PTF --> BU
    PQT --> BU
    SF --> BU

    BU --> DC
    BU --> SD

    TF --> GT
    CTF --> GT
    PTF --> GT
    PQT --> GT

    GT --> GTX
    GT --> MD
    GT --> PR
    GT --> MOS
    GT --> PF
    GT --> RPF
    GT --> HR

    RPF --> HR

    MOS --> PF

    TF --> CRSMod
    CTF --> CRSMod
    PTF --> CRSMod
    PQT --> CRSMod

    CRSMod --> TRANS

    TF --> ACC
    TF --> CAT
    TF --> QUANT
    TF --> NOD
    TF --> REM
    TF --> SAMP
    TF --> SIMD
    TF --> HS
    TF --> HST

    CTF --> CAT
    CTF --> NOD
    CTF --> REM
    CTF --> SAMP
    CTF --> HST

    TF --> CTRL
    CTF --> CTRL
    PTF --> CTRL
    PQT --> CTRL

    CTRL --> CFG
    CTRL --> LIFE
    CTRL --> SHARD
    CTRL --> COMP
    CTRL --> SPILL
    CTRL --> CONT
    CTRL --> CSTR
    CTRL --> CATM
    CTRL --> CSTRM
    CTRL --> SPEC
    CTRL --> WALK
    CTRL --> COORD

    CONT --> WALK
    CSTR --> WALK
    CATM --> WALK
    CSTRM --> WALK

    WALK --> COORD
    WALK --> ACC
    WALK --> CAT
    WALK --> SAMP
    WALK --> SIMD

    PTF --> PW
    PTF --> MVT
    PTF --> PYR
    PTF --> FEAT
    PTF --> TIL

    PQT --> GM
    PQT --> PIPE
    PQT --> WR

    TIL --> PT

    TF --> FH
    TF --> WKB
    SF --> FH
    SF --> WKB

    style CTRL fill:#fff4e1
    style GT fill:#ffe1f5
    style TRANS fill:#e1f5ff
    style PW fill:#e1ffe1
```

## Data Flow Pipeline

```mermaid
sequenceDiagram
    participant SQL as SQL Query
    participant TF as Table Function
    participant Bind as Bind Utils
    participant Raster as GeoTIFF Reader
    participant CRS as CRS Transform
    participant Agg as Multi-Horizon Aggregator
    participant Export as Export Engine
    participant Output as Output File/Table

    SQL->>TF: CALL h3_raster_continuous_aggregate()
    TF->>Bind: Parse Parameters
    Bind->>Bind: Extract H3 resolutions, bbox, sampling
    Bind->>Raster: Initialize GeoTIFF Reader
    Raster->>Raster: Detect CRS & Metadata
    Raster->>Raster: Setup Prefetch Queue
    Raster->>CRS: Create Coordinate Transformer

    loop Scanline Streaming (North→South)
        Raster->>Raster: Fetch Next Chunk
        Raster->>CRS: Transform Pixel→WGS84
        CRS->>Agg: Submit Chunk to Workers
        Agg->>Agg: Rayon Parallel Processing
        Agg->>Agg: H3 Index per Sample
        Agg->>Agg: Accumulate Statistics
        Agg->>Agg: Horizon Eviction Check
        alt Cell Complete
            Agg->>Output: Emit Record
        end
    end

    alt Memory Budget Exceeded
        Agg->>Agg: Spill to Disk
        Agg->>Agg: Merge at EOF
    end

    alt Multi-Resolution
        Agg->>Agg: Hierarchical Compaction (7→1)
        Agg->>Output: Emit Parent Cells
    end

    alt PMTiles Export
        Export->>Export: MVT Encoding
        Export->>Export: Tile Pyramid Generation
        Export->>Output: Write .pmtiles
    else Parquet Export
        Export->>Export: GeoParquet Metadata
        Export->>Output: Write .parquet
    else DuckDB Table
        Agg->>Output: Return to SQL
    end

    Output->>SQL: Query Results
```

## Multi-Resolution Horizon Streaming Engine

```mermaid
graph TB
    subgraph "Input Layer"
        Chunks[Prefetched Chunks<br/>Remote/Local]
    end

    subgraph "Controller"
        MC[MultiHorizonStreamer<br/>Central Orchestrator]
    end

    subgraph "Worker Pool"
        Pool[Rayon ThreadPool<br/>Hardware-Saturating]
    end

    subgraph "Per-Worker State"
        Maps[ShardedResolutionMap<br/>32-way Lock-free]
        Walk[WalkContext<br/>Pixel-by-Pixel Loop]
    end

    subgraph "Processing Pipeline"
        Coord[CoordinateTransformer<br/>Affine + CRS]
        Sample[SamplingPattern<br/>Sub-pixel Offsets]
        H3Idx[H3 Indexing<br/>Exact Cell Lookup]
        Accum[Accumulators<br/>Welford/Histogram]
    end

    subgraph "Memory Management"
        Budget[Aggregation Budget<br/>64 MiB Default]
        Spill[Spill Runs<br/>Disk Overflow]
        Merge[Sorted Merge<br/>EOF Consolidation]
    end

    subgraph "Eviction & Output"
        Horizon[Horizon Queue<br/>South-Lat Priority]
        Compact[Hierarchical Compactor<br/>7→1 Parent Merge]
        Output[Output Buffer<br/>Record Queue]
    end

    subgraph "Output Layer"
        DuckDB[DuckDB Table]
        Parquet[Parquet Writer]
        PMTiles[PMTiles Tiler]
    end

    Chunks --> MC
    MC --> Pool
    Pool --> Maps
    Pool --> Walk

    Walk --> Coord
    Walk --> Sample
    Coord --> H3Idx
    Sample --> H3Idx
    H3Idx --> Accum
    Accum --> Maps

    Maps --> Budget
    Budget --> Spill
    Spill --> Merge
    Merge --> Output

    Maps --> Horizon
    Horizon --> Compact
    Compact --> Output

    Output --> DuckDB
    Output --> Parquet
    Output --> PMTiles

    style MC fill:#fff4e1
    style Horizon fill:#ffe1e1
    style Budget fill:#e1ffe1
```

## CRS Transformation Three-Tier Pipeline

```mermaid
graph LR
    subgraph "Tier 1: Zero-Cost"
        Identity[Wgs84Identity<br/>EPSG:4326/4269<br/>No Transform]
    end

    subgraph "Tier 2: Closed-Form"
        WM[WebMercatorFast<br/>EPSG:3857<br/>Analytical Transform]
        Albers[AlbersConicFast<br/>Certified Albers<br/>Newton-Raphson Inverse]
    end

    subgraph "Tier 3: General-Purpose"
        Proj4[Proj4<br/>proj4rs Library<br/>All Other Projections]
    end

    Input[Pixel Coordinates] --> Identity
    Input --> WM
    Input --> Albers
    Input --> Proj4

    Identity --> Output[WGS84 Lat/Lon]
    WM --> Output
    Albers --> Output
    Proj4 --> Output

    style Identity fill:#e1ffe1
    style WM fill:#fff4e1
    style Albers fill:#fff4e1
    style Proj4 fill:#ffe1e1
```

## Scanline Horizon Eviction

```mermaid
graph TB
    subgraph "Scanline Progress"
        Row[Current Scanline<br/>North→South]
    end

    subgraph "Active Cells"
        Cell1[Cell A<br/>South: 37.75°]
        Cell2[Cell B<br/>South: 37.70°]
        Cell3[Cell C<br/>South: 37.65°]
    end

    subgraph "Horizon Queue"
        Q1[Cell A<br/>Priority: 37.75°]
        Q2[Cell B<br/>Priority: 37.70°]
        Q3[Cell C<br/>Priority: 37.65°]
    end

    subgraph "Eviction Logic"
        Check[Check: Row > Cell South?]
        Evict[Evict & Emit]
        Retain[Retain in Map]
    end

    subgraph "Output"
        Emitted[Finalized Cells<br/>Streamed to Results]
    end

    Row --> Check
    Check -->|Yes| Evict
    Check -->|No| Retain

    Cell1 --> Q1
    Cell2 --> Q2
    Cell3 --> Q3

    Q1 --> Check
    Q2 --> Check
    Q3 --> Check

    Evict --> Emitted
    Retain --> Cell1
    Retain --> Cell2
    Retain --> Cell3

    style Evict fill:#ffe1e1
    style Emitted fill:#e1ffe1
```

## PMTiles Multi-Resolution Pyramid

```mermaid
graph TB
    subgraph "Input"
        H3Cells[H3 Aggregated Cells<br/>Multiple Resolutions]
    end

    subgraph "Zoom Mapping"
        Map[h3_res_to_zoom<br/>Resolution→Zoom Level]
    end

    subgraph "Tile Generation"
        Tiler[Multi-zoom Tiler<br/>Rayon Parallel]
    end

    subgraph "MVT Encoding"
        MVT[MVT Encoder<br/>Protobuf]
    end

    subgraph "PMTiles Archive"
        Header[127-byte Header]
        Directory[Hilbert-Indexed<br/>Tile Directory]
        Tiles[Compressed Tiles<br/>Gzip/Zlib]
    end

    subgraph "Output"
        File[.pmtiles File<br/>Single-File Archive]
    end

    H3Cells --> Map
    Map --> Tiler
    Tiler --> MVT
    MVT --> Tiles
    Tiles --> Directory
    Directory --> Header
    Header --> File

    style Tiler fill:#fff4e1
    style MVT fill:#ffe1f5
    style File fill:#e1ffe1
```

## Key Design Patterns

### 1. Streaming Scanline Architecture
- **Moving Scanner Front**: Processes raster row-by-row from North to South
- **Immediate Eviction**: Cells are finalized when scanline passes their southern boundary
- **Budgeted Aggregation**: Default 64 MiB accumulator budget with disk spilling
- **Constant Memory**: Only active accumulators in memory at any time

### 2. Multi-Resolution Streaming
- **Single Pass**: Aggregates all requested H3 resolutions in one raster scan
- **Hierarchical Compaction**: 7 child cells merge into 1 parent cell during eviction
- **Horizon-Based Eviction**: Prioritized by southernmost latitude per resolution
- **32-Way Sharding**: Lock-free hash maps eliminate thread contention

### 3. Three-Tier CRS Pipeline
- **Tier 1**: Zero-cost passthrough for WGS84 (EPSG:4326/4269)
- **Tier 2**: Closed-form analytical transforms for Web Mercator and certified Albers
- **Tier 3**: General-purpose proj4rs fallback for all other projections

### 4. Cloud-Native I/O
- **HTTP Range Prefetching**: Coalesces chunk ranges to cut round-trips 2×–5×
- **Mosaic Support**: Multi-file ingestion with overlap resolution
- **Memory-Mapped Files**: Zero-copy local file access via memmap2

### 5. Export Formats
- **PMTiles v3**: Single-file vector tile pyramids with Hilbert curve indexing
- **GeoParquet 1.1**: OGC-compliant with PROJJSON metadata
- **Zero Intermediate Files**: Direct transcoding from raster to final format

## Performance Optimizations

1. **SIMD Vectorization**: Span accumulation across native data types
2. **Row-Constant Hoisting**: CRS transform computed once per row, not per pixel
3. **Exact Per-Sample Indexing**: No coordinate drift accumulation
4. **Sub-Pixel Super-Sampling**: RGSS, Hex, Gaussian patterns for accuracy
5. **Work-Stealing Parallelism**: Rayon thread pool saturates all CPU cores
6. **Lock-Free Sharding**: 32-way partitioned maps eliminate contention
7. **Cache-Local Hot Path**: Tightly coupled walker/coordinates/accumulator loop

## Module Interdependencies

**Critical Path** (Hot Loop):
```
walker.rs → coordinates.rs → h3_scanline.rs → accumulator.rs → sharded_map.rs
```

**Supporting Path**:
```
geotiff.rs → prefetch.rs → metadata.rs → crs/transformer.rs → aggregator/multi_horizon/controller.rs
```

**Export Path**:
```
controller.rs → pmtiles/tiler.rs → pmtiles/mvt.rs → pmtiles/writer.rs
controller.rs → parquet/pipeline.rs → parquet/writer.rs
```

## Error Handling

All errors are unified in `src/error.rs`:
- `RasterH3Error` enum covers all recoverable error types
- I/O, TIFF decoding, metadata parsing, CRS detection, PROJ4, H3, invalid parameters
- DuckDB C-FFI errors, streaming failures
- Complete panic containment across all `extern "C"` boundaries
