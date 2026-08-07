# Asuna Memory System — AI Agent Integration Guide

This document is for AI Agents only. It covers installation, MCP server startup, tool parameters, and usage patterns. Concise format optimized for token efficiency.

**Server version covered:** v2.6.1 (Project Aegis)

## 1. Install

### Option A: Download pre-built package (recommended)

Download from [GitHub Releases](https://github.com/Michaol/asuna-memory-system/releases). Each archive includes the binary + ONNX Runtime library:

- Windows x64: `asuna-memory-windows-x64.exe.zip`
- Linux x64: `asuna-memory-linux-x64.tar.gz`
- Linux ARM64: `asuna-memory-linux-arm64.tar.gz`
- macOS Apple Silicon: `asuna-memory-macos-apple-silicon.tar.gz`

```bash
# Linux x64
curl -sL https://github.com/Michaol/asuna-memory-system/releases/latest/download/asuna-memory-linux-x64.tar.gz | tar xz
sudo mv asuna-memory /usr/local/bin/
sudo mv libonnxruntime.so* /usr/local/lib/

# macOS Apple Silicon
curl -sL https://github.com/Michaol/asuna-memory-system/releases/latest/download/asuna-memory-macos-apple-silicon.tar.gz | tar xz
sudo mv asuna-memory /usr/local/bin/
sudo mv libonnxruntime.dylib /usr/local/lib/
```

### Option B: Build from source

Requires: Rust 1.75+, Windows/Linux/macOS.

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system
cargo build --release
# Binary: target/release/asuna-memory (.exe on Windows)
```

No external dependencies. SQLite is bundled. ONNX Runtime and model files are optional (semantic search falls back to keyword search if absent).

## 2. Download Embedding Model

Semantic search requires the `embeddinggemma-300m-q8` model (~300MB). Download from GitHub Release Assets:

```bash
asuna-memory model-download
```

This downloads 6 files (ONNX model + tokenizer) to `~/.asuna/models/embeddinggemma-300m-q8/`. Without this step, only keyword search is available.

Alternative: download manually from [HuggingFace](https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX) and place in `~/.asuna/models/embeddinggemma-300m-q8/`.

### Option B: Third-party API (for VPS with limited RAM)

Instead of running the ONNX model locally, configure an embedding API in `~/.asuna/config.json`. When both `api_url` and `api_model` are set, the API backend is used automatically (takes priority over local ONNX):

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

**Embedding fields:**

| Field | Default | Description |
|-------|---------|-------------|
| `dimensions` | `1024` | Vector dimensions for `vec0` tables (cosine distance). Must match the embedding model (local ONNX EmbeddingGemma = 768); a mismatch errors. Switching requires `rebuild --full` |
| `batch_size` | `32` | Max texts per embedding API call. DashScope limits to 10 |
| `api_url` | `""` | OpenAI-compatible or DashScope base URL |
| `api_key` | `""` | API key. Also reads `AMS_EMBEDDING_API_KEY` env var |
| `api_model` | `""` | Model name (e.g. `text-embedding-v4`, `text-embedding-3-small`) |
| `api_format` | `""` | `"openai"` or `"dashscope"`. Auto-detected from `api_url` |

**Backend priority**: API (if `api_url` + `api_model` set) → Local ONNX → disabled (keyword-only).

**Network retry (v2.5.3+)**: embedding API calls retry up to 3× with exponential backoff (1s/2s/4s) on network errors (connection reset/refused/timeout). API validation errors are not retried. Worst case adds ~7s latency to a failing batch — size timeouts accordingly.

**OpenAI-compatible example** (OpenAI, Ollama, vLLM, etc.):

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

Switching between backends or changing `dimensions` requires `asuna-memory rebuild --full` to regenerate all vectors. `dimensions` must also match the embedding model's native output (local ONNX EmbeddingGemma = 768); a mismatch now errors instead of silently leaving the vector index empty.

### Full config reference

All fields optional — only override what you need:

```json
{
  "data_dir": "~/.asuna",
  "profile_id": "default",
  "conversation": { "enabled": true, "auto_embed": true, "preview_length": 200 },
  "memory": { "memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true, "atom_capacity_ratio": 0.3 },
  "search": { "default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true },
  "embedding": { "dimensions": 1024, "batch_size": 32, "api_url": "", "api_key": "", "api_model": "", "api_format": "" },
  "graph": { "enabled": true, "remind_on_save": true },
  "pipeline": { "enable_extraction": true, "every_n_turns": 5 },
  "llm": { "base_url": "", "api_key": "", "model": "" },
  "gateway": { "auth_enabled": false, "api_key": "", "cors_origins": [] }
}
```

## 3. Start Server

```bash
asuna-memory serve
```

Protocol: JSON-RPC 2.0 over stdio. One request per line on stdin, one response per line on stdout. **Do not write anything else to stdout.**

### MCP Handshake Sequence

1. Send `initialize` request → receive `initialize` response
2. Send `notifications/initialized` notification (no response expected)
3. Use `tools/list` and `tools/call` freely

### Example: Initialize

Request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "2024-11-05",
    "capabilities": {},
    "clientInfo": { "name": "your-agent", "version": "1.0" }
  }
}
```

Response:

```json
{
  "id": 1,
  "jsonrpc": "2.0",
  "result": {
    "capabilities": { "tools": {} },
    "protocolVersion": "2024-11-05",
    "serverInfo": { "name": "asuna-memory", "version": "2.6.1" }
  }
}
```

Then send:

```json
{ "jsonrpc": "2.0", "method": "notifications/initialized" }
```

## 4. Tools

All tools are called via `tools/call` method with `name` and `arguments` params.

**Error handling:** Tool-level errors (invalid arguments, capacity limits, security-scan failures, etc.) return a successful JSON-RPC response whose `content` array contains the error message and `isError: true` is set, per the MCP protocol specification. Transport-level errors (malformed JSON, unknown method) return a JSON-RPC `error` field instead.

**Strict validation (v1.2.1+):**

- `target` must be exactly `memory` or `user` — anything else (including `../foo`) is rejected.
- Every `turn` must contain `timestamp` + `role` + `content`. Missing fields are rejected (no longer silently coerced).
- `role` must be one of `user` / `assistant` / `tool_call` / `system` — other values are rejected.

### 4.1 save_session

Save a conversation to the fact layer. **Dual-write order**: SQLite transaction → commit → JSONL on disk → old-JSONL cleanup. If the SQLite transaction fails, no JSONL file is created. Vector embeddings are produced on save (when model available) using the **Document** task prefix.

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/call",
  "params": {
    "name": "save_session",
    "arguments": {
      "session_id": "unique-session-id",
      "turns": [
        {
          "timestamp": "2026-04-10T10:00:00+08:00",
          "role": "user",
          "content": "Hello"
        },
        {
          "timestamp": "2026-04-10T10:00:05+08:00",
          "role": "assistant",
          "content": "Hi!",
          "metadata": {
            "model": "gpt-4",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
          }
        }
      ],
      "source": "openclaw",
      "title": "Greeting",
      "tags": ["greeting"]
    }
  }
}
```

Params:

- `session_id` (string, required): Unique session identifier. Re-saving the same id replaces the previous record (`INSERT OR REPLACE` + DELETE-then-INSERT on turns / vectors).
- `turns` (array, required, **non-empty**): One object per turn. Each item:
  - `timestamp` (string, required): ISO 8601 timestamp.
  - `role` (string, required): one of `user`, `assistant`, `tool_call`, `system`.
  - `content` (string, required): Turn content.
  - `metadata` (object, optional): Arbitrary metadata. `metadata.usage.input_tokens` + `output_tokens` are summed into `total_tokens` if present.
- `source` (string, optional): Source identifier.
- `title` (string, optional): Session title.
- `tags` (string[], optional): Tags.
- `profile` (string, optional): Override default profile for this save.

Side effects:

- Writes JSONL to `~/.asuna/profiles/{profile}/conversations/YYYY/MM/DD/{compact_time}_{first8_of_id}.jsonl`.
- Inserts into `sessions`, `turns`, `turns_fts`, `vec_turns` (if embedder available).
- Preview length is governed by `config.conversation.preview_length` (default 200 chars, character-safe).

### 4.2 search_sessions

Search historical conversations. Supports keyword, semantic, and hybrid modes. Query side uses the **Query** task prefix; documents indexed with `save_session` / `rebuild_index` use the **Document** prefix — the split is automatic.

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "search_sessions",
    "arguments": {
      "query": "Rust async runtime",
      "search_mode": "hybrid",
      "top_k": 5,
      "time_range": { "last_days": 30 },
      "role": "assistant"
    }
  }
}
```

Params:

- `query` (string, required): Search query.
- `search_mode` (string, optional): `keyword` | `semantic` | `hybrid` (default from `config.search.search_mode`, fallback `hybrid`).
- `top_k` (integer, optional): Max results (default from `config.search.default_top_k`, fallback `5`).
- `time_range` (object, optional): `after` (ISO string), `before` (ISO string), or `last_days` (integer).
- `role` (string, optional): Filter by role (`user`/`assistant`/`tool_call`/`system`).

Result objects contain `turn_id`, `score`, `preview`, `session_id`, `timestamp_ms`, `role`, and (v2.6) `scores` — per-source components (`semantic`/`keyword`) that sum to `score`.

### 4.3 memory_write

Write a new entry to growth memory (`MEMORY.md` for `target=memory`, `USER.md` for `target=user`). Content is security-scanned (Prompt injection, credential leaks, invisible Unicode) before write; rejected on hit. Capacity limits apply: memory=2200 chars, user=1375 chars. Duplicate content (exact string match against existing § entries) is rejected. **One entry per call**: `content` containing the entry separator `\n§\n` is rejected — call `memory_write` once per logical entry.

```json
{
  "name": "memory_write",
  "arguments": {
    "target": "memory",
    "content": "User prefers Rust over Go for backend services.",
    "confidence": "high",
    "session_id": "source-session-uuid"
  }
}
```

Params:

- `target` (string, required): `memory` or `user` (strict whitelist).
- `content` (string, required): Entry content.
- `confidence` (string, optional): `high` | `medium` | `low` (default: `medium`).
- `session_id` (string, optional): Source session UUID for provenance tracking.

Stored in both the `.md` file (as a § -separated entry) and the SQLite `bounded_memory` table (one row).

### 4.4 memory_update

Update existing entries by substring match. Matching is **entry-level**: any entry containing `old_text` has its `old_text` replaced with `new_text`. Multiple matching entries are all updated atomically. SQLite-side update uses LIKE with `\` as `ESCAPE`, so `%` / `_` / `\` in `old_text` are treated as literals.

```json
{
  "name": "memory_update",
  "arguments": {
    "target": "memory",
    "old_text": "prefers Rust over Go",
    "new_text": "prefers Rust and Go equally"
  }
}
```

Params:

- `target` (string, required): `memory` or `user`.
- `old_text` (string, required): Substring to find (literal, not regex).
- `new_text` (string, required): Replacement text.
- `session_id` (string, optional): Source session UUID for audit trail.

Returns an error if `old_text` is not found anywhere in the body. Capacity is rechecked after replacement. **`new_text` must not contain `\n§\n`** (the entry separator) — `memory_update` operates on a single entry, and embedding a separator would split it into multiple entries.

### 4.5 memory_remove

Remove **entire entries** that contain `old_text`. Filter is at the § -separated entry level: an entry hit by `old_text` is dropped wholesale (use `memory_update` for partial edits). Adjacent-entry deletion does not leave residual `§§§` separators.

```json
{
  "name": "memory_remove",
  "arguments": {
    "target": "memory",
    "old_text": "prefers Rust over Go"
  }
}
```

Params:

- `target` (string, required): `memory` or `user`.
- `old_text` (string, required): Substring identifying entries to drop.
- `session_id` (string, optional): Source session UUID for audit trail.

### 4.6 memory_read

Read the full growth memory content (including metadata header).

```json
{
  "name": "memory_read",
  "arguments": { "target": "memory" }
}
```

Params:

- `target` (string, required): `memory` or `user`.

### 4.7 user_profile

Read/write user profile (alias for memory operations on `user` target).

```json
{
  "name": "user_profile",
  "arguments": {
    "action": "write",
    "content": "User is a senior Rust developer.",
    "confidence": "high"
  }
}
```

Params:

- `action` (string, required): `read` | `write` | `update` | `remove`.
- `content` (string): For `write`.
- `old_text` (string): For `update` / `remove`.
- `new_text` (string): For `update`.
- `confidence` (string, optional): `high` | `medium` | `low`.

### 4.8 rebuild_index

Rebuild the SQLite index from all JSONL files. **v2.2.2+: Incremental by default** — if DB already has data matching the JSONL files (session count matches), automatically skips Phase 1 (metadata + FTS) and resumes Phase 2 (vector embedding) from the last completed batch. Use `--full` flag to force complete rebuild from scratch.

**Two-phase architecture:**
- **Phase 1** (metadata + FTS): DELETE + INSERT sessions/turns, rebuild FTS index (~7s for 4020 sessions)
- **Phase 2** (vector embedding): Batch embed + insert into `vec_turns` (32 records per ONNX batch, 320 per DB transaction)

**Incremental mode** (default): Skips Phase 1 if DB has matching data, only embeds missing vectors. Ideal for resuming interrupted rebuilds.

**Full mode** (`--full` flag): Deletes all data and rebuilds everything from scratch. Use when JSONL files have changed significantly.

```json
{
  "name": "rebuild_index",
  "arguments": {}
}
```

Response includes `sessions_processed`, `turns_indexed`, `vectors_indexed`, `vectors_skipped`, `errors`.

CLI equivalent: `asuna-memory rebuild [--full]`

### 4.8.5 rebuild_status

Query the progress of a background `rebuild_index` operation. Returns current status (`idle` / `running` / `completed` / `failed`), counts, elapsed time, and any errors.

```json
{
  "name": "rebuild_status",
  "arguments": {}
}
```

Response: `{status, sessions_processed, turns_indexed, vectors_indexed, errors, elapsed_ms}`.

### 4.9 memory_provenance

Verify that growth-memory entries can be traced back to source sessions. Reports `total_entries`, `verified` (source exists), `missing_source` (referenced session_id no longer in DB), and `no_source` (no source recorded).

```json
{
  "name": "memory_provenance",
  "arguments": { "target": "memory" }
}
```

Params:

- `target` (string, required): `memory` or `user`.

### 4.10 `graph_assert`

Write entity-relation triples to the graph layer. canonical-normalizes `src`/`dst` (lowercase + trim + whitespace fold). MERGE semantics: existing entities preserve their first-written `name`/`entity_type`; existing relations have `confidence` updated to `MAX(existing, new)`.

```json
{
  "name": "graph_assert",
  "arguments": {
    "triples": [
      {
        "src": "Alice Smith",
        "rel": "works_at",
        "dst": "OpenAI",
        "src_type": "person",
        "dst_type": "org",
        "confidence": 0.9,
        "source_turn": 42
      }
    ],
    "session_id": "uuid"
  }
}
```

Params:

- `triples` (required, non-empty array). Each triple:
  - `src` / `rel` / `dst` (required, non-empty strings)
  - `src_type` / `dst_type` (optional, free string, default `'unknown'`)
  - `confidence` (optional, 0.0..=1.0, default 0.5)
  - `source_turn` (optional INT64, for provenance — strongly recommended)
- `session_id` (optional)

Returns: `{status, entities_created, entities_updated, relations_created, relations_updated}`. Single transaction; any error rolls back.

### 4.11 `graph_neighbors`

Query N-hop neighbors of an entity.

```json
{
  "name": "graph_neighbors",
  "arguments": {
    "entity": "Alice Smith",
    "rel_type": "works_at",
    "direction": "both",
    "hops": 1,
    "limit": 50
  }
}
```

- `direction` ∈ `out` (default = `both`) — out follows edges from src to dst; in follows the reverse; both is undirected
- `hops` ∈ 1..=5 (default 1)
- `limit` (default 50, max 200)
- `rel_type` optional filter; applied at EVERY hop (a 2-hop "knows" query requires both edges be "knows")

Returns: `{status, neighbors: [{canonical, name, type, distance}]}`. Seed is excluded from results.

### 4.12 `graph_path`

Find shortest path between two entities. Returns `length` plus the full alternating `[Entity, Edge, Entity, Edge, ..., Entity]` sequence (`2 * length + 1` elements).

```json
{
  "name": "graph_path",
  "arguments": {
    "src": "Alice",
    "dst": "OpenAI",
    "max_hops": 5
  }
}
```

- `max_hops` ∈ 1..=10 (default 5)
- `src == dst` after canonicalize → `{found: true, length: 0, path: []}` (no edges to traverse)
- Either empty after canonicalize → `{found: false, length: 0, path: []}`
- Otherwise returns `{status, found, length, path}` where `path` is a non-empty alternating sequence of `{canonical, name}` (Entity) and `{rel_type}` (Edge) objects
- `name` is resolved from the `entities` table; falls back to `canonical` if the row is missing

### 4.13 `graph_link_entity`

Merge `from` entity into `to`: rewires all edges, then deletes `from`. **Irreversible**.

```json
{
  "name": "graph_link_entity",
  "arguments": {"from": "alice", "to": "alice smith", "session_id": "uuid"}
}
```

- Duplicate edges after rewiring are auto-merged (target side wins; confidence not MAX'd in v1.3.0)
- `from` doesn't exist → silent no-op returning `{edges_rewired: 0}`
- `from == to` after canonicalize → error
- Self-loops on `from` are dropped (not rewired to self-loops on `to`)

Returns: `{status, edges_rewired, old_canonical, old_original_input}`. `old_canonical` is the DB-level key that was actually removed; `old_original_input` echoes back the `from` argument verbatim for round-trip clarity.

### 4.14 `graph_prune_dangling`

Clean up dangling `source_turn` references in both `relations` and `entities`: set the field to `NULL` where the referenced turn no longer exists in the `turns` table. Does **NOT** delete relations or entities — only clears stale provenance links.

```json
{
  "name": "graph_prune_dangling",
  "arguments": {}
}
```

- Returns `{status, relations_pruned: <count>}`. `count` is the number of `relations` rows whose `source_turn` was cleared. Entity-level prune count is not surfaced (typically tiny).
- Idempotent: a second call immediately after returns `{relations_pruned: 0}`.
- Single transaction; rolls back atomically on failure.

Use after large `turns` deletions to keep `doctor --verbose` dangling count at 0.

## 5. Usage Patterns

### Pattern: Save then search

Saved sessions are **immediately searchable** via keyword/FTS5. Semantic and hybrid searches additionally require the ONNX model — when it is loaded, `save_session` auto-generates int8 vectors using the Document task prefix in the same transaction.

### Pattern: Incremental memory building

Use `memory_write` with explicit `confidence` and `session_id`. Use `memory_update` to refine existing entries (entry-level § matching) instead of writing duplicates. Periodically call `memory_provenance` to verify traceability.

### Pattern: Atomic save

`save_session` writes the SQLite transaction **first**, then JSONL after commit. If the transaction fails, no JSONL file is created. If JSONL write fails after commit, the DB is consistent but the file is missing — a subsequent `save_session` with the same `session_id` will recreate it; `rebuild_index` will simply skip that session until the JSONL exists.

### Pattern: Rebuild after upgrade

After upgrading from v1.2.0 or earlier to v1.2.1, run `rebuild_index` to regenerate vectors with the new Document prefix. Document/query prefix mismatch in older versions silently degraded recall quality.

### Pattern: When to save

| Scenario           | When             | Notes                                                      |
| ------------------ | ---------------- | ---------------------------------------------------------- |
| Agent conversation | End of each turn | Conversation is archived for later search                  |
| Batch migration    | One-time import  | Use `asuna-memory import` CLI to bulk-import JSONL files   |
| Periodic archive   | On a schedule    | Good for high-frequency chat (e.g., support bots)          |
| User-triggered     | On user request  | Important conversations saved on demand                    |

Recommended: save after each conversation turn. Same `session_id` = overwrite (DELETE-then-INSERT on turns/vectors; INSERT OR REPLACE on sessions).

### Pattern: Graph-aware memory

After each `save_session`, inspect the response for `graph_pending.turn_ids`:

1. For each unreferenced `turn_id`, examine the turn's content
2. Extract `(subject, relation, object)` triples
3. Call `graph_assert` with `source_turn=<turn_id>` so the graph layer can resolve later
4. Periodically call `graph_neighbors` / `graph_path` to surface relationships during search

The graph layer is only useful as you write to it. Without `graph_assert` calls, it stays empty.

To disable the soft hint, set `graph.remind_on_save = false` in config.json.
To disable the graph layer entirely, set `graph.enabled = false`.

### Anti-patterns to avoid

- **Do not** pass user-controlled strings as `target` — the server enforces a whitelist, but always pass the literal `"memory"` or `"user"`.
- **Do not** rely on previous behavior of silently coercing missing/invalid `role` to `user` — pass an explicit valid role.
- **Do not** stuff `%` or `_` into `old_text` hoping for wildcard matching — they are now treated as literals.
- **Do not** split a single logical entry across multiple `memory_write` calls — capacity is per-file, not per-entry; use one entry per fact.

## 6. JSONL File Format (for `import` command)

The `import` CLI command reads a JSONL file: **1 Header line + N Turn lines**, one JSON object per line. (The `save_session` MCP tool builds equivalent records itself — you only need this format for the `import` CLI or for hand-prepared files.)

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

### Example JSONL file

````jsonl
{"v":1,"type":"session_header","session_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","start_time":"2026-04-10T10:02:00+08:00","profile_id":"default","source":"manual","title":"Example session","tags":["demo"]}
{"ts":"2026-04-10T10:02:00+08:00","seq":1,"role":"user","content":"Hello, help me write a Rust Hello World"}
{"ts":"2026-04-10T10:02:05+08:00","seq":2,"role":"assistant","content":"Sure! Here is a minimal Rust Hello World:\n\n```rust\nfn main() {\n    println!(\"Hello, World!\");\n}\n```","model":"gpt-4","usage":{"input_tokens":15,"output_tokens":42}}
````

> **Note**: `import` uses JSONL format (`ts` / `seq` fields). `save_session` MCP tool uses `timestamp` field and auto-assigns `seq`. Both produce the same stored format.

## 7. Integration Examples

### Python: Generate JSONL and import via CLI

```python
import json
import subprocess
import uuid
from datetime import datetime, timezone, timedelta

def save_conversation_cli(turns: list[dict], title: str = None, source: str = "python-app"):
    """Generate a JSONL file and import via CLI."""
    tz = timezone(timedelta(hours=8))
    now = datetime.now(tz)
    session_id = str(uuid.uuid4())

    header = {
        "v": 1,
        "type": "session_header",
        "session_id": session_id,
        "start_time": now.isoformat(),
        "profile_id": "default",
        "source": source,
        "title": title,
        "tags": [],
    }

    lines = [json.dumps(header, ensure_ascii=False)]
    for i, turn in enumerate(turns, 1):
        ts = (now + timedelta(seconds=i)).isoformat()
        line = {"ts": ts, "seq": i, "role": turn["role"], "content": turn["content"]}
        if "metadata" in turn:
            line.update(turn["metadata"])
        lines.append(json.dumps(line, ensure_ascii=False))

    path = f"/tmp/{session_id}.jsonl"
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")

    subprocess.run(["asuna-memory", "import", path], check=True)
    return session_id

# Usage
save_conversation_cli([
    {"role": "user", "content": "What is Rust?"},
    {"role": "assistant", "content": "Rust is a systems programming language..."},
], title="Rust intro")
```

### Python: Call save_session via MCP stdio

```python
import json
import subprocess

def save_session_mcp(session_id: str, turns: list[dict], **kwargs):
    """Call save_session via MCP stdio.

    NOTE for v1.2.1+:
      - Each turn MUST include: timestamp, role, content. Missing/empty -> error.
      - role MUST be one of: user, assistant, tool_call, system.
    """
    proc = subprocess.Popen(
        ["asuna-memory", "serve"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True,
    )

    # Initialize
    init_req = json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"py-client","version":"1.0"}}})
    proc.stdin.write(init_req + "\n")
    proc.stdin.flush()
    proc.stdout.readline()  # init response

    notify = json.dumps({"jsonrpc":"2.0","method":"notifications/initialized"})
    proc.stdin.write(notify + "\n")
    proc.stdin.flush()

    # Save session
    args = {"session_id": session_id, "turns": turns, **kwargs}
    req = json.dumps({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"save_session","arguments":args}})
    proc.stdin.write(req + "\n")
    proc.stdin.flush()
    resp = json.loads(proc.stdout.readline())

    proc.stdin.close()
    proc.wait()
    return resp

# Usage
save_session_mcp(
    session_id="my-session-001",
    turns=[
        {"timestamp": "2026-04-10T10:00:00+08:00", "role": "user", "content": "Hello"},
        {"timestamp": "2026-04-10T10:00:05+08:00", "role": "assistant", "content": "Hi!"},
    ],
    source="python-mcp",
    title="Test session",
)
```

### Node.js: Generate JSONL and import via CLI

```javascript
const { execSync } = require("child_process");
const fs = require("fs");
const crypto = require("crypto");

function saveConversationCli(turns, { title, source = "node-app" } = {}) {
  const sessionId = crypto.randomUUID();
  const now = new Date();

  const header = {
    v: 1,
    type: "session_header",
    session_id: sessionId,
    start_time: now.toISOString(),
    profile_id: "default",
    source,
    title: title || null,
    tags: [],
  };

  const lines = [JSON.stringify(header)];
  turns.forEach((turn, i) => {
    const ts = new Date(now.getTime() + (i + 1) * 1000).toISOString();
    const line = { ts, seq: i + 1, role: turn.role, content: turn.content };
    if (turn.metadata) Object.assign(line, turn.metadata);
    lines.push(JSON.stringify(line));
  });

  const path = `/tmp/${sessionId}.jsonl`;
  fs.writeFileSync(path, lines.join("\n") + "\n", "utf-8");
  execSync(`asuna-memory import ${path}`);
  return sessionId;
}

// Usage
saveConversationCli(
  [
    { role: "user", content: "What is Node.js?" },
    { role: "assistant", content: "Node.js is a JavaScript runtime..." },
  ],
  { title: "Node.js intro" },
);
```

## 8. Data Layout

```text
~/.asuna/
├── config.json                         # Optional config (uses defaults if absent)
├── profiles/
│   └── default/                        # Per-profile isolation
│       ├── memory.db                   # SQLite (sessions, turns, FTS5, vec_turns, bounded_memory, audit_log)
│       ├── conversations/
│       │   └── YYYY/MM/DD/
│       │       └── {time}_{id}.jsonl   # JSONL: header line + turn lines
│       └── memory/
│           ├── MEMORY.md               # AI knowledge memory (§-separated entries, 2200 char cap)
│           └── USER.md                 # User profile (§-separated entries, 1375 char cap)
└── models/                             # Optional ONNX model files
    └── embeddinggemma-300m-q8/
```

## 9. CLI Commands (for scripting)

```bash
asuna-memory serve                      # Start MCP stdio server (default)
asuna-memory gateway --port 8765        # Start HTTP REST gateway
asuna-memory doctor                     # Environment check (version, FK status, vector count, embedder dim)
asuna-memory doctor --verbose           # Extended diagnostics (graph coverage, dangling references)
asuna-memory doctor --split-entries     # Split any DB row whose content contains multiple §-separated entries
                                        #   (fixes ".md entry count ≠ DB row count"; idempotent, rebuilds .md)
asuna-memory doctor --fix               # Auto-fix DB/.md inconsistencies (runs --split-entries internally first)
asuna-memory model-download             # Download embedding model (~300MB) from GitHub Release Assets
asuna-memory list-profiles              # List profiles
asuna-memory list-sessions --last-days 7 --limit 20
asuna-memory search "query" --mode hybrid --top-k 5   # modes: hybrid | semantic (alias: vector) | keyword (alias: fts); filters: --role --after --before --last-days
asuna-memory rebuild                    # Rebuild FTS + vector index from JSONL (incremental by default)
asuna-memory rebuild --full             # Force complete rebuild, ignore existing data
asuna-memory import file.jsonl          # Import a session file (auto-generates vectors with Document prefix)
asuna-memory export <session_id>        # Export session summary
asuna-memory delete-turn <id>           # Safely delete a turn (auto-cleans FTS + vector indexes)
asuna-memory sql "SELECT ..."           # Read-only SQL query (first-token allowlist + PRAGMA query_only; in-process UDF available)
```

Global flags: `--config <path>` (default: `~/.asuna/config.json`), `--profile <id>` (default: `default`).

## 10. HTTP REST Gateway

In addition to MCP stdio, AMS provides an HTTP REST gateway for integration with agent frameworks (Hermes, LangChain, custom HTTP clients).

### Starting the Gateway

```bash
asuna-memory gateway --port 8765
```

### Authentication (optional)

Set `gateway.auth_enabled = true` in config.json and configure `AMS_GATEWAY_API_KEY` environment variable. Authenticate via:
- `Authorization: Bearer <key>` header
- `X-API-Key: <key>` header

The `/health` endpoint skips authentication.

**CORS**: when `gateway.cors_origins` is empty and auth is disabled, the gateway allows only localhost origins (`http(s)://localhost / 127.0.0.1 / [::1]`, any port) — a public site the user visits cannot cross-origin read the local memory store. Set `cors_origins` to an explicit allowlist, or enable auth, to permit other origins. With auth enabled, any origin is allowed (the caller must present a key).

### Endpoints

#### `GET /health`
Returns server status and version.

```json
{ "status": "ok", "version": "2.6.1" }
```

#### `GET /stats`
Returns database statistics.

```json
{ "sessions": 42, "turns": 156, "vectors": 156, "entities": 23, "relations": 31 }
```

#### `POST /capture`
Save conversation turns. Requires `session_id` (string) and `turns` (non-empty array). Each turn must have `role` and `content`; `timestamp` (Unix ms) is optional.

```json
// Request
{
  "session_id": "unique-session-id",
  "turns": [
    { "timestamp": 1714000000000, "role": "user", "content": "Hello" },
    { "timestamp": 1714000005000, "role": "assistant", "content": "Hi!" }
  ]
}
// Response
{ "status": "ok", "turns_saved": 2 }
```

#### `POST /recall`
Progressive disclosure retrieval. Returns memories from L3 (persona) → L2 (scenarios) → L1 (atoms via FTS) → L0 (recent turns via LIKE).

```json
// Request
{ "query": "Rust programming", "top_k": 10 }
// Request with v2.6 options
{ "query": "Rust programming", "top_k": 10, "max_tokens": 1500, "after": "2026-01-01T00:00:00Z", "last_days": 30 }
// Response
{
  "memories": [
    { "layer": "L3", "type": "persona", "content": "..." },
    { "layer": "L2", "type": "scenario", "content": "..." },
    { "layer": "L1", "type": "fact", "content": "...", "confidence": 0.85, "created_at": 1714000000000, "ordered_by": "confidence+recency" },
    { "layer": "L0", "type": "turn", "role": "user", "content": "...", "timestamp": 1714000000000 }
  ],
  "context": "[Persona] ...\n[Scenario] ...\nfact ...\n",
  "truncated": false
}
```

- `query` (string, required, max 10000 chars): Search query.
- `top_k` (integer, optional, default 10, max 50): Max results per layer.
- `max_tokens` (integer, optional, default from config `recall.token_budget` = 2000): v2.6 response token budget. Greedy prefix cut in layer order: the first memory whose content exceeds the remaining budget is dropped whole (never truncated), later items are not backfilled. `truncated` reports whether anything was dropped. Note: **v2.5.3 applied no budget to `/recall` responses at all** — responses larger than the default 2000-token budget now return fewer memories. `max_tokens: 0` yields an empty result (drop semantics).
- `after` / `before` (RFC3339 string, optional, v2.6): Filter L1 by `bounded_memory.created_at` and L0 by `turns.timestamp_ms` (same semantics as `/search`). Malformed values return 400, never a silently widened window. **Deliberate decisions**: L1 filters on `created_at` (recorded-at, parallel to `timestamp_ms`), not `updated_at`; L3 persona and L2 scenarios are evergreen layers and stay unfiltered.
- L1 items carry additive `created_at` (epoch ms) and `ordered_by` (ranking basis — L1 has no numeric score; ordering is confidence tier then `updated_at` recency).

#### `POST /search`
Text search or multi-hop graph search.

```json
// Text search request
{ "query": "Rust async", "mode": "hybrid", "top_k": 5 }

// Text search request with turn filters
{ "query": "Rust async", "mode": "hybrid", "top_k": 5, "role": "user", "after": "2026-01-01T00:00:00Z", "last_days": 7 }

// Multi-hop graph search request
{ "query": "", "entity": "Alice", "max_hops": 2, "relation_filter": "knows" }

// Response (text search)
{ "results": [{ "turn_id": 42, "score": 0.95, "preview": "...", "session_id": "...", "timestamp_ms": 0, "role": "user", "scores": { "semantic": 0.008, "keyword": 0.016 } }], "query_type": "text", "count": 1, "status": "ok" }

// Response (multi-hop)
{ "results": [{ "id": 1, "content": "...", "memory_type": "atom", "confidence_score": 0.9, "created_at": 0 }], "query_type": "multi_hop", "entity": "Alice", "max_hops": 2, "status": "ok" }
```

- `query` (string, max 10000 chars): Text search query.
- `mode` (string, optional): `keyword` | `semantic` | `hybrid` (default).
- `role` (string, optional): Filter turns by role (`user` / `assistant`). Text search only.
- `after` / `before` (RFC3339 string, optional): Filter turns by timestamp. Malformed values return 400.
- `last_days` (integer, optional): Restrict to the last N days (overrides `after`).
- v2.6 score transparency: each text-search result carries additive `scores` — the per-source components (`semantic` / `keyword`) that sum to `score` (RRF contributions in hybrid mode; the single active component in keyword/semantic modes). Ranking is inspectable; no absolute-score cutoffs are applied (scores are uncalibrated).
- `entity` (string, optional): If set, performs multi-hop graph search instead of text search.
- `max_hops` (integer, optional, default 2, max 10): Graph traversal depth.
- `relation_filter` (string, optional): Filter graph edges by relation type.

#### `GET /persona`
Returns the user persona from `USER.md`.

```json
{ "persona": "# User Profile\n...", "status": "ok" }
// or if not found:
{ "persona": null, "status": "not_found" }
```

#### `POST /offload`
Store long text to `refs/` directory. Returns a `node_id` for later recall.

```json
// Request
{ "task_id": "task_001", "content": "very long text..." }
// Response
{ "node_id": "task_001/step_1", "bytes_stored": 15234 }
```

- `task_id` (string, required): Alphanumeric, underscores, hyphens only. Max 255 chars. Path traversal protected.
- `content` (string, required): Text content to store.

#### `GET /recall/:node_id`
Recall previously offloaded text by node_id (URL-encoded, e.g., `task_001/step_1`).

```json
{ "node_id": "task_001/step_1", "content": "very long text..." }
```

#### `POST /graph/assert`
Write entity-relation triples to the knowledge graph. Same semantics as the MCP `graph_assert` tool.

```json
// Request
{
  "subject": "Alice Smith",
  "predicate": "works_at",
  "object": "OpenAI",
  "confidence": "0.9"
}
// Response
{ "status": "ok", "subject": "alice smith", "predicate": "works_at", "object": "openai", "confidence": 0.9 }
```

- `subject` / `predicate` / `object` (string, required, max 1000 chars each): Triple components.
- `confidence` (string, optional, 0.0-1.0, default 0.5): Confidence score.

Canonical normalization (lowercase + trim + whitespace fold) is applied automatically.

#### `POST /graph/neighbors`
Query N-hop neighbors of an entity in the knowledge graph.

```json
// Request
{ "entity": "Alice", "hops": 2, "direction": "both", "relation_kind": "knows" }
// Response
{
  "entity": "Alice",
  "canonical": "alice",
  "neighbors": [{ "entity": "bob", "relation": "knows", "confidence": 0.8, "relation_kind": "asserted" }],
  "count": 1,
  "status": "ok"
}
```

- `entity` (string, required, max 1000 chars): Entity name.
- `hops` (integer, optional, default 1, max 10): Traversal depth.
- `direction` (string, optional): `out` | `in` | `both` (default).
- `relation_kind` (string, optional): Filter by relation kind.

#### `POST /session/end`
Record session end timestamp.

```json
// Request
{ "session_id": "unique-session-id" }
// Response
{ "status": "ok", "session_id": "unique-session-id", "end_ts": 1714000100000, "message": "Session end timestamp recorded. Async aggregation pipeline not yet implemented." }
```

### Error Responses

All errors return HTTP status codes with a JSON body:

```json
{ "error": "descriptive error message" }
```

Common status codes: `400` (bad request), `401` (unauthorized), `404` (not found), `500` (internal error).

### Request Limits

- Body size: 10MB max (`RequestBodyLimitLayer`)
- Query length: 10,000 characters max
- Entity name: 1,000 characters max
- Multi-hop depth: 10 max
- `top_k`: 50 max

## 11. Behavioral Contracts

These are the **invariants you can rely on** when integrating:

- **Atomicity**: A `save_session` either fully succeeds (JSONL + DB + vectors consistent) or fully fails (nothing persisted). No half states.
- **Idempotency**: Re-issuing `save_session` with the same `session_id` deterministically overwrites; old JSONL on a different timestamp is cleaned up.
- **Target whitelist**: `memory_*` and `user_profile` tools reject any `target` outside `{memory, user}` — including path-traversal attempts.
- **Role whitelist**: `save_session` rejects any `role` outside `{user, assistant, tool_call, system}`.
- **LIKE safety**: `%`, `_`, `\` inside `old_text` for `memory_update` / `memory_remove` are treated as literal characters, not SQL wildcards.
- **Embedding correctness**: With local provider, stored documents always use the EmbeddingGemma `title: none | text:` prefix; queries always use `task: search result | query:`. With API provider, raw text is sent (no prefix — API models handle this internally). Mixing providers without `rebuild --full` produces inconsistent vectors.
- **Foreign keys**: `turns.session_id` must reference a present `sessions.session_id` (enforced by `PRAGMA foreign_keys = ON`).
- **No silent fallbacks**: Missing required fields produce explicit error responses instead of defaults.
- **Graph as third layer**: `entities` + `relations` tables in the same `memory.db`. Independent of fact/growth layers.
- **canonical normalization**: lowercase + trim + whitespace fold is the only entity-identity logic. "Alice" and "Alice Smith" remain separate nodes unless `graph_link_entity` is called.
- **Confidence is MAX-merge**: re-asserting the same triple with higher confidence updates the stored value; lower confidence is ignored.
- **Constant-time auth**: API key comparison uses `subtle::ConstantTimeEq` to prevent timing attacks.
- **FTS5 safety**: User queries in `/recall` are wrapped in double-quotes to prevent FTS5 operator injection.
- **LIKE safety (HTTP)**: `/recall` L0 search escapes `%`, `_`, `\` in user queries before LIKE matching.
- **Transaction integrity**: `/capture` uses explicit `tx.commit()` — all INSERTs are persisted atomically.
- **Cycle detection**: Evolution chain traversal (`get_chain`, `get_latest_version`) uses HashSet cycle detection + depth limit of 1000.
- **Auto-backfill (v2.2.3+)**: On startup, atoms in `bounded_memory` missing vectors in `vec_bounded_memory` are automatically re-embedded. This is idempotent — already-indexed atoms are skipped. Failures are logged as warnings and never block service startup.
- **FTS tokenizer (v2.4.0+)**: FTS5 tables (`turns_fts`, `bounded_memory_fts`) use the **jieba** native tokenizer for word-level Chinese segmentation. No preprocessing is needed — pass raw text directly to FTS INSERT/DELETE operations. The old `tokenize_zh` UDF is deprecated but retained for `asuna-memory sql` compatibility. External tools can now INSERT/UPDATE/DELETE on `turns` and `bounded_memory` without `no such function: tokenize_zh` errors. Auto-migration from `unicode61` happens on first startup.
- **Supersedes-safe deletes (v2.5.3+)**: `bounded_memory.supersedes_id` is a self-referential FK. Any delete path (atom capacity eviction, `memory_remove`, `doctor --split-entries`) detaches references first — the surviving entry's `supersedes_id` becomes `NULL` — so deletes never fail on the FK. Eviction runs in a single transaction and `MEMORY.md` is rebuilt from the DB afterward; the extraction pipeline can no longer leave `.md` silently diverged.
- **Exact-text guard (v2.6.0+)**: before embedding/admission, an extracted atom whose trimmed content exactly matches any existing `bounded_memory` row is skipped and audited as `duplicate_skip` (action in `audit_log`). Closes the silent-duplication hole when no embedder is configured. Scope is deliberately broad (all targets): atoms identical to the persona (`target='user'`) or manual entries are also skipped, preventing double entries in `MEMORY.md`. Split children (`doctor --split-entries`) inherit all parent metadata.
- **`edited_at` user-edit protection (v2.6.0+)**: `bounded_memory.edited_at` marks user-authored content — stamped by `memory_update` (BoundedMemory::update) and by `doctor --fix` reinsertion of `.md`-only entries (and inherited by split children). Contract for future automatic rewrite mechanisms: rows with `edited_at` set must not be overwritten. Programmatic writes (atom extraction, `memory_write`) leave it NULL.
- **`memory_history` snapshot table (v2.6.0+)**: pre-rewrite version snapshots for future automatic rewrite mechanisms (`source_table`, `source_id`, `content_snapshot`, `changed_by`, `changed_at`). Inert in v2.6.1 (no writers); survives `rebuild --full`.
- **Retrieval benchmark (v2.6.0+)**: `src/fact/bench_test.rs` — Chinese fixture corpus with golden relevance judgments (Success@5 / Recall@5 / MRR / latency). Gated with `#[ignore]`; run `cargo test -- --ignored retrieval_benchmark --nocapture`. Recorded baseline at v2.5.3: Success@5=1.000, MRR=0.833. Hard gates: Success@5 = 1.000 and MRR ≥ 0.65; the recorded baseline is the regression reference. Any retrieval change must re-run it.
- **L2 scenario aggregation (v2.6.1+)**: the post-session pipeline can cluster this session's newly-stored atoms by embedding similarity (cosine > threshold) and summarize each cluster via the LLM into a `memory_type='scenario'` row (so `/recall` L2 surfaces it) plus a human-readable Markdown file under `memory/scenarios/`. Opt-in via `config.scenarios.enabled` (default false) — requires both an LLM and an embedder. Config: `scenarios.similarity_threshold` (default 0.8), `scenarios.min_cluster_size` (default 2), `scenarios.max_scenarios` (default 50; oldest scenario rows evicted beyond this — scenarios bypass the atom capacity budget, so the cap bounds growth). Best-effort: failures are logged and never block the pipeline. Scenario rows are deduped by summary content (near-duplicate summaries across sessions are skipped). Note: L2 re-embeds this session's atoms to cluster them (`store_atoms` computes embeddings internally but doesn't return them); with the default local-ONNX embedder this is cheap CPU work, but with an HTTP embedding API (OpenAI/DashScope) it roughly doubles the per-session embedding cost — weigh accordingly. A `store_atoms`-returns-embeddings refactor to avoid the re-embed is tracked as future work.

## 12. Hermes Plugin Integration

The `hermes-plugin` Python package provides `AMSMemoryProvider` for [Hermes Agent](https://github.com/NousResearch/hermes-agent) integration. It implements the Hermes `MemoryProvider` ABC.

### Installation

**Step 1**: Copy plugin files to Hermes plugin directory:

```bash
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
mkdir -p "$HERMES_HOME/plugins/ams_memory"
cp hermes-plugin/ams_memory/* "$HERMES_HOME/plugins/ams_memory/"
pip3 install requests  # only external dependency
```

Or use the install script: `cd hermes-plugin && ./install.sh`

**Step 2**: Activate in Hermes config (`~/.hermes/config.yaml`):

```yaml
memory:
  provider: ams_memory
```

**Step 3**: Configure via environment variables or `~/.hermes/ams.json`:

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

### Plugin Discovery

Hermes scans `$HERMES_HOME/plugins/` for directories containing `provider.py`. Each `provider.py` must export a `register(ctx)` function that calls `ctx.register_memory_provider()`. The AMS plugin's `register()` function loads configuration from env vars / `ams.json` and registers `AMSMemoryProvider`.

### Configuration

| Env Var | JSON Key | Default | Description |
|---------|----------|---------|-------------|
| `AMS_GATEWAY_URL` | `gateway_url` | `http://127.0.0.1:8765` | Gateway URL |
| `AMS_API_KEY` | `api_key` | *(empty)* | Auth key |
| `AMS_RECALL_TOP_K` | `recall_top_k` | `5` | Memories per query |
| `AMS_AUTO_RECALL` | `auto_recall` | `true` | Auto recall |
| `AMS_AUTO_STORE` | `auto_store` | `true` | Auto store |

JSON file values override environment variables.

### Behavior

- **`prefetch(query)`**: Called before each LLM API call. Sends `query` to `POST /recall`, returns formatted `<recalled_memories>` block for context injection. Skipped if `auto_recall=false`.
- **`sync_turn(user, assistant)`**: Called after each turn. Sends messages to `POST /capture` for persistent storage. Skipped if `auto_store=false`.
- **`on_session_end(messages)`**: Sends `POST /session/end` for future aggregation pipeline.
- **`handle_tool_call(name, args)`**: Handles `memory_search` and `memory_save` tool calls from the LLM. Returns JSON string.
- **`get_tool_schemas()`**: Returns `memory_search` and `memory_save` tool definitions in OpenAI function calling format.
- **Timeout handling**: Recall 5s, capture 10s. Failures logged but never block the agent.

## 13. Docker Deployment

```bash
# Build
docker build -t asuna-memory .

# Run with persistent data
docker run -d \
  -p 8765:8765 \
  -v ~/.asuna:/home/asuna/.asuna \
  -e AMS_GATEWAY_API_KEY=your-secret-key \
  --name asuna-memory \
  asuna-memory
```

Multi-stage build: Rust 1.75 builder → Debian bookworm-slim runtime. Includes Python3 + Hermes plugin pre-installed. Health check on `/health` every 30s. The runtime runs as a non-root user `asuna`; data persists via the Docker volume at `/home/asuna/.asuna`.
