# AMS (asuna-memory) v2.7.3 — AI Agent Execution Script

**Reader:** an AI agent with shell access, no prior knowledge. **Goal:** install AMS, configure, start, verify. This is your only execution script: run steps in order, verify each, continue on PASS only; FAIL → §7. All names/paths/ports/env vars are literal.

Data root: `~/.asuna` (Linux/macOS), `%USERPROFILE%\.asuna` (Windows). Binary: `asuna-memory` / `asuna-memory.exe`.

**Shell convention:** all commands are bash. PowerShell equivalents: `curl -s <url>` → `curl.exe -s <url>` (bare `curl` is an `Invoke-WebRequest` alias and `-s` breaks it); `VAR=value cmd` → `$env:VAR='value'` then `cmd`; background/detach → `Start-Process` (§5.2).

**Release status:** `v2.7.3` is published — `releases/latest` serves 2.7.3 (4 platform binaries + all 6 model assets) and default branch `main` carries the same version. v2.7.3 is a patch on v2.7.1: release test gate, unified `bounded_memory` writer, docs honesty (ARCHIVED specs + backlog). Version expectations below are still written against `<X>` = the exact version your installed binary prints (§2.1/§2.2), so a newer release never breaks this script.

## 1 Prerequisites

| Need | Check | Pass | Needed for |
|---|---|---|---|
| Rust ≥ 1.88 | `rustc --version` | `rustc 1.88+` | source build only (`rust-version = "1.88"`) |
| git | `git --version` | prints version | source build (§2.2) **and** Docker route — §5.3 clones the repo |
| curl | `curl --version` | prints version | any route |
| Docker | `docker --version` | prints version | Docker route |
| disk | `df -h ~` | ≥ 500 MB; ~4 GB more if building source (`target/`) | any route (model ~308 MB) |

No other system deps; SQLite is bundled.

## 2 Install — pick one route

### 2.1 Prebuilt release binary (recommended)

Base URL `https://github.com/Michaol/asuna-memory-system/releases/latest/download/`:

| Platform | Archive | Contains |
|---|---|---|
| Windows x64 | `asuna-memory-windows-x64.exe.zip` | exe + `onnxruntime.dll` + `onnxruntime_providers_shared.dll` |
| Linux x64 | `asuna-memory-linux-x64.tar.gz` | binary + `libonnxruntime.so` |
| Linux arm64 | `asuna-memory-linux-arm64.tar.gz` | binary + `libonnxruntime.so` |
| macOS arm64 | `asuna-memory-macos-apple-silicon.tar.gz` | binary + `libonnxruntime.dylib` |

```bash
mkdir -p ~/ams && cd ~/ams
curl -sL https://github.com/Michaol/asuna-memory-system/releases/latest/download/asuna-memory-linux-x64.tar.gz | tar xz
```

Windows PowerShell (same directory, `%USERPROFILE%\ams`):

```powershell
New-Item -ItemType Directory -Force $HOME\ams | Out-Null; Set-Location $HOME\ams
Invoke-WebRequest <zip url> -OutFile ams.zip; Expand-Archive ams.zip .
```

Leave library and binary in the same directory (auto-discovery §2.4).

**Verify:** `./asuna-memory --version` → a line `asuna-memory 2.x.y` (today `asuna-memory 2.7.3`). Record the exact number as `<X>` — §3.2/§5.1/§6 expect it. No such line (error page saved as file, corrupt archive) → STOP.

**Put the binary on PATH — REQUIRED before continuing.** §3 onward spells every command bare `asuna-memory`; the binary currently sits only in `~/ams` / `%USERPROFILE%\ams`, so without this step the first such command fails command-not-found (→ §7 row C).

bash/zsh (current session; append the same `export` line to `~/.bashrc` or `~/.zshrc` so future shells keep it):

```bash
export PATH="$HOME/ams:$PATH"
```

PowerShell (current session; for future shells run once, then restart: `[Environment]::SetEnvironmentVariable('Path', "$HOME\ams;$([Environment]::GetEnvironmentVariable('Path','User'))", 'User')`):

```powershell
$env:PATH = "$HOME\ams;$env:PATH"
```

**Verify:** `asuna-memory --version` → same `<X>` line. Not found → §7 row C (or keep using the full path: `~/ams/asuna-memory` / `$HOME\ams\asuna-memory.exe` for every later command).

### 2.2 Build from source

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system && cargo build --locked --release --bin asuna-memory
./target/release/asuna-memory --version   # must equal this checkout's Cargo.toml `version` (main today: 2.7.3); record it as <X>
```

Source builds lack `libonnxruntime` → keyword-only until §2.4 (or skip; §3.1).

**Put the binary on PATH — REQUIRED before continuing.** Same procedure and rationale as the PATH step of §2.1, with the directory `$PWD/target/release` (PowerShell: `$PWD\target\release`) instead of `~/ams`; persist for future shells the same way.

**Verify:** `asuna-memory --version` → same `<X>` line. Not found → §7 row C (or keep the full path `./target/release/asuna-memory`).

### 2.3 Docker — no host binary

The compose build in §5.3 compiles from source, so the route needs the repo checkout; the release archives of §2.1 contain neither the Dockerfile nor the compose file. §5.3 is self-contained (it starts with the `git clone`). Choose this route instead of §2.1/§2.2, not in addition — the PATH steps above don't apply; §6 HTTP checks (§6.2/§6.3) run against the container's published port, §6.1 `doctor` runs inside it (`docker exec ams-gateway asuna-memory doctor`).

### 2.4 ONNX Runtime library (local semantic search only)

File: `onnxruntime.dll` (win) / `libonnxruntime.so` (linux) / `libonnxruntime.dylib` (mac). Startup search order (first hit wins; exported as `ORT_DYLIB_PATH`): ① pre-set `ORT_DYLIB_PATH` ② binary's directory ③ `$HOME/.asuna/lib/` ④ `/usr/lib`, `/usr/local/lib`, `/usr/lib64` ⑤ system loader (`LD_LIBRARY_PATH` etc.). Missing → startup warn `ORT 动态库 ... 未在已知路径找到。语义搜索不可用。`; semantic search degrades to keyword (process keeps running). Fix: get the lib from `https://github.com/microsoft/onnxruntime/releases` (v1.24.4 ships in archives), place per ②③④ or set `ORT_DYLIB_PATH=/abs/path`.

## 3 Embedding backend — pick one

Runtime priority: API (`embedding.api_url` + `embedding.api_model` both non-empty) → local ONNX model → none (keyword-only).

**3.1 Keyword-only:** skip §3; §6 shows `嵌入引擎状态: DISABLED` = success.

**3.2 Local ONNX (~308 MB):** `asuna-memory model-download` fetches 6 files from GitHub Release tag `v<X>` (the release matching your binary's `--version`; the published v2.7.3 release carries all six assets — a binary newer than the last published tag 404s) into `~/.asuna/models/embeddinggemma-300m-q8/`; existing files with exact size are skipped (resumes). Verification = exact byte size + HTTPS (SHA256 branch exists in code; hashes not yet published). If an asset 404s: download the same names from `https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX` (`onnx/` subdir for the 2 model files, repo root for the rest) into the target dir flat:

| File | Bytes |
|---|---|
| `model_quantized.onnx` | 3347993 |
| `model_quantized.onnx_data` | 302010368 |
| `tokenizer.json` | 17518607 |
| `tokenizer_config.json` | 20671 |
| `config.json` | 1308 |
| `special_tokens_map.json` | 2432 |

**CRITICAL:** model outputs **768** dims, config default is 1024 → with local model set `embedding.dimensions=768` (§4.2), else embeddings are rejected at save time and vectors skipped (§7 D/I). Verify §6.1: `嵌入引擎状态: OK (维度=768)` proves lib+model load; a config-dim mistake surfaces only on first write (§7 D).

**3.3 Embedding API (no model files):**

| Field | Default | Rule |
|---|---|---|
| `embedding.api_url` | `""` | OpenAI-compatible or DashScope base URL |
| `embedding.api_model` | `""` | e.g. `text-embedding-v4`; with `api_url` activates API |
| `embedding.api_key` | `""` | env `AMS_EMBEDDING_API_KEY` fills when empty |
| `embedding.api_format` | `""` | `openai` \| `dashscope`; auto→dashscope if url contains `dashscope` |
| `embedding.dimensions` | `1024` | must equal model output dim; mismatch → §7 row D |
| `embedding.batch_size` | `32` | per-call text count; clamped to 10 for dashscope |

API failures make at most **3 attempts** per batch (first call + 2 retries): sleep 1 s before attempt 2, 2 s before attempt 3 — there is no third sleep, attempt 3's failure returns. Retried causes: network/transport errors and HTTP 429/500/502/503/504 only (other statuses and local validation errors, e.g. dimension mismatch, fail immediately). After the third failure the batch is skipped with a warning (data saved).

## 4 Configuration

`~/.asuna/config.json` (override: `--config <path>`; `~` expands; `src/config.rs`): every section/key optional — `{}` boots, missing file boots, unknown keys ignored; precedence config.json > env > default — **one exception, gateway auth**: a non-empty `AMS_GATEWAY_API_KEY` flips `auth_enabled` on even if config.json says `false`, and `AMS_GATEWAY_AUTH_ENABLED` outranks both (security-direction overrides; §4.1). Storage bound to the startup profile (`--profile <id>`).

### 4.1 Env vars

| Var | Effect |
|---|---|
| `AMS_GATEWAY_API_KEY` | fills `gateway.api_key` (trimmed; whitespace-only = unset); **non-empty key also enables auth** unless `AMS_GATEWAY_AUTH_ENABLED=false` |
| `AMS_GATEWAY_AUTH_ENABLED` | `true`/`1` on, `false`/`0` off (outranks implication), other → warn+ignore |
| `AMS_GATEWAY_BIND_HOST` | fills empty `gateway.bind_host`; blank/absent → `127.0.0.1`; non-loopback without auth+key refuses startup |
| `AMS_LLM_BASE_URL` / `AMS_LLM_API_KEY` / `AMS_LLM_MODEL` | fill `llm.*` (aliases `OPENAI_BASE_URL`/`OPENAI_API_KEY`/`OPENAI_MODEL`); model fallback `deepseek-v3` |
| `AMS_EMBEDDING_API_KEY` | fills `embedding.api_key` |
| `RUST_LOG` | log filter (default `info`); logs → **stderr** (stdio-safe) |
| `ORT_DYLIB_PATH` | abs path to ONNX Runtime lib (§2.4) |
| `ASUNA_DEV_ROOT` | Windows only: model dir `$ASUNA_DEV_ROOT/models/embeddinggemma-300m-q8` checked before `~/.asuna/models/...` |
| `AMS_GATEWAY_PORT` | **not read by the binary** — Docker entrypoint only (§5.3) |

LLM config gates the post-session pipeline only; capture/search work without it (`/session/end` → `pipeline:"skipped (no LLM configured)"`).

### 4.2 Example: local ONNX (keyword route needs no file)

```json
{ "embedding": { "dimensions": 768 } }
```

### 4.3 Example: API embeddings + LLM

```json
{
  "embedding": {
    "dimensions": 1024,
    "api_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
    "api_model": "text-embedding-v4"
  },
  "llm": { "base_url": "https://api.deepseek.com/v1", "api_key": "sk-replace-me", "model": "deepseek-chat" }
}
```

Remaining sections (`conversation`/`memory`/`search`/`graph`/`pipeline`/`admission`/`recall`/`scenarios`/`persona`) all have working defaults — field list: `src/config.rs`; behavior-relevant ones are cited in §7/§8.3.

## 5 Start — pick one form

### 5.1 MCP stdio

```bash
asuna-memory serve        # bare `asuna-memory` = serve
```

JSON-RPC 2.0 line protocol: 1 request → 1 response, one per line. stdout = protocol only; logs on stderr. Client registration:

```json
{ "mcpServers": { "asuna-memory": { "command": "/abs/path/to/asuna-memory", "args": ["serve"] } } }
```

`/abs/path/to/asuna-memory` must be the real absolute path — MCP clients spawn the server without your shell PATH, so a bare `asuna-memory` here typically fails. Get the path from the §2.1/§2.2 binary: `command -v asuna-memory` (bash, after the PATH step) / `(Get-Command asuna-memory).Source` (PowerShell); §2.1 route: `~/ams/asuna-memory` or `%USERPROFILE%\ams\asuna-memory.exe`.

Sequence: ① `initialize` → ② `notifications/initialized` (no response) → ③ `tools/list`/`tools/call`. First request:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}
```

Expected response: `"protocolVersion":"2024-11-05"` + `"serverInfo":{"name":"asuna-memory","version":"<X>"}` (`<X>` = binary's `--version`). Tool errors return `result.content` + `"isError":true` (not JSON-RPC `error`).

### 5.2 HTTP gateway

```bash
asuna-memory gateway --port 8765
```

**This command blocks forever (the server runs in the foreground) — it never "finishes".** Keep it running while you execute §6.2/§6.3: either run it in a second terminal, or detach —

```bash
nohup asuna-memory gateway --port 8765 >/tmp/ams-gw.log 2>&1 &   # stop: pkill -f "asuna-memory gateway"
```

PowerShell: `Start-Process asuna-memory -ArgumentList 'gateway','--port','8765' -RedirectStandardError ams-gw.log` (stop: `Stop-Process -Name asuna-memory`). Success criterion = the listening line below appears (in the terminal or the log file), then proceed to §6.

**Always pass `--port`:** default `0` = random port. Bind `127.0.0.1` unless `gateway.bind_host`/`AMS_GATEWAY_BIND_HOST`; non-loopback needs auth+key (§7 E). Auth: `Authorization: Bearer <key>` or `X-API-Key: <key>`, `/health` exempt. CORS: empty `cors_origins` + auth on → any origin; auth off → localhost origins only. Verify line: `AMS Gateway listening on http://127.0.0.1:8765` (stdout+stderr).

### 5.3 Docker

Compose (`hermes-plugin/docker-compose.yml`: builds repo root, `8765:8765`, volume `ams-data:/home/asuna/.asuna`, binds `0.0.0.0`). The image builds from source, so first fetch the repo (the §2.1 release archives don't contain it; skip the clone if you already have a checkout from §2.2):

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system/hermes-plugin && AMS_GATEWAY_API_KEY=your-secret-key docker compose up -d --build
```

Empty key → refuses (0.0.0.0 needs auth). Without compose, from the **repo root** (where the Dockerfile is): `docker build -t asuna-memory .` then `docker run -d -p 8765:8765 -v ~/.asuna:/home/asuna/.asuna -e AMS_GATEWAY_BIND_HOST=0.0.0.0 -e AMS_GATEWAY_API_KEY=<key> asuna-memory`.

Entrypoint: `doctor` (DB init; abort on failure) → auto `model-download` unless doctor shows `嵌入引擎状态: OK` (failure tolerated → keyword-only) → `gateway --port ${AMS_GATEWAY_PORT:-8765}`. Container default `dimensions`=1024 ≠ downloaded model's 768 → for semantic vectors write `{"embedding":{"dimensions":768}}` into `/home/asuna/.asuna/config.json`.

## 6 Verify — all must PASS

### 6.1 `asuna-memory doctor` (first run creates `~/.asuna` + DB)

| Line (literal prefix) | Healthy value (fail action) |
|---|---|
| `版本:` | `v<X>` (must equal `--version`) |
| `完整性检查:` | `OK` (else DB corrupt) |
| `外键约束:` | `ON` |
| `嵌入后端:` | `本地 ONNX` / `API (...)` / `无` (= keyword route) |
| `嵌入引擎状态:` | `OK (维度=768)` / `OK (API, 维度=N)` / `DISABLED` (keyword route); `FAILED` → §7 A. (A config-dim mistake does NOT show here — doctor probes without the configured dim; it surfaces on first write, §3.2/§7 D.) |
| `索引统计:` | `n 会话, n 轮对话, n 个向量` |
| `图谱:` | `ENABLED (n entities, n relations)`; a ⚠ note here = config.json has no `graph` section so defaults are in use — informational, not a failure |
| `bounded_memory[...]:` | `OK (n entries)`; `DIVERGED` → `doctor --fix` |
| `一致性:` | ends `→ OK`; else run `rebuild` |

### 6.2 `/health` (HTTP route): `curl -s http://127.0.0.1:8765/health` → `{"status":"ok","version":"<X>"}` (`<X>` = binary's `--version`)

### 6.3 Capture→recall smoke (HTTP; with auth add `-H "X-API-Key: <key>"`)

```bash
curl -s -X POST http://127.0.0.1:8765/capture -H 'content-type: application/json' \
  -d '{"session_id":"smoke-1","turns":[{"role":"user","content":"The device is called quantumfluxbanana"},{"role":"assistant","content":"noted quantumfluxbanana"}]}'
# expect {"status":"ok","turns_saved":2}
curl -s -X POST http://127.0.0.1:8765/recall -H 'content-type: application/json' \
  -d '{"query":"quantumfluxbanana","top_k":5}'
# expect memories[] contains {"layer":"L0","type":"turn",...} whose content contains quantumfluxbanana; context non-empty; truncated present
```

CLI cross-check: `asuna-memory search quantumfluxbanana` (default `--mode keyword`) → prints `共 2 条结果`: keyword search is turn-granular with no session dedup, and both smoke turns above contain the word (fresh DB; re-running the capture adds 2 more matches each time — the two listed results must be the `smoke-1` turns).

## 7 Troubleshooting: symptom → cause → fix

| ID | Symptom (exact text / behavior) | Cause | Fix |
|---|---|---|---|
| MSRV | cargo: `cannot be built because it requires rustc 1.88.0 or newer`（或依赖报 `feature edition2024 is required`） | old toolchain | `rustup update stable`, rebuild |
| A | warn `ORT 动态库 ... 未在已知路径找到。语义搜索不可用。` + doctor `嵌入引擎状态: FAILED` mentioning `ONNX Runtime` | lib not on search path | §2.4: place lib / `ORT_DYLIB_PATH` |
| B | `model-download` HTTP 404 (or `下载大小不符 <file>: X bytes (期望恰好 Y bytes)`) | release lacks model assets / truncated download | manual HF download (§3.2) or §3.3 API; size mismatch: delete file, rerun |
| C | `asuna-memory: command not found` (bash) / `The term 'asuna-memory' is not recognized...` (PowerShell) for any §3+ command | binary dir never added to PATH — §2.1/§2.2 PATH step skipped, or this is a new shell after it ran | rerun the §2.1/§2.2 PATH line in this shell, or use the full path everywhere: `~/ams/asuna-memory` (Windows `$HOME\ams\asuna-memory.exe`), source build `./target/release/asuna-memory` from the repo root; Docker route has no host binary (§2.3) |
| D | `嵌入维度不匹配：模型输出 768 维，但 config.embedding.dimensions=1024` | config dim ≠ model dim | set `dimensions` to model output (768 local), then `rebuild --full` |
| E | abort `gateway bind_host '0.0.0.0' is not a loopback address; non-loopback binding requires authentication` | public bind, no auth | `export AMS_GATEWAY_API_KEY=<key>`; restart |
| F | abort `Gateway auth is enabled but no API key is configured. Set AMS_GATEWAY_API_KEY environment variable.` | auth on, key empty | set key or `AMS_GATEWAY_AUTH_ENABLED=false` (loopback only) |
| G | bind error `Address already in use` (Linux, os error 98) / `Only one usage of each socket address (protocol/network address/port) is normally permitted` (Windows, os error 10048) | port taken | other `--port` (holder: `ss -ltnp` / `Get-NetTCPConnection`) |
| G2 | §6.2/§6.3 curl fails `Connection refused` / `Failed to connect` | gateway not running (foreground command exited / never started / detached process died — check the §5.2 log) | restart per §5.2 (keep it running), re-run §6 |
| H | writes fail `database is locked` (SQLITE_BUSY) | other writer held DB > 5000 ms (`busy_timeout=5000`) | serialize; one server per profile |
| I | `embeddings unavailable, vectors skipped — run rebuild later to backfill` / warn `capture: embedding batch failed` | embedder down (A/D/network) — data WAS saved | fix backend, then `asuna-memory rebuild` |
| J | log `vec_turns 距离度量/维度变更（现有: int8[1024], 目标: int8[768] cosine），已清空向量索引` | dim change wipes vec tables (by design) | `asuna-memory rebuild` (embedder working) |
| K | external `sqlite3` on memory.db: `no such tokenizer: jieba` | jieba registered in-process only | use `asuna-memory sql "SELECT ..."` (read-only) |

## 8 Reference

### 8.1 HTTP endpoints (auth on all but `/health`; errors `{"error":...}` 400/401/404/500; body ≤10MB, query ≤10k, entity ≤1k chars)

| Route | Semantics |
|---|---|
| `GET /health` | `{"status":"ok","version"}` |
| `GET /stats` | `{sessions,turns,vectors,entities,relations}` |
| `POST /capture` | `{session_id,turns:[{role,content,timestamp?}]}` → `{status,turns_saved}`; roles user/assistant/tool_call/system; ts epoch-ms or ISO; **appends** turns to existing session (save_session replaces) |
| `POST /recall` | `{query,top_k≤50,max_tokens,after,before,last_days}` → `{memories,context,truncated}`; layers §8.3 |
| `GET /recall/{node_id}` | offloaded text; id `task/step_n` (URL-encode `/`) |
| `POST /search` | text `{query,mode:keyword/semantic/hybrid,top_k≤50,role,after,before,last_days}` (results carry `scores`) or graph `{entity,max_hops≤10,relation_filter}` |
| `GET /persona` | chain USER.md → persona.md → last user-row; `{persona,source}` or `{persona:null,status:"not_found"}` |
| `POST /offload` | `{task_id,content}` → `{node_id:"<task>/step_<n>",bytes_stored}` → `refs/<task>/step_n.md` |
| `POST /graph/assert` | `{subject,predicate,object,confidence?}` — confidence is a JSON **string** here (e.g. `"0.8"`; the MCP tool takes a number); canonicalized, security-scanned |
| `POST /graph/neighbors` | `{entity,hops≤5,direction,rel_type,limit≤200}` |
| `POST /session/end` | `{session_id}` → `{status,end_ts,pipeline:"spawned"/"skipped (no LLM configured)"}`; 404 unknown |

Error-surface caveat: request-body JSON parse/type errors are rejected by the axum extractor itself — **415** (missing/invalid `Content-Type`) or **422** (wrong field types (valid JSON)) with a **plain-text** body; malformed JSON likewise returns **400** (also plain-text); the `{"error":...}` contract covers handler-level validation (400/401/404/500). Graph caveat: REST `/graph/*` does **not** check `graph.enabled` (the MCP `graph_*` tools do) — with the graph layer disabled in config, the REST endpoints still read and write graph rows.

### 8.2 MCP tools (15; `tools/call` params; bold = required)

| Tool | Notes |
|---|---|
| **save_session**(`session_id`,`turns`) | turn = `timestamp`(ISO 8601)+`role`+`content`; same id **replaces**; `profile` if sent must equal server profile (else `profile override not supported; start the server with --profile <id>`); → `{status,session_id,file_path,turns_saved}` + optional `warning`,`graph_pending` |
| **search_sessions**(`query`) | `search_mode` keyword/semantic/hybrid (default config, `hybrid`), `top_k` (default 5), `time_range{after,before,last_days}`, `role` |
| **memory_write**(`target`,`content`) | target ∈ `memory`/`user`; one entry/call (no `\n§\n`); scanned; exact dup rejected; caps 2200/1375 ch; `confidence` high/medium/low; `session_id` provenance |
| **memory_update**(`target`,`old_text`,`new_text`) | entry-level literal substring; `%`/`_` not wildcards; updates all hits |
| **memory_remove**(`target`,`old_text`) | drops whole matching entries |
| **memory_read**(`target`) | full file incl. header |
| **user_profile**(`action`) | read/write/update/remove = target user alias |
| rebuild_index() | background JSONL→DB (incremental) |
| rebuild_status() | `{status,idle/running/completed/failed, sessions_processed,turns_indexed,vectors_indexed,errors,elapsed_ms}` |
| **memory_provenance**(`target`) | `{total_entries,verified,missing_source,no_source}` |
| **graph_assert**(`triples`) | per triple src/rel/dst (+`src_type`,`dst_type`,`confidence` 0..1 def .5, `source_turn` recommended); entities keep first name; confidence MAX |
| **graph_neighbors**(`entity`) | `hops`1..5, `direction` out/in/both, `rel_type` (every hop), `limit`≤200 |
| **graph_path**(`src`,`dst`) | `max_hops`1..10; path alternates Entity/Edge |
| **graph_link_entity**(`from`,`to`) | irreversible alias merge; from==to → error |
| graph_prune_dangling() | NULL stale `source_turn` refs (after turn deletes) |

### 8.3 Recall & pipeline

Fill order L3 persona → L4 mental-models → L5 intent → L2 scenarios → L1 atoms(FTS) → L0 recent turns(LIKE). L4/L5 files >7 days old skipped. `context` always opens with the untrusted-data banner. Budget: the first item that does not fit in remaining `max_tokens` (default 2000 = `recall.token_budget`) is dropped whole **and so is everything after it** (prefix truncation, no backfill of smaller later items) → `truncated`; `max_tokens:0` → empty. Pipeline on `/session/end`: gated first by `pipeline.enable_extraction` **and `graph.enabled` — `graph.enabled=false` early-exits the entire post-session pipeline** (`run_pipeline`), i.e. it also turns off L1 extraction, L2 and the L3-L5 refresh, not just the graph step. Then: L1 extraction (sessions shorter than `pipeline.every_n_turns` (5) turns skipped — minimum-length gate, not throttle) → graph → optional L2 (`scenarios.enabled`, default off) → L3/L4/L5 refresh every `persona.trigger_every_n` (10; 0=off; only when `scenarios.enabled`).

### 8.4 CLI — beyond the commands in §2–§6 (globals `--config <path>` `--profile <id>`)

```bash
doctor [--verbose|--fix|--split-entries]   # verbose: graph coverage/dangling; fix: lossless DB/.md merge; split-entries: split multi-§ rows
search <query> [--top-k 5] [--mode keyword|semantic|hybrid] [--role r] [--after|--before RFC3339|--last-days N]  # default mode keyword
list-profiles | list-sessions [--last-days N] [--limit 20]
rebuild [--full] | export <session_id> | delete-turn <id>
import <file.jsonl>        # line 1 {"v":1,"type":"session_header","session_id","profile_id"(e.g. "default"),"start_time"(ISO),source?/title?/tags?}; then per turn {"ts","seq":1+,"role","content",+flattened metadata}
sql "SELECT ..."           # read-only: SELECT/PRAGMA/EXPLAIN/WITH only
```

### 8.5 Data layout

```text
~/.asuna/
├── config.json                      # optional; {} boots
├── models/embeddinggemma-300m-q8/   # §3.2 six files (shared across profiles)
└── profiles/<id>/
    ├── memory.db                    # SQLite WAL: sessions, turns, FTS, vectors, bounded_memory, audit, graph
    ├── conversations/YYYY/MM/DD/<YYYYMMDD>T<HHMMSS>_<sha256(session_id) first 8 hex>.jsonl   # literal uppercase T between date and time
    ├── refs/<task>/step_<n>.md      # /offload
    └── memory/MEMORY.md, USER.md, persona.md, scenarios/, mental_models/, intent/
```

### 8.6 Hermes plugin (optional)

Agent-framework integration provider, in `hermes-plugin/` (also holds the §5.3 compose file); install/env details in `hermes-plugin/README.md`; drives `/recall` `/capture` `/session/end`; never blocks the agent.

### 8.7 Source map

| Topic | File |
|---|---|
| CLI + doctor output | `src/main.rs` |
| config/env/paths/bind rules | `src/config.rs` |
| ORT discovery, embedder, dim validation | `src/embedder/mod.rs` |
| model files/sizes/download | `src/model_download.rs` |
| HTTP routes/handlers/auth | `src/transport/http.rs` |
| MCP schemas/handlers | `src/mcp/tools.rs` |
| JSONL naming / schema+migrations / recall engine | `src/index/conversation.rs` / `src/index/schema.rs` + `src/index/db.rs` / `src/memory/retrieval.rs` |
| container | `Dockerfile`, `docker/entrypoint.sh`, `hermes-plugin/docker-compose.yml` |

## 9 Operational red lines

1. JSONL under `conversations/` is the truth source: `delete-turn` clears DB only — `rebuild --full` restores it. Permanent delete = also remove from JSONL.
2. `rebuild --full` wipes all vectors then re-embeds; no working embedder → index stays empty (keyword still works).
3. Never hand-edit `MEMORY.md`/`USER.md` — use the memory tools; reconcile drift with `doctor --fix`.
4. `audit_log` has no retention — grows forever; prune manually.
5. Backup = stop the process, copy `~/.asuna/profiles/<id>/` + `config.json`; live copies can lose the WAL tail.
6. `/capture` (HTTP) appends turns to an existing session; `save_session` (MCP) replaces it — don't mix for one `session_id`.
7. One server per profile DB; external concurrent writers hit `busy_timeout` 5000 ms (row H).
8. Changing embedding backend or `dimensions` invalidates vectors → `rebuild --full` right after the config change.
