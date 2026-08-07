# Multi-stage build for AMS + Hermes
# Stage 1: Build AMS Gateway
FROM rust:1.75-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies (sorted, no recommends to keep the layer lean)
RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl-dev \
        pkg-config \
        sqlite3 \
    && rm -rf /var/lib/apt/lists/*

# Copy source code
COPY . .

# Build release binary (--locked enforces the committed Cargo.lock)
RUN cargo build --locked --release --bin asuna-memory

# Stage 2: Runtime image
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies + ONNX Runtime + Python packages (single layer).
# Packages sorted alphanumerically; --no-install-recommends keeps the image lean.
ARG ORT_VERSION=1.24.4
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        python3 \
        python3-pip \
        sqlite3 \
    && rm -rf /var/lib/apt/lists/* \
    && ARCH=$(case "${TARGETARCH}" in \
        "arm64") echo "aarch64" ;; \
        *) echo "x64" ;; \
    esac) && \
    curl -sL --proto '=https' --tlsv1.2 "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${ARCH}-${ORT_VERSION}.tgz" \
        | tar xz -C /usr/local/lib --strip-components=2 --wildcards '*/lib/libonnxruntime.so*' \
    && pip3 install --no-cache-dir --only-binary :all: \
        aiohttp==3.12.0 \
        pyyaml==6.0.2 \
        requests==2.32.3 \
    && mkdir -p /data/asuna

# Copy Hermes plugin + install from a prebuilt wheel (--only-binary forbids
# running arbitrary setup scripts at install time; the wheel build runs our
# own trusted setup.py once, then the install consumes the artifact).
COPY hermes-plugin /app/hermes-plugin
RUN pip3 wheel --no-build-isolation -w /tmp/wheels /app/hermes-plugin && \
    pip3 install --no-cache-dir --only-binary :all: /tmp/wheels/ams_memory-*.whl && \
    rm -rf /tmp/wheels

# Copy AMS binary from builder
COPY --from=builder /app/target/release/asuna-memory /usr/local/bin/

# Startup script (root-owned so the non-root runtime user can execute but not
# modify it). COPY runs as root regardless of USER; chmod is merged with the
# user-creation RUN to avoid two consecutive RUN instructions.
COPY docker/entrypoint.sh /entrypoint.sh

# Create a non-root runtime user; AMS writes to ~/.asuna by default.
RUN chmod +x /entrypoint.sh && \
    groupadd -r asuna && useradd -r -g asuna -d /home/asuna -s /usr/sbin/nologin asuna && \
    mkdir -p /home/asuna/.asuna /data/asuna && \
    chown -R asuna:asuna /home/asuna /data/asuna
USER asuna

# Expose Gateway port
EXPOSE 8765

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8765/health || exit 1

# Environment variables
ENV AMS_GATEWAY_PORT=8765 \
    RUST_LOG=info

# Volume for persistent data (owned by the asuna user)
VOLUME ["/home/asuna/.asuna"]

ENTRYPOINT ["/entrypoint.sh"]
