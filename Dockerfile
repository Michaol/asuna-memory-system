# Multi-stage build for AMS + Hermes
# Stage 1: Build AMS Gateway
FROM rust:1.75-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    sqlite3 \
    && rm -rf /var/lib/apt/lists/*

# Copy source code
COPY . .

# Build release binary
RUN cargo build --release --bin asuna-memory

# Stage 2: Runtime image
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    sqlite3 \
    python3 \
    python3-pip \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install Python dependencies for Hermes plugin
RUN pip3 install --no-cache-dir \
    aiohttp \
    pyyaml \
    requests

# Copy AMS binary from builder
COPY --from=builder /app/target/release/asuna-memory /usr/local/bin/

# Create data directory
RUN mkdir -p /data/asuna

# Copy Hermes plugin
COPY hermes-plugin /app/hermes-plugin

# Install Hermes plugin
RUN pip3 install -e /app/hermes-plugin

# Expose Gateway port
EXPOSE 8765

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8765/health || exit 1

# Environment variables
ENV AMS_GATEWAY_PORT=8765 \
    RUST_LOG=info

# Volume for persistent data
VOLUME ["/root/.asuna"]

# Startup script
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
