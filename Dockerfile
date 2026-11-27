# Multi-stage build for AMS + Hermes
# Stage 1: Build AMS Gateway
# NOTE: this base image must stay in sync with Cargo.toml `rust-version`
# (MSRV) — when rust-version is bumped, the pinned digest below must be
# refreshed in the same commit or `cargo build` fails with an MSRV error.
# Image: rust:1.89-slim-bookworm (digest-pinned per docker:S8431).
FROM rust@sha256:d7fc7de78bb8c1469933aeecbf801314d30d7d6e9f0578bba4cfa285bfa37fe6 AS builder

WORKDIR /app

# Cache Cargo dependencies before copying full source tree.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && \
    printf 'fn main() {}\n' > src/main.rs && \
    cargo build --locked --release --bin asuna-memory && \
    rm -rf src target/release/asuna-memory target/release/libasuna_memory* target/release/deps/asuna_memory* target/release/deps/libasuna_memory* target/release/.fingerprint/asuna-memory-*

# Copy real source tree and build release binary (--locked enforces Cargo.lock).
COPY src ./src
RUN cargo build --locked --release --bin asuna-memory

# Stage 2: Runtime image
# Image: debian:bookworm-slim (digest-pinned per docker:S8431).
FROM debian@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc

WORKDIR /app

# Install runtime dependencies and Python plugin requirements (exact pins + SHA-256 hashes + wheels-only).
# python3-venv ships the ensurepip wheels, so /opt/venv gets its own pip; the
# apt python3-pip package is never used (all installs go through /opt/venv/bin/pip).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        python3 \
        python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/venv

# Copy Hermes plugin package and install hash-verified runtime dependencies.
COPY hermes-plugin/requirements.txt /tmp/requirements.txt
RUN /opt/venv/bin/pip install --no-cache-dir --require-hashes --only-binary :all: -r /tmp/requirements.txt && \
    rm -f /tmp/requirements.txt
COPY hermes-plugin/ams_memory /app/hermes-plugin/ams_memory
ENV PATH="/opt/venv/bin:$PATH" \
    PYTHONPATH="/app/hermes-plugin"

# Copy AMS binary from builder
COPY --from=builder /app/target/release/asuna-memory /usr/local/bin/

# W1: ship license attributions alongside the binary in the runtime image.
COPY NOTICE.md /app/NOTICE.md
COPY LICENSES/ /app/LICENSES/

# Startup script (root-owned so the non-root runtime user can execute but not modify it).
COPY docker/entrypoint.sh /entrypoint.sh

# Create configurable non-root runtime user; AMS writes to ~/.asuna by default.
ARG UID=1000
ARG GID=1000
RUN chmod +x /entrypoint.sh && \
    groupadd -r -g "${GID}" asuna && \
    useradd -r -u "${UID}" -g asuna -d /home/asuna -s /usr/sbin/nologin asuna && \
    mkdir -p /home/asuna/.asuna && \
    chown -R asuna:asuna /home/asuna
USER asuna

# Expose Gateway port
EXPOSE 8765

# Health check (respects AMS_GATEWAY_PORT override)
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${AMS_GATEWAY_PORT:-8765}/health" || exit 1

# Environment variables
ENV AMS_GATEWAY_PORT=8765 \
    RUST_LOG=info

# Volume for persistent data (owned by the asuna user)
VOLUME ["/home/asuna/.asuna"]

ENTRYPOINT ["/entrypoint.sh"]
