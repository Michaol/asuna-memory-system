#!/bin/bash
# AMS Hermes Plugin Installation Script
#
# Installs the AMS memory provider plugin into Hermes's plugin directory.
# Usage: cd hermes-plugin && ./install.sh

set -e

echo "=== AMS Hermes Plugin Installer ==="
echo ""

# Resolve HERMES_HOME
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
PLUGIN_DIR="$HERMES_HOME/plugins/ams_memory"

echo "Hermes home: $HERMES_HOME"
echo "Plugin dir:  $PLUGIN_DIR"
echo ""

# Check Python version
python3 --version || {
    echo "Error: Python 3 is required but not installed." >&2
    exit 1
}

# Check if running in correct directory
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ ! -f "$SCRIPT_DIR/ams_memory/provider.py" ]]; then
    echo "Error: ams_memory/provider.py not found." >&2
    echo "Run this script from the hermes-plugin directory." >&2
    exit 1
fi

# Install Python dependency (only requests needed)
echo "Installing Python dependency (requests)..."
pip3 install --user --only-binary :all: requests || {
    echo "Failed to install requests" >&2
    exit 1
}

# Copy plugin to Hermes plugins directory
echo ""
echo "Installing AMS plugin to $PLUGIN_DIR..."
mkdir -p "$PLUGIN_DIR"
cp -r "$SCRIPT_DIR/ams_memory"/* "$PLUGIN_DIR/"

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
echo "Then configure via environment variables or ~/.hermes/ams.json:"
echo ""
cat <<'EOF'
# Environment variables:
export AMS_GATEWAY_URL="http://127.0.0.1:8765"
export AMS_API_KEY="your-secret-key"
export AMS_RECALL_TOP_K=5
export AMS_AUTO_RECALL=true
export AMS_AUTO_STORE=true

# Or create ~/.hermes/ams.json:
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
echo "  docker run -p 8765:8765 -v ~/.asuna:/home/asuna/.asuna asuna-memory"
echo ""
