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

# Install runtime dependencies + ONNX Runtime + Python packages (single layer)
ARG ORT_VERSION=1.24.4
ARG TARGETARCH
RUN apt-get update && apt-get install -y \
    ca-certificates \
    sqlite3 \
    python3 \
    python3-pip \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && ARCH=$(case "${TARGETARCH}" in \
        "arm64") echo "aarch64" ;; \
        *) echo "x64" ;; \
    esac) && \
    curl -sL "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${ARCH}-${ORT_VERSION}.tgz" \
    | tar xz -C /usr/local/lib --strip-components=2 --wildcards '*/lib/libonnxruntime.so*' \
    && pip3 install --no-cache-dir aiohttp pyyaml requests \
    && mkdir -p /data/asuna

# Copy AMS binary from builder
COPY --from=builder /app/target/release/asuna-memory /usr/local/bin/

# Copy Hermes plugin + install
COPY hermes-plugin /app/hermes-plugin
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
