# Asuna Memory System 实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 构建一个 Rust 原生的 AI Agent 记忆系统 MCP Server，实现事实层（JSONL 全量存档 + SQLite 索引）与成长层（有界 MEMORY.md / USER.md）的双层记忆架构。

**Architecture:** 独立 Rust binary 通过 MCP stdio 协议对接 AI Agent，使用 ONNX (`model_O4.onnx`) 本地 embedding + sqlite-vec 向量检索 + FTS5 全文检索。数据存储在 `~/.asuna/` 目录，支持智能发现复用 RustRAG 模型文件。

**Tech Stack:** Rust 2021, rusqlite + sqlite-vec, ort (ONNX Runtime), serde + serde_json, chrono, uuid, tokio, clap

**架构文档:** [`docs/architecture.md`](../architecture.md)
**设计决策:** [`docs/design_decisions.md`](../design_decisions.md)

---

## Phase 1：项目骨架与基础设施

### Task 1: Cargo 项目初始化

**Files:**

- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/config.rs`
- Create: `src/util/mod.rs`
- Create: `src/util/time.rs`
- Create: `src/util/id.rs`

**Step 1: 初始化 Cargo 项目**

```bash
cd e:\DEV\Asuna_memory_system
cargo init --name asuna-memory
```

**Step 2: 配置 Cargo.toml 核心依赖**

```toml
[package]
name = "asuna-memory"
version = "0.1.0"
edition = "2021"
description = "AI Agent Memory System - MCP Server"

[dependencies]
rusqlite = { version = "0.32", features = ["bundled", "blob"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[profile.release]
lto = true
strip = true
```

> 注: `ort` 和 `sqlite-vec` 在 Task 6 (嵌入引擎) 时再添加，避免首次编译过重

**Step 3: 实现 config.rs**

- 定义 `Config` struct (对照 architecture.md §十 配置文件)
- 实现 `Config::load(path)` + `Config::default()`
- 模型发现逻辑：按优先级搜索 model_O4.onnx 路径

**Step 4: 实现 util/time.rs**

- `ts_to_unix_ms(iso: &str) -> i64`
- `unix_ms_to_iso(ms: i64) -> String`
- `now_unix_ms() -> i64`

**Step 5: 实现 util/id.rs**

- `generate_session_id() -> String` (UUID v4)

**Step 6: 验证编译**

```bash
cargo check
cargo clippy
cargo test
```

**Step 7: Commit**

```bash
git add -A
git commit -m "feat: initialize project with config and utils"
```

---

### Task 2: SQLite 数据库层

**Files:**

- Create: `src/index/mod.rs`
- Create: `src/index/db.rs`
- Create: `src/index/schema.rs`
- Test: `src/index/db.rs` (内联 `#[cfg(test)]`)

**Step 1: 定义 schema.rs**

- 包含完整 SQL 建表语句 (对照 architecture.md §3.3)
- `sessions`, `turns`, `turns_fts`, `bounded_memory`, `audit_log` 五张表
- 所有索引定义

**Step 2: 实现 db.rs**

- `Db::open(path)` — 打开/创建数据库，启用 WAL 模式
- `Db::init_schema()` — 执行建表
- `Db::pragma_check()` — 完整性校验 (`PRAGMA integrity_check`)

**Step 3: 编写测试**

```rust
#[test]
fn test_open_and_init() {
    let db = Db::open(":memory:").unwrap();
    db.init_schema().unwrap();
    // 验证表存在
}

#[test]
fn test_wal_mode() {
    let db = Db::open_file(temp_path()).unwrap();
    // 验证 journal_mode = wal
}
```

**Step 4: 运行测试**

```bash
cargo test -- index
```

**Step 5: Commit**

```bash
git commit -am "feat: sqlite database layer with schema"
```

---

### Task 3: JSONL 对话存储（事实层核心）

**Files:**

- Create: `src/fact/mod.rs`
- Create: `src/fact/conversation.rs`
- Create: `src/fact/session_store.rs`
- Test: 内联测试

**Step 1: 定义数据模型**

```rust
// conversation.rs
#[derive(Serialize, Deserialize)]
pub struct SessionHeader {
    pub v: u32,
    pub r#type: String, // "session_header"
    pub session_id: String,
    pub start_time: String,
    pub profile_id: String,
    pub source: Option<String>,
    pub agent_model: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Turn {
    pub ts: String,
    pub seq: u32,
    pub role: String, // user/assistant/tool_call/system
    pub content: String,
    #[serde(flatten)]
    pub metadata: Option<serde_json::Value>,
}
```

**Step 2: 实现 JSONL 文件写入**

- `write_session(data_dir, header, turns)` — 生成年/月/日目录结构，写入 JSONL
- 文件名格式: `{ISO日期时间}_{session_id}.jsonl`

**Step 3: 实现 JSONL 文件读取**

- `read_session(path) -> (SessionHeader, Vec<Turn>)`
- `list_sessions(data_dir) -> Vec<PathBuf>` — 遍历目录

**Step 4: 实现 session_store.rs 双写**

- `SessionStore::save(header, turns)` — JSONL 写入 + SQLite sessions/turns 表插入

**Step 5: 测试**

```rust
#[test]
fn test_write_and_read_session() {
    // 写入临时目录 → 读回 → 验证内容完全一致
}

#[test]
fn test_directory_structure() {
    // 验证 年/月/日 目录正确创建
}

#[test]
fn test_session_store_dual_write() {
    // 验证 JSONL + SQLite 双写一致性
}
```

**Step 6: Commit**

```bash
git commit -am "feat: fact layer - JSONL storage and session store"
```

---

### Task 4: 成长记忆层

**Files:**

- Create: `src/growth/mod.rs`
- Create: `src/growth/bounded_memory.rs`
- Create: `src/growth/security.rs`
- Create: `src/growth/audit.rs`
- Test: 内联测试

**Step 1: 实现 bounded_memory.rs**

- `BoundedMemory::read(target: "memory"|"user")` → 读取 MEMORY.md / USER.md 全文
- `BoundedMemory::write(target, content, confidence)` → 追加条目 (检查容量上限)
- `BoundedMemory::update(target, old_text, new_text)` → 子串替换
- `BoundedMemory::remove(target, old_text)` → 删除匹配条目
- 容量检查：MEMORY 2200 chars / USER 1375 chars (可配置)
- 元数据头自动更新 `<!-- ASUNA MEMORY | capacity: ... | updated: ... -->`

**Step 2: 实现 security.rs 安全扫描**

```rust
pub fn scan_content(text: &str) -> ScanResult {
    // 1. Prompt injection 模式匹配
    // 2. 凭据格式检测 (sk-xxx, ghp_xxx, AKIA 等)
    // 3. 不可见 Unicode 检测
}
```

**Step 3: 实现 audit.rs 审计日志**

- `log_action(db, action, target, detail, session_id)` → 写入 audit_log 表

**Step 4: 测试**

```rust
#[test]
fn test_memory_write_and_read() { /* ... */ }
#[test]
fn test_capacity_limit() { /* 超过 2200 字符应报错 */ }
#[test]
fn test_update_substring() { /* ... */ }
#[test]
fn test_security_scan_injection() { /* 检测 "ignore previous instructions" */ }
#[test]
fn test_security_scan_api_key() { /* 检测 "sk-xxxx" 格式 */ }
```

**Step 5: Commit**

```bash
git commit -am "feat: growth layer - bounded memory, security, audit"
```

---

### Task 5: MCP Server 框架

**Files:**

- Create: `src/mcp/mod.rs`
- Create: `src/mcp/server.rs`
- Create: `src/mcp/tools.rs`
- Create: `src/mcp/protocol.rs`
- Modify: `src/main.rs`

**Step 1: 实现 MCP stdio 协议层**

- JSON-RPC 2.0 over stdin/stdout
- `initialize` → 返回 server info + capabilities
- `tools/list` → 返回工具列表
- `tools/call` → 路由到具体工具实现
- 参照 RustRAG 的 MCP 实现模式

**Step 2: 注册工具**

对照 architecture.md §5.1 工具清单，注册 8 个 MCP 工具：

- `save_session` → `fact::session_store::save()`
- `search_sessions` → Phase 2 实现 (先返回 placeholder)
- `memory_write` → `growth::bounded_memory::write()`
- `memory_update` → `growth::bounded_memory::update()`
- `memory_remove` → `growth::bounded_memory::remove()`
- `memory_read` → `growth::bounded_memory::read()`
- `user_profile` → `growth::bounded_memory` 的 `target="user"` 变体
- `rebuild_index` → Phase 2 实现

**Step 3: 实现 main.rs 入口**

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 解析 CLI 参数 (clap)
    // 2. 加载配置
    // 3. 初始化 DB
    // 4. 启动 MCP stdio server
}
```

**Step 4: 手动测试 MCP 通信**

```bash
# 编译
cargo build

# 通过 stdin 发送 initialize 请求
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}' | cargo run
```

**Step 5: Commit**

```bash
git commit -am "feat: MCP server framework with tool registration"
```

---

### Task 6: 嵌入引擎集成

**Files:**

- Create: `src/embedder/mod.rs`
- Create: `src/embedder/onnx.rs`
- Create: `src/embedder/tokenizer.rs`
- Create: `src/embedder/download.rs`
- Modify: `Cargo.toml` (添加 ort, sqlite-vec 依赖)

**Step 1: 添加依赖到 Cargo.toml**

```toml
ort = "2"
tokenizers = "0.20"
reqwest = { version = "0.12", features = ["blocking"] }
indicatif = "0.17"
```

> 注: sqlite-vec 的 Rust 绑定需确认版本，可参照 RustRAG 的 Cargo.toml

**Step 2: 实现模型智能发现 (download.rs)**

```rust
const MODEL_FILES: &[(&str, &str)] = &[
    ("model_O4.onnx", "onnx/model_O4.onnx"),  // 使用 O4 优化版
    ("tokenizer.json", "tokenizer.json"),
    ("config.json", "config.json"),
    ("special_tokens_map.json", "special_tokens_map.json"),
    ("tokenizer_config.json", "tokenizer_config.json"),
];

pub fn discover_model_dir(config: &Config) -> PathBuf {
    // 1. config 手动指定 → 直接使用
    // 2. RustRAG 目录 → 复用
    // 3. ~/.asuna/models/ → 自己的缓存
    // 4. 都没有 → 下载
}
```

**Step 3: 实现 onnx.rs + tokenizer.rs**

- 复用 RustRAG 的 ONNX 推理逻辑（同样的 mean pooling + L2 normalize）
- `OnnxEmbedder::embed(text) -> Vec<f32>`
- `OnnxEmbedder::embed_batch(texts) -> Vec<Vec<f32>>`
- `serialize_vector_int8(vec) -> Vec<u8>`

**Step 4: Lazy Loading 包装**

```rust
pub struct LazyEmbedder {
    inner: OnceCell<OnnxEmbedder>,
    config: EmbeddingConfig,
}
// 首次调用 embed() 时才实际加载模型
```

**Step 5: 测试 (需要模型文件)**

```rust
#[test]
#[ignore] // 需要模型文件
fn test_embed_single() { /* ... */ }

#[test]
fn test_model_discovery_rustrag_path() {
    // mock RustRAG 路径存在
}
```

**Step 6: Commit**

```bash
git commit -am "feat: ONNX embedding engine with model discovery"
```

---

## Phase 2：检索能力

### Task 7: 向量检索 (sqlite-vec)

**Files:**

- Create: `src/index/vector.rs`
- Modify: `src/index/schema.rs` (添加 vec_chunks 虚拟表)
- Modify: `src/fact/session_store.rs` (保存时写入向量)

**Step 1: 添加向量表到 schema**

```sql
CREATE VIRTUAL TABLE vec_turns USING vec0(
    embedding INT8[384]
);
```

**Step 2: 实现 vector.rs**

- `insert_vector(rowid, embedding)` — 向量写入
- `search_vectors(query_vec, top_k) -> Vec<(i64, f32)>` — 余弦相似度搜索

**Step 3: 修改 session_store 双写流程**

- `save()` 时：JSONL → SQLite sessions/turns → 生成 embedding → vec_turns

**Step 4: 测试**

```rust
#[test]
fn test_vector_insert_and_search() { /* ... */ }
```

**Step 5: Commit**

```bash
git commit -am "feat: vector search with sqlite-vec"
```

---

### Task 8: 全文检索与 hybrid search

**Files:**

- Create: `src/index/fts.rs`
- Create: `src/fact/search.rs`
- Modify: `src/mcp/tools.rs` (实现 search_sessions)

**Step 1: 实现 fts.rs**

- FTS5 同步触发器 (turns 插入时同步到 turns_fts)
- `fts_search(query, top_k) -> Vec<(i64, f64)>`

**Step 2: 实现 search.rs hybrid 搜索**

```rust
pub fn search_sessions(params: SearchParams) -> Vec<SearchResult> {
    // 1. 时间范围预过滤
    // 2. 根据 search_mode:
    //    - semantic: 向量搜索
    //    - keyword: FTS5 搜索
    //    - hybrid: RRF 融合排序
    // 3. role 过滤
    // 4. 返回 top_k 结果（含 session 上下文）
}
```

**Step 3: 连接 MCP 工具 search_sessions**

**Step 4: 测试**

```rust
#[test]
fn test_hybrid_search() { /* ... */ }
#[test]
fn test_time_range_filter() { /* ... */ }
```

**Step 5: Commit**

```bash
git commit -am "feat: hybrid search - semantic + keyword + time"
```

---

### Task 9: 索引重建

**Files:**

- Create: `src/index/rebuild.rs`
- Modify: `src/mcp/tools.rs` (实现 rebuild_index)

**Step 1: 实现 rebuild.rs**

```rust
pub fn rebuild_from_jsonl(data_dir: &Path, db: &Db, embedder: &LazyEmbedder) -> Result<Stats> {
    // 1. 清空 sessions, turns, vec_turns, turns_fts
    // 2. 遍历 data_dir 下所有 .jsonl 文件
    // 3. 逐文件解析 + 重建索引
    // 4. 返回统计信息
}
```

**Step 2: 启动时一致性检查**

```rust
pub fn check_consistency(data_dir: &Path, db: &Db) -> ConsistencyResult {
    // 对比 JSONL 文件数 vs sessions 表行数
}
```

**Step 3: Commit**

```bash
git commit -am "feat: index rebuild from JSONL"
```

---

## Phase 3：安全与增强

### Task 10: 成长层 ↔ 事实层溯源

**Files:**

- Modify: `src/growth/bounded_memory.rs`
- Modify: `src/mcp/tools.rs`

- memory_write 自动关联 source_session
- 审计日志完善
- 事实层溯源验证辅助函数

---

### Task 11: 多 Profile 支持

**Files:**

- Modify: `src/config.rs`
- Modify: `src/fact/session_store.rs`
- Modify: `src/growth/bounded_memory.rs`

- 目录结构改为 `~/.asuna/profiles/{profile_id}/`
- MCP 工具参数添加可选 `profile` 字段

---

### Task 12: CLI 子命令

**Files:**

- Modify: `src/main.rs`
- Create: `src/cli.rs`

- `asuna serve` — 启动 MCP stdio server
- `asuna list-sessions` — 列出会话
- `asuna search <query>` — CLI 搜索
- `asuna rebuild` — 重建索引
- `asuna import <file>` — 导入对话
- `asuna export <session_id>` — 导出对话

---

### Task 13: 与 OpenClaw Asuna 对接测试

- 在 3865 服务器编译部署
- 配置 OpenClaw 添加 Asuna Memory MCP server
- 注入 system prompt 记忆指令模板
- 端到端测试：对话 → 保存 → 检索 → 记忆沉淀

---

## 实施优先级总结

| Phase | Task | 描述             | 依赖  | 预估  |
| ----- | ---- | ---------------- | ----- | ----- |
| 1     | T1   | Cargo 项目初始化 | 无    | 30min |
| 1     | T2   | SQLite 数据库层  | T1    | 45min |
| 1     | T3   | JSONL 对话存储   | T1,T2 | 1h    |
| 1     | T4   | 成长记忆层       | T1,T2 | 1h    |
| 1     | T5   | MCP Server 框架  | T1-T4 | 1.5h  |
| 1     | T6   | 嵌入引擎集成     | T1    | 1.5h  |
| 2     | T7   | 向量检索         | T2,T6 | 1h    |
| 2     | T8   | hybrid search    | T7    | 1.5h  |
| 2     | T9   | 索引重建         | T2,T3 | 45min |
| 3     | T10  | 溯源验证         | T3,T4 | 30min |
| 3     | T11  | 多 Profile       | T3,T4 | 45min |
| 3     | T12  | CLI 子命令       | T5    | 1h    |
| 3     | T13  | 对接测试         | All   | 2h    |
