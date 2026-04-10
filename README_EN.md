# Asuna Memory System

> Long-term memory system for AI Agents — MCP Server

[中文](README.md) | [AI Agent Install Guide](for_ai.md)

---

## Installation

### Option 1: Download from GitHub Release (Recommended)

Go to [Releases](https://github.com/Michaol/asuna-memory-system/releases) and download the pre-built package for your platform (includes ONNX Runtime dynamic library):

| Platform | File |
|----------|------|
| Windows x86_64 | `asuna-memory-windows-x64.exe.zip` |
| Linux x86_64 | `asuna-memory-linux-x64.tar.gz` |
| Linux ARM64 | `asuna-memory-linux-arm64.tar.gz` |
| macOS Apple Silicon | `asuna-memory-macos-apple-silicon.tar.gz` |

Extract and add to PATH:

```bash
# Linux x64
tar xzf asuna-memory-linux-x64.tar.gz
sudo mv asuna-memory /usr/local/bin/

# macOS
tar xzf asuna-memory-macos-apple-silicon.tar.gz
sudo mv asuna-memory /usr/local/bin/

# Windows: extract zip, place asuna-memory.exe in PATH
```

### Option 2: Build from Source

Requires **Rust 1.75+** (install via `rustup`). No external database needed — SQLite is bundled.

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system
cargo build --release
```

Binary at `target/release/asuna-memory` (`.exe` on Windows).

### Quick Verification

```bash
# Check environment
asuna-memory doctor

# Start MCP server
asuna-memory serve
```

### MCP Client Configuration

Add to your MCP client config:

```json
{
  "mcpServers": {
    "asuna-memory": {
      "command": "asuna-memory",
      "args": ["serve"]
    }
  }
}
```

> **For AI Agents**: If you are an AI Agent that needs to auto-install this system, read [for_ai.md](for_ai.md) for complete installation, invocation, and tool parameter documentation.

### Important Notes

1. **ONNX Runtime (optional)**: Semantic search requires the ONNX Runtime dynamic library. Without it, the system gracefully falls back to keyword-only search.
2. **Model files (optional)**: Semantic search requires the `multilingual-e5-small` model. The system searches these paths in order:
   - `~/.rustrag/models/multilingual-e5-small`
   - `~/.asuna/models/multilingual-e5-small`
   - On Windows, supports `ASUNA_DEV_ROOT` env var for dev paths
   - Falls back to keyword search if not found
3. **Data directory**: Defaults to `~/.asuna/`. Created automatically on first run.
4. **Profile isolation**: Each profile's data is stored under `~/.asuna/profiles/{profile_id}/`.

---

## Architecture

Asuna Memory System uses a **dual-layer memory architecture**:

```
┌─────────────────────────────────────────────┐
│              MCP Server (stdio)              │
│          JSON-RPC 2.0 over stdin/out        │
├──────────────┬──────────────────────────────┤
│  Growth Layer │       Fact Layer            │
│               │                             │
│  MEMORY.md    │  JSONL files (archives)     │
│  USER.md      │  SQLite index (sessions,    │
│  Bounded      │    turns, FTS5, vectors)    │
│  Security     │  Immutable storage          │
│  Provenance   │                             │
├──────────────┴──────────────────────────────┤
│              Embedder (ONNX)                │
│      multilingual-e5-small (384-dim)        │
└─────────────────────────────────────────────┘
```

### Fact Layer

- **Conversation storage**: Each conversation archived as JSONL in `conversations/YYYY/MM/DD/`
- **Index**: SQLite stores session metadata and turn summaries
- **Full-text search**: FTS5 virtual table with Unicode tokenization
- **Vector search**: sqlite-vec extension, 384-dim INT8 quantized vectors
- **Hybrid search**: Reciprocal Rank Fusion (RRF) combining semantic + keyword results

### Growth Layer

- **Bounded memory**: `MEMORY.md` (AI knowledge, 2200 char limit) and `USER.md` (user profile, 1375 char limit)
- **Entry separator**: Uses `§` between entries
- **Security scan**: Automatic detection of prompt injection, credential leakage, and invisible Unicode on every write/update
- **Provenance tracking**: Each memory entry traces back to its source conversation session

---

## MCP Tools

| Tool | Description |
|------|-------------|
| `save_session` | Save a complete conversation to the fact layer |
| `search_sessions` | Multi-dimensional historical conversation search |
| `memory_write` | Write a new entry to growth memory |
| `memory_update` | Update an existing memory entry via substring match |
| `memory_remove` | Remove a memory entry |
| `memory_read` | Read the full growth memory content |
| `user_profile` | Read/write user profile |
| `rebuild_index` | Rebuild SQLite index from JSONL files |
| `memory_provenance` | Verify provenance of growth memory entries |

Detailed parameter documentation in [for_ai.md](for_ai.md).

---

## CLI Commands

```bash
# Start MCP server (default command)
asuna-memory serve

# Environment check
asuna-memory doctor

# List all profiles
asuna-memory list-profiles

# List recent sessions
asuna-memory list-sessions --last-days 7 --limit 20

# Search conversations
asuna-memory search "Rust async" --mode keyword --top-k 5

# Rebuild index from JSONL
asuna-memory rebuild

# Import a JSONL file
asuna-memory import session.jsonl

# Export session summary
asuna-memory export <session_id>
```

### Global Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--config` | `~/.asuna/config.json` | Config file path |
| `--profile` | `default` | Active profile |

---

## Configuration

JSON format, default path `~/.asuna/config.json`. Uses built-in defaults if absent.

```json
{
  "data_dir": "~/.asuna",
  "profile_id": "default",
  "conversation": {
    "enabled": true,
    "auto_embed": true,
    "preview_length": 200
  },
  "memory": {
    "memory_enabled": true,
    "user_profile_enabled": true,
    "memory_char_limit": 2200,
    "user_char_limit": 1375,
    "security_scan": true
  },
  "search": {
    "default_top_k": 5,
    "search_mode": "hybrid",
    "fts_enabled": true
  },
  "embedding": {
    "model_name": "multilingual-e5-small",
    "dimensions": 384,
    "batch_size": 32
  },
  "db_path": "memory.db",
  "model_path": null
}
```

---

## Data Directory Structure

```
~/.asuna/
├── config.json
├── profiles/
│   └── default/
│       ├── memory.db
│       ├── conversations/
│       │   └── 2026/
│       │       └── 04/
│       │           └── 10/
│       │               └── 20260410T100200_abc12345.jsonl
│       └── memory/
│           ├── MEMORY.md
│           └── USER.md
└── models/
    └── multilingual-e5-small/
```

---

## Security

Automatic pre-write security scanning on the growth layer:

- **Prompt injection detection**: Pattern matching in English and Chinese (e.g., "ignore previous instructions", "忽略之前的指令")
- **Credential leak detection**: OpenAI `sk-*`, GitHub `ghp_*`, AWS `AKIA*`, PEM private keys
- **Invisible Unicode detection**: Zero-width characters, BOM, etc.

Write operations are rejected with a specific reason when scanning fails.

---

## License

MIT
