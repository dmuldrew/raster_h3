# DuckDB version the extension is packaged for and the runtime CLI/library.
# They must match: DuckDB rejects extensions tagged for another platform name.
ARG DUCKDB_VERSION=v1.2.0

# ==============================================================================
# Stage 1: Build the Rust DuckDB Extension
# ==============================================================================
FROM rust:bookworm AS builder

ARG DUCKDB_VERSION

WORKDIR /build

# Copy workspace manifests, member crates, and source code
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ ./.cargo/
COPY crates/ ./crates/
COPY src/ ./src/
COPY tests/ ./tests/
COPY examples/ ./examples/

# Compile release libraries (every member's cdylib, which the cross-engine tests load),
# run tests, then build CLI utilities, sample data, and the metadata-tagged extension
RUN cargo build --release --workspace && \
    cargo test --release --workspace && \
    cargo build --release --examples && \
    cargo run --release --example generate_sample -- sample_sf.tif && \
    cargo run --release --bin package_extension -- \
        --input target/release/libraster_h3.so \
        --output target/release/raster_h3.duckdb_extension \
        --duckdb-version "${DUCKDB_VERSION}"

# ==============================================================================
# Stage 2: Runtime Environment with DuckDB CLI
# ==============================================================================
FROM debian:bookworm-slim AS runtime

# Install runtime utilities
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    unzip \
    bash \
    && rm -rf /var/lib/apt/lists/*

# Install the DuckDB CLI and library matching the packaged extension
ARG DUCKDB_VERSION
RUN set -eux; \
    ARCH=$(uname -m); \
    case "$ARCH" in \
        x86_64)  DUCK_ARCH="linux-amd64" ;; \
        aarch64|arm64) DUCK_ARCH="linux-arm64" ;; \
        *) echo "Unsupported architecture: $ARCH" && exit 1 ;; \
    esac; \
    CLI_URL="https://github.com/duckdb/duckdb/releases/download/${DUCKDB_VERSION}/duckdb_cli-${DUCK_ARCH}.zip"; \
    LIB_URL="https://github.com/duckdb/duckdb/releases/download/${DUCKDB_VERSION}/libduckdb-${DUCK_ARCH}.zip"; \
    curl -fsSL "$CLI_URL" -o duckdb.zip; \
    unzip duckdb.zip -d /usr/local/bin; \
    chmod +x /usr/local/bin/duckdb; \
    rm duckdb.zip; \
    curl -fsSL "$LIB_URL" -o libduckdb.zip; \
    unzip libduckdb.zip -d /tmp/libduckdb; \
    mv /tmp/libduckdb/libduckdb.so /usr/local/lib/; \
    rm -rf /tmp/libduckdb libduckdb.zip; \
    ldconfig

# Create directories
RUN mkdir -p /extensions /data /app /root

# Copy extension library, CLI tools, and sample data from builder
COPY --from=builder /build/target/release/raster_h3.duckdb_extension /extensions/raster_h3.duckdb_extension
COPY --from=builder /build/target/release/libraster_h3.so /extensions/libraster_h3.so
COPY --from=builder /build/target/release/examples/raster_to_pmtiles /usr/local/bin/raster_to_pmtiles
COPY --from=builder /build/target/release/examples/parquet_to_pmtiles /usr/local/bin/parquet_to_pmtiles
COPY --from=builder /build/sample_sf.tif /data/sample_sf.tif
COPY sql/ /app/sql/
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh /usr/local/bin/raster_to_pmtiles /usr/local/bin/parquet_to_pmtiles

ENV LD_PRELOAD=/usr/local/lib/libduckdb.so

# Set working directory
WORKDIR /data

# Expose volume mount point for external GeoTIFF data
VOLUME ["/data"]

ENTRYPOINT ["/app/entrypoint.sh"]
