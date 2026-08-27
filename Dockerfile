# ==============================================================================
# Stage 1: Build the Rust DuckDB Extension
# ==============================================================================
FROM rust:bookworm AS builder

WORKDIR /build

# Copy source code and configuration
COPY Cargo.toml ./
COPY src/ ./src/
COPY tests/ ./tests/
COPY examples/ ./examples/

# Run tests and compile release dynamic library
RUN cargo test --release && \
    cargo build --release && \
    cargo run --release --example generate_sample sample_sf.tif

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

# Install latest DuckDB CLI with multi-arch support
ARG DUCKDB_VERSION=latest
RUN set -eux; \
    ARCH=$(uname -m); \
    case "$ARCH" in \
        x86_64)  DUCK_ARCH="linux-amd64" ;; \
        aarch64|arm64) DUCK_ARCH="linux-arm64" ;; \
        *) echo "Unsupported architecture: $ARCH" && exit 1 ;; \
    esac; \
    if [ "$DUCKDB_VERSION" = "latest" ]; then \
        CLI_URL="https://github.com/duckdb/duckdb/releases/latest/download/duckdb_cli-${DUCK_ARCH}.zip"; \
        LIB_URL="https://github.com/duckdb/duckdb/releases/latest/download/libduckdb-${DUCK_ARCH}.zip"; \
    else \
        CLI_URL="https://github.com/duckdb/duckdb/releases/download/${DUCKDB_VERSION}/duckdb_cli-${DUCK_ARCH}.zip"; \
        LIB_URL="https://github.com/duckdb/duckdb/releases/download/${DUCKDB_VERSION}/libduckdb-${DUCK_ARCH}.zip"; \
    fi; \
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

# Copy extension library and sample data from builder
COPY --from=builder /build/target/release/libraster_h3.so /extensions/raster_h3.duckdb_extension
COPY --from=builder /build/target/release/libraster_h3.so /extensions/libraster_h3.so
COPY --from=builder /build/sample_sf.tif /data/sample_sf.tif
COPY demo.sql /app/demo.sql
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

ENV LD_PRELOAD=/usr/local/lib/libduckdb.so

# Set working directory
WORKDIR /data

# Expose volume mount point for external GeoTIFF data
VOLUME ["/data"]

ENTRYPOINT ["/app/entrypoint.sh"]
