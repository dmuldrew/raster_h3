#!/bin/sh
# Run the Spark tests against the standalone cluster, from the spark-client container:
#   docker compose exec spark-client sh /opt/raster_h3/examples/cluster/run_tests.sh [pytest args]
# The driver runs here; executors run in the spark-worker containers and read the test
# rasters from the shared /shared volume (RASTER_H3_SHARED_DIR).
set -e
# Executors connect back to the driver at this address.
SPARK_LOCAL_IP="$(hostname -i)"
export SPARK_LOCAL_IP
# A standalone application takes every core by default. Cap it, and give each executor a
# whole 2-core worker with half its memory, so the session leaves a worker free for the
# second application that test_demo_script_end_to_end starts (it inherits these settings).
PYSPARK_SUBMIT_ARGS="--conf spark.cores.max=${RASTER_H3_TEST_CORES:-4} --conf spark.executor.cores=2 --conf spark.executor.memory=512m pyspark-shell"
export PYSPARK_SUBMIT_ARGS
exec python3 -m pytest -v -p no:cacheprovider /opt/raster_h3/examples/tests "$@"
