#!/bin/bash
set -e

# AMS Gateway + Hermes Entry Point

echo "=== Asuna Memory System ==="
echo "Version: $(asuna-memory --version)"
echo "Data directory: $AMS_DATA_DIR"
echo "Gateway port: $AMS_GATEWAY_PORT"
echo ""

# Initialize database if needed
echo "Checking database..."
asuna-memory doctor --data-dir "$AMS_DATA_DIR" || {
    echo "Database initialization failed"
    exit 1
}

# Download model if not present
if [ ! -f "$AMS_DATA_DIR/models/bge-m3/model.safetensors" ]; then
    echo "Downloading embedding model (this may take a few minutes)..."
    asuna-memory model-download --data-dir "$AMS_DATA_DIR"
fi

# Start Gateway in background
echo "Starting AMS Gateway on port $AMS_GATEWAY_PORT..."
asuna-memory gateway \
    --data-dir "$AMS_DATA_DIR" \
    --port "$AMS_GATEWAY_PORT" \
    --host 0.0.0.0 &

GATEWAY_PID=$!

# Wait for Gateway to be ready
echo "Waiting for Gateway to be ready..."
for i in {1..30}; do
    if curl -f http://localhost:$AMS_GATEWAY_PORT/health > /dev/null 2>&1; then
        echo "Gateway is ready!"
        break
    fi
    if [ $i -eq 30 ]; then
        echo "Gateway failed to start"
        exit 1
    fi
    sleep 1
done

# Function to cleanup
cleanup() {
    echo "Shutting down..."
    kill $GATEWAY_PID 2>/dev/null || true
    wait $GATEWAY_PID 2>/dev/null || true
    exit 0
}

trap cleanup SIGTERM SIGINT

echo ""
echo "=== AMS Gateway Running ==="
echo "Health: http://localhost:$AMS_GATEWAY_PORT/health"
echo "Stats:  http://localhost:$AMS_GATEWAY_PORT/stats"
echo ""
echo "Use Hermes plugin to connect:"
echo "  - Install: pip install hermes-plugin/ams_memory"
echo "  - Config: gateway_url=http://localhost:$AMS_GATEWAY_PORT"
echo ""

# Keep container running
wait $GATEWAY_PID
