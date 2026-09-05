-- DuckDB Raster-to-H3 Extension Demo Script
-- 1. Load the compiled extension
LOAD '/extensions/raster_h3.duckdb_extension';

-- 2. Preview aggregated H3 cells from GeoTIFF at H3 Resolution 8 with StdDev (Continuous)
SELECT
    h3_hex,
    round(mean, 2) AS avg_elevation_m,
    round(stddev, 2) AS elevation_stddev_m,
    count AS pixel_count,
    round(min, 2) AS min_elevation,
    round(max, 2) AS max_elevation
FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 8, band := 1)
ORDER BY pixel_count DESC
LIMIT 10;

-- 3. Calculate spatial centroid of each H3 cell using helper functions
SELECT
    h3_to_string(h3_index) AS h3_cell,
    round(h3_to_lat(h3_index), 4) AS centroid_lat,
    round(h3_to_lng(h3_index), 4) AS centroid_lng,
    h3_get_resolution(h3_index) AS h3_res,
    round(mean, 2) AS mean_value,
    count
FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 9)
LIMIT 5;

-- 4. Fast Region of Interest (ROI) bounding box pruning
SELECT
    h3_hex,
    round(mean, 2) AS mean_val,
    count AS pixels
FROM h3_raster_continuous_aggregate(
    '/data/sample_sf.tif',
    resolution := 9,
    min_lon := -122.45,
    min_lat := 37.75,
    max_lon := -122.40,
    max_lat := 37.79
);

-- 5. Bidirectional Hex String to H3 Integer Conversion
SELECT
    string_to_h3('8828308281fffff') AS h3_int,
    h3_to_string(string_to_h3('8828308281fffff')) AS roundtrip_hex,
    h3_get_resolution(string_to_h3('8828308281fffff')) AS res;

-- 6. Query Plan & Cardinality Estimation
EXPLAIN SELECT count(*) FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 8);

-- 7. Anti-Aliased Sub-Pixel Super-Sampling (Rotated Grid Super-Sampling RGSS)
SELECT
    h3_hex,
    round(mean, 2) AS mean_elev,
    round(count, 2) AS weighted_pixel_count,
    min,
    max
FROM h3_raster_continuous_aggregate(
    '/data/sample_sf.tif',
    resolution := 9,
    sampling := 'rgss'  -- Presets: 'center', 'rgss', 'hex', 'gaussian', '5point', '8rooks', '9point', '16point'
)
LIMIT 10;

-- 8. Export H3 Aggregations Directly to Parquet
COPY (
    SELECT
        h3_index,
        h3_hex,
        mean,
        count,
        min,
        max,
        h3_to_lat(h3_index) AS centroid_lat,
        h3_to_lng(h3_index) AS centroid_lng
    FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 8, sampling := 'rgss')
) TO '/data/sf_elevation_h3.parquet' (FORMAT PARQUET);

-- 9. Categorical Raster Aggregation: Dominant / Majority Class & Histogram (Wide Format)
SELECT
    h3_hex,
    majority_class,
    round(majority_fraction * 100, 1) AS dominance_pct,
    majority_count,
    unique_classes,
    total_count,
    histogram
FROM h3_raster_categorical_aggregate('/data/sample_sf.tif', resolution := 8)
LIMIT 10;

-- 10. Categorical Raster Aggregation: Normalized Long Form
SELECT
    h3_hex,
    category,
    count AS category_pixels,
    round(fraction * 100, 2) AS pct_coverage,
    total_count AS hex_total_pixels
FROM h3_raster_categorical_aggregate('/data/sample_sf.tif', resolution := 8, format := 'long')
WHERE fraction >= 0.10
ORDER BY h3_hex, fraction DESC
LIMIT 15;

-- 11. Anti-Aliased Sub-Pixel Categorical Aggregation (Hexagonal 7-Point Lattice)
SELECT
    h3_hex,
    majority_class,
    round(majority_fraction * 100, 1) AS majority_pct,
    round(total_count, 2) AS weighted_total_pixels,
    histogram
FROM h3_raster_categorical_aggregate(
    '/data/sample_sf.tif',
    resolution := 9,
    sampling := 'hex'
)
LIMIT 10;

-- 12. Direct Single-Pass PMTiles v3 Vector Pyramid Export
SELECT * FROM h3_raster_to_pmtiles(
    '/data/sample_sf.tif',
    '/data/sample_sf.pmtiles',
    min_resolution := 7,
    max_resolution := 9,
    sampling := 'center'
);

-- 13. Strict H3 Mathematical Validation Function
SELECT
    h3_is_valid('8828308281fffff') AS valid_sf_hex,
    h3_is_valid(596823908204938239::UBIGINT) AS valid_sf_u64,
    h3_is_valid('corrupted_string') AS invalid_hex,
    h3_is_valid(0::UBIGINT) AS invalid_u64;

-- 14. Convert Existing H3 Parquet Dataset Directly to PMTiles v3
SELECT * FROM h3_parquet_to_pmtiles(
    '/data/sf_elevation_h3.parquet',
    '/data/sf_from_parquet.pmtiles',
    h3_column := 'h3_index'
);

-- 15. Multi-Resolution Single-Pass Aggregation (Continuous & Categorical)
-- Query multiple H3 resolutions (e.g. Res 7 and 8) in a single streaming pass over the GeoTIFF
SELECT
    resolution,
    count(*) AS cell_count,
    round(avg(mean), 2) AS avg_mean_val
FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolutions := '7,8')
GROUP BY resolution
ORDER BY resolution;

-- Range syntax: min_resolution and max_resolution
SELECT
    resolution,
    majority_class,
    count(*) AS hex_count
FROM h3_raster_categorical_aggregate(
    '/data/sample_sf.tif',
    min_resolution := 7,
    max_resolution := 8
)
GROUP BY resolution, majority_class
ORDER BY resolution, hex_count DESC
LIMIT 10;

-- 16. Landscape Diversity & Shannon Entropy Metrics
-- Compute Shannon-Wiener entropy index (-sum(p_i * ln(p_i))) and distinct landcover classes
SELECT
    h3_hex,
    majority_class,
    distinct_classes,
    round(shannon_entropy, 3) AS entropy,
    total_count AS total_pixels
FROM h3_raster_categorical_aggregate(
    '/data/sample_sf.tif',
    resolution := 8
)
WHERE distinct_classes > 1
ORDER BY shannon_entropy DESC
LIMIT 10;

-- 17. Selective Property Emission (MVT Projection Pushdown)
-- Export only requested properties (e.g. 'mean,count' or 'majority,entropy') into vector tiles,
-- reducing protobuf encoding overhead and shrinking .pmtiles archive size by 15-30%
SELECT * FROM h3_raster_to_pmtiles(
    '/data/sample_sf.tif',
    '/data/sample_sf_compact.pmtiles',
    min_resolution := 7,
    max_resolution := 9,
    properties := 'mean,count'
);





