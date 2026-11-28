# Asuna Memory System

> Long-term memory system for AI Agents — MCP Server & HTTP Gateway

[![Quality gate status](https://sonarcloud.io/api/project_badges/measure?project=Michaol_asuna-memory-system&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=Michaol_asuna-memory-system)

> The SonarCloud badge is produced by CI: the `sonar` job in GitHub Actions scans `main` on every push (config: [sonar-project.properties](sonar-project.properties)). The enforced gates are GitHub Actions: fmt / clippy `-D warnings` / cargo test / plugin pytest, plus a Docker build smoke job — see [.github/workflows/ci.yml](.github/workflows/ci.yml).

[中文](README_ZH.md) | [AI Agent Install Guide](for_ai.md) | [Changelog History](HISTORY.md)

## Upgrade Guide

### Upgrading from v3.0.0 to v3.1.0

v3.1.0 is a concurrency + build-correctness release: it adds **cross-process multi-writer support** (1 gateway + N MCP `serve` + CLI commands on the same profile), fixes the **no-AVX release build baseline** (release binaries no longer embed unguarded AVX2 C code that SIGILLs on CPUs without AVX), and tunes **embedding batching** for DashScope qwen3.7 models. Full changelog: [HISTORY.md](HISTORY.md).

**Breaking (must-read)**: (1) The v3.0.0 **1024-dim lock** still applies — `embedding.dimensions != 1024` is rejected at load; run `asuna-memory rebuild --full` once when migrating pre-v3.0 vector indexes. (2) **Overwrite failure contract changed**: `save_session` / CLI import pre-write the JSONL outside the tx and rename it inside the tx; a prepare/rename failure now rolls the DB back atomically (the residual rename-OK/COMMIT-fail window heals via the next save or `rebuild --full`). (3) **Multi-writer version lower bound**: all concurrent writers on a profile must be >= v3.1.0; mixed old/new binaries are not covered by the multi-writer guarantees. (4) `/capture` SQLITE_BUSY/LOCKED now retries up to 2 times and then returns **503 Service Unavailable** (retryable) instead of 500. (5) Carried from v2.7.0: a non-empty `AMS_GATEWAY_API_KEY` env var still implies auth enabled; set `AMS_GATEWAY_AUTH_ENABLED=false` to explicitly opt out on loopback.

Upgrade: replace the binary; **no schema migration** (`user_version = 6` unchanged); run `asuna-memory rebuild --full` only if migrating pre-v3.0 vectors. Network filesystems remain forbidden (SQLite WAL); concurrent writers must share `embedding.dimensions`. Source builds need Rust **1.89+**.

Upgrade guides for older versions (v3.0.0 and earlier) live in [HISTORY.md](HISTORY.md).

### Architecture: Project Aegis

Project Aegis is a 6-layer hierarchical memory architecture (L0-L5) with an axum HTTP REST gateway, Hermes Agent integration, Knowledge Graph layer, and JSON-RPC 2.0 MCP stdio server.

🟢 **Multi-Layer Memory (L0-L5)**

- **L0 Conversation**: JSONL source-of-truth archives + SQLite `turns` + `turns_fts` (jieba tokenizer, `LIKE` fallback for punctuation-only queries)
- **L1 Atom Extraction** (P3): LLM-based automatic fact extraction with lock-free JSONL turn loading, A-MAC batched admission scoring, Evolution Chain versioning (`supersedes_id` pointer chain), and hybrid FTS5 + `vec_bounded_memory` RRF recall
- **L2 Scenario** (P5, opt-in): Semantic clustering (reusing Phase 3 atom embeddings) + LLM summarization stored in `bounded_memory` (`memory_type='scenario'`) + `memory/scenarios/*.md` mirrors
- **L3 Persona** (P5, opt-in): Generated user profile (`memory/persona.md`) synthesized from L2 scenarios with multi-row DB fallback and token-budget capping (1200-char ceiling, 40% share cap)
- **L4 Mental Models & L5 Intent Prediction** (P6, opt-in): Single-call batched cognitive framework (`memory/mental_models/*.md`) and intent prediction (`memory/intent/*.md`) generation with per-document fallback and 7-day freshness gating on `/recall`

🟢 **HTTP REST Gateway (P1)**

- axum HTTP server with 11 endpoints, `spawn_blocking` DB/FS offloading, 60s request timeout, and graceful SIGTERM/Ctrl+C shutdown
- Constant-time `Authorization: Bearer` (case-insensitive) / `X-API-Key` authentication + localhost-only CORS by default (auth on or off; explicit `gateway.cors_origins` overrides); all CORS configurations explicitly allow the `Authorization`, `Content-Type`, and `X-API-Key` headers
- Single-session dedup & bounded semaphore concurrency (`MAX_CONCURRENT_PIPELINES = 4`) on `/session/end`

🟢 **Hermes Plugin + Docker (P9)**

- Python `AMSMemoryProvider` with ordered single FIFO background queue (bounded 128) and isolated `requests.Session`
- Thread-safe recall & automatic turn capture + `/session/end` flush
- Multi-stage Rust 1.89 Docker build with hash-safe runtime environment

🟢 **Graph Enhancements (P8)**

- Bounded BFS traversal — HTTP `/search` (multi-hop) + `/graph/neighbors` and MCP `graph_neighbors` cap visited nodes at `10_000` (`MAX_VISITED_NODES` / `MAX_NEIGHBOR_VISITED`); MCP `graph_path` caps node expansions at `5_000` (`MAX_PATH_RESULTS`)
- Automatic graph integration for L1 atoms (`mentions` / `supersedes` / `from_session` / `related_to` relations)

🔵 **Security & Storage Integrity**

- Per-vector absmax INT8 quantization (`127.0 / max_abs`) with non-finite vector guards
- `Db::immediate_tx()` RAII write transactions (`PRAGMA user_version = 6`), narrow FTS update triggers, and `util::atomic_write` for JSONL and L1–L5 `.md` files
- BiDi/Unicode-tag stripping, full-width normalization, credential redaction, untrusted-data XML delimiters + recall escaping, and read-only SQLite Authorizer on `asuna-memory sql`
- Full L1–L5 write-side security scanning + canonical containment checks across JSONL cleanup and `/offload`

> **Historical changelog** (v1.0.x – v2.7.3): See [HISTORY.md](HISTORY.md)

---

## Installation

### Option 1: Download from GitHub Release (Recommended)

Go to [Releases](https://github.com/Michaol/asuna-memory-system/releases) and download the pre-built archive for your platform — it ships the binary plus `NOTICE.md` and `LICENSES/` (verified via `SHA256SUMS.txt`):

| Platform            | File                                      |
| ------------------- | ----------------------------------------- |
| Windows x86_64      | `asuna-memory-windows-x64.zip`            |
| Linux x86_64        | `asuna-memory-linux-x64.tar.gz`           |
| Linux ARM64         | `asuna-memory-linux-arm64.tar.gz`         |
| macOS Apple Silicon | `asuna-memory-macos-apple-silicon.tar.gz` |

Extract and install:

```bash
# Linux x64
tar xzf asuna-memory-linux-x64.tar.gz
sudo mv asuna-memory /usr/local/bin/

# macOS
tar xzf asuna-memory-macos-apple-silicon.tar.gz
sudo mv asuna-memory /usr/local/bin/

# Windows: extract asuna-memory-windows-x64.zip and place asuna-memory.exe in PATH
```

### Option 2: Build from Source

Requires **Rust 1.89+** (matches `rust-version` in `Cargo.toml`). No external database needed — SQLite is bundled.

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system
cargo build --locked --release
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

> **For AI Agents**: If you are an AI Agent installing or operating this system, read [for_ai.md](for_ai.md) for the complete command script and endpoint schemas.

### Important Notes

1. **Embedding API (optional for semantic search)**: Configure `embedding.api_url` + `embedding.api_model` (+ `AMS_EMBEDDING_API_KEY`) for **1024-dim** semantic/hybrid search. When unconfigured, AMS runs cleanly in keyword-only mode.
2. **Data directory**: Defaults to `~/.asuna/`. Created automatically on first run.
3. **Profile isolation**: Each profile's data is stored under `~/.asuna/profiles/{profile_id}/`.

---

## Architecture

**Protocol**: MCP stdio · JSON-RPC 2.0  
**Embedder**: Online 1024-dim API (OpenAI-compatible or DashScope) · Per-vector absmax INT8 quantized · Keyword-only fallback when unconfigured

**Multi-layer memory architecture (Project Aegis)** — see [dedicated section below](#project-aegis--multi-layer-memory-architecture) for full L0-L5 details.

**Graph Layer (v1.3+)**: SQLite tables `entities` + `relations`, populated by the agent via `graph_assert` and derived by L1 integration; canonical normalization (lowercase + trim + whitespace fold).

### Fact Layer

- **Conversation storage**: Each conversation archived as JSONL in `conversations/YYYY/MM/DD/`
- **Index**: SQLite WAL stores session metadata and turn summaries (`user_version = 6`)
- **Full-text search**: FTS5 virtual tables (`turns_fts`, `bounded_memory_fts`) using our inline MIT-licensed `JiebaTokenizer` (`tokenize='jieba'`, sharing one in-process `jieba-rs 0.11` dictionary with query-side OR fallback; preserves `.`/`_`/`-` ASCII compounds and keeps pure Chinese segmentation unchanged; see [NOTICE.md](NOTICE.md)) and multi-word `AND`/`OR` safe query building
- **Vector search**: sqlite-vec extension, 1024-dim absmax INT8 quantized vectors (`vec_turns`, `vec_bounded_memory`)
- **Hybrid search**: Deterministic Reciprocal Rank Fusion (RRF) combining semantic + keyword candidates

### Growth Layer

- **Bounded memory**: `MEMORY.md` (AI knowledge, 2200 char limit) and `USER.md` (user profile, 1375 char limit)
- **Entry separator**: Uses `§` between entries
- **Security scan**: Detection of prompt injection, credential leakage, and invisible BiDi/Tag Unicode before persistence across L1–L5 and `reconcile_fix`
- **Provenance tracking**: Each memory entry traces back to its source conversation session

---

## MCP Tools

| Tool                   | Description                                           |
| ---------------------- | ----------------------------------------------------- |
| `save_session`         | Save a complete conversation (auto-generates vectors) |
| `search_sessions`      | Multi-dimensional historical conversation search      |
| `memory_write`         | Write a new entry to growth memory                    |
| `memory_update`        | Update an existing memory entry via substring match   |
| `memory_remove`        | Remove a memory entry                                 |
| `memory_read`          | Read the full growth memory content                   |
| `user_profile`         | Read/write user profile                               |
| `rebuild_index`        | Rebuild index from JSONL files (async, background)    |
| `rebuild_status`       | Query rebuild_index execution progress                |
| `memory_provenance`    | Verify provenance of growth memory entries            |
| `graph_assert`         | Write entity-relation triples                         |
| `graph_neighbors`      | Query N-hop neighbors                                 |
| `graph_path`           | Bounded BFS shortest path between two entities        |
| `graph_link_entity`    | Alias merge (`from` -> `to`)                          |
| `graph_prune_dangling` | Null out dangling `source_turn` references            |

Detailed parameter documentation in [for_ai.md](for_ai.md).

---

## Graph Memory (v1.3+)

The graph layer lives alongside the fact and growth layers in SQLite (`entities` + `relations`). Agents assert triples via `graph_assert`; `canonical` normalization handles case and whitespace drift, while explicit alias merging uses `graph_link_entity`.

### Soft Reminder

When `graph.enabled && graph.remind_on_save`, `save_session` returns `graph_pending: { turn_ids, hint }` listing turn IDs not yet referenced by any relation. Disable with `graph.remind_on_save = false` in `config.json`.

### Disabling Entirely

When `graph.enabled = false`:
- MCP `graph_*` tools return `"graph disabled in config"`.
- HTTP REST `/graph/*` and multi-hop `/search` return `403 Forbidden`.
- Post-session L1/L2/L3-L5 pipeline early-exits. The fact and manual growth layers are unaffected.

### Diagnostics

`asuna-memory doctor` prints `图谱: ENABLED (N entities, M relations)` by default (CLI output labels are in Chinese).
Adding `--verbose` displays graph coverage and dangling references.

---

## Project Aegis — Multi-Layer Memory Architecture

Project Aegis organizes long-term memory across 6 hierarchical layers (L0-L5):

| Layer | Name              | Storage                                                                  | Description                                                                   |
| ----- | ----------------- | ------------------------------------------------------------------------ | ----------------------------------------------------------------------------- |
| L0    | Conversation      | JSONL + SQLite `turns` + `turns_fts` + `vec_turns`                       | Raw conversation turns & hybrid retrieval                                     |
| L1    | Atom              | SQLite `bounded_memory` + `bounded_memory_fts` + `vec_bounded_memory`    | Atomic facts extracted from conversations via LLM (hybrid FTS+vector recall)  |
| L2    | Scenario          | SQLite rows (`memory_type='scenario'`) + `memory/scenarios/*.md` mirrors | Scene blocks clustered using Phase 3 atom embeddings (opt-in)                 |
| L3    | Persona           | `memory/persona.md` (generated); `USER.md` / `bounded_memory`            | User profile generated from L2 scenarios (1200-char / 40% budget protection)  |
| L4    | Mental Model      | `memory/mental_models/*.md`                                              | Abstract cognitive frameworks (batched single-call generation)                |
| L5    | Intent Prediction | `memory/intent/*.md`                                                     | Predicted future needs & topics (batched single-call generation)              |

### Core Mechanisms

- **Evolution Chain** (P3): Confidence-gated `supersedes_id` pointer chain tracks fact evolution; all read surfaces exclude superseded rows.
- **A-MAC Admission Scoring** (P4): 5-dimensional scoring (utility, novelty, recency, importance, confidence) batched into one JSON LLM utility call.
- **Progressive Disclosure Retrieval** (P5): Layered retrieval (`L3 -> L4 -> L5 -> L2 -> L1 -> L0`) with token budget control and query-vector pre-embedding off the DB lock.
- **Graph Integration** (P8): L1 atoms automatically create graph entities and `mentions` / `supersedes` / `from_session` relations.

### HTTP REST Gateway (P1)

```bash
asuna-memory gateway --port 8765
```

| Endpoint            | Method | Description                                                   |
| ------------------- | ------ | ------------------------------------------------------------- |
| `/health`           | GET    | Health check (`{"status":"ok","version":"3.1.0"}`)            |
| `/stats`            | GET    | Database statistics                                           |
| `/capture`          | POST   | Save conversation turns                                       |
| `/recall`           | POST   | Progressive disclosure retrieval (`L3->L4->L5->L2->L1->L0`)   |
| `/search`           | POST   | Text (`hybrid`/`semantic`/`keyword`) or multi-hop graph search|
| `/persona`          | GET    | Read user persona (`USER.md` -> `persona.md` -> DB)           |
| `/offload`          | POST   | Store long text to `refs/` directory                          |
| `/recall/*node_id`  | GET    | Recall offloaded text (wildcard unencoded path supported)     |
| `/graph/assert`     | POST   | Write entity-relation triples (numeric or string confidence)  |
| `/graph/neighbors`  | POST   | Query N-hop neighbors                                         |
| `/session/end`      | POST   | Record session end & trigger bounded async pipeline           |

Authentication (all endpoints except `/health`): optional API key via `Authorization: Bearer <key>` (case-insensitive) or `X-API-Key` header. Setting a non-empty `AMS_GATEWAY_API_KEY` enables auth automatically (opt out on loopback only via `AMS_GATEWAY_AUTH_ENABLED=false`). Non-loopback binds (`gateway.bind_host` / `AMS_GATEWAY_BIND_HOST`) require auth or startup refuses.

CORS: when `gateway.cors_origins` is empty, only localhost/loopback origins are permitted — with auth enabled too (v4 hardening; was `Any`). Set an explicit origin list (or `*`) to allow web clients. All CORS configurations explicitly allow the `Authorization`, `Content-Type`, and `X-API-Key` headers.

### Hermes Plugin (P9)

Python plugin for [Hermes Agent](https://github.com/NousResearch/hermes-agent) implementing the `MemoryProvider` ABC (`requests` only; wheels-only lockfile `hermes-plugin/requirements-ci.txt` in CI).

```bash
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
mkdir -p "$HERMES_HOME/plugins/ams_memory"
cp hermes-plugin/ams_memory/*.py "$HERMES_HOME/plugins/ams_memory/"
python3 -m pip install --require-hashes --only-binary :all: -r hermes-plugin/requirements.txt
```

Activate in `$HERMES_HOME/config.yaml`:

```yaml
memory:
  provider: ams_memory
```

Configure via env vars or `$HERMES_HOME/ams.json`:

| Env Var            | JSON Key       | Default                 | Description         |
| ------------------ | -------------- | ----------------------- | ------------------- |
| `AMS_GATEWAY_URL`  | `gateway_url`  | `http://127.0.0.1:8765` | Gateway URL         |
| `AMS_API_KEY`      | `api_key`      | *(empty)*               | Auth key            |
| `AMS_RECALL_TOP_K` | `recall_top_k` | `5`                     | Memories per query  |
| `AMS_AUTO_RECALL`  | `auto_recall`  | `true`                  | Auto recall         |
| `AMS_AUTO_STORE`   | `auto_store`   | `true`                  | Auto store          |

### Docker Support

```bash
docker build -t asuna-memory .

# Create the host data dir FIRST: if ~/.asuna is missing, dockerd creates it
# as root:root and the container's non-root asuna user (uid 1000) cannot write
# to it — the entrypoint doctor fails on the very first run.
mkdir -p ~/.asuna

docker run -p 127.0.0.1:8765:8765 -v ~/.asuna:/home/asuna/.asuna \
  -e AMS_GATEWAY_BIND_HOST=0.0.0.0 -e AMS_GATEWAY_API_KEY=your-secret-key \
  asuna-memory
```

Multi-stage build: Rust 1.89 builder -> Debian slim runtime (`non-root` UID/GID `asuna`, Python `/opt/venv` + Hermes plugin pre-installed). Entrypoint extracts `--config`/`--profile` (space or `=` form) and forwards them to the pre-start `asuna-memory doctor` check, then launches the gateway on `AMS_GATEWAY_PORT` (default `8765`); the runtime image ships `/app/NOTICE.md` and `/app/LICENSES/`.

> **Bind-mount ownership**: `mkdir -p ~/.asuna` makes the host dir owned by your host user — writable by the container only when the uids match (image default `1000`). If your host uid differs, build with `--build-arg UID=$(id -u) --build-arg GID=$(id -g)` or use a named volume (`-v ams-data:/home/asuna/.asuna`) instead. The named volume in `hermes-plugin/docker-compose.yml` is unaffected by this issue.

---

## JSONL File Format

`import` and `rebuild` read JSONL files: **1 Header line + N Turn lines**, one JSON object per line.

### Header (line 1)

| Field         | Type     | Required | Description                                            |
| ------------- | -------- | -------- | ------------------------------------------------------ |
| `v`           | integer  | yes      | Format version, currently `1`                          |
| `type`        | string   | yes      | Always `"session_header"`                              |
| `session_id`  | string   | yes      | Unique session ID (max 255 chars, no path/control chars or `:`)|
| `start_time`  | string   | yes      | ISO 8601 timestamp (e.g., `2026-04-10T10:02:00+08:00`) |
| `profile_id`  | string   | yes      | Profile ID (`[A-Za-z0-9_-]+`, default `"default"`)     |
| `source`      | string   | no       | Source identifier                                      |
| `agent_model` | string   | no       | Agent model name                                       |
| `title`       | string   | no       | Session title                                          |
| `tags`        | string[] | no       | Tag list                                               |

### Turn (lines 2+)

| Field          | Type    | Required | Description                                             |
| -------------- | ------- | -------- | ------------------------------------------------------- |
| `ts`           | string  | yes      | ISO 8601 timestamp                                      |
| `seq`          | integer | yes      | Turn sequence number, starts at `1`                     |
| `role`         | string  | yes      | `"user"` / `"assistant"` / `"tool_call"` / `"system"`   |
| `content`      | string  | yes      | Turn content                                            |
| _extra fields_ | any     | no       | Flattened metadata                                      |

## CLI Commands

```bash
asuna-memory serve                                 # Start MCP stdio server (default)
asuna-memory gateway --port 8765                   # Start HTTP REST gateway
asuna-memory doctor [--verbose|--fix|--split-entries]
asuna-memory list-profiles
asuna-memory list-sessions --last-days 7 --limit 20
asuna-memory search "Rust async" --mode hybrid --top-k 5
asuna-memory rebuild [--full]                      # Rebuild FTS + 1024-dim vector index from JSONL
asuna-memory import session.jsonl
asuna-memory export <session_id>
asuna-memory delete-turn <id>
asuna-memory sql "SELECT id, preview FROM turns LIMIT 5"   # Read-only SQLite Authorizer
```

### Global Parameters

| Parameter   | Default                | Description      |
| ----------- | ---------------------- | ---------------- |
| `--config`  | `~/.asuna/config.json` | Config file path |
| `--profile` | `default`              | Active profile   |

---

## Configuration

JSON format, default `~/.asuna/config.json`. All sections/keys are optional (`{}` boots); precedence `config.json > env > defaults` (except `AMS_GATEWAY_AUTH_ENABLED` explicitly overrides auth enablement).

### Minimal Config (Online 1024-dim API Embedding + LLM)

```json
{
  "embedding": {
    "dimensions": 1024,
    "batch_size": 20,
    "api_url": "https://dashscope.aliyuncs.com/api/v1",
    "api_key": "sk-your-key-here",
    "api_model": "qwen3.7-text-embedding-flash"
  },
  "llm": {
    "base_url": "https://api.deepseek.com/v1",
    "api_key": "sk-your-llm-key",
    "model": "deepseek-chat"
  }
}
```

> **Endpoint note**: the shared domain `dashscope.aliyuncs.com` is verified to serve `qwen3.7-text-embedding-flash` (tested 2026-11-28). It has been in maintenance since 2026-09-30; official docs recommend workspace domains for new deployments: `https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/api/v1` (find `{WorkspaceId}` / the ready-to-copy API Host in the console's workspace management page; workspace domains only accept that workspace's API keys, otherwise HTTP 401). Both domain families use the same native path and are auto-detected by AMS.
>
> **Model note**: `qwen3.7-text-embedding-flash` defaults to 1024 dims (also 768/512/256, but AMS v3.0.0 locks 1024) and accepts up to 20 texts per native-format request (v3/v4: 10); AMS clamps `batch_size` per model. On `compatible-mode` endpoints, flash's `dimensions` parameter and batch cap are not officially documented, so AMS conservatively clamps to 10 — prefer the native format and run one semantic search to verify.

### Full Config Reference

```json
{
  "data_dir": "~/.asuna",
  "profile_id": "default",
  "conversation": {
    "preview_length": 200
  },
  "memory": {
    "memory_char_limit": 2200,
    "user_char_limit": 1375,
    "security_scan": true,
    "atom_capacity_ratio": 0.3
  },
  "search": {
    "default_top_k": 5,
    "search_mode": "hybrid"
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
  "admission": {
    "enabled": true,
    "threshold": 0.6,
    "weights": [0.3, 0.2, 0.2, 0.2, 0.1]
  },
  "recall": {
    "token_budget": 2000
  },
  "scenarios": {
    "enabled": false,
    "similarity_threshold": 0.8,
    "min_cluster_size": 2,
    "max_scenarios": 50
  },
  "persona": {
    "trigger_every_n": 10
  },
  "llm": {
    "base_url": "",
    "api_key": "",
    "model": ""
  },
  "gateway": {
    "auth_enabled": false,
    "api_key": "",
    "cors_origins": [],
    "bind_host": ""
  }
}
```

> **LLM env fallbacks**: when `llm.*` fields are empty, AMS checks `AMS_LLM_BASE_URL` / `OPENAI_BASE_URL`, `AMS_LLM_API_KEY` / `OPENAI_API_KEY` (`OPENAI_API_KEY` is skipped when a custom base URL is set — config.json `llm.base_url` or `AMS_LLM_BASE_URL`), and `AMS_LLM_MODEL` / `OPENAI_MODEL` (built-in default model `"deepseek-v3"`).

### Embedding Fields (v3.0.0)

| Field        | Default | Description                                                                                 |
| ------------ | ------- | ------------------------------------------------------------------------------------------- |
| `dimensions` | `1024`  | Locked to `1024` in v3.0.0; explicit non-1024 values fail validation with `rebuild --full`  |
| `batch_size` | `32`    | Max texts per embedding API call (`1..=1024`; DashScope auto-clamp: qwen3.7-text-embedding family native format `20`, other DashScope models/compatible-mode `10`) |
| `api_url`    | `""`    | OpenAI-compatible base URL or DashScope native base URL (incl. workspace domains `*.maas.aliyuncs.com`) |
| `api_key`    | `""`    | API key (falls back to `AMS_EMBEDDING_API_KEY`; optional for unauthenticated local servers) |
| `api_model`  | `""`    | Embedding model name (e.g. `qwen3.7-text-embedding-flash`, `text-embedding-v4`, `text-embedding-3-small`) |
| `api_format` | `""`    | `"openai"` or `"dashscope"`; native DashScope URLs (shared or workspace domain) auto-detect, `compatible-mode` URLs stay OpenAI format |

---

## Data Directory Layout

```text
~/.asuna/
├── config.json
└── profiles/
    └── default/
        ├── memory.db                   # SQLite WAL (turns, FTS5, vec_turns/vec_bounded_memory, graph, audit_log)
        ├── conversations/
        │   └── 2026/04/10/
        │       └── 20260410T100200_a1b2c3d4.jsonl   # sha256(session_id) first 8 hex
        ├── refs/                       # /offload long-text storage
        └── memory/
            ├── MEMORY.md
            ├── USER.md
            ├── persona.md              # L3 (when consolidation runs)
            ├── scenarios/              # L2 mirror files ({created_at}_{db_id}.md)
            ├── mental_models/          # L4 docs
            └── intent/                 # L5 docs
```

---

## Security

Automatic security scanning (`scan_content`) strips invisible BiDi/Unicode-tag characters, normalizes full-width homoglyphs, and detects prompt injections, leaked credentials (OpenAI, GitHub, AWS, PEM, PGP private key blocks), and zero-width characters across **L1–L5** and manual paths:

- **Hard gates (rejected/skipped before persistence)**: growth-layer writes/updates (`memory_write`, `memory_update`, `reconcile_fix`), L1 extracted atoms (skipped + audited), L2 scenario titles/summaries, L3 `persona.md` fields, L4 mental-model items, L5 intent items, `graph_assert`, `graph_link_entity`, and `/offload`.
- **Soft audit (raw evidence kept, flagged in `audit_log`)**: L0 conversation turns saved via `/capture`, `save_session`, or CLI `import` are persisted verbatim for historical accuracy and recorded in `audit_log`.
- **Framing & redaction**: LLM extraction/scoring/consolidation prompts isolate transcripts inside XML untrusted-data tags; every retrieval surface (`/recall` banner, Hermes plugin `<recalled_memories>` block with delimiter escaping) marks recalled memories as data, never commands; logs and audit entries redact detected credentials.

---

## License

MIT

Third-party notices: the inline FTS5 `JiebaTokenizer` (`src/index/jieba_tokenizer.rs`) and the FTS5 v2 FFI shim (`src/index/fts5_ffi.rs`) are adapted from `sqlite-simple-tokenizer` under the MIT license — see [NOTICE.md](NOTICE.md) and [LICENSES/sqlite-simple-tokenizer-MIT](LICENSES/sqlite-simple-tokenizer-MIT).
