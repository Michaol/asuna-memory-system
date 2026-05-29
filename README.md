# Asuna Memory System

> Long-term memory system for AI Agents — MCP Server

[中文](README_ZH.md) | [AI Agent Install Guide](for_ai.md)

## Upgrade Guide

### Project Aegis (v2.0.0-dev)

Project Aegis is the next-generation architecture extending v1.3.1 with multi-layer hierarchical memory (L0-L5), HTTP REST gateway, and agent framework integration. Currently in active development on the `feat/aegis-p1-transport` branch.

🟢 **New: Multi-Layer Memory (L0-L5)**

- **L1 Atom Extraction** (P3): LLM-based automatic fact extraction from conversations with Evolution Chain versioning (`supersedes_id` pointer chain)
- **A-MAC Admission Scoring** (P4): 5-dimensional scoring (utility / novelty / recency / importance / confidence) for memory admission decisions
- **L2-L3 Scenario + Persona** (P5): Automatic scenario aggregation from related L1 atoms; persona generation from L2 scenarios; progressive disclosure retrieval engine
- **L4-L5 Mental Models + Intent** (P6): Abstract cognitive framework generation (work patterns, decision criteria, communication style); intent prediction for anticipatory memory
- **Skill Memory** (P7): Execution trace recording, pattern recognition (3+ occurrences), automatic SOP generation via LLM

🟢 **New: HTTP REST Gateway (P1)**

- axum-based HTTP server with 11 endpoints for agent framework integration
- Optional API key authentication (`Bearer` / `X-API-Key`)
- CORS configuration with origin allowlist
- 10MB request body limit

🟢 **New: Hermes Plugin + Docker (P9)**

- Python `AMSProvider` for Hermes Agent integration
- Automatic memory recall before responses, automatic storage after conversations
- Multi-stage Docker build with health checks

🟢 **New: Graph Enhancements (P8)**

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

### Upgrading from v1.3.0 to v1.3.1

v1.3.1 fixes 6 known bugs from v1.3.0 and adds automatic model downloading plus several CLI safety tools.

Upgrade steps:

1. Replace the binary
2. Run `asuna-memory doctor --fix` to repair any DB/.md inconsistencies
3. Run `asuna-memory model-download` to download the embedding model (if not placed manually)
4. Run `asuna-memory doctor` to verify everything is OK

**v1.3.1 Changelog:**

🔴 **Critical Fixes**

- **Embedding dimension error**: tarball version outputs 10 dims instead of 768. Fixed to prefer `sentence_embedding` output (2D pooled), falling back to `last_hidden_state` + masked mean pooling
- **rebuild_index timeout**: MCP `rebuild_index` now runs asynchronously in background; new `rebuild_status` tool to query progress, no longer blocks until timeout
- **DB/.md desync**: Growth layer writes reordered to SQLite FIRST → .md LAST; added `reconcile_check` / `reconcile_fix` and `doctor --fix` repair command

🟡 **New Features**

- **`model-download` CLI**: Downloads EmbeddingGemma model (~300MB) from GitHub Release Assets, replacing manual placement
- **`delete-turn` CLI**: Safely deletes a turn (auto-cleans FTS + vector indexes, solves `tokenize_zh` UDF missing in external tools)
- **`sql` CLI**: Read-only SQL queries (in-process UDF available)
- **`doctor --fix`**: Automatically repairs DB/.md inconsistencies
- **`rebuild_status` MCP tool**: Query async rebuild progress
- **`graph` config detection**: `doctor` warns when `graph` section is missing from config.json

🔵 **Quality Improvements**

- `memory_update` / `memory_remove` now correctly pass `session_id` to audit log
- Removed 3 unused dependencies (`thiserror`, `reqwest`, `uuid`), net reduction of ~40 transitive crates
- Removed dead code modules (`download.rs`, `id.rs`) and 6 zero-call methods
- `server.rs` serialization failure no longer panics (falls back to internal error JSON)
- Magic numbers centralized (`MS_PER_DAY`, `JSONRPC_VERSION`)

<details>
<summary><strong>Historical changelog (click to expand)</strong></summary>

### Upgrading from v1.2.1 to v1.3.0

v1.3.0 adds a **graph memory layer** (the third layer) alongside the fact and growth layers. The fact and growth layers are untouched; existing data remains fully compatible.

Upgrade steps:

1. Replace the binary
2. On first launch `init_schema` automatically creates the `entities` + `relations` tables
3. Run `asuna-memory doctor`, expect:
   - `Graph: ENABLED (0 entities, 0 relations)`

No `rebuild` required: the graph is accumulated by the agent; rebuild has no meaning for graph data.

**v1.3.0 Changelog:**

🟢 **New: Graph Memory Layer**

- Two new tables (`entities`, `relations`) added to the same SQLite database — zero new dependencies
- 5 new MCP tools: `graph_assert` / `graph_neighbors` / `graph_path` / `graph_link_entity` / `graph_prune_dangling`
- canonical normalization (lowercase + trim + whitespace fold); no fuzzy or semantic merging
- `save_session` returns a `graph_pending` soft reminder listing turn_ids not yet referenced by the graph
- `doctor --verbose` shows graph coverage and dangling-reference diagnostics
- No LLM calls: graph content is asserted by the agent; no optional rule-based extraction was introduced

🟡 **API Surface**

- `Config.graph.enabled` / `Config.graph.remind_on_save` (serde defaults — zero migration for old configs)
- When `graph.enabled = false`, all graph MCP tools return `"graph disabled in config"`
- Binary size unchanged (no new crates)

### Upgrading from v1.2.0 to v1.2.1 (Strongly Recommended)

v1.2.1 is a **security and quality hardening** release that closes 1 Critical-severity **path traversal** vulnerability and several data-correctness issues. All users should upgrade as soon as possible.

```bash
# 1. Replace the binary

# 2. Rebuild index — query/document prefix split means new vectors recall noticeably better
asuna-memory rebuild

# 3. Verify (new fields surfaced)
asuna-memory doctor
# Expected:
#   版本: v1.2.1
#   外键约束: ON
```

**v1.2.1 Changelog:**

**🔴 Critical Fixes:**

- **Path Traversal**: `memory_write` / `memory_update` / `memory_remove` no longer trust the `target` argument as a path component. A strict whitelist (`memory` / `user`) is enforced, blocking `../../foo` style escapes.

**🟠 Important Fixes:**

- **EmbeddingGemma prefix separation**: Save/rebuild paths now use the `title: none | text:` (Document) prefix; search-query path uses `task: search result | query:` (Query). The two no longer share the query prefix, so vector recall quality is significantly better — **`rebuild` after upgrade is strongly recommended**.
- **JSONL/SQLite atomicity**: `save_session` is now _DB tx → commit → write JSONL_, with any tx error triggering automatic `ROLLBACK`. The old "JSONL written, DB half-written" residue state is eliminated.
- **LIKE wildcard injection**: `memory_update` / `memory_remove` SQLite `LIKE` clauses use `ESCAPE '\\'` and escape `% _ \`. Operations are also now **entry-level (§-separated)** to avoid silent edits of unrelated entries.
- **§ separator robustness**: Removing several adjacent entries no longer leaves `§§§`; removing the last entry leaves only the metadata header; removing the first no longer leaves a leading `\n§\n`.
- **Chinese long-content panic**: Audit-log content truncation switched from byte slicing to `chars().take(N)` — multi-byte characters can no longer panic.
- **Foreign keys**: `PRAGMA foreign_keys = ON` is now default to prevent dangling `session_id` in `turns`.
- **Strict save_session validation**: missing `timestamp` / `role` / `content` is rejected; `role` must be one of `user` / `assistant` / `tool_call` / `system` (no silent fallback to `user`).

**🟡 Minor Improvements:**

- **Dynamic ONNX padding**: tokenizer no longer pads to a fixed 2048; pads to the batch max instead, 5–20× faster for short previews.
- **Credential regex caching**: 5 credential regexes compiled once via `OnceCell`, no per-scan recompilation.
- **Model download integrity**: streams to `.partial` temp file, validates `Content-Length`, atomic rename — interruptions can no longer leave a half-downloaded file mistakenly treated as complete.
- **doctor enhancements**: surfaces version / foreign-keys / embedding dimensions.
- **Config wiring**: `conversation.preview_length` / `search.default_top_k` / `search.search_mode` / `memory.security_scan` now actually take effect.
- **e2e tests wired in**: 6 end-to-end tests (save-then-search, overwrite, delete residue, rebuild consistency) lifted from an orphan file into the test suite.
- **Dead column removed**: `turns.embedding BLOB` dropped from schema (vectors always live in `vec_turns` virtual table).
- **Dependency cleanup**: removed unused `indicatif`; added `once_cell` / `tempfile (dev)`.

> Note: the legacy `turns.embedding` column persists in pre-existing DBs (SQLite has no automatic column drop). It is unused and harmless.

### Upgrading from v1.1.4 to v1.2.0

v1.2.0 is a **reliability and security hardening** release, fixing 2 Critical data consistency issues and 8 Important functional defects.

```bash
# 1. Replace the binary

# 2. Rebuild index to apply char_count fix (bytes → characters)
asuna-memory rebuild

# 3. Verify
asuna-memory doctor
```

**v1.2.0 Changelog:**

**🔴 Critical Fixes:**

- **Transaction Safety**: All database writes in `save_session` and `rebuild` are now wrapped in `BEGIN IMMEDIATE ... COMMIT` transactions, preventing half-written inconsistent state on process crash

**🟡 Important Fixes:**

- **Streaming Model Download**: Large model files no longer loaded entirely into memory; uses streaming `io::copy` to disk, avoiding OOM in memory-constrained environments
- **char_count Correction**: `turns.char_count` field now stores Unicode character count instead of UTF-8 byte count (Chinese content was inflated 3x)
- **unsafe FFI Documentation**: Complete SAFETY comments and ABI compatibility notes added to the sqlite-vec extension registration `transmute`
- **Growth Layer update() Fix**: Replacement now operates on body only, preventing accidental metadata header modification; auto-updates timestamp; syncs changes to SQLite `bounded_memory` table
- **Growth Layer remove() Fix**: Delete operations now sync to SQLite `bounded_memory` table; `list_entries()` and `verify_provenance()` no longer return deleted entries
- **Query Optimization**: `list_entries()` merged duplicate queries for the same session_id
- **MCP Error Handling Documentation**: tools/call `content + isError` error format documented with MCP protocol spec reference

**🟢 Minor Improvements:**

- **Empty turns validation**: `save_session` validates non-empty turns before parsing, returning a friendly error instead of timestamp parse failure
- **Timestamp safety**: `unix_ms_to_iso()` uses epoch fallback for invalid timestamps, eliminating potential panics
- **Deprecated db_path field**: `db_path` in `config.json` marked as deprecated (actual DB path determined by `profile_db_path()`), backward compatible

### Upgrading from v1.1.3 to v1.1.4

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

</details>

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
**Embedder**: embeddinggemma-300m (ONNX) · 768d INT8 quantized

**Multi-layer memory architecture (Project Aegis)** — see [dedicated section below](#project-aegis--multi-layer-memory-architecture) for full L0-L5 details.

**Graph Layer (v1.3+)**: SQLite tables `entities` + `relations`, populated by the agent via `graph_assert`; canonical normalization (lowercase + trim + whitespace fold); no LLM calls, no rule-based extraction.

### Fact Layer

- **Conversation storage**: Each conversation archived as JSONL in `conversations/YYYY/MM/DD/`
- **Index**: SQLite stores session metadata and turn summaries
- **Full-text search**: FTS5 contentless virtual table with Chinese unigram tokenization (v1.1.3+ automatic schema migration)
- **Vector search**: sqlite-vec extension, 768-dim INT8 quantized vectors, automatically written on save/import/rebuild
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

### Hermes Plugin (P9)

Python plugin for [Hermes Agent](https://github.com/NousResearch/hermes-agent) integration:

```bash
pip install -e hermes-plugin/
```

```python
from ams_memory import AMSProvider

provider = AMSProvider({
    "gateway_url": "http://127.0.0.1:8765",
    "auto_recall": True,
    "auto_store": True,
})
```

### Docker Support

```bash
docker build -t asuna-memory .
docker run -p 8765:8765 -v ~/.asuna:/data/asuna asuna-memory
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
    "model_name": "embeddinggemma-300m-q8",
    "dimensions": 768,
    "batch_size": 32
  },
  "graph": {
    "enabled": true,
    "remind_on_save": true
  },
  "gateway": {
    "auth_enabled": false,
    "api_key": "",
    "cors_origins": []
  },
  "model_path": null
}
```

---

## Data Directory Structure

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
