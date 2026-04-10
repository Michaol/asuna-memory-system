# Asuna Memory System — Code Review Report

> **审查范围**: 全部 27 个 Rust 源文件 (~3500 行)
> **初审日期**: 2026-04-10 | **复审日期**: 2026-04-10
> **审查粒度**: 全项目

## 问题状态总览

| ID    | 级别         | 状态      | 简述                                                       |
| ----- | ------------ | --------- | ---------------------------------------------------------- |
| C1    | 🔴 Critical  | ✅ 已修复 | 向量量化算法不兼容 → 改用 `quantize_to_int8` (clamp)       |
| C2    | 🔴 Critical  | ✅ 已修复 | vec_turns float/INT8 矛盾 → 统一为 `int8[384]` + raw bytes |
| I1    | 🟡 Important | ✅ 已修复 | 伪正则安全扫描 → 删除伪模块，添加 `regex-lite` crate      |
| I2    | 🟡 Important | ✅ 已修复 | `list_entries` 溯源死代码 → 删除错误计算，`session_exists: false` |
| I3    | 🟡 Important | ✅ 已修复 | tokenizer `.unwrap()` → 改为 `?` 传播                     |
| I4    | 🟡 Important | ✅ 已修复 | 硬编码 Windows 路径 → `fn model_search_paths()` + `#[cfg(windows)]` |
| I5    | 🟡 Important | ✅ 已修复 | 容量检查 `len()` → `chars().count()`                      |
| I6    | 🟡 Important | ✅ 已修复 | MCP 通知返回 `null` → `Option<Value>` 跳过输出            |
| M1    | 🟢 Minor     | ✅ 已修复 | 硬编码 +08:00 → `chrono::Local` 系统时区                   |
| M2    | 🟢 Minor     | ✅ 已修复 | `expand_path` 重复 → 合并为 `config::expand_tilde`         |
| M3    | 🟢 Minor     | ✅ 已修复 | `ort::init()` 无保护 → `ORT_INIT: Once` 包裹              |
| M4    | 🟢 Minor     | ✅ 已修复 | `update()` 容量含元数据头 → `extract_body().chars().count()` |
| M5    | 🟢 Minor     | ✅ 已修复 | `memory_write` session_id 丢失 → 传递 MCP 参数            |
| M6    | 🟢 Minor     | ✅ 已修复 | rebuild 不清 FTS → 添加 `DELETE FROM turns_fts`            |
| M7    | 🟢 Minor     | ✅ 已修复 | `title` 字段丢失 → `SessionHeader` 增加 title + 全链路传递 |

---

## 优点

1. **模块划分清晰**: 7 个顶级模块（fact/growth/index/embedder/mcp/util/config）职责分明，与架构文档一一对应
2. **测试覆盖良好**: 几乎每个模块都有内联 `#[cfg(test)]`，覆盖核心路径（dual-write、FTS、rebuild、bounded_memory 容量/去重/update/remove）
3. **MCP 协议实现干净**: JSON-RPC 2.0 分层合理（protocol → server → tools），9 个工具注册完整
4. **JSONL 四级时间索引**: 目录结构（年/月/日）+ 文件名 + 行级 ts + SQLite B-tree，设计精巧
5. **FTS5 触发器同步**: 使用 `AFTER INSERT/DELETE/UPDATE` 触发器自动同步 `turns_fts`，免维护
6. **模型智能发现**: 按优先级搜索 RustRAG → Asuna 缓存 → 下载，资源复用无冗余
7. **Lazy Loading**: `LazyEmbedder` 用 `Mutex<Option<>>` 实现首次用时加载，避免启动延迟

---

## 问题

### 🔴 Critical（必须修复）

#### ~~C1: `serialize_vector_int8` 量化算法与 RustRAG 不兼容~~ ✅ 已修复

- **位置**: [onnx.rs:129-144](file:///e:/DEV/Asuna_memory_system/src/embedder/onnx.rs#L129-L144)
- **问题**: 使用 min-max 归一化量化 `(v - min) / (max - min) * 255`，而 RustRAG 使用 `clamp(-1, 1) * 127` 的定点量化
- **影响**: 如果未来需要混合搜索两个系统的向量索引，量化格式不一致将导致相似度计算错误。即使不混合，当前 vec_turns 表声明为 `float[384]`（见 C2），`serialize_vector_int8` 实际上**没有被任何地方调用**
- **建议**: 统一使用 RustRAG 的 `clamp(-1,1)*127` 方案，或在确认独立使用后删除此死代码

```rust
// RustRAG 方案（推荐）
pub fn serialize_vector_int8(vec: &[f32]) -> Vec<u8> {
    vec.iter().map(|&v| {
        let q = (v.clamp(-1.0, 1.0) * 127.0).round() as i8;
        q as u8
    }).collect()
}
```

#### ~~C2: vec_turns 表类型不一致 — float vs INT8~~ ✅ 已修复

- **位置**: [db.rs:51](file:///e:/DEV/Asuna_memory_system/src/index/db.rs#L51) vs [schema.rs:81](file:///e:/DEV/Asuna_memory_system/src/index/schema.rs#L81) vs [vector.rs:16](file:///e:/DEV/Asuna_memory_system/src/index/vector.rs#L16)
- **问题**: 三处定义互相矛盾：
  - `db.rs:51` 实际创建: `embedding float[384]`
  - `schema.rs:81` 注释: `embedding INT8[384]`
  - `vector.rs:16` 插入: JSON 字符串格式 f32
- **影响**: 当前用 `float[384]` 存储 f32 向量，消耗是 INT8 的 4 倍内存。如果改为 INT8，`insert()` 必须传 INT8 blob 而非 JSON 字符串
- **建议**: 统一决定存储精度。如果选 float（当前状态），删除 `serialize_vector_int8`；如果选 INT8（更省空间），修改 `VectorStore::insert()` 使用 byte blob

#### C1+C2 修复指令（Claude Code 直接执行）

> 修复目标：统一向量量化为 INT8[384]（clamp 方案），消除 float/INT8 矛盾和死代码。
>
> 涉及 4 个文件：
>
> **1. `src/embedder/onnx.rs`**
>
> - 删除当前的 `serialize_vector_int8` 函数（L129-144）及其测试（L146-165）
> - 替换为 RustRAG 兼容的 clamp 量化函数：
>
> ```rust
> /// 将 L2 归一化的 f32 向量量化为 INT8 存储格式（与 RustRAG 兼容）
> pub fn quantize_to_int8(vec: &[f32]) -> Vec<u8> {
>     vec.iter().map(|&v| {
>         let q = (v.clamp(-1.0, 1.0) * 127.0).round() as i8;
>         q as u8
>     }).collect()
> }
> ```
>
> - 添加对应测试：`0.5 → 64`, `-1.0 → -127(129u8)`, `0.0 → 0`
>
> **2. `src/index/db.rs` (L51)**
>
> - 将 `embedding float[384]` 改为 `embedding int8[384]`
>
> ```diff
> -"CREATE VIRTUAL TABLE IF NOT EXISTS vec_turns USING vec0(embedding float[384]);"
> +"CREATE VIRTUAL TABLE IF NOT EXISTS vec_turns USING vec0(embedding int8[384]);"
> ```
>
> **3. `src/index/schema.rs` (L81)**
>
> - 更新注释，与 db.rs 保持一致：`embedding int8[384]`
>
> **4. `src/index/vector.rs`**
>
> - `insert()`: 不再用 JSON 字符串，改为传 `quantize_to_int8()` 后的 `Vec<u8>` 字节 blob
> - `search()`: query 向量同样用 `quantize_to_int8()` 转换后传入
>
> ```rust
> use crate::embedder::onnx::quantize_to_int8;
>
> pub fn insert(&self, turn_id: i64, embedding: &[f32]) -> anyhow::Result<()> {
>     let bytes = quantize_to_int8(embedding);
>     self.db.conn().execute(
>         "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, ?2)",
>         rusqlite::params![turn_id, bytes],
>     )?;
>     Ok(())
> }
>
> pub fn search(&self, query_vec: &[f32], top_k: usize) -> anyhow::Result<Vec<(i64, f32)>> {
>     let bytes = quantize_to_int8(query_vec);
>     // ... 用 bytes 替换原来的 emb_str
> }
> ```
>
> 修改后运行 `cargo test` 确保所有测试通过。

---

### 🟡 Important（应该修复）

#### ~~I1: `security.rs` 的 `regex_lite` 是伪正则引擎~~ ✅ 已修复

- **位置**: [security.rs:82-117](file:///e:/DEV/Asuna_memory_system/src/growth/security.rs#L82-L117)
- **问题**: 自实现的 `regex_lite::Regex::is_match()` 用文本 `contains()` 和字符类计数近似模拟正则匹配，会产生大量误报/漏报
  - `min_len` 计算 (`matches("[a-zA-Z0-9]").count() * 10`) 完全不靠谱
  - `AKIA[A-Z0-9]{16}` 模式无法正确匹配固定长度
- **影响**: 安全扫描名存实亡，可能放过真实凭据或误拒合法内容
- **建议**: 添加 `regex-lite` crate (零依赖正则库, ~30KB) 替换自实现

> **I1 修复指令（Claude Code 直接执行）**:
>
> 1. 在 `Cargo.toml` 的 `[dependencies]` 下添加 `regex-lite = "0.1"`
> 2. 在 `src/growth/security.rs` 中：
>    - 删除 `mod regex_lite { ... }` 整个模块 (L81-L117)
>    - 将 `regex_lite::Regex::new` 替换为 `regex_lite::Regex::new` (使用真实 crate)

#### ~~I2: `list_entries` 溯源验证有逻辑 bug~~ ✅ 已修复

- **位置**: [bounded_memory.rs:200](file:///e:/DEV/Asuna_memory_system/src/growth/bounded_memory.rs#L200)
- **问题**: `query_map` 回调中的 `session_exists` 计算使用了 `row.get::<_, i64>(0)` 取的是 `target` 列的值（字符串被当整数取，始终失败），结果永远是 `false`
- **影响**: 溯源验证的 `session_exists` 在 `query_map` 内永远为 false（好在后面 L224-237 又用单独查询修正了，但 L200 的代码是死代码/bug）
- **建议**: 删除 L199-204 的 `session_exists` 计算，在构造 `ProvenanceInfo` 时改为 `session_exists: false`，只依赖后续单独查询

> **I2 修复指令（Claude Code 直接执行）**:
>
> 1. 在 `src/growth/bounded_memory.rs` 的 `list_entries` 中：
>    - 删除 L199-L204 的 `let session_exists = if let ... else false;` 代码块
>    - 将 `Ok(ProvenanceInfo { ... session_exists, ... })` 改为 `session_exists: false,`

#### ~~I3: tokenizer `encode()` 内部 `.unwrap()` 可能 panic~~ ✅ 已修复

- **位置**: [tokenizer.rs:21](file:///e:/DEV/Asuna_memory_system/src/embedder/tokenizer.rs#L21)
- **问题**: 编码失败时直接 panic，而不是返回 `Result`
- **影响**: 恶意或超长输入可能导致 MCP server 崩溃
- **建议**: 改为 `-> anyhow::Result<(Vec<i64>, Vec<i64>)>`，调用方传播错误

#### ~~I4: 硬编码 Windows 路径~~ ✅ 已修复

- **位置**: [config.rs:7](file:///e:/DEV/Asuna_memory_system/src/config.rs#L7)
- **问题**: `MODEL_SEARCH_PATHS` 第一项硬编码 `"E:/DEV/RustRAG/models/..."` — 这只在你的 Windows 开发机有效，Linux 部署时失效
- **影响**: Linux 服务器上多搜一个永远不存在的路径（性能无大碍但不规范）
- **建议**: 用 `cfg!(target_os)` 条件编译或环境变量

> **I4 修复指令（Claude Code 直接执行）**:
> 在 `src/config.rs` 中，删除 `const MODEL_SEARCH_PATHS` 常量定义，修改 `discover_model_dir` 内部搜索逻辑，包含平台判断：
>
> ```rust
> let mut paths = vec![
>     expand_tilde(Path::new("~/.rustrag/models/multilingual-e5-small")),
>     expand_tilde(Path::new("~/.asuna/models/multilingual-e5-small")),
> ];
> #[cfg(windows)]
> paths.insert(0, PathBuf::from("E:/DEV/RustRAG/models/multilingual-e5-small"));
> ```

#### ~~I5: 容量检查用 `len()` 而非 Unicode 字符数~~ ✅ 已修复

- **位置**: [bounded_memory.rs:91](file:///e:/DEV/Asuna_memory_system/src/growth/bounded_memory.rs#L91)
- **问题**: `new_body.len()` 返回的是 **字节数** 而非字符数，但配置注释和 capacity 标注说的是 "chars"。中文字符 3 字节/字符，2200 字节只能存 ~733 个中文字符
- **影响**: 实际可用容量远小于预期
- **建议**: 使用 `new_body.chars().count()` 替换 `new_body.len()`

> **I5 修复指令（Claude Code 直接执行）**:
> 在 `src/growth/bounded_memory.rs` 中：
>
> 1. L91 左右的 `if new_body.len() > capacity` 改为 `if new_body.chars().count() > capacity`
> 2. 及其下方的错误提示 `new_body.len()` 改为 `new_body.chars().count()`
> 3. 同理，在 `update()` 函数中 (约 L140)，`if updated.len() > capacity` 必须改为正文长度检查，例如 `if extract_body(&updated).chars().count() > capacity`。

#### ~~I6: MCP server 对 `notifications/initialized` 返回 `Value::Null`~~ ✅ 已修复

- **位置**: [server.rs:73-75](file:///e:/DEV/Asuna_memory_system/src/mcp/server.rs#L73-L75)
- **问题**: 返回 `Value::Null` 后仍会被序列化为 `"null"` 写入 stdout。MCP 规范中通知不应产生任何响应
- **影响**: 某些 MCP 客户端可能将 `null` 解释为无效响应
- **建议**: 使用 `Option<Value>` 返回 `None`，跳过输出

> **I6 修复指令（Claude Code 直接执行）**:
> 在 `src/mcp/server.rs` 中：
>
> - 修改 `handle_line()` 签名为 `fn handle_line(&self, line: &str, handler: &ToolHandler) -> Option<Value>`
> - 现有的 `return Value::Null;` 改为 `return None;`
> - 外层封装正常情况为 `Some(...)`
> - 在 `run()` 中判断 `if let Some(response) = self.handle_line(...) { ... writeln!(stdout_lock) }`

---

### 🟢 Minor（建议改进）

#### ~~M1: `unix_ms_to_iso` 硬编码 +08:00 时区~~ ✅ 已修复

- **位置**: [time.rs:23](file:///e:/DEV/Asuna_memory_system/src/util/time.rs#L23)
- **建议**: 使用 `chrono::Local` 自动获取系统时区，或用 UTC

#### ~~M2: `expand_path` 重复实现~~ ✅ 已修复

- **位置**: [main.rs:324-334](file:///e:/DEV/Asuna_memory_system/src/main.rs#L324-L334) vs [config.rs:189-197](file:///e:/DEV/Asuna_memory_system/src/config.rs#L189-L197)
- **建议**: 合并到 `util/` 模块，消除重复

#### ~~M3: `ort::init()` 不应在构造函数中调用~~ ✅ 已修复

- **位置**: [onnx.rs:24-28](file:///e:/DEV/Asuna_memory_system/src/embedder/onnx.rs#L24-L28)
- **修复**: C1 修复时一并用 `ORT_INIT: Once` 包裹，确保只初始化一次

#### ~~M4: `update()` 容量检查对象错误~~ ✅ 已修复

- **位置**: [bounded_memory.rs:140](file:///e:/DEV/Asuna_memory_system/src/growth/bounded_memory.rs#L140)
- **问题**: `updated.len()` 包含了元数据头的长度，而 `capacity` 应该只约束正文。写入时有 `extract_body()` 提取正文，但更新时对全文做容量检查
- **建议**: 统一用 `extract_body(&updated).len()/chars().count()` 做容量检查

#### ~~M5: `save_session` 工具未传递 `session_id` 到成长层~~ ✅ 已修复

- **位置**: [tools.rs:298](file:///e:/DEV/Asuna_memory_system/src/mcp/tools.rs#L298)
- **问题**: `memory_write` 调用 `bm.write(..., None)` 永远传 `None` 作为 `session_id`，导致溯源信息丢失
- **建议**: 让 MCP 接口传递当前 session_id

#### ~~M6: `rebuild_from_jsonl` 不清理 FTS 索引~~ ✅ 已修复

- **位置**: [rebuild.rs:27-31](file:///e:/DEV/Asuna_memory_system/src/index/rebuild.rs#L27-L31)
- **问题**: 只 DELETE 了 turns/sessions/vec_turns，未清空 turns_fts。虽然 DELETE trigger 会逐行清理，但在大量数据时效率低
- **建议**: 在清空前先 `DELETE FROM turns_fts`

#### ~~M7: 缺少 `title` 字段写入~~ ✅ 已修复

- **位置**: [tools.rs:178-239](file:///e:/DEV/Asuna_memory_system/src/mcp/tools.rs#L178-L239)
- **问题**: `save_session` 工具定义了 `title` 参数，但 `SessionHeader` 没有 `title` 字段，Agent 传入的 title 被丢弃
- **建议**: 在 `SessionHeader` 或 `SessionStore::save()` 中处理 title

---

## 建议

1. **添加 `regex-lite` 依赖** 替换伪正则实现，这是安全扫描的基础
2. **统一向量存储策略**（float vs int8），消除 schema 定义和代码之间的不一致
3. **容量检查使用 `chars().count()`** 而非 `len()`，这对中文用户影响显著
4. **考虑用事务 (Transaction)** 包裹 `session_store::save()` 中的多条 INSERT，确保双写原子性
5. **MCP 通知处理** 不应产生输出，修正 `notifications/initialized` 分支

---

## 裁决

**可以合并？** ✅ 可以

**当前状态（复审后）:** 全部 15 个问题已修复，37 个测试全部通过。2 个 Critical（向量量化统一为 INT8 clamp 方案）、6 个 Important（伪正则→regex-lite、溯源 dead code、tokenizer panic、跨平台路径、chars 容量检查、MCP 通知静默）、7 个 Minor 全部完成。
