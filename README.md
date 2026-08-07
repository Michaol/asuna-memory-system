# Asuna Memory System

> Long-term memory system for AI Agents — MCP Server

[![Quality gate status](https://sonarcloud.io/api/project_badges/measure?project=Michaol_asuna-memory-system&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=Michaol_asuna-memory-system)

[中文](README_ZH.md) | [AI Agent Install Guide](for_ai.md) | [Changelog History](HISTORY.md)

## Upgrade Guide

### Upgrading from v2.5.3 to v2.6.0

v2.6.0 is the "lightweight pack": `/recall` gains a response token budget (greedy prefix cut, default 2000 from `recall.token_budget`) and explicit time-range filters (`after`/`before`/`last_days`, same semantics as `/search`); `/search` results expose per-source `scores` components. Governance groundwork: an exact-text guard skips re-extracted duplicate atoms before embedding and audits them as `duplicate_skip`; `bounded_memory.edited_at` marks user-edited content (stamped by `memory_update` and `doctor --fix` reinserts, inherited by `--split-entries` children); a `memory_history` snapshot table lands for future rewrite safety. A Chinese retrieval benchmark (`cargo test -- --ignored retrieval_benchmark`) records the quality baseline. Zero new dependencies; binary size unchanged. Full changelog: [HISTORY.md](HISTORY.md).

Upgrade: replace the binary. No data migration (old databases gain `edited_at` and `memory_history` automatically on first start). **Behavior notes**: `/recall` responses above the token budget now return fewer memories with `truncated: true` — v2.5.3 applied no budget at all. **Docker**: the runtime is now a non-root user `asuna`; update your volume mount from `-v ~/.asuna:/root/.asuna` to `-v ~/.asuna:/home/asuna/.asuna`.

### Upgrading from v2.5.2 to v2.5.3

v2.5.3 fixes atom eviction failing with `FOREIGN KEY constraint failed` whenever the eviction target was referenced by a newer atom's `supersedes_id` (self-referential FK with no `ON DELETE` action + `foreign_keys=ON` + oldest-first eviction). The failure aborted `sync_atoms_to_md()` before the MEMORY.md rebuild, so extracted atoms reached the DB but `.md` silently diverged — permanently, since the same row blocked every retry and `doctor --fix` does not evict. Deletes (eviction, `memory_remove`, `--split-entries`) now detach `supersedes_id` references first, and eviction runs inside a transaction. Full changelog: [HISTORY.md](HISTORY.md).

Upgrade: replace the binary, then run `asuna-memory doctor --fix` once if your MEMORY.md had diverged. No data migration.

### Upgrading from v2.5.1 to v2.5.2

v2.5.2 fixes a bounded-memory integrity bug where a single DB row could contain multiple `§`-separated entries, making `.md` and DB entry counts disagree and misleading `doctor`. New CLI flag `asuna-memory doctor --split-entries` splits any existing multi-entry rows (preserves metadata, skips duplicates, rebuilds `.md`); `doctor --fix` now runs the split automatically before merging. Full changelog: [HISTORY.md](HISTORY.md).

Upgrade: replace the binary, then run `asuna-memory doctor --split-entries` once. *(v2.5.0 cosine / `dimensions=768` / CORS steps still apply if you're coming from v2.4.x.)*

### Upgrading from v2.5.0 to v2.5.1

v2.5.1 fixes the `role` (and time) filters being **ignored** on the REST `/search` endpoint and the CLI `search` command — a request like `{"query":"x","role":"assistant"}` previously returned turns of all roles. `SearchRequest` now accepts `role`/`after`/`before`/`last_days`, and the CLI gains `--role`/`--after`/`--before`/`--last-days`, matching the MCP `search_sessions` tool. `/recall` is unaffected (it returns layered memory, not turns). Full changelog: [HISTORY.md](HISTORY.md).

Upgrade: replace the binary. No data migration. *(If coming from v2.4.x, the v2.5.0 steps below still apply.)*

### Upgrading from v2.4.1 to v2.5.0

v2.5.0 is a **security + correctness hardening** release. Vector search now uses **cosine distance**, embedding dimension mismatches fail loudly instead of silently, and ~40 issues from a full code review are fixed. Full changelog: [HISTORY.md](HISTORY.md).

**⚠️ Upgrade steps:**

1. Replace the binary and restart — `vec_turns` / `vec_bounded_memory` auto-migrate to the cosine metric.
2. **Run `asuna-memory rebuild`** to re-embed turn vectors (semantic/hybrid search over historical turns is keyword-only until this completes; bounded-memory atom vectors auto-backfill on startup).
3. **Local ONNX users**: set `embedding.dimensions` to match your model (EmbeddingGemma = 768) — a mismatch now errors instead of silently emptying the index.
4. **Gateway without auth**: CORS no longer defaults to "any origin" — set `gateway.cors_origins` or enable `auth_enabled` if a browser client needs cross-origin access.

Highlights: cosine semantic scores · `--mode vector/fts` aliases · loud dimension validation · localhost-only default CORS · read-only `sql` via `query_only` · superseded-vector de-indexing · in-batch dedup · filter-aware search (no under-return) · graph neighbor dedup · DashScope query/document `text_type` · pipeline & `/capture` no longer hold the DB lock across network calls · `/capture` INT8 vector fix · MCP panic isolation. **188 tests, 0 new clippy warnings.**

### Architecture: Project Aegis

Project Aegis is the production multi-layer hierarchical memory architecture (L0-L5) with HTTP REST gateway, agent framework integration, and MCP server.

🟢 **Multi-Layer Memory (L0-L5)**

- **L1 Atom Extraction** (P3): LLM-based automatic fact extraction from conversations with Evolution Chain versioning (`supersedes_id` pointer chain)
- **A-MAC Admission Scoring** (P4): 5-dimensional scoring (utility / novelty / recency / importance / confidence) for memory admission decisions
- **L2-L3 Scenario + Persona** (P5): Automatic scenario aggregation from related L1 atoms; persona generation from L2 scenarios; progressive disclosure retrieval engine
- **L4-L5 Mental Models + Intent** (P6): Abstract cognitive framework generation (work patterns, decision criteria, communication style); intent prediction for anticipatory memory
- **Skill Memory** (P7): Execution trace recording, pattern recognition (3+ occurrences), automatic SOP generation via LLM

🟢 **HTTP REST Gateway (P1)**

- axum-based HTTP server with 11 endpoints for agent framework integration
- Optional API key authentication (`Bearer` / `X-API-Key`)
- CORS configuration with origin allowlist
- 10MB request body limit

🟢 **Hermes Plugin + Docker (P9)**

- Python `AMSMemoryProvider` for Hermes Agent integration
- Automatic memory recall before responses, automatic storage after conversations
- Multi-stage Docker build with health checks

🟢 **Graph Enhancements (P8)**

- Multi-hop graph query from HTTP gateway
- Automatic graph integration for L1 atoms (entities + `mentions` / `supersedes` / `related_to` relations)
- `relation_kind` column for distinguishing asserted vs derived relations

🔵 **Security & Quality (Code Review)**

- API key comparison uses constant-time `subtle::ConstantTimeEq` (timing attack mitigation)
- `bounded_memory_fts` FTS5 table created with sync triggers for full-text search on bounded memory
- N+1 query elimination in search, recall, and bounded_memory list_entries (batch IN queries)
- Evolution chain cycle detection (HashSet + depth limit)
- Graph store transaction management via RAII `unchecked_transaction()`
- LIKE wildcard escaping and FTS5 operator injection prevention
- Model download SHA256 verification infrastructure

> **Historical changelog** (v1.0.x – v2.4.1): See [HISTORY.md](HISTORY.md)

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

Extract and install:

```bash
# Linux x64
tar xzf asuna-memory-linux-x64.tar.gz
sudo mv asuna-memory /usr/local/bin/
sudo mv libonnxruntime.so* /usr/local/lib/   # ONNX Runtime for semantic search

# macOS
tar xzf asuna-memory-macos-apple-silicon.tar.gz
sudo mv asuna-memory /usr/local/bin/
sudo mv libonnxruntime.dylib /usr/local/lib/

# Windows: extract zip, place asuna-memory.exe and onnxruntime.dll in PATH
```

> **Note**: The archive includes both the binary and ONNX Runtime library. The binary auto-discovers `libonnxruntime.so` from the same directory, `~/.asuna/lib/`, or standard system paths. If you only move the binary, ensure the `.so` is in one of these locations, or set `ORT_DYLIB_PATH` to its absolute path.

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

# Download embedding model (first install, ~300MB)
asuna-memory model-download

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

1. **ONNX Runtime (required)**: Semantic search requires the ONNX Runtime dynamic library. Pre-built packages from GitHub Release already include it; source builds need it placed manually. Falls back to keyword-only search if missing.
2. **Model files (recommended)**: Semantic search requires the `embeddinggemma-300m-q8` model (~300MB).
   - **Auto-download** (recommended): Run `asuna-memory model-download` to download from GitHub Release Assets to `~/.asuna/models/`
   - Manual placement: Download from [HuggingFace](https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX) and place in `~/.asuna/models/embeddinggemma-300m-q8/`
   - On Windows, supports `ASUNA_DEV_ROOT` env var for dev paths
   - Falls back to keyword search if not found

3. **Data directory**: Defaults to `~/.asuna/`. Created automatically on first run.
4. **Profile isolation**: Each profile's data is stored under `~/.asuna/profiles/{profile_id}/`.

---

## Architecture

**Protocol**: MCP stdio · JSON-RPC 2.0  
**Embedder**: Local ONNX (embeddinggemma-300m, 768d) or third-party API (configurable dimensions, default 1024d) · INT8 quantized

**Multi-layer memory architecture (Project Aegis)** — see [dedicated section below](#project-aegis--multi-layer-memory-architecture) for full L0-L5 details.

**Graph Layer (v1.3+)**: SQLite tables `entities` + `relations`, populated by the agent via `graph_assert`; canonical normalization (lowercase + trim + whitespace fold); no LLM calls, no rule-based extraction.

### Fact Layer

- **Conversation storage**: Each conversation archived as JSONL in `conversations/YYYY/MM/DD/`
- **Index**: SQLite stores session metadata and turn summaries
- **Full-text search**: FTS5 contentless virtual table with Chinese unigram tokenization (v1.1.3+ automatic schema migration)
- **Vector search**: sqlite-vec extension, INT8 quantized vectors (configurable dimensions, default 1024d), automatically written on save/import/rebuild
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
| `rebuild_index`     | Rebuild index from JSONL files (async, background)    |
| `rebuild_status`    | Query rebuild_index execution progress                |
| `memory_provenance` | Verify provenance of growth memory entries            |

Detailed parameter documentation in [for_ai.md](for_ai.md).

---

## Graph Memory (v1.3+)

The graph layer is a third layer alongside the fact and growth layers, reusing the same SQLite database with two new tables (`entities` + `relations`). The agent accumulates triples via `graph_assert`; the server does **not** call any LLM or run rule-based extraction. `canonical` normalization handles case drift but performs **no semantic merging** ("Alice" and "Alice Smith" are two separate nodes unless `graph_link_entity` is called explicitly).

### New MCP Tools

| Tool | Purpose |
|---|---|
| `graph_assert` | Write entity-relation triples (with confidence, source_turn) |
| `graph_neighbors` | Query N-hop neighbors (supports rel_type / direction / hops filters) |
| `graph_path` | Shortest path between two nodes (returns full alternating Entity/Edge sequence) |
| `graph_link_entity` | Alias merge: rewire all `from` edges to `to`, then delete `from` (irreversible) |
| `graph_prune_dangling` | Clean up dangling `source_turn` references: NULL out fields pointing to deleted turns (does NOT delete relations) |

### Soft Reminder

When `graph.enabled && graph.remind_on_save`, `save_session` returns `graph_pending: { turn_ids, hint }` listing the turn_ids from this session that are **not yet referenced by any relation**. Disable by setting `graph.remind_on_save = false` in config.json.

### Disabling Entirely

When `graph.enabled = false` all `graph_*` tools return `"graph disabled in config"`. The fact and growth layers are completely unaffected.

### Diagnostics

`asuna-memory doctor` shows `Graph: ENABLED (N entities, M relations)` by default.
Adding `--verbose` also displays graph coverage (fraction of turns referenced) and dangling references (source_turn pointing to deleted turns).

---

## Project Aegis — Multi-Layer Memory Architecture

Project Aegis extends the original dual-layer (fact + growth) architecture into a 6-layer hierarchical memory system (L0-L5), inspired by human cognitive models:

| Layer | Name | Storage | Description |
|-------|------|---------|-------------|
| L0 | Conversation | JSONL + SQLite `turns` | Raw conversation turns (existing fact layer) |
| L1 | Atom | SQLite `bounded_memory` | Atomic facts extracted from conversations via LLM |
| L2 | Scenario | Markdown files | Scene blocks aggregated from related L1 atoms |
| L3 | Persona | `USER.md` | User profile generated from L2 scenarios |
| L4 | Mental Model | Markdown files | Abstract cognitive frameworks (work patterns, decision criteria) |
| L5 | Intent Prediction | In-memory | Predicted future needs based on L4 patterns |

### Core Mechanisms

- **Evolution Chain** (P3): `supersedes_id` pointer chain tracks how facts evolve over time. `get_chain()` / `get_latest_version()` follow the chain with cycle detection.
- **A-MAC Admission Scoring** (P4): 5-dimensional scoring (utility, novelty, recency, importance, confidence) decides whether extracted facts are worth storing as long-term memory.
- **Progressive Disclosure Retrieval** (P5): Layered retrieval (L3→L2→L1→L0) with token budget control — higher layers get fewer tokens, lower layers fill remaining budget.
- **Skill Memory** (P7): Tracks agent problem-solving paths, recognizes repeated patterns (3+ occurrences), and auto-generates SOPs via LLM abstraction.
- **Graph Integration** (P8): L1 atoms automatically create graph entities and `mentions` / `supersedes` / `related_to` relations. Multi-hop queries traverse the graph.

### HTTP REST Gateway (P1)

An axum-based HTTP server for integration with agent frameworks (Hermes, LangChain, etc.):

```bash
asuna-memory gateway --port 8765
```

| Endpoint | Method | Status | Description |
|----------|--------|--------|-------------|
| `/health` | GET | ✅ | Health check |
| `/stats` | GET | ✅ | Database statistics |
| `/capture` | POST | ✅ | Save conversation turns |
| `/recall` | POST | ✅ | Progressive disclosure retrieval (L3→L2→L1→L0) |
| `/search` | POST | ✅ | Text or multi-hop graph search |
| `/persona` | GET | ✅ | Read user persona |
| `/offload` | POST | ✅ | Store long text to refs/ directory |
| `/recall/:node_id` | GET | ✅ | Recall offloaded text |
| `/graph/assert` | POST | ✅ | Write entity-relation triples |
| `/graph/neighbors` | POST | ✅ | Query N-hop neighbors |
| `/session/end` | POST | ✅ | Record session end timestamp |

Authentication: optional API key via `Authorization: Bearer <key>` or `X-API-Key` header. Enable with `gateway.auth_enabled = true` and set `AMS_GATEWAY_API_KEY`.

CORS: when `gateway.cors_origins` is empty and auth is disabled, the gateway allows **only localhost origins** (blocking public sites from cross-origin reading your memory). Set `cors_origins` to an explicit allowlist, or enable auth, to permit other origins.

### Hermes Plugin (P9)

Python plugin for [Hermes Agent](https://github.com/NousResearch/hermes-agent) integration. Implements the Hermes `MemoryProvider` ABC.

```bash
# Copy plugin to Hermes plugins directory
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
mkdir -p "$HERMES_HOME/plugins/ams_memory"
cp hermes-plugin/ams_memory/* "$HERMES_HOME/plugins/ams_memory/"
pip3 install requests
```

Activate in `~/.hermes/config.yaml`:

```yaml
memory:
  provider: ams_memory
```

Configure via environment variables or `~/.hermes/ams.json`:

| Env Var | JSON Key | Default | Description |
|---------|----------|---------|-------------|
| `AMS_GATEWAY_URL` | `gateway_url` | `http://127.0.0.1:8765` | Gateway URL |
| `AMS_API_KEY` | `api_key` | *(empty)* | Auth key |
| `AMS_RECALL_TOP_K` | `recall_top_k` | `5` | Memories per query |
| `AMS_AUTO_RECALL` | `auto_recall` | `true` | Auto recall |
| `AMS_AUTO_STORE` | `auto_store` | `true` | Auto store |

### Docker Support

```bash
docker build -t asuna-memory .
docker run -p 8765:8765 -v ~/.asuna:/home/asuna/.asuna asuna-memory
```

Multi-stage build: Rust builder → Debian slim runtime with Python3 + Hermes plugin pre-installed.

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

# Start HTTP REST gateway
asuna-memory gateway --port 8765

# Environment check
asuna-memory doctor

# Auto-fix DB/.md inconsistencies
asuna-memory doctor --fix

# Download embedding model (first install, ~300MB)
asuna-memory model-download

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

# Safely delete a turn (auto-cleans FTS + vector indexes)
asuna-memory delete-turn <id>

# Read-only SQL query
asuna-memory sql "SELECT id, preview FROM turns LIMIT 5"
```

### Global Parameters

| Parameter   | Default                | Description      |
| ----------- | ---------------------- | ---------------- |
| `--config`  | `~/.asuna/config.json` | Config file path |
| `--profile` | `default`              | Active profile   |

---

## Configuration

JSON format, default path `~/.asuna/config.json`. Uses built-in defaults if absent.
All fields are optional — only override what you need.

### Minimal Config (API embedding, recommended for VPS)

```json
{
  "embedding": {
    "dimensions": 1024,
    "batch_size": 10,
    "api_url": "https://dashscope.aliyuncs.com/api/v1",
    "api_key": "sk-your-key-here",
    "api_model": "text-embedding-v4"
  }
}
```

### Full Config Reference

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
    "security_scan": true,
    "atom_capacity_ratio": 0.3
  },
  "search": {
    "default_top_k": 5,
    "search_mode": "hybrid",
    "fts_enabled": true
  },
  "embedding": {
    "dimensions": 1024,
    "batch_size": 32,
    "api_url": "",
    "api_key": "",
    "api_model": "",
    "api_format": ""
  },
  "graph": {
    "enabled": true,
    "remind_on_save": true
  },
  "pipeline": {
    "enable_extraction": true,
    "every_n_turns": 5
  },
  "llm": {
    "base_url": "",
    "api_key": "",
    "model": ""
  },
  "gateway": {
    "auth_enabled": false,
    "api_key": "",
    "cors_origins": []
  }
}
```

### Embedding Fields

| Field | Default | Description |
|-------|---------|-------------|
| `dimensions` | `1024` | Vector dimensions for `vec0` tables. Switching requires `rebuild --full` |
| `batch_size` | `32` | Max texts per embedding API call. DashScope limits to 10 |
| `api_url` | `""` | OpenAI-compatible base URL. When set with `api_model`, uses API backend |
| `api_key` | `""` | API key. Also reads `AMS_EMBEDDING_API_KEY` env var |
| `api_model` | `""` | Model name (e.g. `text-embedding-v4`, `text-embedding-3-small`) |
| `api_format` | `""` | `"openai"` or `"dashscope"`. Auto-detected from `api_url` |

**Backend priority**: API (if `api_url` + `api_model` set) → Local ONNX → disabled (keyword-only search)

### Embedding Providers

The embedding backend is auto-detected based on configuration:

1. **API backend** — if both `api_url` and `api_model` are set, uses an OpenAI-compatible or DashScope HTTP API
2. **Local ONNX** — otherwise, uses the local model (requires model files + `libonnxruntime.so`, ~300MB RAM)
3. **Disabled** — if neither is available, falls back to keyword-only search

**Recommended: DashScope text-embedding-v4** (best multilingual Chinese, configurable dimensions):

```json
{
  "embedding": {
    "dimensions": 1024,
    "batch_size": 10,
    "api_url": "https://dashscope.aliyuncs.com/api/v1",
    "api_key": "sk-your-key-here",
    "api_model": "text-embedding-v4"
  }
}
```

**OpenAI-compatible API** (OpenAI, Ollama, vLLM, LiteLLM, etc.):

```json
{
  "embedding": {
    "dimensions": 1024,
    "api_url": "https://api.example.com/v1",
    "api_key": "your-key-here",
    "api_model": "text-embedding-3-small"
  }
}
```

- `api_url`: OpenAI-compatible base URL, or DashScope native URL
- `api_key`: Can also be set via `AMS_EMBEDDING_API_KEY` environment variable. Optional for local endpoints like Ollama
- `api_model`: Model name as recognized by the API endpoint
- `api_format`: `"openai"` (default) or `"dashscope"`. Auto-detected from `api_url` — URLs containing "dashscope" use DashScope format automatically
- `batch_size`: Max texts per API call (default 32). DashScope limits to 10
- `dimensions`: Vector dimensions (default 1024). Must match across all data **and your embedding model** — for the local ONNX model (EmbeddingGemma, 768) set this to 768. A mismatch now errors loudly. Switching requires `rebuild --full`

> **Note**: Switching between backends or changing `dimensions` requires rebuilding vectors: `asuna-memory rebuild --full`

---

```text
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
    └── embeddinggemma-300m-q8/
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
