#!/usr/bin/env bash
set -e


if [ "$#" -eq 0 ]; then
    echo "================================================================="
    echo "  Raster H3 Hexification - DuckDB Extension Container"
    echo "================================================================="
    echo "Extension loaded at: /extensions/raster_h3.duckdb_extension"
    echo "Sample GeoTIFF at:   /data/sample_sf.tif"
    echo ""
    echo "Running demonstration query on sample GeoTIFF..."
    echo "-----------------------------------------------------------------"
    duckdb -unsigned < /app/demo.sql
    echo "-----------------------------------------------------------------"
    echo "Starting interactive DuckDB session..."
    echo "Run: SELECT * FROM h3_raster_continuous_aggregate('/data/your_file.tif', 8);"
    echo "     SELECT * FROM h3_raster_categorical_aggregate('/data/landcover.tif', 8);"
    echo "================================================================="
    exec duckdb -unsigned
else
    exec "$@"
fi
