# AMS (asuna-memory) v3.0.0 — AI Agent Execution Script

**Reader:** an AI agent with shell access, no prior knowledge. **Goal:** install AMS, configure, start, verify. This is your only execution script: run steps in order, verify each, continue on PASS only; FAIL -> §7. All names/paths/ports/env vars are literal.

Data root: `~/.asuna` (Linux/macOS), `%USERPROFILE%\.asuna` (Windows). Binary: `asuna-memory` / `asuna-memory.exe`.

**Shell convention:** all commands are bash. PowerShell equivalents: `curl -s <url>` -> `curl.exe -s <url>`; `VAR=value cmd` -> `$env:VAR='value'` then `cmd`; background/detach -> `Start-Process` (§5.2).

**Release status:** `v3.0.0` removes all local ONNX dependencies and locks embeddings to **1024-dim online APIs** (with keyword-only fallback when unconfigured). Release archives ship the binary plus `NOTICE.md` and `LICENSES/` (no ONNX libs), verified with `SHA256SUMS.txt`. Version expectations below still accept `<X>` = the exact version printed by your binary (`3.0.0` for this checkout).

## 1 Prerequisites

| Need | Check | Pass | Needed for |
|---|---|---|---|
| Rust >= 1.89 | `rustc --version` | `rustc 1.89+` | source build only (`rust-version = "1.89"`) |
| git | `git --version` | prints version | source build (§2.2) **and** Docker route (§5.3) |
| curl | `curl --version` | prints version | any route |
| Docker | `docker --version` | prints version | Docker route |
| disk | `df -h ~` | >= 100 MB (binary); ~4 GB if building from source | SQLite bundled, no local model weights |

## 2 Install — pick one route

### 2.1 Prebuilt release binary (recommended)

Base URL `https://github.com/Michaol/asuna-memory-system/releases/latest/download/`:

| Platform | Archive | Contains |
|---|---|---|
| Windows x64 | `asuna-memory-windows-x64.zip` | `asuna-memory.exe`, `NOTICE.md`, `LICENSES/` |
| Linux x64 | `asuna-memory-linux-x64.tar.gz` | `asuna-memory`, `NOTICE.md`, `LICENSES/` |
| Linux arm64 | `asuna-memory-linux-arm64.tar.gz` | `asuna-memory`, `NOTICE.md`, `LICENSES/` |
| macOS arm64 | `asuna-memory-macos-apple-silicon.tar.gz` | `asuna-memory`, `NOTICE.md`, `LICENSES/` |

Also published: `SHA256SUMS.txt` for checksum verification.

```bash
mkdir -p ~/ams && cd ~/ams
curl -sL https://github.com/Michaol/asuna-memory-system/releases/latest/download/asuna-memory-linux-x64.tar.gz | tar xz
```

Windows PowerShell (`%USERPROFILE%\ams`):

```powershell
New-Item -ItemType Directory -Force $HOME\ams | Out-Null; Set-Location $HOME\ams
Invoke-WebRequest <zip url> -OutFile ams.zip; Expand-Archive ams.zip . -Force
```

**Verify:** `./asuna-memory --version` -> `asuna-memory 3.0.0` (record exact `<X>`).

**Put the binary on PATH — REQUIRED before continuing:**

```bash
export PATH="$HOME/ams:$PATH"
```

```powershell
$env:PATH = "$HOME\ams;$env:PATH"
```

**Verify:** `asuna-memory --version` -> same `<X>` line.

### 2.2 Build from source

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system && cargo build --locked --release --bin asuna-memory
./target/release/asuna-memory --version   # must print: asuna-memory 3.0.0
```

Add `$PWD/target/release` to `PATH` and verify `asuna-memory --version`.

### 2.3 Docker — no host binary

Self-contained container build from repo root (§5.3); §6 HTTP checks run against the published port and `doctor` runs inside the container (`docker exec ams-gateway asuna-memory doctor`).

## 3 Embedding backend — pick one (1024-dim locked)

**3.1 Keyword-only (zero config):** skip §3; §6.1 shows `嵌入后端: 无（纯关键词模式）` + `嵌入引擎状态: DISABLED` = success.

**3.2 Online 1024-dim Embedding API (no local model files):**

| Field | Default | Rule |
|---|---|---|
| `embedding.api_url` | `""` | OpenAI-compatible or DashScope base URL |
| `embedding.api_model` | `""` | e.g. `text-embedding-v4`, `text-embedding-3-small`; with `api_url` activates API |
| `embedding.api_key` | `""` | env `AMS_EMBEDDING_API_KEY` fills when empty (optional for Ollama/local API) |
| `embedding.api_format` | `""` | `openai` \| `dashscope`; auto-detects `dashscope` ONLY for native DashScope URLs (URLs containing `compatible-mode` stay OpenAI format) |
| `embedding.dimensions` | `1024` | **Locked to `1024` in v3.0.0.** Any other explicit value is rejected on load with `rebuild --full` guidance |
| `embedding.batch_size` | `32` | Per-call text count (`1..=1024`; DashScope URLs/format clamp automatically to `10`) |

Embedding API failures retry at most **3 attempts** per batch (sleeps `1s`, `2s` in production) on transient transport errors and HTTP `429`/`500`/`502`/`503`/`504` (exact enum — other 5xx such as 501/505/507 are permanent; strict permutation index validation + 32 MiB response limit). On exhaustion, the batch logs a warning and degrades to vectorless persistence (atoms are stored without a vector index and re-embedded by startup backfill).

## 4 Configuration

`~/.asuna/config.json` (override: `--config <path>`; `~` expands; implementation in `src/config/`): every section/key optional — `{}` boots, unknown keys ignored (`model_path` and `db_path` kept as ignored legacy keys); precedence `config.json > env > default` (except `AMS_GATEWAY_API_KEY` implies auth on unless `AMS_GATEWAY_AUTH_ENABLED=false`).

### 4.1 Env vars

| Var | Effect |
|---|---|
| `AMS_GATEWAY_API_KEY` | fills `gateway.api_key` (trimmed); **non-empty key enables auth** unless `AMS_GATEWAY_AUTH_ENABLED=false` |
| `AMS_GATEWAY_AUTH_ENABLED` | `true`/`1` on, `false`/`0` off (outranks key implication), other -> warn+ignore |
| `AMS_GATEWAY_BIND_HOST` | fills empty `gateway.bind_host`; blank/absent -> `127.0.0.1`; non-loopback without auth+key refuses startup |
| `AMS_LLM_BASE_URL` / `AMS_LLM_API_KEY` / `AMS_LLM_MODEL` | fill `llm.*` (with `OPENAI_BASE_URL` / `OPENAI_API_KEY` / `OPENAI_MODEL` fallbacks; `OPENAI_API_KEY` disabled when a custom base URL is set — config.json `llm.base_url` or `AMS_LLM_BASE_URL`; built-in default model `"deepseek-v3"`) |
| `AMS_EMBEDDING_API_KEY` | fills `embedding.api_key` |
| `RUST_LOG` | log filter (default `info`); logs -> **stderr** (stdio-safe) |
| `AMS_GATEWAY_PORT` | **Docker entrypoint only** (§5.3), not read directly by the binary |

### 4.2 Example: Online 1024-dim embeddings + LLM

```json
{
  "embedding": {
    "dimensions": 1024,
    "api_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
    "api_model": "text-embedding-v4"
  },
  "llm": {
    "base_url": "https://api.deepseek.com/v1",
    "api_key": "sk-replace-me",
    "model": "deepseek-chat"
  }
}
```

## 5 Start — pick one form

### 5.1 MCP stdio

```bash
asuna-memory serve
```

JSON-RPC 2.0 over newline-delimited stdio (`initialize` -> `notifications/initialized` -> `tools/list` / `tools/call` / `ping`). Strict validation returns `-32700` on invalid UTF-8 or malformed JSON, `-32600` on invalid envelope/`jsonrpc` or float `id`, and `-32602` on invalid params; `notifications/initialized` stays silent only when `id` is absent/`null` (a non-null `id` returns `-32601`).

### 5.2 HTTP gateway

```bash
asuna-memory gateway --port 8765
```

PowerShell detach: `Start-Process asuna-memory -ArgumentList 'gateway','--port','8765' -RedirectStandardError ams-gw.log`. Always pass `--port` (`0` = random). Verify log prints `AMS Gateway listening on http://127.0.0.1:8765`.

CORS: with `gateway.cors_origins` empty, only localhost/loopback origins are allowed (whole `127.0.0.0/8` + `::1`, http/https, any port) — in both auth modes (auth-on default narrowed from `Any` in v4 hardening; a warn is logged). Set `gateway.cors_origins` to an explicit origin list (or `*`) to allow web clients. All CORS configurations explicitly allow the `Authorization`, `Content-Type`, and `X-API-Key` headers.

### 5.3 Docker

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system/hermes-plugin && AMS_GATEWAY_API_KEY=your-secret-key docker compose up -d --build
```

Entrypoint extracts `--config` / `--profile` (both space and `=` forms) and forwards them to the pre-start `asuna-memory doctor` check, then starts `asuna-memory gateway --port "${AMS_GATEWAY_PORT:-8765}"`, forwarding extra `docker run` args; a caller-supplied `--port` replaces the injected default (clap rejects a duplicate `--port`). The runtime image ships `/app/NOTICE.md` and `/app/LICENSES/`.

## 6 Verify — all must PASS

### 6.1 `asuna-memory doctor`

| Line (literal prefix) | Healthy value (fail action) |
|---|---|
| `版本:` | `v<X>` (`v3.0.0`) |
| `完整性检查:` | `OK` |
| `外键约束:` | `ON` |
| `嵌入后端:` | `无（纯关键词模式）` or `API (<url> / <model>, format=<fmt>, dim=1024)` |
| `嵌入引擎状态:` | `DISABLED` (keyword) or `OK (API, 维度=1024)` or `FAILED (...)` -> check API url/model/key |
| `索引统计:` | `n 会话, n 轮对话, n 个向量` |
| `图谱:` | `ENABLED (n entities, n relations)` or `DISABLED (config.graph.enabled = false)` |
| `bounded_memory[...]:` | `OK (n entries)` (`DIVERGED` -> `doctor --fix`) |
| `一致性:` | ends `→ OK` (else `rebuild`) |

### 6.2 `/health`

`curl -s http://127.0.0.1:8765/health` -> `{"status":"ok","version":"3.0.0"}`.

### 6.3 Capture -> recall smoke

Auth: with `AMS_GATEWAY_API_KEY` set (mandatory in the §5.3 compose deployment) every endpoint except `/health` requires `Authorization: Bearer <key>` (or `X-API-Key: <key>`); a missing/wrong key returns `401` (see §7 L). The commands below assume the key is exported as `AMS_GATEWAY_API_KEY`; omit the `-H "Authorization: ..."` line for a loopback gateway started without a key (§5.2).

```bash
curl -s -X POST http://127.0.0.1:8765/capture -H 'content-type: application/json' \
  -H "Authorization: Bearer $AMS_GATEWAY_API_KEY" \
  -d '{"session_id":"smoke-1","turns":[{"role":"user","content":"The device is called quantumfluxbanana"},{"role":"assistant","content":"noted quantumfluxbanana"}]}'
# expect {"status":"ok","turns_saved":2}
curl -s -X POST http://127.0.0.1:8765/recall -H 'content-type: application/json' \
  -H "Authorization: Bearer $AMS_GATEWAY_API_KEY" \
  -d '{"query":"quantumfluxbanana","top_k":5}'
# expect memories[] contains {"layer":"L0","type":"turn",...}
```

## 7 Troubleshooting: symptom -> cause -> fix

| ID | Symptom | Cause | Fix |
|---|---|---|---|
| MSRV | cargo fails needing rustc `1.89.0` / `edition2024` | old toolchain | `rustup update stable` |
| C | `asuna-memory: command not found` | binary dir not in `PATH` | export PATH per §2.1/§2.2 or invoke full path |
| D | config load fails: `config.embedding.dimensions=... 不受支持` | non-1024 dimension in `config.json` | change `embedding.dimensions` to `1024` (or remove key), run `asuna-memory rebuild --full` |
| E | abort `non-loopback binding requires authentication` | public bind without auth | set `AMS_GATEWAY_API_KEY=<key>` |
| F | abort `Gateway auth is enabled but no API key is configured` | auth enabled with empty key | set key or `AMS_GATEWAY_AUTH_ENABLED=false` (loopback only) |
| G | `Address already in use` | port occupied | pick another `--port` |
| H | `database is locked` (`SQLITE_BUSY`) | external concurrent writer > 5000ms | serialize writers (one server per profile) |
| I | `embeddings unavailable, vectors skipped` | API embedder down/unconfigured | verify API url/model/key then run `asuna-memory rebuild` |
| J | `vec_turns ... 维度变更，已清空向量索引` | schema upgraded or metric changed | run `asuna-memory rebuild` with API embedder active |
| K | external `sqlite3` fails `no such tokenizer: jieba` | jieba tokenizer registered in-process only | use `asuna-memory sql "SELECT ..."` |
| L | runtime `401` `{"error":"API key required"}` or `{"error":"Invalid API key"}` on every endpoint except `/health` | auth enabled (compose always sets `AMS_GATEWAY_API_KEY`) but the request carries no key or a wrong one | send `Authorization: Bearer <key>` (or `X-API-Key: <key>`) matching `AMS_GATEWAY_API_KEY` (§6.3); startup aborts are E/F instead |

## 8 Reference

### 8.1 HTTP endpoints (auth on all except `/health`; errors return JSON `{"error":...}` via typed `ApiError`)

| Route | Semantics |
|---|---|
| `GET /health` | `{"status":"ok","version":"3.0.0"}` |
| `GET /stats` | `{sessions,turns,vectors,entities,relations}` |
| `POST /capture` | `{session_id,turns:[{role,content,timestamp?}],batch_id?}` -> `{status,turns_saved}` (appends turns; lock-free pre-embedding). Optional `batch_id` (<=255 chars, no control chars) is an idempotency key: a same-`(session_id,batch_id)` retry returns the first stored `turns_saved` without re-appending; absent -> normal append (backward-compatible) |
| `POST /recall` | `{query,top_k<=50 (default 10),max_tokens (clamped to 1_000_000),after,before,last_days}` -> `{memories,context,truncated}`; `top_k=0` or `max_tokens=0` -> skips DB/embedder and returns strictly empty (`memories:[]`, `context:""`, `truncated:false` — no L3-L5 either), and validation runs first so an invalid `after`/`before` still returns `400`; pre-embeds off lock |
| `GET /recall/*node_id` | offloaded text by wildcard id (handles `/` unencoded; `400` invalid id, `404` missing) |
| `POST /search` | text `{query,mode:hybrid/semantic/keyword,top_k<=50 (default 10),role,after,before,last_days}` or multi-hop `{entity,max_hops<=10,relation_filter (<=1000 chars),top_k<=50 (default 10)}` -> `{results,query_type,entity,max_hops,status,truncated}` (`truncated:true` iff BFS visited-set hit the `10_000`-node cap — partial results, server also warns); `relation_filter` >1000 chars -> `400`; explicit `top_k` applies to multi-hop too; `semantic` mode without an embedder returns empty without locking the DB; invalid `role` -> `400`; whitespace-only `entity` falls back to text search; `entity`+`query` both non-empty -> multi-hop wins (`query` ignored, warn logged) |
| `GET /persona` | `USER.md -> persona.md -> DB` (a comment-header-only `USER.md` falls through to `persona.md` -> DB); async `tokio::fs` |
| `POST /offload` | `{task_id,content}` -> `{node_id,bytes_stored}` (`400` scan reject/bad task_id/bad JSON shape) |
| `POST /graph/assert` | `{subject,predicate,object,confidence?}` (`confidence` accepts JSON number or string in `0.0..=1.0`; security scanned); response reflects resolved entity canonicals |
| `POST /graph/neighbors` | `{entity,hops 1..=5,direction out/in/both,rel_type,limit 1..=200}` -> `{entity,canonical,neighbors,count,truncated,status}`; `limit` 0/>200 or empty/whitespace `entity` -> `400`; `rel_type` capped at `1000` chars (`400` on exceed). `truncated:true` means the 10_000-node visit cap was hit — results are a partial set |
| `POST /session/end` | `{session_id}` -> `{status,session_id,end_ts,pipeline}` (`spawned` / `already_running` / `skipped (busy)` / `skipped (no LLM configured)`); returns `503` during graceful shutdown |

Graph gate: when `graph.enabled = false`, `/graph/assert`, `/graph/neighbors`, and multi-hop `/search` all return **`403 Forbidden`**.

**Capture retry (W4.3):** a `/capture` `408` (60s request timeout) does **not** mean the write failed — the detached `spawn_blocking` persist may still complete. Send an optional `batch_id` to make retries idempotent: a same-`(session_id, batch_id)` retry returns the first stored `turns_saved` without re-appending (the `capture_dedup` insert, `sessions`/`turns`/`vec_turns` writes **and the JSONL truth-source append** all execute in the same atomic transaction, and a pre-check short-circuits retries before embedding). If the JSONL append fails the whole batch rolls back and `/capture` returns `500` — the `batch_id` is **not** consumed, so a retry is safe. Without `batch_id`, `/capture` appends — query before retrying (e.g. `POST /search` for the turn content) and resend only the missing turns; a blind retry duplicates turns.

**Extractor pattern (W5.1 / W1 resolved):** `/capture`, `/offload`, `/session/end`, `/recall`, `/search`, `/graph/assert`, and `/graph/neighbors` all use the `JsonBytes` pre-body extractor (implemented in `src/transport/http/models.rs`): it validates `Content-Type: application/json` (charset parameter stripped, case-insensitive; `+json` suffixes rejected -> `415`) and runs endpoint pre-checks (`graph.enabled` -> `403` on `/graph/*`, shutdown -> `503` on `/session/end`) **before** buffering up to 10 MB (`DefaultBodyLimit` + `RequestBodyLimitLayer`, `413`), then serde-parses inside the blocking pool (a near-10MB body never monopolizes a tokio worker; malformed JSON/shape maps to `400` via `ApiError`, not axum's `422`).

### 8.2 MCP tools (15 tools; bold = required)

| Tool | Notes |
|---|---|
| **save_session**(`session_id`,`turns`) | replaces same `session_id` & remaps graph `source_turn` refs; `profile` if sent must match server profile |
| **search_sessions**(`query`) | strict `search_mode` (`hybrid`/`semantic`/`keyword`), `top_k<=50`, `time_range`, `role` |
| **memory_write**(`target`,`content`) | `memory`/`user`, single entry (no `\n§\n`), security scan + dup check + capacity guard |
| **memory_update**(`target`,`old_text`,`new_text`) | literal substring replacement; de-indexes stale vector & preserves schema |
| **memory_remove**(`target`,`old_text`) | removes matching rows (incl. superseded) and cleans vector/entity refs |
| **memory_read**(`target`) | full file incl. header |
| **user_profile**(`action`) | alias for `target="user"` (`read`/`write`/`update`/`remove`) |
| rebuild_index() | async JSONL -> DB rebuild |
| rebuild_status() | progress JSON |
| **memory_provenance**(`target`) | session source verification |
| **graph_assert**(`triples`) | max 200/batch, max 1000 chars/field, security scanned, MAX confidence |
| **graph_neighbors**(`entity`) | non-blank `entity`, `hops` 1..=5, `direction` out/in/both, `rel_type` <= 1000 chars, `limit` 1..=200 |
| **graph_path**(`src`,`dst`) | non-blank `src`/`dst`, `max_hops` 1..=10, bounded BFS (`MAX_PATH_RESULTS = 5_000` node expansions); result carries `truncated` — `found:false` with `truncated:true` means the search was cut short, **not** that no path exists |
| **graph_link_entity**(`from`,`to`) | irreversible alias merge, security scanned |
| graph_prune_dangling() | NULL stale `source_turn` refs |

**Validation gates:** unknown tool names -> protocol-level `-32602` (not `content`+`isError`); `session_id` arguments (`save_session` required; `memory_write`/`memory_update`/`memory_remove` optional) pass `validate_session_id` — rejects `':'`, path separators/`..`, control chars, >255 chars.

### 8.3 Recall & pipeline

- **Retrieval order**: `L3 persona` (DB `target='user'` multi-row concat fallback + `1200`-char ceiling + 40% share cap when `budget >= 200`) -> `L4 mental-models` (fresh < 7d) -> `L5 intent` (fresh < 7d) -> `L2 scenarios` (with `updated_at` time window) -> `L1 atoms` (**hybrid** `bounded_memory_fts` + `vec_bounded_memory` RRF with `0.75` cosine distance threshold) -> `L0 turns` (`turns_fts` multi-word AND/OR fallback, `LIKE` fallback for punctuation-only queries). Each layer degrades to warn + empty on its own query/IO failure, so `/recall` never fails wholesale over a single layer (L0 included, A4).
- **Post-session pipeline**: gated by `pipeline.enable_extraction && graph.enabled` -> Phase 1 reads JSONL full turns lock-free (4000-char cap + containment check + DB preview fallback) -> Phase 2 extracts L1 atoms (XML delimited) -> Phase 3a/3b/3c split-lock store + batched JSON admission (`score_batch`) + deferred atomic `MEMORY.md` flush -> L2 clustering reusing Phase 3 atom embeddings -> L3/L4/L5 consolidation (batched `generate_all` / `predict_all` with per-doc fallback & single-flight lock; L2–L5 security scanned).
- **L2–L5 opt-in gates** (`pipeline/mod.rs`): L2 clustering runs only when `scenarios.enabled` (default **false**); L3/L4/L5 consolidation requires `scenarios.enabled && persona.trigger_every_n > 0` (default `10`; `0` disables the cycle). With stock defaults the pipeline stores L1 only — recall surfaces whatever documents already exist; nothing L2–L5 is generated out of the box.

### 8.4 CLI

```bash
doctor [--verbose|--fix|--split-entries]   # exits non-zero on diagnostic errors
search <query> [--top-k 5] [--mode keyword|semantic|hybrid (default keyword; aliases vector=semantic, fts=keyword; unknown value -> error)] [--role r] [--after|--before RFC3339|--last-days N]   # rejects empty/blank query
list-profiles | list-sessions [--last-days N] [--limit 20]   # list-profiles: zero write/DB-creation side effects
rebuild [--full] | export <session_id> | delete-turn <id>
import <file.jsonl>          # rejects empty turns, cross-profile files (switch with --profile), and bad turn ts/role; logs L0 audit_log flags for unsafe turns
sql "SELECT ..."           # quote/backtick/bracket/comment-aware single-statement lexer + read-only SQLite Authorizer (query_only=ON; allows read-only PRAGMA integrity_check/quick_check/data_version)
```

### 8.5 Data layout

```text
~/.asuna/
├── config.json
└── profiles/<id>/
    ├── memory.db
    ├── conversations/YYYY/MM/DD/<YYYYMMDD>T<HHMMSS>_<sha256_prefix8>.jsonl
    ├── refs/<task>/step_<n>.md
    └── memory/{MEMORY.md, USER.md, persona.md, scenarios/, mental_models/, intent/}
```

### 8.6 Hermes plugin

`hermes-plugin/` provides `AMSMemoryProvider` (ordered single FIFO queue bounded at 128, isolated `requests.Session`, thread-safe session-ID locking, non-dict JSON response guarding, recall delimiter escaping) + `docker-compose.yml` + hash-locked `requirements-ci.txt`.

### 8.7 Source map

| Topic | Path |
|---|---|
| CLI + doctor + read-only SQL | `src/main.rs`, `src/cli/` (`mod.rs`, `args.rs`, `data_ops.rs`, `doctor.rs`, `sql.rs`) |
| Config / env / gateway bind | `src/config/` (`mod.rs`, `sections.rs`, `gateway.rs`) |
| 1024d API embedder & absmax INT8 | `src/embedder/` (`mod.rs`, `api.rs`) |
| Fact layer (session store & search) | `src/fact/` (`search.rs`, `session_store/{mod.rs, append.rs, jsonl_io.rs, legacy_files.rs, overwrite.rs}`) |
| Graph layer (canonical, store & BFS queries) | `src/graph/` (`mod.rs`, `canonical.rs`, `store.rs`, `query.rs`, `query/{neighbors.rs, path.rs}`) |
| HTTP gateway / `ApiError` / handlers | `src/transport/state.rs`, `src/transport/http/` (`mod.rs`, `error.rs`, `middleware.rs`, `models.rs`, `capture.rs`, `recall.rs`, `search.rs`, `ops_handlers.rs`, `backfill.rs`) |
| MCP server & tool schemas/handlers | `src/mcp/` (`server.rs`, `protocol.rs`, `tools/{mod.rs, schema.rs, handlers.rs, graph_handlers.rs}`) |
| L0-L5 memory & hybrid retrieval | `src/memory/` (`l1/{mod.rs, plan.rs, plan/{store.rs, embed.rs}}`, `retrieval/{mod.rs, budget.rs, cache.rs, l0_fallback.rs, l1_search.rs, layers.rs}`, `admission.rs`, `mental_model.rs`, `intent_prediction.rs`, `persona.rs`, `scenario.rs`, `chain.rs`, `dedup.rs`, `graph_integration.rs`, `llm.rs`) |
| Post-session pipeline & L2-L5 refresh | `src/service/pipeline/` (`mod.rs`, `l2.rs`, `consolidation.rs`) |
| Growth memory / security / audit | `src/growth/` (`bounded_memory/{mod.rs, write_state.rs, mutations.rs, reconcile.rs, format.rs}`, `security.rs`, `audit.rs`) |
| Short-term offload & quota | `src/short_term/` (`offload.rs`, `offload/quota.rs`) |
| SQLite schema, TX, FTS, vec, JSONL & rebuild | `src/index/` (`db.rs`, `db_migrations.rs`, `db_vec.rs`, `db_backfill.rs`, `schema.rs`, `fts.rs`, `fts5_ffi.rs`, `jieba_tokenizer.rs`, `vector.rs`, `conversation.rs`, `provenance.rs`, `sql_util.rs`, `rebuild/{mod.rs, consistency.rs, phase1_fact.rs, phase2_vectors.rs, progress.rs}`) |
| Shared utilities & HTTP client | `src/util/` (`mod.rs`, `time.rs`, `text.rs`, `frontmatter.rs`, `rrf.rs`, `http_client.rs`) |
| Container & license notices | `Dockerfile`, `docker/entrypoint.sh`, `hermes-plugin/docker-compose.yml`, `NOTICE.md`, `LICENSES/` |

## 9 Operational red lines

1. JSONL under `conversations/` is the source of truth (`delete-turn` removes DB rows only; `rebuild --full` restores from JSONL).
2. Always run `asuna-memory rebuild --full` once after upgrading from pre-v3.0 vector indexes (absmax INT8 scaling & 1024-dim invariant).
3. Never hand-edit `MEMORY.md`/`USER.md` directly — use `memory_*` tools or repair with `doctor --fix`.
4. Stop the server before copying `<profile>/` + `config.json` for backups.
5. Do not mix `/capture` (append) and `save_session` (overwrite) on the same `session_id`.
6. Keep `sqlite3` out of write paths; use `asuna-memory sql` for read-only inspection (`jieba` is in-process).
7. `audit_log` is append-only with no automatic retention/prune (by design — the audit trail is immutable). It grows unbounded on long-lived deployments; monitor disk usage and plan out-of-band retention if needed.
8. One server process per profile: the MEMORY.md generation guard (C8) and JSONL append serialization are in-process only. Do not run gateway + MCP/CLI concurrently against the same `<profile>/` — cross-process `.md` last-writer-wins and startup FTS-backfill races are not guarded.
9. `capture_dedup` (the `batch_id` idempotency table) is append-only with no automatic prune — one row per distinct `(session_id, batch_id)`, growing unbounded. `rebuild --full` and CLI `delete-turn` do not clear it (keys can outlive their data). The dedup row commits in the same atomic transaction as `sessions`/`turns`, so a partial-write phantom row cannot occur. Monitor disk usage and prune out-of-band if needed.
