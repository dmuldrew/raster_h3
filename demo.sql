-- DuckDB Raster-to-H3 Extension Demo Script
-- 1. Load the compiled extension
LOAD '/extensions/libraster_h3.so';

-- 2. Preview aggregated H3 cells from GeoTIFF at H3 Resolution 8
SELECT
    h3_hex,
    round(mean, 2) AS avg_elevation_m,
    count AS pixel_count,
    round(min, 2) AS min_elevation,
    round(max, 2) AS max_elevation
FROM h3_raster_aggregate('/data/sample_sf.tif', resolution := 8)
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
FROM h3_raster_aggregate('/data/sample_sf.tif', resolution := 9)
LIMIT 5;
