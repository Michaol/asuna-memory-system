# Asuna Memory System — AI Agent Integration Guide

This document is for AI Agents only. It covers installation, MCP server startup, tool parameters, and usage patterns. Concise format optimized for token efficiency.

## 1. Install

### Option A: Download pre-built binary (recommended)

Download from [GitHub Releases](https://github.com/Michaol/asuna-memory-system/releases):

- Windows: `asuna-memory-x86_64-pc-windows-msvc.exe`
- Linux: `asuna-memory-x86_64-unknown-linux-gnu`

```bash
# Linux
curl -L -o asuna-memory https://github.com/Michaol/asuna-memory-system/releases/latest/download/asuna-memory-x86_64-unknown-linux-gnu
chmod +x asuna-memory
sudo mv asuna-memory /usr/local/bin/asuna-memory
```

### Option B: Build from source

Requires: Rust 1.75+, Windows/Linux.

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system
cargo build --release
# Binary: target/release/asuna-memory (.exe on Windows)
```

No external dependencies. SQLite is bundled. ONNX Runtime and model files are optional (semantic search falls back to keyword search if absent).

## 2. Start Server

```bash
asuna-memory serve
```

Protocol: JSON-RPC 2.0 over stdio. One request per line on stdin, one response per line on stdout. Do not write anything else to stdout.

### MCP Handshake Sequence

1. Send `initialize` request → receive `initialize` response
2. Send `notifications/initialized` notification (no response expected)
3. Use `tools/list` and `tools/call` freely

### Example: Initialize

Request:
```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"your-agent","version":"1.0"}}}
```

Response:
```json
{"id":1,"jsonrpc":"2.0","result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"asuna-memory","version":"1.0.0"}}}
```

Then send:
```json
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

## 3. Tools

All tools are called via `tools/call` method with `name` and `arguments` params.

### 3.1 save_session

Save a conversation to the fact layer. Dual-writes: JSONL file + SQLite index.

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
        {"timestamp": "2026-04-10T10:00:00+08:00", "role": "user", "content": "Hello"},
        {"timestamp": "2026-04-10T10:00:05+08:00", "role": "assistant", "content": "Hi!", "metadata": {"model": "gpt-4", "usage": {"input_tokens": 10, "output_tokens": 5}}}
      ],
      "source": "openclaw",
      "title": "Greeting",
      "tags": ["greeting"]
    }
  }
}
```

Params:
- `session_id` (string, required): Unique session identifier
- `turns` (array, required): Each item has:
  - `timestamp` (string, required): ISO 8601 timestamp
  - `role` (string, required): one of `user`, `assistant`, `tool_call`, `system`
  - `content` (string, required): Turn content
  - `metadata` (object, optional): Arbitrary metadata (model, usage, tool info, etc.)
- `source` (string, optional): Source identifier
- `title` (string, optional): Session title
- `tags` (string[], optional): Tags
- `profile` (string, optional): Override default profile

### 3.2 search_sessions

Search historical conversations. Supports keyword, semantic, and hybrid modes.

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
      "time_range": {"last_days": 30},
      "role": "assistant"
    }
  }
}
```

Params:
- `query` (string, required): Search query
- `search_mode` (string, optional): `keyword` | `semantic` | `hybrid` (default: `hybrid`)
- `top_k` (integer, optional): Max results (default: 5)
- `time_range` (object, optional): `after` (ISO string), `before` (ISO string), or `last_days` (integer)
- `role` (string, optional): Filter by role

### 3.3 memory_write

Write a new entry to growth memory (MEMORY.md or USER.md). Content is security-scanned before write.

```json
{
  "name": "memory_write",
  "arguments": {
    "target": "memory",
    "content": "User prefers Rust over Go for backend services.",
    "confidence": "high"
  }
}
```

Params:
- `target` (string, required): `memory` or `user`
- `content` (string, required): Entry content
- `confidence` (string, optional): `high` | `medium` | `low` (default: `medium`)

Capacity limits: memory=2200 chars, user=1375 chars. Duplicate content is rejected. Entries are separated by `§`.

### 3.4 memory_update

Update an existing entry by substring match.

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
- `target` (string, required): `memory` or `user`
- `old_text` (string, required): Substring to find
- `new_text` (string, required): Replacement text

### 3.5 memory_remove

Remove an entry by substring match.

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
- `target` (string, required): `memory` or `user`
- `old_text` (string, required): Substring to match for removal

### 3.6 memory_read

Read the full growth memory content.

```json
{
  "name": "memory_read",
  "arguments": {"target": "memory"}
}
```

Params:
- `target` (string, required): `memory` or `user`

### 3.7 user_profile

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
- `action` (string, required): `read` | `write` | `update` | `remove`
- `content` (string): For `write` action
- `old_text` (string): For `update`/`remove` actions
- `new_text` (string): For `update` action
- `confidence` (string, optional): `high` | `medium` | `low`

### 3.8 rebuild_index

Rebuild SQLite index from all JSONL files. Use after manual JSONL edits or sync issues.

```json
{
  "name": "rebuild_index",
  "arguments": {}
}
```

No required params.

### 3.9 memory_provenance

Verify that growth memory entries can be traced back to source sessions.

```json
{
  "name": "memory_provenance",
  "arguments": {"target": "memory"}
}
```

Params:
- `target` (string, required): `memory` or `user`

## 4. Usage Patterns

### Pattern: Save then search

After saving a session, it becomes immediately searchable via keyword search. Semantic search requires the ONNX model.

### Pattern: Incremental memory building

Use `memory_write` with `confidence` levels. Periodically use `memory_provenance` to verify traceability. Use `memory_update` to refine entries rather than creating duplicates.

### Pattern: Session-based memory

When saving a session, the `session_id` can be referenced in `memory_write` calls (though not directly linked — provenance tracks source sessions separately).

### Pattern: Rebuild after migration

If you copy `~/.asuna/` to a new machine, run `rebuild_index` to sync the SQLite index with the JSONL files.

## 5. Data Layout

```
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
    └── multilingual-e5-small/
```

## 6. CLI Commands (for scripting)

```bash
asuna-memory serve                      # Start MCP stdio server (default)
asuna-memory doctor                     # Environment check
asuna-memory list-profiles              # List profiles
asuna-memory list-sessions --last-days 7 --limit 20
asuna-memory search "query" --mode hybrid --top-k 5
asuna-memory rebuild                    # Rebuild index from JSONL
asuna-memory import file.jsonl          # Import a session file
asuna-memory export <session_id>        # Export session summary
```

Global flags: `--config <path>` (default: `~/.asuna/config.json`), `--profile <id>` (default: `default`).
