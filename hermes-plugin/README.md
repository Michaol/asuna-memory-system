# AMS Hermes Plugin

Asuna Memory System (AMS) integration for [Hermes Agent](https://github.com/NousResearch/hermes-agent). This plugin implements the Hermes `MemoryProvider` ABC to provide L0-L5 hierarchical memory with automatic recall and storage.

## Installation

**Step 1**: Copy plugin to Hermes plugins directory

```bash
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
mkdir -p "$HERMES_HOME/plugins/ams_memory"
cp hermes-plugin/ams_memory/* "$HERMES_HOME/plugins/ams_memory/"
pip3 install requests  # only external dependency
```

Or use the install script: `cd hermes-plugin && ./install.sh`

**Step 2**: Activate in config (`~/.hermes/config.yaml`)

```yaml
memory:
  provider: ams_memory
```

**Step 3**: Configure via environment variables or `~/.hermes/ams.json`

```bash
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
    "recall_top_k": 10
}
```

JSON file values override environment variables.

## Plugin Discovery

Hermes scans `$HERMES_HOME/plugins/` for directories containing `provider.py` with a `register(ctx)` function. The AMS plugin's `register()` function loads configuration from env vars / `ams.json` and calls `ctx.register_memory_provider(AMSMemoryProvider(config))`.

## Configuration

**Environment variables / ams.json** — plugin behavior:

| Env Var | JSON Key | Default | Description |
|---------|----------|---------|-------------|
| `AMS_GATEWAY_URL` | `gateway_url` | `http://127.0.0.1:8765` | AMS Gateway URL |
| `AMS_API_KEY` | `api_key` | *(empty)* | API key for authentication |
| `AMS_RECALL_TOP_K` | `recall_top_k` | `5` | Memories per query |
| `AMS_AUTO_RECALL` | `auto_recall` | `true` | Auto recall before responses |
| `AMS_AUTO_STORE` | `auto_store` | `true` | Auto store after turns |

## Starting AMS Gateway

### Docker

```bash
docker run -d \
  --name ams-gateway \
  -p 8765:8765 \
  -v ~/.asuna:/root/.asuna \
  asuna-memory
```

### Manual

```bash
asuna-memory gateway --port 8765
```

## API Reference

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check |
| `/stats` | GET | Database statistics |
| `/recall` | POST | Progressive disclosure retrieval (L3→L2→L1→L0) |
| `/capture` | POST | Save conversation turns |
| `/persona` | GET | Read user persona (from USER.md or bounded_memory) |
| `/search` | POST | Multi-mode search (keyword/semantic/hybrid) |
| `/session/end` | POST | Record session end |

### Manual Memory Operations

```bash
# Recall memories
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
    ↓ register(ctx) → ctx.register_memory_provider()
    ↓
AMS Gateway (HTTP API)
    ↓
Multi-layer Memory System
    ├── L0: Conversation (raw turns)
    ├── L1: Atoms (extracted facts)
    ├── L2: Scenarios (grouped atoms)
    ├── L3: Persona (USER.md / bounded_memory)
    ├── L4: Mental Models (cognitive frameworks)
    └── L5: Intent Prediction (future needs)
```

## Troubleshooting

### Plugin not loading

Check that plugin files exist in the correct location:
```bash
ls $HERMES_HOME/plugins/ams_memory/provider.py
```

### Gateway connection failed

1. Verify Gateway is running: `curl http://127.0.0.1:8765/health`
2. Check `AMS_GATEWAY_URL` matches Gateway address
3. Check firewall settings

### Memories not being recalled

1. Check `auto_recall: true` in config or `AMS_AUTO_RECALL=true`
2. Verify Gateway has memories: `curl http://127.0.0.1:8765/stats`
3. Check logs: `docker logs ams-gateway`

### Memories not being stored

1. Check `auto_store: true` in config or `AMS_AUTO_STORE=true`
2. Check Gateway logs for errors

## Development

```bash
# Run tests
cd hermes-plugin && python -m pytest tests/ -v

# Build Docker image
docker build -t asuna-memory .
```

## License

MIT

## Support

- GitHub Issues: https://github.com/Michaol/asuna-memory-system/issues
