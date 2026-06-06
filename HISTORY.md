# Asuna Memory System — Changelog History

This document contains the full upgrade guide and changelog for all versions prior to the current release.

For the latest version, see [README.md](README.md).

---

### Upgrading from v2.3.0 to v2.3.1

v2.3.1 fixes `reconcile_fix` (used by `doctor --fix`) from lossy overwriting `.md` with SQLite data to lossless merging both sources.

Upgrade steps:

1. Replace the binary
2. No configuration changes required
3. Run `asuna-memory doctor` — if any DIVERGED warnings appear, `doctor --fix` will now merge instead of overwrite

**v2.3.1 Changelog:**

🔴 **Critical Fix: `reconcile_fix` Lossless Merge**

- **Root cause**: `reconcile_fix()` overwrote `.md` entirely from SQLite data. Entries manually added to `.md` (not yet in DB) were silently lost when running `doctor --fix`
- **Fix**: Rewritten as a three-step lossless merge: (1) `.md`-only entries → INSERT into SQLite, (2) SQLite-only entries → appended to `.md`, (3) entries in both → unchanged
- **Bug fixes**: Corrected `datetime('now')` (TEXT) to `time::now_unix_ms()` (INTEGER) for `created_at`/`updated_at` columns; corrected `confidence = 0.5` (REAL) to `'medium'` (TEXT) matching schema type
- **`sync_atoms_to_md()` decoupled**: No longer calls `reconcile_fix()` — after atom capacity eviction, writes `.md` directly from DB state. This prevents evicted atoms from being re-inserted by the merge logic (regression in the old flow)
- **`doctor --fix` output**: Updated from `"rewrote .md from SQLite"` to `"merged .md and SQLite"`

🔵 **Code Quality**

- `test_reconcile_fix_preserves_md_only`: verifies `.md`-only entries survive merge (2 `.md` + 1 DB → 3 in both)
- `test_sync_atoms_no_regression`: verifies evicted atoms don't reappear in `.md`
- `test_reconcile_fix_restores_consistency`: updated for lossless semantics ("corrupted" is preserved as a `.md`-only entry)
- 178/178 tests pass

---

### Upgrading from v2.2.3 to v2.3.0

v2.3.0 adds configurable vector dimensions, third-party embedding API support (OpenAI-compatible + DashScope native), and configurable batch sizes.

Upgrade steps:

1. Replace the binary
2. Update `config.json` — add `embedding` fields if using API backend (see [Configuration](README.md#configuration))
3. If switching from local ONNX to API (or changing dimensions): `asuna-memory rebuild --full`
4. Run `asuna-memory doctor` to verify

**v2.3.0 Changelog:**

🟢 **New: Third-Party Embedding API Support**

- **Dual backend**: Auto-detects API backend when `api_url` + `api_model` are set; falls back to local ONNX otherwise
- **OpenAI-compatible format**: Works with OpenAI, Azure, Ollama, vLLM, LiteLLM, SiliconFlow, etc.
- **DashScope native format**: Supports Alibaba `text-embedding-v3/v4` with native request/response format and `text_index` ordering
- **Auto-detection**: `api_format` auto-detects DashScope from URL; can be overridden explicitly
- **API key via env var**: `AMS_EMBEDDING_API_KEY` environment variable as alternative to `config.json`

🟢 **New: Configurable Vector Dimensions**

- **Dynamic `vec0` DDL**: `init_schema()` generates vector table DDL from `config.embedding.dimensions` instead of hardcoded 768
- **Auto-migration**: Detects dimension mismatch on startup and automatically drops/recreates `vec_turns` and `vec_bounded_memory` tables
- **Default 1024d**: Recommended for best quality/cost balance with models like `text-embedding-v4`
- **Dimension-agnostic code**: All SQL queries use parameterized `vec_int8()` — no hardcoded dimension assumptions

🟡 **Performance: Configurable Batch Size**

- `embedding.batch_size` now controls actual API batch size (previously hardcoded to 32)
- DashScope users should set `"batch_size": 10` (API limit)
- `rebuild` and `backfill` both read batch size from embedder config

🔵 **Code Quality**

- `Db::dimensions()` / `set_dimensions()` for runtime dimension configuration
- `ApiEmbedder` module with OpenAI + DashScope format support and L2 normalization
- `LazyEmbedder` refactored with `Backend` enum (Onnx/Api)
- 176/176 tests pass

---

### Upgrading from v2.2.2 to v2.2.3

v2.2.3 fixes `vec_bounded_memory` being empty after schema migrations (float32→int8), which caused bounded memory semantic search to silently degrade to FTS-only.

Upgrade steps:

1. Replace the binary
2. Restart `ams-gateway.service` — on startup, `maybe_backfill_bounded_memory_vec()` automatically detects atoms missing vector embeddings and re-embeds them
3. Run `asuna-memory doctor` to verify `vec_bounded_memory` count matches atom count

**v2.2.3 Changelog:**

🔴 **Critical Fix: vec_bounded_memory Auto-Backfill**

- **Root cause**: `init_schema()` had a backfill for `bounded_memory_fts` but none for `vec_bounded_memory`. After the float32→int8 migration (v2.2.1) drops and recreates the table, existing atom entries lose their vectors permanently — semantic search on bounded memory silently degrades to FTS
- **Fix**: New `Db::maybe_backfill_bounded_memory_vec()` method checks for atoms in `bounded_memory` (where `memory_type='atom'`) that lack entries in `vec_bounded_memory`, batch-embeds them (32 per batch, Document prefix), and inserts transactionally
- **Auto-trigger**: Called on startup in both `run_gateway()` (HTTP mode) and `ToolHandler::new()` (MCP mode) — backfill failure never blocks service startup (errors logged as warnings)
- **Idempotent**: Already-indexed atoms are detected via existing `vec_bounded_memory` rowids and skipped; repeated restarts have zero overhead once fully backfilled

🔵 **Code Quality**

- `maybe_backfill_bounded_memory_vec()` mirrors the existing `maybe_backfill_bounded_memory_fts()` pattern
- Two call sites: `transport/http.rs` and `mcp/tools.rs`, both with `if let Err` graceful degradation
- 170/170 tests pass

---

### Upgrading from v2.2.1 to v2.2.2

v2.2.2 adds incremental rebuild mode: when JSONL files haven't changed, rebuild automatically skips Phase 1 (metadata + FTS) and resumes Phase 2 (vector embedding) from where it left off. This fixes the issue where interrupted rebuilds had to start from scratch.

Upgrade steps:

1. Replace the binary
2. No configuration changes required
3. Run `asuna-memory rebuild` — if previous rebuild was interrupted, it will automatically resume from the last completed batch

**v2.2.2 Changelog:**

🟢 **New: Incremental Rebuild Mode**

- **Auto-detect resume scenario**: `rebuild` command now checks if DB already has data matching the JSONL files. If counts match, it skips Phase 1 (metadata + FTS) and goes directly to Phase 2 (vector embedding)
- **`--full` flag**: Force complete rebuild from scratch with `asuna-memory rebuild --full` (ignores existing data)
- **Incremental by default**: When JSONL count matches DB session count, rebuild automatically skips Phase 1 and only embeds missing vectors
- **Clear logging**: Indicates which mode is running ("增量模式" vs "完整重建模式")

🟡 **Performance: Rebuild Resume Capability**

- **Phase 1 skip**: When in incremental mode, skips the expensive DELETE + INSERT of sessions/turns/FTS (~7 seconds for 4020 sessions)
- **Vector resume**: Phase 2 checks `vec_turns` for existing rowids and only embeds missing turns
- **Batch progress**: Continues from the last completed 320-record batch, not from the beginning
- **Safety**: If JSONL count differs from DB count, automatically falls back to full rebuild

🔵 **Code Quality**

- `should_do_incremental_rebuild()` helper function detects resume scenarios
- `rebuild_from_jsonl_with_callback()` now accepts `full_rebuild: bool` parameter
- All test cases updated to use `full_rebuild: true` for test isolation
- Zero new clippy warnings on modified files
- 170/170 tests pass

---

### Upgrading from v2.2.0 to v2.2.1

v2.2.1 unifies vector storage format: `vec_bounded_memory` now uses INT8 quantization (matching `vec_turns`), reducing storage by 4×.

Upgrade steps:

1. Replace the binary
2. Restart `ams-gateway.service` — existing `vec_bounded_memory` data (float32) is automatically migrated to int8 on first startup (old vectors are dropped and will be re-embedded on next pipeline run)
3. Run `asuna-memory doctor` to verify

**v2.2.1 Changelog:**

🟡 **Performance: Vector Storage Unification**

- **Schema change**: `vec_bounded_memory` virtual table changed from `float32[768]` to `int8[768]`, matching `vec_turns` format — **4× storage reduction** (768 bytes vs 3072 bytes per atom)
- **Write paths**: `L1Extractor::store_atoms()` now uses `quantize_to_int8()` + `vec_int8()` for both Unique and Conflict branches
- **Read path**: `load_existing_embeddings()` decodes int8 bytes back to f32 (`byte as i8 as f32 / 127.0`)
- **Search path**: `RetrievalEngine::search_atoms()` quantizes query embedding via `quantize_to_int8()` + `vec_int8()` for distance comparison
- **Migration**: `Db::init_schema()` detects old float32 schema and automatically drops/recreates the table with int8 format; existing atom vectors are lost (re-embedded on next pipeline run)

🔵 **Code Quality**

- `quantize_to_int8` imported from `embedder::onnx` into `memory::l1` and `memory::retrieval`
- Zero new clippy warnings on modified files
- 170/170 tests pass

---

### Upgrading from v2.1.1 to v2.2.0

v2.2.0 adds LLM-based entity extraction for automatic graph `mentions` relations and dual-writes auto-extracted atoms to MEMORY.md with capacity-aware LRU eviction.

Upgrade steps:

1. Replace the binary
2. No configuration changes required (new `memory.atom_capacity_ratio` defaults to 0.3)
3. Restart `ams-gateway.service` — new sessions will automatically extract entities and sync atoms to MEMORY.md

**v2.2.0 Changelog:**

🟢 **New: LLM-Based Entity Extraction for Graph**

- **Atom entities field**: `Atom` struct now includes `entities: Vec<String>` extracted by LLM alongside content/atom_type/confidence
- **LLM prompt update**: `extract_from_turns()` system prompt now instructs LLM to extract proper nouns, technical terms, product names, people, and organizations (max 5 per atom, original language)
- **Graph mentions relations**: Pipeline passes extracted entities to `integrate_atom_with_graph()`, creating `mentions` relations (atom → entity) automatically
- **Entity filtering**: Names shorter than 2 characters are filtered out to reduce noise

🟢 **New: Growth Layer Dual-Write**

- **MEMORY.md sync**: `L1Extractor::store_atoms()` now dual-writes atoms to both `bounded_memory` table (DB) and `MEMORY.md` (file), resolving the DB/.md inconsistency where `doctor` reported 3 extra entries in DB
- **Capacity-aware eviction**: `BoundedMemory::sync_atoms_to_md()` manages atom capacity with LRU eviction — oldest `memory_type='atom'` entries are evicted first when the atom budget (30% of MEMORY.md capacity by default) is exceeded
- **Manual entries protected**: `memory_type='manual'` entries are never evicted; only auto-extracted atoms are candidates for eviction
- **Configurable ratio**: `memory.atom_capacity_ratio` (default 0.3) controls the fraction of MEMORY.md reserved for atoms

🔵 **Code Quality**

- `BoundedMemory` gains `with_atom_capacity_ratio()` builder and `sync_atoms_to_md()` method
- `L1Extractor` gains `with_growth()` builder for optional growth layer integration
- `MemoryConfig` gains `atom_capacity_ratio` field with `#[serde(default)]` for backward compatibility
- Zero new clippy warnings on modified files
- 170/170 tests pass

---

### Upgrading from v2.1.0 to v2.1.1

v2.1.1 optimizes the `rebuild` command with two-phase transaction splitting, batch commits, and resume capability for vector embedding.

Upgrade steps:

1. Replace the binary
2. No configuration changes required
3. Run `asuna-memory rebuild` to benefit from improved performance and crash resilience

**v2.1.1 Changelog:**

🟡 **Performance: Rebuild Transaction Optimization**

- **Two-phase rebuild**: Phase 1 (metadata + FTS) runs in a single fast transaction (~21s); Phase 2 (vector embedding) runs in batched transactions (1000 turns per batch, ~30s each)
- **Crash resilience**: Previously, a 2-hour rebuild ran in one giant transaction — crash at 99% lost all work. Now, crash loses at most the current batch (~30s of work)
- **Resume capability**: Vector embedding phase checks `vec_turns` for existing turn_ids and skips already-indexed vectors. Re-running `rebuild` after a crash only embeds the remaining turns
- **Progress visibility**: `rebuild_from_jsonl_with_callback` accepts progress callback; MCP `rebuild_status` now shows real-time vector embedding progress per batch
- **WAL management**: Smaller transactions reduce WAL file growth (previously 18MB+ for 27,742 turns)

🔵 **Code Quality**

- `rebuild_from_jsonl` refactored into `rebuild_metadata()` (Phase 1) and `rebuild_vectors()` (Phase 2)
- New `RebuildStats.vectors_skipped` field for resume visibility
- Zero new clippy warnings
- 170/170 tests pass

---

### Upgrading from v2.0.4 to v2.1.0

v2.1.0 adds automatic graph construction pipeline that extracts L1 atoms and builds knowledge graph entities/relations when sessions end.

Upgrade steps:

1. Replace the binary
2. Set LLM API credentials: `export AMS_LLM_BASE_URL=https://api.deepseek.com/v1` and `export AMS_LLM_API_KEY=sk-...`
3. Enable pipeline in `config.json`: `"pipeline": { "enable_extraction": true, "every_n_turns": 5 }`
4. Restart `ams-gateway.service`
5. Graph entities/relations will be automatically created when sessions end (triggers `/session/end`)

**v2.1.0 Changelog:**

🟢 **New: Automatic Graph Construction Pipeline**

- **Post-session L1 extraction**: When `/session/end` is called, the gateway spawns a background task that reads session turns, extracts atomic facts via LLM (`L1Extractor`), stores atoms with embeddings, and integrates them into the knowledge graph (`integrate_atom_with_graph`)
- **Configuration-driven**: Pipeline controlled by `pipeline.enable_extraction` (default: false) and `graph.enabled` (default: true) in `config.json`. Sessions shorter than `pipeline.every_n_turns` (default: 5) are skipped
- **Non-blocking**: Pipeline runs in `tokio::task::spawn_blocking` to avoid starving the HTTP server during LLM calls (~2-5s)
- **LLM client**: `LlmClient` now derives `Clone` and adds `from_config(&LlmConfig)` constructor; reads from `AMS_LLM_BASE_URL` / `AMS_LLM_API_KEY` / `AMS_LLM_MODEL` environment variables or `config.json` `llm` section
- **AppState extended**: HTTP gateway state now includes `llm: Option<Arc<LlmClient>>`; pipeline gracefully skips when LLM is not configured (Lite mode)
- **Response field**: `/session/end` now returns `"pipeline": "spawned"` or `"skipped (no LLM configured)"` instead of generic message

🔵 **Code Quality**

- New module `transport/pipeline.rs` — isolated pipeline logic (~180 lines)
- `transport/mod.rs` updated to export `pipeline` module
- Zero new clippy warnings on modified files
- 170/170 tests pass

---

### Upgrading from v2.0.3 to v2.0.4

v2.0.4 fixes MCP serve process crash when `libonnxruntime.so` is not found, causing `search_sessions` to return "Connection closed".

Upgrade steps:

1. Replace the binary **and** `libonnxruntime.so` files (both included in the release archive)
2. Restart `ams-gateway.service` — the binary now auto-discovers `libonnxruntime.so` from the same directory, `~/.asuna/lib/`, or `/usr/local/lib/`
3. Run `asuna-memory doctor` — expect `嵌入引擎状态: OK` if `.so` is found, or a clear warning with fix instructions if not

**v2.0.4 Changelog:**

🔴 **Critical Fix**

- **MCP serve crash on missing ONNX Runtime**: `ort` crate (`load-dynamic` feature) panics when `libonnxruntime.so` is not loadable. New `init_ort_library_path()` auto-discovers the library from the executable directory, `~/.asuna/lib/`, or standard system paths (`/usr/lib`, `/usr/local/lib`) and sets `ORT_DYLIB_PATH` before any `ort` call. If the library is truly absent, `ort_available()` safely probes via `libloading` and caches the failure, enabling graceful degradation to keyword search instead of process crash.

🟡 **Docker & Installation**

- **Dockerfile**: Runtime image now installs ONNX Runtime from Microsoft official releases (auto-selects x64/aarch64 via `TARGETARCH`), merged into single `RUN` layer
- **Installation docs**: README and `for_ai.md` now include `sudo mv libonnxruntime.so* /usr/local/lib/` step; notes about auto-discovery behavior
- **`doctor` command**: Shows actionable fix instructions when ORT is unavailable (`LD_LIBRARY_PATH`, `ORT_DYLIB_PATH`, or standard path suggestions)

🔵 **Code Quality**

- `libloading` promoted to direct dependency (was already transitive via `ort`)
- `OnceCell<bool>` global cache for ORT probe result (zero-cost after first check)
- Per-instance `load_failed` cache avoids repeated global cache lookups

---

### Upgrading from v2.0.2 to v2.0.3

v2.0.3 fixes L1 FTS recall failures on unmigrated databases, WAL data visibility issues, and reliability of the `/capture` gateway endpoint.

Upgrade steps:

1. Replace the binary
2. Restart `ams-gateway.service` (triggers `wal_checkpoint(TRUNCATE)`, flushes stale WAL data into the main DB file)
3. Run `asuna-memory rebuild` to backfill vector embeddings for turns previously captured without embeddings
4. Run `asuna-memory doctor` to verify

**v2.0.3 Changelog:**

🔴 **Critical Fixes**

- **L1 FTS column mismatch**: SQL referenced `confidence_score` (REAL, P3 migration column) which may not exist on unmigrated databases, causing `"no such column: bm.confidence_score"` errors. All queries now use the always-present `confidence` (TEXT) column with `CASE` mapping (`'high'`→1.0 / `'medium'`→0.5 / `'low'`→0.25). Affects L1 FTS recall, chain queries, retrieval fallback ordering, and batch search.
- **WAL never checkpointed**: `Db::open()` now executes `PRAGMA wal_checkpoint(TRUNCATE)` on startup, flushing WAL data accumulated by the long-running gateway process into the main DB file. This fixes external tools (`asuna-memory sql`, MCP serve subprocess) seeing stale/empty tables despite data being present in the WAL.

🟡 **`/capture` Gateway Endpoint Overhaul**

- **Transaction atomicity restored**: Session + turns INSERT wrapped in `unchecked_transaction()`, preventing half-written state on mid-operation failure
- **Vector embedding generation**: `/capture` now generates turn embeddings via `embed_document()` (Document task prefix) and writes to `vec_turns`, enabling L0 vector search for gateway-captured turns
- **Embedder lock optimization**: Lock acquired once outside the turn loop instead of per-turn, reducing contention on concurrent requests
- **JSONL archival**: Turns are now appended to JSONL files (using `OpenOptions::append`) for archival and `rebuild` compatibility, matching the `save_session` MCP path
- **Error visibility**: `vec_turns` insert failures and JSONL write failures now produce `tracing::debug` / `tracing::warn` log entries instead of silent discard
- **TOCTOU-safe JSONL creation**: Uses `file.metadata().len()` after opening instead of `path.exists()` before, eliminating the race window

🔵 **Code Quality**

- `confidence_text()` extracted to `memory/mod.rs` (was in `chain.rs`), reducing cross-module coupling
- `parse_timestamp()` helper deduplicates timestamp parsing (was repeated 3× in capture)
- Preview length now uses `config.conversation.preview_length` instead of hardcoded 500

---

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

---

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

---

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

---

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

---

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

---

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
