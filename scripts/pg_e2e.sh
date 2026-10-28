#!/usr/bin/env bash
# Build the raster-h3-pg loadable module for a PostgreSQL major version inside Docker,
# install it into a running `postgres:<major>` container, and print the container name.
#
# Usage: scripts/pg_e2e.sh <pg-major>          # build + start, prints container name
#        scripts/pg_e2e.sh <pg-major> --stop   # remove the container
#
# Env:   RASTER_H3_PG_RUST_IMAGE  build image (default rust:1-bookworm)
#        RASTER_H3_PG_IMAGE       server image template (default postgres:<major>-bookworm)
#
# Used by tests/it/pg_extension_e2e.rs; can also be run by hand and then queried with
#   docker exec -it raster-h3-pg-e2e-<major> psql -U postgres
set -euo pipefail

major="${1:?usage: pg_e2e.sh <pg-major> [--stop]}"
container="raster-h3-pg-e2e-${major}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "${2:-}" == "--stop" ]]; then
    docker rm -f "$container" >/dev/null 2>&1 || true
    exit 0
fi

rust_image="${RASTER_H3_PG_RUST_IMAGE:-rust:1-bookworm}"
pg_image="${RASTER_H3_PG_IMAGE:-postgres:${major}-bookworm}"
out_dir="$repo/target/pg-e2e/pg${major}"
mkdir -p "$out_dir"

# Build on bookworm so the module's glibc requirement matches the server image. The Linux
# target dir lives under target/ so it is reused across versions; only raster-h3-pg rebuilds
# when RASTER_H3_PG_MAJOR changes.
echo "building raster-h3-pg for PostgreSQL ${major} in ${rust_image}" >&2
docker run --rm \
    -v "$repo:/src" \
    -v raster-h3-pg-e2e-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/src/target/pg-e2e/linux \
    -e RASTER_H3_PG_MAJOR="$major" \
    -w /src \
    "$rust_image" \
    cargo build --locked --release -p raster-h3-pg --features pg-extension >&2
cp "$repo/target/pg-e2e/linux/release/libraster_h3_pg.so" "$out_dir/raster_h3_pg.so"

docker rm -f "$container" >/dev/null 2>&1 || true
docker run -d --name "$container" \
    -e POSTGRES_HOST_AUTH_METHOD=trust \
    "$pg_image" >/dev/null

# The official images expose pg_config, so install exactly where the server looks.
libdir="$(docker exec "$container" pg_config --pkglibdir)"
sharedir="$(docker exec "$container" pg_config --sharedir)"
docker cp "$out_dir/raster_h3_pg.so" "$container:$libdir/raster_h3_pg.so"
docker cp "$repo/crates/raster-h3-pg/raster_h3.control" "$container:$sharedir/extension/"
docker cp "$repo/crates/raster-h3-pg/sql/raster_h3--0.5.0.sql" "$container:$sharedir/extension/"

# The entrypoint restarts the server once after initdb; wait for the final instance.
for _ in $(seq 1 60); do
    if docker exec "$container" psql -U postgres -tAc 'SELECT 1' >/dev/null 2>&1 \
        && docker logs "$container" 2>&1 | grep -q 'PostgreSQL init process complete'; then
        echo "$container"
        exit 0
    fi
    sleep 1
done
echo "PostgreSQL ${major} did not become ready" >&2
docker logs "$container" >&2
exit 1
