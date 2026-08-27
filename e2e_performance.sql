-- End-to-End DuckDB Performance & Functional Benchmark
.timer on
SET allow_extensions_metadata_mismatch=true;

-- 1. Load the compiled native DuckDB extension
LOAD '/extensions/raster_h3.duckdb_extension';

-- 2. Verify Scalar Helper Functions
SELECT 
    'Checking Scalar Helpers' AS test_stage,
    h3_to_string(612450371424681983::UBIGINT) AS hex_str,
    round(h3_to_lat(612450371424681983::UBIGINT), 4) AS lat,
    round(h3_to_lng(612450371424681983::UBIGINT), 4) AS lng,
    h3_get_resolution(612450371424681983::UBIGINT) AS res;

-- 3. Continuous Full Scan & In-Engine Aggregation (Single-Pass Welford)
SELECT 
    'Continuous Full Scan' AS benchmark,
    count(*) AS total_h3_cells,
    round(sum(count), 0) AS total_valid_pixels,
    round(avg(mean), 2) AS avg_cell_mean,
    round(min(min), 2) AS global_min,
    round(max(max), 2) AS global_max
FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 8);

-- 4. Multi-Point Sub-Pixel Super-Sampling (RGSS 4-Point)
SELECT 
    'RGSS 4-Point Anti-Aliased Super-Sampling' AS benchmark,
    count(*) AS total_h3_cells,
    round(sum(count), 2) AS total_weighted_pixels,
    round(avg(mean), 2) AS avg_cell_mean
FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 8, sampling := 'rgss');

-- 5. Spatial Bounding Box ROI Pruning
SELECT 
    'Spatial ROI Bounding Box Pruning' AS benchmark,
    count(*) AS pruned_cells,
    round(sum(count), 0) AS pruned_pixels
FROM h3_raster_continuous_aggregate(
    '/data/sample_sf.tif', 
    resolution := 9,
    min_lon := -122.48, 
    min_lat := 37.82, 
    max_lon := -122.42, 
    max_lat := 37.86
);

-- 6. Categorical Landcover Mode & Frequency Breakdown
SELECT 
    'Categorical Mode & Richness' AS benchmark,
    majority_class,
    round(majority_fraction * 100, 1) AS dominance_pct,
    unique_classes,
    total_count,
    histogram
FROM h3_raster_categorical_aggregate('/data/sample_sf.tif', resolution := 8)
ORDER BY total_count DESC
LIMIT 5;

-- 7. Long-Format Normalized Category Breakdown
SELECT 
    'Categorical Long Format' AS benchmark,
    category,
    sum(count) AS total_category_pixels,
    round(avg(fraction) * 100, 2) AS avg_pct_in_cells
FROM h3_raster_categorical_aggregate('/data/sample_sf.tif', resolution := 8, format := 'long')
GROUP BY category
ORDER BY total_category_pixels DESC;

-- 8. In-Database Vector Pipelining to Parquet (Zero-Copy)
COPY (
    SELECT 
        h3_index,
        h3_hex,
        mean,
        stddev,
        count,
        min,
        max
    FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 9)
) TO '/tmp/e2e_h3_export.parquet' (FORMAT PARQUET);

-- Verify written parquet
SELECT 
    'Parquet Verification' AS benchmark,
    count(*) AS rows_in_parquet
FROM parquet_scan('/tmp/e2e_h3_export.parquet');
