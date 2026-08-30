#!/usr/bin/env bash
set -e

# Case 1: Pipe input via stdin (e.g. cat query.sql | docker run -i ...)
if [ ! -t 0 ] && [ "$#" -eq 0 ]; then
    { echo "LOAD '/extensions/raster_h3.duckdb_extension';"; cat; } | exec duckdb -unsigned
fi

# Case 2: No arguments in interactive terminal
if [ "$#" -eq 0 ]; then
    echo "================================================================="
    echo "  Raster H3 Hexification - DuckDB Extension Container"
    echo "================================================================="
    echo "Extension pre-loaded at: /extensions/raster_h3.duckdb_extension"
    echo "Sample GeoTIFF at:       /data/sample_sf.tif"
    echo ""
    echo "Running demonstration query on sample GeoTIFF..."
    echo "-----------------------------------------------------------------"
    duckdb -unsigned < /app/sql/demo.sql
    echo "-----------------------------------------------------------------"
    echo "Starting interactive DuckDB session..."
    echo "Example queries:"
    echo "  SELECT * FROM h3_raster_continuous_aggregate('/data/sample_sf.tif', resolution := 8);"
    echo "  SELECT * FROM h3_raster_to_pmtiles('/data/sample_sf.tif', '/data/out.pmtiles', min_resolution := 7, max_resolution := 9);"
    echo "================================================================="
    exec duckdb -unsigned -init <(echo "LOAD '/extensions/raster_h3.duckdb_extension';")
fi

# Case 3: Passed -c "SQL" or --query "SQL"
if [ "$1" = "-c" ] || [ "$1" = "--query" ]; then
    exec duckdb -unsigned -c "LOAD '/extensions/raster_h3.duckdb_extension'; $2"
fi

# Case 4: Passed a direct SQL string (e.g. docker run ... "SELECT * FROM ...")
FIRST_WORD=$(echo "$1" | awk '{print toupper($1)}')
case "$FIRST_WORD" in
    SELECT*|WITH*|COPY*|CREATE*|EXPLAIN*|DESCRIBE*|SHOW*|LOAD*|PRAGMA*)
        exec duckdb -unsigned -c "LOAD '/extensions/raster_h3.duckdb_extension'; $1"
        ;;
esac

# Case 5: Passed a .sql file (e.g. docker run ... my_script.sql)
if [[ "$1" == *.sql ]] && [ -f "$1" ]; then
    { echo "LOAD '/extensions/raster_h3.duckdb_extension';"; cat "$1"; } | exec duckdb -unsigned
fi

# Case 6: Passed arbitrary command or binary (e.g. duckdb, raster_to_pmtiles, parquet_to_pmtiles, bash)
exec "$@"
