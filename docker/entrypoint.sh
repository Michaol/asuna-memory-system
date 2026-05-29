#!/bin/bash
set -e

# AMS Gateway + Hermes Entry Point

echo "=== Asuna Memory System ==="
echo "Version: $(asuna-memory --version)"
echo "Gateway port: ${AMS_GATEWAY_PORT:-8765}"
echo ""

# Initialize database if needed
echo "Checking database..."
asuna-memory doctor || {
    echo "Database initialization failed"
    exit 1
}

# Download model if not present
if ! asuna-memory doctor 2>&1 | grep -q "嵌入引擎状态: OK"; then
    echo "Downloading embedding model (this may take a few minutes)..."
    asuna-memory model-download || echo "Model download failed, continuing with keyword-only search"
fi

# Start Gateway
PORT="${AMS_GATEWAY_PORT:-8765}"
echo "Starting AMS Gateway on port $PORT..."
exec asuna-memory gateway --port "$PORT"
