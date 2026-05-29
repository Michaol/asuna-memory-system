# AMS Hermes Plugin

Asuna Memory System (AMS) integration for Hermes Agent. This plugin provides multi-layer memory (L0-L5) with automatic recall and storage.

## Features

- **Multi-layer Memory**: L0 (conversation) → L1 (atoms) → L2 (scenarios) → L3 (persona) → L4 (mental models) → L5 (intent prediction)
- **Automatic Recall**: Injects relevant memories into context before each response
- **Automatic Storage**: Captures conversations and extracts memories automatically
- **Evolution Chains**: Tracks how memories evolve over time
- **Progressive Disclosure**: Retrieves memories in layers based on relevance

## Installation

### Quick Install

```bash
cd hermes-plugin
./install.sh
```

### Manual Install

```bash
# Install dependencies
pip install -r requirements.txt

# Install plugin in development mode
pip install -e .
```

### Installation

**Step 1: Copy plugin to Hermes plugins directory**

```bash
cd hermes-plugin
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
mkdir -p "$HERMES_HOME/plugins/ams_memory"
cp -r ams_memory/* "$HERMES_HOME/plugins/ams_memory/"
```

Or use the install script:
```bash
cd hermes-plugin
./install.sh
```

**Step 2: Activate in config.yaml**

Add to your Hermes config file (`~/.hermes/config.yaml`):

```yaml
memory:
  provider: ams_memory
```

**Step 3: Configure via environment variables or `~/.hermes/ams.json`**

```bash
# Environment variables
export AMS_GATEWAY_URL="http://127.0.0.1:8765"
export AMS_API_KEY="your-secret-key"
export AMS_RECALL_TOP_K=5
export AMS_AUTO_RECALL=true
export AMS_AUTO_STORE=true
```

Or create `~/.hermes/ams.json`:
```json
{
    "gateway_url": "http://127.0.0.1:8765",
    "api_key": "your-secret-key",
    "recall_top_k": 5
}
```

JSON file values override environment variables.

## Configuration

**config.yaml** — only the provider name:

```yaml
memory:
  provider: ams_memory
```

**Environment variables / ams.json** — plugin behavior:

| Variable | JSON Key | Default | Description |
|----------|----------|---------|-------------|
| `AMS_GATEWAY_URL` | `gateway_url` | `http://127.0.0.1:8765` | AMS Gateway URL |
| `AMS_API_KEY` | `api_key` | *(empty)* | API key for authentication |
| `AMS_RECALL_TOP_K` | `recall_top_k` | `5` | Memories per query |
| `AMS_AUTO_RECALL` | `auto_recall` | `true` | Auto recall before responses |
| `AMS_AUTO_STORE` | `auto_store` | `true` | Auto store after turns |

The plugin discovers configuration in order:
1. `~/.hermes/ams.json` (highest priority)
2. Environment variables
3. Hardcoded defaults

Plugin discovery: Hermes scans `$HERMES_HOME/plugins/` for directories containing `provider.py` with a `register(ctx)` function.

## Starting AMS Gateway

### Using Docker (Recommended)

```bash
docker run -d \
  --name ams-gateway \
  -p 8765:8765 \
  -v ~/.asuna:/root/.asuna \
  ams-hermes
```

### Using Docker Compose

```bash
docker-compose up -d
```

### Manual Start

```bash
asuna-memory gateway --port 8765
```

## Usage

Once installed and configured, Hermes will automatically:

1. **Recall memories** before generating responses
2. **Store conversations** after interactions
3. **Extract atoms** from conversations
4. **Build scenarios** from related atoms
5. **Update persona** based on patterns

### Manual Memory Operations

You can also manually interact with memories through the AMS Gateway API:

```bash
# Recall memories (progressive disclosure: L3→L2→L1→L0)
curl -X POST http://127.0.0.1:8765/recall \
  -H "Content-Type: application/json" \
  -d '{"query": "user preferences", "top_k": 5}'

# Get persona
curl http://127.0.0.1:8765/persona

# Get stats
curl http://127.0.0.1:8765/stats
```

## Architecture

```
Hermes Agent
    ↓
AMSMemoryProvider (this plugin)
    ↓
AMS Gateway (HTTP API)
    ↓
Multi-layer Memory System
    ├── L0: Conversation (raw turns)
    ├── L1: Atoms (extracted facts)
    ├── L2: Scenarios (grouped atoms)
    ├── L3: Persona (user profile)
    ├── L4: Mental Models (cognitive frameworks)
    └── L5: Intent Prediction (future needs)
```

## Troubleshooting

### Plugin not loading

Check that the plugin is installed:
```bash
pip show ams-memory
```

### Gateway connection failed

1. Verify Gateway is running: `curl http://127.0.0.1:8765/health`
2. Check `gateway_url` in config matches Gateway address
3. Check firewall settings

### Memories not being recalled

1. Check `auto_recall: true` in config
2. Verify Gateway has memories: `curl http://127.0.0.1:8765/stats`
3. Check logs for errors: `docker logs ams-gateway`

### Memories not being stored

1. Check `auto_store: true` in config
2. Verify `auto_store: true` in config
3. Check Gateway logs for extraction errors

## Development

### Running Tests

```bash
# Install test dependencies
pip install pytest pytest-asyncio

# Run tests
pytest tests/
```

### Building Docker Image

```bash
docker build -t ams-hermes .
```

## License

MIT License - see LICENSE file for details

## Support

- GitHub Issues: https://github.com/Michaol/asuna-memory-system/issues
- Documentation: https://github.com/Michaol/asuna-memory-system#readme
