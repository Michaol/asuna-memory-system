# Asuna Memory System

> AI Agent 长期记忆系统 — MCP Server

[English](README_EN.md) | [AI Agent 安装指南](for_ai.md)

---

## 安装方式

### 方式一：从 GitHub Release 下载（推荐）

前往 [Releases](https://github.com/Michaol/asuna-memory-system/releases) 下载对应平台的预编译包（含 ONNX Runtime 动态库）：

| 平台 | 文件 |
|------|------|
| Windows x86_64 | `asuna-memory-windows-x64.exe.zip` |
| Linux x86_64 | `asuna-memory-linux-x64.tar.gz` |
| Linux ARM64 | `asuna-memory-linux-arm64.tar.gz` |
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
│  有界容量      │    turns, FTS5, vectors)    │
│  安全扫描      │  不可变存储                  │
│  溯源追踪      │                             │
├──────────────┴──────────────────────────────┤
│              Embedder (ONNX)                │
│      multilingual-e5-small (384-dim)        │
└─────────────────────────────────────────────┘
```

### 事实层（Fact Layer）

- **对话存储**：每次对话以 JSONL 格式归档到 `conversations/YYYY/MM/DD/` 目录
- **索引**：SQLite 存储会话元数据和对话轮次摘要
- **全文检索**：FTS5 虚拟表，支持 Unicode 分词
- **向量检索**：sqlite-vec 扩展，384 维 INT8 量化向量
- **混合搜索**：Reciprocal Rank Fusion (RRF) 融合语义 + 关键词结果

### 成长层（Growth Layer）

- **有界记忆**：`MEMORY.md`（AI 知识记忆，2200 字符上限）和 `USER.md`（用户画像，1375 字符上限）
- **条目分隔**：使用 `§` 分隔符区分不同条目
- **安全扫描**：每次写入/更新前自动检测 prompt injection、凭据泄露、不可见 Unicode
- **溯源追踪**：每条记忆可追溯到原始对话 session

---

## MCP 工具列表

| 工具名 | 说明 |
|--------|------|
| `save_session` | 保存完整对话到事实层 |
| `search_sessions` | 多维度检索历史对话 |
| `memory_write` | 向成长记忆写入新条目 |
| `memory_update` | 通过子串匹配更新记忆条目 |
| `memory_remove` | 删除记忆条目 |
| `memory_read` | 读取当前成长记忆全文 |
| `user_profile` | 读写用户画像 |
| `rebuild_index` | 从 JSONL 文件重建 SQLite 索引 |
| `memory_provenance` | 验证成长记忆的溯源信息 |

详细参数说明见 [for_ai.md](for_ai.md)。

---

## CLI 命令

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

# 从 JSONL 重建索引
asuna-memory rebuild

# 导入 JSONL 文件
asuna-memory import session.jsonl

# 导出会话摘要
asuna-memory export <session_id>
```

### 全局参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--config` | `~/.asuna/config.json` | 配置文件路径 |
| `--profile` | `default` | 指定 profile |

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
│       ├── memory.db         # SQLite 索引数据库
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
