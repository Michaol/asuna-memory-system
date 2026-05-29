#!/bin/bash
# AMS Hermes Plugin Installation Script

set -e

echo "=== AMS Hermes Plugin Installer ==="
echo ""

# Check Python version
python3 --version || {
    echo "Python 3 is required but not installed."
    exit 1
}

# Check if running in correct directory
if [ ! -f "plugin.yaml" ]; then
    echo "Error: plugin.yaml not found. Run this script from the hermes-plugin directory."
    exit 1
fi

echo "Installing Python dependencies..."
pip3 install --user -r requirements.txt || {
    echo "Failed to install dependencies"
    exit 1
}

echo ""
echo "Installing AMS plugin..."
pip3 install --user -e . || {
    echo "Failed to install plugin"
    exit 1
}

echo ""
echo "=== Installation Complete ==="
echo ""
echo "To use with Hermes, add to your Hermes config:"
echo ""
cat <<EOF
providers:
  memory:
    type: ams_memory
    config:
      gateway_url: "http://127.0.0.1:8765"
      auto_recall: true
      auto_store: true
      recall_top_k: 5
      store_threshold: 0.7
EOF

echo ""
echo "Start AMS Gateway:"
echo "  docker run -p 8765:8765 -v ~/.asuna:/data/asuna ams-hermes"
echo ""
echo "Or manually:"
echo "  asuna-memory gateway --port 8765"
echo ""
