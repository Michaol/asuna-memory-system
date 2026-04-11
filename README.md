# Asuna Memory System

> AI Agent 长期记忆系统 — MCP Server

[English](README_EN.md) | [AI Agent 安装指南](for_ai.md)

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
2. **模型文件（可选）**：语义搜索需要 `multilingual-e5-small` 模型。系统会按以下优先级搜索：

- `~/.rustrag/models/multilingual-e5-small`
  - `~/.asuna/models/multilingual-e5-small`
  - Windows 下支持 `ASUNA_DEV_ROOT` 环境变量指定开发路径
  - 自动检测 ONNX 模型输入需求，兼容不含 `token_type_ids` 的模型（如多语言版 E5）
  - 未找到时自动降级为关键词搜索

3. **数据目录**：默认为 `~/.asuna/`。首次运行会自动创建。
4. **Profile 隔离**：每个 profile 的数据独立存储在 `~/.asuna/profiles/{profile_id}/` 下。

---

## 系统架构

Asuna Memory System 采用 **双层记忆架构**：

```
┌─────────────────────────────────────────────┐
│              MCP Server (stdio)              │
│          JSON-RPC 2.0 over stdin/out        │
├──────────────┬──────────────────────────────┤
│   成长层      │         事实层               │
│  Growth Layer │       Fact Layer            │
│               │                             │
│  MEMORY.md    │  JSONL 文件 (对话归档)       │
│  USER.md      │  SQLite 索引 (sessions,      │
│  有界容量      │    turns, FTS5, vec_turns)  │
│  安全扫描      │  不可变存储                  │
│  溯源追踪      │  向量持久化 (int8[384])      │
├──────────────┴──────────────────────────────┤
│              Embedder (ONNX)                │
│      multilingual-e5-small (384-dim)        │
└─────────────────────────────────────────────┘
```

### 事实层（Fact Layer）

- **对话存储**：每次对话以 JSONL 格式归档到 `conversations/YYYY/MM/DD/` 目录
- **索引**：SQLite 存储会话元数据和对话轮次摘要
- **全文检索**：FTS5 虚拟表，支持中文分词（v1.1.3 实现了完善的 schema 向下兼容自动迁移）
- **向量检索**：sqlite-vec 扩展，384 维 INT8 量化向量，save/import/rebuild 均自动写入
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

```
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

## 升级指南

### 从任意旧版本升级到 v1.1.3 (强烈推荐)

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
    "model_name": "multilingual-e5-small",
    "dimensions": 384,
    "batch_size": 32
  },
  "db_path": "memory.db",
  "model_path": null
}
```

---

## 数据目录结构

```
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
    └── multilingual-e5-small/
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
