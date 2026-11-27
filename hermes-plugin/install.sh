#!/bin/bash
# AMS Hermes Plugin Installation Script
#
# Installs the AMS memory provider plugin into Hermes's plugin directory.
# Usage: cd hermes-plugin && ./install.sh

set -euo pipefail

echo "=== AMS Hermes Plugin Installer ==="
echo ""

# Resolve HERMES_HOME
HERMES_HOME="${HERMES_HOME:-${HOME:?HOME is not set}/.hermes}"
PLUGIN_DIR="$HERMES_HOME/plugins/ams_memory"

echo "Hermes home: $HERMES_HOME"
echo "Plugin dir:  $PLUGIN_DIR"
echo ""

# Check Python version and pip module
python3 --version || {
    echo "Error: Python 3 is required but not installed." >&2
    exit 1
}
python3 -m pip --version >/dev/null 2>&1 || {
    echo "Error: python3 -m pip is required." >&2
    exit 1
}

# Check if running in correct directory
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ ! -f "$SCRIPT_DIR/ams_memory/provider.py" ]]; then
    echo "Error: ams_memory/provider.py not found." >&2
    echo "Run this script from the hermes-plugin directory." >&2
    exit 1
fi

# Install Python dependency (exact pins + SHA-256 hashes from requirements.txt)
echo "Installing Python dependency (requests)..."
if [[ -n "${VIRTUAL_ENV:-}" ]]; then
    PIP_USER_FLAG=()
else
    PIP_USER_FLAG=(--user)
fi
python3 -m pip install ${PIP_USER_FLAG[@]+"${PIP_USER_FLAG[@]}"} --require-hashes --only-binary :all: -r "$SCRIPT_DIR/requirements.txt" || {
    echo "Failed to install requests. If your system Python enforces PEP 668 (externally-managed-environment), activate a virtualenv first and re-run ./install.sh." >&2
    exit 1
}

# Copy plugin to Hermes plugins directory (exclude __pycache__ / bytecode)
echo ""
echo "Installing AMS plugin to $PLUGIN_DIR..."
mkdir -p "$PLUGIN_DIR"
cp "$SCRIPT_DIR/ams_memory"/*.py "$PLUGIN_DIR/"

echo ""
echo "=== Installation Complete ==="
echo ""
echo "To activate the plugin, add this to your Hermes config.yaml:"
echo ""
cat <<'EOF'
memory:
  provider: ams_memory
EOF

echo ""
echo "Then configure via environment variables or \$HERMES_HOME/ams.json (default ~/.hermes/ams.json):"
echo ""
cat <<'EOF'
# Environment variables:
export AMS_GATEWAY_URL="http://127.0.0.1:8765"
export AMS_API_KEY="your-secret-key"
export AMS_RECALL_TOP_K=5
export AMS_AUTO_RECALL=true
export AMS_AUTO_STORE=true

# Or create $HERMES_HOME/ams.json:
{
    "gateway_url": "http://127.0.0.1:8765",
    "api_key": "your-secret-key",
    "recall_top_k": 5
}
EOF

echo ""
echo "Start AMS Gateway:"
echo "  asuna-memory gateway --port 8765"
echo ""
echo "Or with Docker:"
echo "  mkdir -p ~/.asuna   # first: create the host dir, else dockerd creates it as root and the container user (uid 1000) cannot write"
echo "  docker run -p 127.0.0.1:8765:8765 -v ~/.asuna:/home/asuna/.asuna -e AMS_GATEWAY_BIND_HOST=0.0.0.0 -e AMS_GATEWAY_API_KEY=your-secret-key asuna-memory"
echo ""
