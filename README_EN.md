# Asuna Memory System

> Long-term memory system for AI Agents — MCP Server

[中文](README.md) | [AI Agent Install Guide](for_ai.md)

---

## Installation

### Option 1: Download from GitHub Release (Recommended)

Go to [Releases](https://github.com/Michaol/asuna-memory-system/releases) and download the pre-built package for your platform (includes ONNX Runtime dynamic library):

| Platform            | File                                      |
| ------------------- | ----------------------------------------- |
| Windows x86_64      | `asuna-memory-windows-x64.exe.zip`        |
| Linux x86_64        | `asuna-memory-linux-x64.tar.gz`           |
| Linux ARM64         | `asuna-memory-linux-arm64.tar.gz`         |
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
  - Automatically detects ONNX input requirements, supporting models without `token_type_ids` (like the multilingual E5)
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
│  Bounded      │    turns, FTS5, vec_turns)  │
│  Security     │  Immutable storage          │
│  Provenance   │  Vector persistence (int8)  │
├──────────────┴──────────────────────────────┤
│              Embedder (ONNX)                │
│      multilingual-e5-small (384-dim)        │
└─────────────────────────────────────────────┘
```

### Fact Layer

- **Conversation storage**: Each conversation archived as JSONL in `conversations/YYYY/MM/DD/`
- **Index**: SQLite stores session metadata and turn summaries
- **Full-text search**: FTS5 virtual table with Chinese tokenization (`tokenize_zh`)
- **Vector search**: sqlite-vec extension, 384-dim INT8 quantized vectors, automatically written on save/import/rebuild
- **Hybrid search**: Reciprocal Rank Fusion (RRF) combining semantic + keyword results

### Growth Layer

- **Bounded memory**: `MEMORY.md` (AI knowledge, 2200 char limit) and `USER.md` (user profile, 1375 char limit)
- **Entry separator**: Uses `§` between entries
- **Security scan**: Automatic detection of prompt injection, credential leakage, and invisible Unicode on every write/update
- **Provenance tracking**: Each memory entry traces back to its source conversation session

---

## MCP Tools

| Tool                | Description                                           |
| ------------------- | ----------------------------------------------------- |
| `save_session`      | Save a complete conversation (auto-generates vectors) |
| `search_sessions`   | Multi-dimensional historical conversation search      |
| `memory_write`      | Write a new entry to growth memory                    |
| `memory_update`     | Update an existing memory entry via substring match   |
| `memory_remove`     | Remove a memory entry                                 |
| `memory_read`       | Read the full growth memory content                   |
| `user_profile`      | Read/write user profile                               |
| `rebuild_index`     | Rebuild index from JSONL files (FTS + vectors)        |
| `memory_provenance` | Verify provenance of growth memory entries            |

Detailed parameter documentation in [for_ai.md](for_ai.md).

---

## JSONL File Format

The `import` and `rebuild` commands read JSONL files: **1 Header line + N Turn lines**, one JSON object per line.

### Header (line 1)

| Field         | Type     | Required | Description                                            |
| ------------- | -------- | -------- | ------------------------------------------------------ |
| `v`           | integer  | yes      | Format version, currently `1`                          |
| `type`        | string   | yes      | Always `"session_header"`                              |
| `session_id`  | string   | yes      | Unique session ID (UUID or custom string)              |
| `start_time`  | string   | yes      | ISO 8601 timestamp (e.g., `2026-04-10T10:02:00+08:00`) |
| `profile_id`  | string   | yes      | Profile ID (usually `"default"`)                       |
| `source`      | string   | no       | Source identifier (e.g., `"openclaw"`, `"chatgpt"`)    |
| `agent_model` | string   | no       | Agent model name                                       |
| `title`       | string   | no       | Session title                                          |
| `tags`        | string[] | no       | Tag list                                               |

### Turn (lines 2+, one per conversation turn)

| Field          | Type    | Required | Description                                                             |
| -------------- | ------- | -------- | ----------------------------------------------------------------------- |
| `ts`           | string  | yes      | ISO 8601 timestamp                                                      |
| `seq`          | integer | yes      | Turn sequence number, starts at `1`                                     |
| `role`         | string  | yes      | `"user"` / `"assistant"` / `"tool_call"` / `"system"`                   |
| `content`      | string  | yes      | Turn content                                                            |
| _extra fields_ | any     | no       | Flattened via `#[serde(flatten)]` (e.g., `model`, `usage`, `tool_name`) |

### Example

````jsonl
{"v":1,"type":"session_header","session_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","start_time":"2026-04-10T10:02:00+08:00","profile_id":"default","source":"manual","title":"Example session","tags":["demo"]}
{"ts":"2026-04-10T10:02:00+08:00","seq":1,"role":"user","content":"Hello, help me write a Rust Hello World"}
{"ts":"2026-04-10T10:02:05+08:00","seq":2,"role":"assistant","content":"Sure! Here is a minimal Rust Hello World:\n\n```rust\nfn main() {\n    println!(\"Hello, World!\");\n}\n```","model":"gpt-4","usage":{"input_tokens":15,"output_tokens":42}}
````

> **Note**: `import` uses JSONL format (`ts` / `seq` fields). `save_session` MCP tool uses `timestamp` field and auto-assigns `seq`. Both produce the same stored format.

### Importing

```bash
# Single file
asuna-memory import my_session.jsonl

# Batch import
for f in sessions/*.jsonl; do
  asuna-memory import "$f"
done
```

## When to Save

| Scenario           | When             | Notes                                                      |
| ------------------ | ---------------- | ---------------------------------------------------------- |
| Agent conversation | End of each turn | Ensures conversation is archived for later search          |
| Batch migration    | One-time import  | Use `import` command to bulk-import JSONL files            |
| Periodic archive   | On a schedule    | Good for high-frequency chat (e.g., customer support bots) |
| User-triggered     | On user request  | Important conversations saved on demand                    |

Recommended: save after each conversation turn. Same `session_id` = overwrite (INSERT OR REPLACE).

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

# Rebuild index from JSONL (FTS + vectors)
asuna-memory rebuild

# Import a JSONL file
asuna-memory import session.jsonl

# Export session summary
asuna-memory export <session_id>
```

### Global Parameters

| Parameter   | Default                | Description      |
| ----------- | ---------------------- | ---------------- |
| `--config`  | `~/.asuna/config.json` | Config file path |
| `--profile` | `default`              | Active profile   |

---

## Upgrade Guide

### Upgrading from v1.1.3 to v1.1.4 (Recommended)

v1.1.4 fixes a regression where the vector index could drop to zero after a `rebuild` command in certain environments, and optimizes rebuild performance.

```bash
# 1. Replace the binary

# 2. Rerun rebuild to restore potentially missing vector indices
asuna-memory rebuild
```

**v1.1.4 Changelog:**

- **Vector Index Regression Fix**: Resolved a silent failure in SQLite `vec0` virtual table writes caused by read/write cursor concurrency conflicts during `rebuild`.
- **Rebuild Performance Optimization**: Consolidated the query paths for FTS and vector index rebuilding, reducing DB IO by 50% and improving speed for large datasets.
- **Improved Diagnostics**: Replaced silent error suppression with proper `warn!` logging for better observability during the index reconstruction process.

### Upgrading from v1.0.x to v1.1.0

v1.1.0 fixes the vector database not being populated. After upgrading, rebuild the index to backfill vector data:

```bash
# 1. Replace the binary

# 2. Rebuild index (rebuilds both FTS and vector index)
asuna-memory rebuild

# 3. Verify
asuna-memory doctor
# Expected output includes:
#   索引统计: 10 会话, 24 轮对话, 24 个向量
```

**v1.1.0 Changelog:**

- `rebuild` now generates int8 embeddings for all turns and writes them to the `vec_turns` table
- `save_session` / `import` automatically generate vectors when the embedding model is available
- `doctor` now shows the vector index count
- All write paths (save / import / rebuild / MCP) share a unified embedding pipeline

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
│       ├── memory.db                   # SQLite (includes vec_turns vector table)
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
