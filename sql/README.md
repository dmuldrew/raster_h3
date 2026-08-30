# DuckDB SQL Examples & Benchmark Queries

This folder contains ready-to-run SQL query scripts for the `raster_h3` DuckDB extension:

- **`demo.sql`**: Complete feature demonstration script showing:
  - Continuous pixel aggregation (`h3_raster_continuous_aggregate`)
  - Categorical dominant class & histogram aggregation (`h3_raster_categorical_aggregate`)
  - Sub-pixel super-sampling (RGSS, Hex lattice)
  - Region of Interest (ROI) bounding box filtering
  - Exporting H3 aggregations directly to Parquet and PMTiles v3
  - H3 coordinate helpers (`h3_to_lat`, `h3_to_lng`, `string_to_h3`, `h3_to_string`, `h3_is_valid`)
- **`e2e_performance.sql`**: End-to-end benchmark script measuring execution time, throughput, and memory scaling across multiple H3 resolutions.
