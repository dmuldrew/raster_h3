#!/bin/sh
# Time one partitioned scan at 1..N workers on the local standalone cluster. Run on the host
# from the repository root:
#   sh examples/sedona_spark/cluster/scaling.sh [max_workers] [benchmark args]
# Every worker has 2 CPUs; all of them share this machine, so the curve flattens once the
# workers' CPUs exceed Docker's (see `docker info`) or disk bandwidth runs out.
set -e
MAX=${1:-4}
[ $# -gt 0 ] && shift
for n in $(seq 1 "$MAX"); do
  docker compose --profile spark-cluster up -d --no-build \
    spark-master spark-worker spark-client --scale spark-worker="$n" >/dev/null 2>&1
  # Wait until the master lists n live workers.
  for _ in $(seq 1 60); do
    alive=$(docker compose exec -T spark-master python3 -c \
      "import json, urllib.request; w = json.load(urllib.request.urlopen('http://localhost:8080/json/'))['workers']; print(sum(x['state'] == 'ALIVE' for x in w))" \
      2>/dev/null || echo 0)
    [ "$alive" = "$n" ] && break
    sleep 2
  done
  docker compose exec -T spark-client sh -c \
    'SPARK_LOCAL_IP=$(hostname -i) python3 /opt/raster_h3/examples/cluster/benchmark.py "$@"' sh --workers "$n" "$@"
done
