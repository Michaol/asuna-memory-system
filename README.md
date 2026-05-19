# Asuna Memory System

> AI Agent 长期记忆系统 — MCP Server

[English](README_EN.md) | [AI Agent 安装指南](for_ai.md)

## 升级指南

### 从 v1.2.1 升级到 v1.3.0

v1.3.0 在事实层和成长层之外新增**图谱记忆层**（第三层）。事实层和成长层一字不动；旧数据完全兼容。

升级步骤：

1. 替换二进制文件
2. 首次启动时 `init_schema` 自动建 `entities` + `relations` 两张表
3. 运行 `asuna-memory doctor`，预期看到：
   - `图谱: ENABLED (0 entities, 0 relations)`

无需 `rebuild`：图谱由 agent 累积，重建对图谱无意义。

**v1.3.0 变更摘要：**

🟢 **新增 · 图谱记忆层**

- 同 SQLite 数据库内新增 `entities` 和 `relations` 两张表，零新依赖
- 4 个 MCP 工具：`graph_assert` / `graph_neighbors` / `graph_path` / `graph_link_entity` / `graph_prune_dangling`
- canonical 归一化（lowercase + trim + 折空白），不做 fuzzy / 语义合并
- `save_session` 返回 `graph_pending` 软提示，列出尚未被图谱引用的 turn_id
- `doctor --verbose` 显示图谱覆盖率和悬空引用诊断
- 不调 LLM：图谱内容由 agent 自主断言，可选规则抽取也未引入

🟡 **API 表面**

- `Config.graph.enabled` / `Config.graph.remind_on_save`（serde default，老配置零迁移）
- `graph.enabled = false` 时所有图谱 MCP 工具返回 `"graph disabled in config"`
- 二进制体积不变（不引入新 crate）

### 从 v1.2.0 升级到 v1.2.1（强烈推荐）

v1.2.1 是一个**安全与质量加固**版本，修复了 1 个 Critical 级别的**路径穿越漏洞**和多个数据正确性问题。所有用户应当尽快升级。

```bash
# 1. 替换二进制文件

# 2. 重建索引（让向量召回质量充分受益于 query/document 前缀分离）
asuna-memory rebuild

# 3. 验证
asuna-memory doctor
# 预期新增字段：
#   版本: v1.2.1
#   外键约束: ON
```

**v1.2.1 变更摘要：**

**🔴 Critical 修复：**

- **路径穿越漏洞**：`memory_write` / `memory_update` / `memory_remove` 的 `target` 参数不再被信任直接拼路径，改为白名单 (`memory` / `user`) 严格校验，杜绝 `../../foo` 之类穿越攻击。

**🟠 Important 修复：**

- **EmbeddingGemma 前缀分离**：保存对话和重建索引时使用 `title: none | text:` (Document) 前缀；搜索查询使用 `task: search result | query:` (Query) 前缀。两者不再共用 query 前缀，召回质量显著提升（**升级后强烈建议 `rebuild`**）。
- **JSONL/SQLite 原子化**：`save_session` 改为 _DB 事务 → commit → 写 JSONL_ 顺序，且事务任意失败自动 `ROLLBACK`。彻底消除了"JSONL 已落盘但 DB 半写"的残骸状态。
- **LIKE 通配符注入**：`memory_update` / `memory_remove` 的 SQLite LIKE 子句改为 `ESCAPE '\\'` 模式，并对 `% _ \` 转义；同时改为**条目级（§ 分隔）**匹配，避免子串误改无关条目。
- **§ 分隔符鲁棒性**：连续删除多条相邻条目不再残留 `§§§`；删除最后一条只留 metadata header；删除首条不留前缀 `\n§\n`。
- **中文长内容 panic**：审计日志的内容截取从字节切片改为 `chars().take(N)`，多字节字符不再触发 panic。
- **外键约束**：默认开启 `PRAGMA foreign_keys = ON`，防止 `turns` 引用悬空 `session_id`。
- **save_session 严格校验**：`timestamp` / `role` / `content` 任一缺失立即报错；`role` 必须在 `user` / `assistant` / `tool_call` / `system` 内，不再静默吞错为 `user`。

**🟡 Minor 改进：**

- **ONNX 动态 padding**：tokenizer 不再恒填 2048，按 batch 内最长长度动态 pad，对 preview 短文本提速 5–20×。
- **凭据正则缓存**：安全扫描的 5 条凭据正则编译一次复用，扫描热路径不再每次重新编译。
- **模型下载完整性**：流式写到 `.partial` 临时文件，校验 `Content-Length` 后原子 rename，避免中断后残留半文件被误判为完成。
- **doctor 增强**：新增版本号 / 外键状态 / 嵌入向量维度展示。
- **配置字段接入**：`conversation.preview_length` / `search.default_top_k` / `search.search_mode` / `memory.security_scan` 现在真正生效。
- **e2e 测试入库**：6 个端到端测试从孤儿文件接入测试套件（覆盖 save→搜索、覆盖写入、删除残留、rebuild 一致性）。
- **死列清理**：`turns.embedding BLOB` 从 schema 移除（向量始终存在 `vec_turns` 虚表）。
- **未使用依赖**：移除 `indicatif`，新增 `once_cell` / `tempfile (dev)`。

> 注意：旧版数据库中的 `turns.embedding` 列会保留（SQLite IF NOT EXISTS 语义），不会回写也不会迁移，无害。

<details>
<summary><strong>历史版本变更日志（点击展开）</strong></summary>

### 从 v1.1.4 升级到 v1.2.0

v1.2.0 是一个**可靠性与安全性加固**版本，修复了 2 个 Critical 级别的数据一致性问题和 8 个 Important 级别的功能缺陷。

```bash
# 1. 替换二进制文件

# 2. 重建索引以应用 char_count 修正（字节数 → 字符数）
asuna-memory rebuild

# 3. 验证
asuna-memory doctor
```

**v1.2.0 变更摘要：**

**🔴 Critical 修复：**

- **事务安全**：`save_session` 和 `rebuild` 的所有数据库写操作现在包裹在 `BEGIN IMMEDIATE ... COMMIT` 事务中，进程崩溃时不再导致数据库处于半写入不一致状态

**🟡 Important 修复：**

- **模型流式下载**：大模型文件不再完整加载到内存，改用流式 `io::copy` 写入磁盘，避免内存受限环境下 OOM
- **char_count 修正**：`turns.char_count` 字段从 UTF-8 字节数修正为 Unicode 字符数，中文内容不再虚高 3 倍
- **unsafe FFI 文档化**：sqlite-vec 扩展注册的 unsafe `transmute` 添加了完整的 SAFETY 注释和 ABI 兼容性说明
- **成长层 update() 修复**：仅在 body 上做替换，不再意外修改 metadata header；替换后自动更新时间戳；同步更新 SQLite `bounded_memory` 表
- **成长层 remove() 修复**：删除操作现在同步清理 SQLite `bounded_memory` 表
- **查询优化**：`list_entries()` 合并了对同一 session_id 的重复查询
- **MCP 错误处理文档化**：tools/call 的 `content + isError` 错误格式添加了 MCP 协议规范引用

**🟢 Minor 改进：**

- **空 turns 校验**：`save_session` 在解析前验证 turns 非空
- **时间戳安全**：`unix_ms_to_iso()` 对无效时间戳使用 epoch fallback
- **废弃 db_path 字段**：`config.json` 中的 `db_path` 字段标记为废弃，保持向后兼容

### 从 v1.1.3 升级到 v1.1.4

v1.1.4 修复了在某些情况下 `rebuild` 命令后向量索引回零的回归问题，并优化了重建性能。

```bash
# 1. 替换二进制文件

# 2. 重新执行重建以恢复可能回零的向量索引
asuna-memory rebuild
```

**v1.1.4 变更摘要：**

- **向量索引回归修复**：解决了 SQLite `vec0` 虚拟表在 `rebuild` 过程中由于读写游标并发冲突导致的静默写入失败。
- **重建性能优化**：合并了 FTS 和向量索引重建的查询路径，减少 50% 的数据库 IO，提升了大数据量下的重建速度。
- **错误诊断增强**：将原有的静默错误忽略改为 `warn!` 日志输出，提升了系统的可观测性。

### 从任意旧版本升级到 v1.1.3

v1.1.3 解决了遗留数据库由于早期 FTS 虚拟表结构而导致的 `Content in the virtual table is corrupt` 运行时损坏问题。

```bash
# 1. 替换二进制文件

# 2. 正常运行即可。如果有必要也可以运行校验：
asuna-memory doctor
```

**v1.1.3 变更摘要：**

- **自动 Schema 迁移**：对于基于旧版 (external-content) 建立的 sqlite 数据库，自动实施了至新型 contentless 结构的转换。
- **Trigger 更新保障**：针对老版本包含错误定义的同步触发器，新增 Drop And Re-create 检查逻辑，杜绝新代码跑出旧版行为，一劳永逸解决了在导入或者检索中随机抛出的虚表损坏 Panic。

### 从 v1.1.x 升级到 v1.1.2

v1.1.2 解决了在某些环境下 CLI 模式下中文搜索失效的问题。强烈建议所有用户升级。

```bash
# 1. 替换二进制文件

# 2. 强制重建索引（以应用增强的分词保障）
asuna-memory rebuild
```

**v1.1.2 变更摘要：**

- **FTS 稳定性增强**：将 Rebuild 阶段的分词逻辑从 SQL 层移回 Rust 层，确保在所有系统环境下分词 Token 的一致性。
- **搜索诊断输出**：CLI `search` 现在会显示分词后的结果，方便调试。

### 从 v1.0.x 升级到 v1.1.0

v1.1.0 修复了向量数据库未写入的问题。升级后需要重建索引以补全向量数据：

```bash
# 1. 替换二进制文件

# 2. 重建索引（会同时重建 FTS 和向量索引）
asuna-memory rebuild

# 3. 验证
asuna-memory doctor
# 预期输出包含：
#   索引统计: 10 会话, 24 轮对话, 24 个向量
```

**v1.1.0 变更摘要：**

- `rebuild` 现在会为每条 turn 生成 int8 向量并写入 `vec_turns` 表
- `save_session` / `import` 在嵌入模型可用时自动生成向量
- `doctor` 现在显示向量索引数量
- 所有写入路径（save / import / rebuild / MCP）共享统一的嵌入管道

</details>

---

## 安装方式

### 方式一：从 GitHub Release 下载（推荐）

前往 [Releases](https://github.com/Michaol/asuna-memory-system/releases) 下载对应平台的预编译包（含 ONNX Runtime 动态库）：

| 平台                | 文件                                      |
| ------------------- | ----------------------------------------- |
| Windows x86_64      | `asuna-memory-windows-x64.exe.zip`        |
| Linux x86_64        | `asuna-memory-linux-x64.tar.gz`           |
| Linux ARM64         | `asuna-memory-linux-arm64.tar.gz`         |
| macOS Apple Silicon | `asuna-memory-macos-apple-silicon.tar.gz` |

下载解压后放到 PATH 中：

```bash
# Linux x64
tar xzf asuna-memory-linux-x64.tar.gz
sudo mv asuna-memory /usr/local/bin/

# macOS
tar xzf asuna-memory-macos-apple-silicon.tar.gz
sudo mv asuna-memory /usr/local/bin/

# Windows: 解压 zip，将 asuna-memory.exe 放到 PATH 中
```

### 方式二：从源码构建

前置要求：**Rust 1.75+**（推荐 `rustup` 安装），无需额外数据库（SQLite 已内嵌）。

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system
cargo build --release
```

编译产物位于 `target/release/asuna-memory`（Windows 为 `.exe`）。

### 快速验证

```bash
# 检查环境
asuna-memory doctor

# 启动 MCP 服务器
asuna-memory serve
```

### 配置 MCP 客户端

在你的 MCP 客户端配置中添加：

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

> **AI Agent 注意**：如果你是 AI Agent，需要自动安装此系统，请阅读 [for_ai.md](for_ai.md) 获取完整的安装、调用和工具参数说明。

### 安装注意事项

1. **ONNX Runtime（可选）**：语义搜索需要 ONNX Runtime 动态库（`onnxruntime.dll` / `libonnxruntime.so`）。若不需要语义搜索，系统会自动降级为纯关键词搜索，不影响核心功能。
2. **模型文件（可选）**：语义搜索需要 `embeddinggemma-300m-q8` 模型。系统会按以下优先级搜索：
   - `~/.asuna/models/embeddinggemma-300m-q8`
   - Windows 下支持 `ASUNA_DEV_ROOT` 环境变量指定开发路径
   - 兼容 EmbeddingGemma 的 tokenizer 格式，不需要 `token_type_ids`
   - 未找到时自动降级为关键词搜索

3. **数据目录**：默认为 `~/.asuna/`。首次运行会自动创建。
4. **Profile 隔离**：每个 profile 的数据独立存储在 `~/.asuna/profiles/{profile_id}/` 下。

---

## 系统架构

**协议**：MCP stdio · JSON-RPC 2.0  
**嵌入**：embeddinggemma-300m (ONNX) · 768d INT8 量化

**双层记忆架构**：

| 成长层（Growth Layer）              | 事实层（Fact Layer）                           |
| ----------------------------------- | ---------------------------------------------- |
| `MEMORY.md` · AI 知识 · 2200 字符上限 | JSONL 不可变归档 · `conversations/YYYY/MM/DD/` |
| `USER.md` · 用户画像 · 1375 字符上限  | SQLite · `sessions` / `turns` 元数据            |
| 安全扫描（注入 / 凭据 / 不可见 Unicode） | FTS5 全文索引 · 中文 unigram                   |
| 溯源追踪 · 条目 → 源会话              | sqlite-vec · 768d INT8 量化向量                 |

**图谱层 (v1.3+)**：SQLite 表 `entities` + `relations`，由 agent 通过 `graph_assert` 累积；canonical 归一化（lowercase + trim + 折空白）；不调 LLM 也不做规则抽取。

### 事实层（Fact Layer）

- **对话存储**：每次对话以 JSONL 格式归档到 `conversations/YYYY/MM/DD/` 目录
- **索引**：SQLite 存储会话元数据和对话轮次摘要
- **全文检索**：FTS5 contentless 虚拟表，支持中文 unigram 分词（v1.1.3+ 完善的 schema 自动迁移）
- **向量检索**：sqlite-vec 扩展，768 维 INT8 量化向量，save/import/rebuild 均自动写入
- **混合搜索**：Reciprocal Rank Fusion (RRF) 融合语义 + 关键词结果

### 成长层（Growth Layer）

- **有界记忆**：`MEMORY.md`（AI 知识记忆，2200 字符上限）和 `USER.md`（用户画像，1375 字符上限）
- **条目分隔**：使用 `§` 分隔符区分不同条目
- **安全扫描**：每次写入/更新前自动检测 prompt injection、凭据泄露、不可见 Unicode
- **溯源追踪**：每条记忆可追溯到原始对话 session

---

## MCP 工具列表

| 工具名              | 说明                                   |
| ------------------- | -------------------------------------- |
| `save_session`      | 保存完整对话到事实层（含自动生成向量） |
| `search_sessions`   | 多维度检索历史对话                     |
| `memory_write`      | 向成长记忆写入新条目                   |
| `memory_update`     | 通过子串匹配更新记忆条目               |
| `memory_remove`     | 删除记忆条目                           |
| `memory_read`       | 读取当前成长记忆全文                   |
| `user_profile`      | 读写用户画像                           |
| `rebuild_index`     | 从 JSONL 文件重建索引（FTS + 向量）    |
| `memory_provenance` | 验证成长记忆的溯源信息                 |

详细参数说明见 [for_ai.md](for_ai.md)。

---

## 图谱记忆 (v1.3+)

图谱层是事实层和成长层之外的第三层，复用同一个 SQLite 数据库新增两张表（`entities` + `relations`）。agent 通过 `graph_assert` 累积三元组，server 不调 LLM 也不做规则抽取；`canonical` 归一化处理大小写漂移，但**不做语义合并**（"Alice" 和 "Alice Smith" 是两个节点，除非显式 `graph_link_entity`）。

### 新增 MCP 工具

| 工具 | 用途 |
|---|---|
| `graph_assert` | 写实体-关系三元组（含 confidence、source_turn） |
| `graph_neighbors` | 查 N-hop 邻居（支持 rel_type / direction / hops 过滤） |
| `graph_path` | 两节点最短路径长度（v1.3.0 仅返回 length，路径节点序列化留作 v1.3.1） |
| `graph_link_entity` | 别名合并：把 `from` 实体的边重定向到 `to`，然后删除 `from`（不可逆） |
| `graph_prune_dangling` | 清理悬空 `source_turn` 引用：把指向已删除 turn 的字段置 NULL（不删 relation 本身） |

### 软提示

`save_session` 在 `graph.enabled && graph.remind_on_save` 时返回 `graph_pending: { turn_ids, hint }`，列出本次 session 中**尚未被任何 relation 引用**的 turn_id。关闭：在 config.json 设 `graph.remind_on_save = false`。

### 整体禁用

`graph.enabled = false` 时所有 `graph_*` 工具返回 `"graph disabled in config"`，事实/成长层完全不受影响。

### 诊断

`asuna-memory doctor` 默认显示 `图谱: ENABLED (N entities, M relations)`。
加 `--verbose` 还显示图谱覆盖率（被引用的 turn 占比）和悬空引用（source_turn 指向已删除 turn）。

---

## JSONL 文件格式

`import` 命令和 `rebuild` 命令读取的 JSONL 文件由 **1 行 Header + N 行 Turn** 组成，每行一个合法 JSON 对象。

### Header（第一行）

| 字段          | 类型     | 必填 | 说明                                              |
| ------------- | -------- | ---- | ------------------------------------------------- |
| `v`           | integer  | 是   | 格式版本，当前固定为 `1`                          |
| `type`        | string   | 是   | 固定为 `"session_header"`                         |
| `session_id`  | string   | 是   | 会话唯一标识（UUID 或自定义字符串）               |
| `start_time`  | string   | 是   | ISO 8601 时间戳（如 `2026-04-10T10:02:00+08:00`） |
| `profile_id`  | string   | 是   | Profile 标识（通常为 `"default"`）                |
| `source`      | string   | 否   | 来源标识（如 `"openclaw"`、`"chatgpt"`）          |
| `agent_model` | string   | 否   | 使用的 Agent 模型名称                             |
| `title`       | string   | 否   | 会话标题                                          |
| `tags`        | string[] | 否   | 标签列表                                          |

### Turn（第二行起，每行一个对话轮次）

| 字段       | 类型    | 必填 | 说明                                                                       |
| ---------- | ------- | ---- | -------------------------------------------------------------------------- |
| `ts`       | string  | 是   | ISO 8601 时间戳                                                            |
| `seq`      | integer | 是   | 轮次序号，从 `1` 开始递增                                                  |
| `role`     | string  | 是   | 角色：`"user"` / `"assistant"` / `"tool_call"` / `"system"`                |
| `content`  | string  | 是   | 对话内容                                                                   |
| _其他字段_ | any     | 否   | 通过 `#[serde(flatten)]` 扁平化存储（如 `model`、`usage`、`tool_name` 等） |

### 完整示例

````jsonl
{"v":1,"type":"session_header","session_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","start_time":"2026-04-10T10:02:00+08:00","profile_id":"default","source":"manual","title":"示例对话","tags":["demo"]}
{"ts":"2026-04-10T10:02:00+08:00","seq":1,"role":"user","content":"你好，帮我写一个 Rust 的 Hello World"}
{"ts":"2026-04-10T10:02:05+08:00","seq":2,"role":"assistant","content":"好的！这是一个最简的 Rust Hello World：\n\n```rust\nfn main() {\n    println!(\"Hello, World!\");\n}\n```","model":"gpt-4","usage":{"input_tokens":15,"output_tokens":42}}
````

> **注意**：`import` 命令使用 JSONL 格式（`ts` / `seq` 字段），而 `save_session` MCP 工具使用 `timestamp` 字段、由系统自动分配 `seq`。两者最终存储格式一致，但输入接口不同。

### 导入方式

```bash
# CLI 导入
asuna-memory import my_session.jsonl

# 批量导入
for f in sessions/*.jsonl; do
  asuna-memory import "$f"
done
```

---

## 保存时机与触发机制

### 何时调用 `save_session`

| 场景         | 建议时机       | 说明                                                 |
| ------------ | -------------- | ---------------------------------------------------- |
| Agent 对话   | 每轮对话结束时 | 确保对话被归档，支持后续检索                         |
| 批量迁移     | 一次性导入     | 使用 `import` 命令批量导入 JSONL 文件                |
| 定时归档     | 周期性触发     | 适合高频对话场景（如客服机器人），按时间窗口批量保存 |
| 用户主动保存 | 用户请求时     | 重要对话由用户手动触发保存                           |

### 推荐模式：每轮对话结束时保存

```text
用户消息 → Agent 处理 → Agent 回复
                          ↓
                    save_session(本轮完整对话)
```

- `session_id` 保持一致，系统会执行 `INSERT OR REPLACE` 语义
- 多次保存同一 `session_id` 会覆盖更新

### CLI 命令

```bash
# 启动 MCP 服务器（默认命令）
asuna-memory serve

# 环境检查
asuna-memory doctor

# 列出所有 profile
asuna-memory list-profiles

# 列出最近会话
asuna-memory list-sessions --last-days 7 --limit 20

# 搜索对话
asuna-memory search "Rust async" --mode keyword --top-k 5

# 从 JSONL 重建索引（FTS + 向量）
asuna-memory rebuild

# 导入 JSONL 文件
asuna-memory import session.jsonl

# 导出会话摘要
asuna-memory export <session_id>
```

### 全局参数

| 参数        | 默认值                 | 说明         |
| ----------- | ---------------------- | ------------ |
| `--config`  | `~/.asuna/config.json` | 配置文件路径 |
| `--profile` | `default`              | 指定 profile |

---

## 配置文件

配置文件为 JSON 格式，默认路径 `~/.asuna/config.json`。不存在时使用内置默认值。

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
  "model_path": null
}
```

---

## 数据目录结构

```text
~/.asuna/
├── config.json              # 配置文件
├── profiles/
│   └── default/
│       ├── memory.db         # SQLite 索引数据库（含 vec_turns 向量表）
│       ├── conversations/    # JSONL 对话归档
│       │   └── 2026/
│       │       └── 04/
│       │           └── 10/
│       │               └── 20260410T100200_abc12345.jsonl
│       └── memory/           # 成长记忆
│           ├── MEMORY.md
│           └── USER.md
└── models/                   # 嵌入模型（可选）
    └── embeddinggemma-300m-q8/
```

---

## 安全机制

成长层写入前自动执行安全扫描：

- **Prompt Injection 检测**：中英文注入模式匹配（如 "ignore previous instructions"、"忽略之前的指令"）
- **凭据泄露检测**：OpenAI `sk-*`、GitHub `ghp_*`、AWS `AKIA*`、PEM 私钥格式
- **不可见 Unicode 检测**：零宽字符、BOM 等

扫描失败时写入操作会被拒绝并返回具体原因。

---

## License

MIT
