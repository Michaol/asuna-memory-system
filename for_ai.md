# Asuna Memory System — AI Agent Integration Guide

This document is for AI Agents only. It covers installation, MCP server startup, tool parameters, and usage patterns. Concise format optimized for token efficiency.

**Server version covered:** v2.0.0-dev (Project Aegis)

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

# macOS Apple Silicon
curl -sL https://github.com/Michaol/asuna-memory-system/releases/latest/download/asuna-memory-macos-apple-silicon.tar.gz | tar xz
sudo mv asuna-memory /usr/local/bin/
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
    "serverInfo": { "name": "asuna-memory", "version": "2.0.0-dev" }
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

Result objects contain `turn_id`, `score`, `preview`, `session_id`, `timestamp_ms`, `role`.

### 4.3 memory_write

Write a new entry to growth memory (`MEMORY.md` for `target=memory`, `USER.md` for `target=user`). Content is security-scanned (Prompt injection, credential leaks, invisible Unicode) before write; rejected on hit. Capacity limits apply: memory=2200 chars, user=1375 chars. Duplicate content (exact string match against existing § entries) is rejected.

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

Returns an error if `old_text` is not found anywhere in the body. Capacity is rechecked after replacement.

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

Rebuild the SQLite index from all JSONL files. Rebuilds `sessions` / `turns` / `turns_fts` / `vec_turns` inside a single transaction with automatic `ROLLBACK` on any error. Use after manual JSONL edits, version upgrades (especially v1.2.0 → v1.2.1 to refresh embeddings with the new Document prefix), or sync issues.

```json
{
  "name": "rebuild_index",
  "arguments": {}
}
```

Response includes `sessions_processed`, `turns_indexed`, `vectors_indexed`, `errors`.

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
    "direction": "out",
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
asuna-memory doctor                     # Environment check (version, FK status, vector count, embedder dim)
asuna-memory list-profiles              # List profiles
asuna-memory list-sessions --last-days 7 --limit 20
asuna-memory search "query" --mode hybrid --top-k 5
asuna-memory rebuild                    # Rebuild FTS + vector index from JSONL (transactional, with rollback)
asuna-memory import file.jsonl          # Import a session file (auto-generates vectors with Document prefix)
asuna-memory export <session_id>        # Export session summary
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

### Endpoints

#### `GET /health`
Returns server status and version.

```json
{ "status": "ok", "version": "2.0.0-dev" }
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
// Response
{
  "memories": [
    { "layer": "L3", "type": "persona", "content": "..." },
    { "layer": "L2", "type": "scenario", "content": "..." },
    { "layer": "L1", "type": "fact", "content": "...", "confidence": 0.85 },
    { "layer": "L0", "type": "turn", "role": "user", "content": "...", "timestamp": 1714000000000 }
  ],
  "context": "[Persona] ...\n[Scenario] ...\nfact ...\n"
}
```

- `query` (string, required, max 10000 chars): Search query.
- `top_k` (integer, optional, default 10, max 50): Max results per layer.

#### `POST /search`
Text search or multi-hop graph search.

```json
// Text search request
{ "query": "Rust async", "mode": "hybrid", "top_k": 5 }

// Multi-hop graph search request
{ "query": "", "entity": "Alice", "max_hops": 2, "relation_filter": "knows" }

// Response (text search)
{ "results": [{ "turn_id": 42, "score": 0.95, "preview": "...", "session_id": "...", "timestamp_ms": 0, "role": "user" }], "query_type": "text", "count": 1, "status": "ok" }

// Response (multi-hop)
{ "results": [{ "id": 1, "content": "...", "memory_type": "atom", "confidence_score": 0.9, "created_at": 0 }], "query_type": "multi_hop", "entity": "Alice", "max_hops": 2, "status": "ok" }
```

- `query` (string, max 10000 chars): Text search query.
- `mode` (string, optional): `keyword` | `semantic` | `hybrid` (default).
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
- **Embedding correctness**: Stored documents always use the EmbeddingGemma `title: none | text:` prefix; queries always use `task: search result | query:`. Mixing of prefixes is impossible from the public API.
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

## 12. Hermes Plugin Integration

The `hermes-plugin` Python package provides `AMSProvider` for [Hermes Agent](https://github.com/NousResearch/hermes-agent) integration.

### Installation

```bash
pip install -e hermes-plugin/
```

### Usage

```python
from ams_memory import AMSProvider

provider = AMSProvider({
    "gateway_url": "http://127.0.0.1:8765",
    "auto_recall": True,     # Auto-recall memories before each response
    "auto_store": True,      # Auto-store conversations after responses
    "recall_top_k": 5,       # Number of memories to recall
    "store_threshold": 0.7,  # Confidence threshold for storage
})
```

### Behavior

- **Before response**: `AMSProvider.before_response()` calls `/recall` with the last user message as query, injects recalled memories as a `<recalled_memories>` system message.
- **After response**: `AMSProvider.after_response()` calls `/capture` with the last 10 messages + the generated response.
- **Timeout handling**: Recall timeout is 5s, capture timeout is 10s. Failures are logged but do not block the agent.

## 13. Docker Deployment

```bash
# Build
docker build -t asuna-memory .

# Run with persistent data
docker run -d \
  -p 8765:8765 \
  -v ~/.asuna:/data/asuna \
  -e AMS_GATEWAY_API_KEY=your-secret-key \
  --name asuna-memory \
  asuna-memory
```

Multi-stage build: Rust 1.75 builder → Debian bookworm-slim runtime. Includes Python3 + Hermes plugin pre-installed. Health check on `/health` every 30s. Data persisted via Docker volume at `/data/asuna`.
