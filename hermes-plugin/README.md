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

## Configuration

Add to your Hermes config file (usually `~/.hermes/config.yaml`):

```yaml
providers:
  memory:
    type: ams_memory
    config:
      gateway_url: "http://127.0.0.1:8765"
      auto_recall: true
      auto_store: true
      recall_top_k: 5
```

### Configuration Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `gateway_url` | string | `http://127.0.0.1:8765` | AMS Gateway URL |
| `api_key` | string | *(empty)* | API key for gateway authentication |
| `auto_recall` | boolean | `true` | Automatically recall memories before responses |
| `auto_store` | boolean | `true` | Automatically store conversations as memories |
| `recall_top_k` | integer | `5` | Number of memories to recall |

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `AMS_GATEWAY_URL` | `http://127.0.0.1:8765` | Gateway URL |
| `AMS_API_KEY` | *(empty)* | API key |
| `AMS_RECALL_TOP_K` | `5` | Memories per query |
| `AMS_AUTO_RECALL` | `true` | Auto recall |
| `AMS_AUTO_STORE` | `true` | Auto store |

Config can also be loaded from `~/.hermes/ams.json`.

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
# Search memories
curl -X POST http://127.0.0.1:8765/search \
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
