# Asuna Memory System — AI Agent Integration Guide

This document is for AI Agents only. It covers installation, MCP server startup, tool parameters, and usage patterns. Concise format optimized for token efficiency.

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

Requires: Rust 1.75+, Windows/Linux.

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system
cargo build --release
# Binary: target/release/asuna-memory (.exe on Windows)
```

No external dependencies. SQLite is bundled. ONNX Runtime and model files are optional (semantic search falls back to keyword search if absent). Automatically detects ONNX input requirements for better model compatibility.

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
    "serverInfo": { "name": "asuna-memory", "version": "1.1.2" }
  }
}
```

Then send:

```json
{ "jsonrpc": "2.0", "method": "notifications/initialized" }
```

## 3. Tools

All tools are called via `tools/call` method with `name` and `arguments` params.

### 3.1 save_session

Save a conversation to the fact layer. Dual-writes: JSONL file + SQLite index + vector embeddings (when model is available).

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
      "time_range": { "last_days": 30 },
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
  "arguments": { "target": "memory" }
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

Rebuild SQLite index from all JSONL files. Rebuilds both FTS index and vector embeddings. Use after manual JSONL edits, version upgrades, or sync issues.

```json
{
  "name": "rebuild_index",
  "arguments": {}
}
```

No required params.

Response includes `vectors_indexed` field indicating how many int8 vectors were written.

### 3.9 memory_provenance

Verify that growth memory entries can be traced back to source sessions.

```json
{
  "name": "memory_provenance",
  "arguments": { "target": "memory" }
}
```

Params:

- `target` (string, required): `memory` or `user`

## 4. Usage Patterns

### Pattern: Save then search

After saving a session, it becomes immediately searchable via keyword search. Semantic/hybrid search requires the ONNX model and produces vector embeddings automatically on save.

### Pattern: Incremental memory building

Use `memory_write` with `confidence` levels. Periodically use `memory_provenance` to verify traceability. Use `memory_update` to refine entries rather than creating duplicates.

### Pattern: Session-based memory

When saving a session, the `session_id` can be referenced in `memory_write` calls (though not directly linked — provenance tracks source sessions separately).

### Pattern: Rebuild after migration or upgrade

If you copy `~/.asuna/` to a new machine or upgrade from v1.0.x to v1.1.0, run `rebuild_index` to sync both the FTS and vector indexes with the JSONL files.

### Pattern: When to save

| Scenario           | When             | Notes                                                      |
| ------------------ | ---------------- | ---------------------------------------------------------- |
| Agent conversation | End of each turn | Ensures conversation is archived for later search          |
| Batch migration    | One-time import  | Use `import` command to bulk-import JSONL files            |
| Periodic archive   | On a schedule    | Good for high-frequency chat (e.g., customer support bots) |
| User-triggered     | On user request  | Important conversations saved on demand                    |

Recommended: save after each conversation turn. Same `session_id` = overwrite (INSERT OR REPLACE).

## 5. JSONL File Format (for `import` command)

The `import` command reads a JSONL file: **1 Header line + N Turn lines**, one JSON object per line.

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

## 6. Integration Examples

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

### Python: Generate JSONL and import via MCP stdio

```python
import json
import subprocess

def save_session_mcp(session_id: str, turns: list[dict], **kwargs):
    """Call save_session via MCP stdio."""
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

## 7. Data Layout

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

## 8. CLI Commands (for scripting)

```bash
asuna-memory serve                      # Start MCP stdio server (default)
asuna-memory doctor                     # Environment check (reports vector count)
asuna-memory list-profiles              # List profiles
asuna-memory list-sessions --last-days 7 --limit 20
asuna-memory search "query" --mode hybrid --top-k 5
asuna-memory rebuild                    # Rebuild FTS + vector index from JSONL
asuna-memory import file.jsonl          # Import a session file (auto-generates vectors)
asuna-memory export <session_id>        # Export session summary
```

Global flags: `--config <path>` (default: `~/.asuna/config.json`), `--profile <id>` (default: `default`).
