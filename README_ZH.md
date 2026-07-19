# Asuna Memory System

> AI Agent 长期记忆系统 — MCP Server

[English](README.md) | [AI Agent 安装指南](for_ai.md) | [历史变更日志](HISTORY_ZH.md)

## 升级指南

### 从 v2.5.2 升级到 v2.5.3

v2.5.3 修复 atom 驱逐在驱逐目标被较新 atom 的 `supersedes_id` 引用时报 `FOREIGN KEY constraint failed` 的问题（自引用外键无 `ON DELETE` 策略 + `foreign_keys=ON` + 最老优先驱逐）。该失败发生在 MEMORY.md 重建之前，导致提取的 atoms 进入 DB 但 `.md` 静默分叉 —— 且是永久性的：同一行会挡住每次重试，`doctor --fix` 也不做驱逐。现在所有删除路径（驱逐、`memory_remove`、`--split-entries`）都会先解除 `supersedes_id` 引用，驱逐全程在事务内执行。完整变更日志见 [HISTORY_ZH.md](HISTORY_ZH.md)。

升级：替换二进制文件；若 MEMORY.md 已分叉，运行一次 `asuna-memory doctor --fix` 重新同步。无需数据迁移。

### 从 v2.5.1 升级到 v2.5.2

v2.5.2 修复有界记忆完整性 bug：单个 DB 行可能包含多个 `§` 分隔条目，导致 `.md` 与 DB 条目数不一致并误导 `doctor`。新 CLI 参数 `asuna-memory doctor --split-entries` 可拆分现存的多条目行（保留元数据、跳过重复、重建 `.md`）；`doctor --fix` 现在会在合并前自动执行拆分。完整变更日志见 [HISTORY_ZH.md](HISTORY_ZH.md)。

升级：替换二进制文件后运行一次 `asuna-memory doctor --split-entries`。*（若从 v2.4.x 升级，下方 v2.5.0 的余弦 / `dimensions=768` / CORS 步骤仍需执行。）*

### 从 v2.5.0 升级到 v2.5.1

v2.5.1 修复 REST `/search` 端点与 CLI `search` 命令**忽略** `role`（及时间）过滤的问题 —— 形如 `{"query":"x","role":"assistant"}` 的请求此前会返回所有角色的 turn。`SearchRequest` 现接受 `role`/`after`/`before`/`last_days`，CLI 新增 `--role`/`--after`/`--before`/`--last-days`，与 MCP `search_sessions` 工具对齐。`/recall` 不受影响（其返回分层记忆而非 turn）。完整变更日志见 [HISTORY_ZH.md](HISTORY_ZH.md)。

升级：替换二进制文件，无需数据迁移。*（若从 v2.4.x 升级，下方 v2.5.0 步骤仍需执行。）*

### 从 v2.4.1 升级到 v2.5.0

v2.5.0 是一次**安全 + 正确性加固**发布。向量检索改用**余弦距离**，嵌入维度不匹配从静默失败改为显式报错，并修复了一次完整代码审查发现的约 40 个问题。完整变更日志见 [HISTORY_ZH.md](HISTORY_ZH.md)。

**⚠️ 升级步骤：**

1. 替换二进制文件并重启 —— `vec_turns` / `vec_bounded_memory` 自动迁移为余弦度量。
2. **运行 `asuna-memory rebuild`** 重新嵌入 turn 向量（在此完成前，历史 turn 的语义/混合搜索仅走关键词；有界记忆 atom 向量在启动时自动回填）。
3. **本地 ONNX 用户**：将 `embedding.dimensions` 设为与模型一致（EmbeddingGemma = 768）—— 不匹配现在会报错，而非静默清空索引。
4. **未启用 auth 的网关**：CORS 不再默认放行任意来源 —— 若浏览器客户端需要跨域访问，请设置 `gateway.cors_origins` 或启用 `auth_enabled`。

要点：余弦语义分数 · `--mode vector/fts` 别名 · 维度显式校验 · 默认仅 localhost 的 CORS · `query_only` 只读 `sql` · 取代向量去索引 · 批内去重 · 过滤感知搜索（不少返回）· 图邻居去重 · DashScope query/document `text_type` · 管线与 `/capture` 不再持锁跨网络调用 · `/capture` INT8 向量修复 · MCP panic 隔离。**188 测试，0 新增 clippy 警告。**

### 架构：Project Aegis

Project Aegis 是生产级多层分层记忆架构（L0-L5），包含 HTTP REST 网关、agent 框架集成和 MCP 服务器。

🟢 **多层记忆（L0-L5）**

- **L1 原子提取**（P3）：基于 LLM 的对话自动事实提取 + 演化链版本管理（`supersedes_id` 指针链）
- **A-MAC 准入评分**（P4）：5 维评分（效用 / 新颖性 / 时效性 / 重要性 / 可信度）决定记忆准入
- **L2-L3 场景 + 画像**（P5）：相关 L1 原子自动聚合为场景；从 L2 场景生成用户画像；渐进式披露检索引擎
- **L4-L5 心智模型 + 意图**（P6）：抽象认知框架生成（工作模式、决策标准、沟通风格）；意图预测与预期性记忆
- **技能记忆**（P7）：执行轨迹记录、模式识别（3+ 次出现）、通过 LLM 自动生成 SOP

🟢 **HTTP REST 网关（P1）**

- 基于 axum 的 HTTP 服务器，11 个端点用于 agent 框架集成
- 可选 API Key 认证（`Bearer` / `X-API-Key`）
- CORS 配置与来源白名单
- 10MB 请求体限制

🟢 **Hermes 插件 + Docker（P9）**

- Python `AMSMemoryProvider` 用于 Hermes Agent 集成
- 响应前自动记忆召回，对话后自动存储
- 多阶段 Docker 构建，含健康检查

🟢 **图谱增强（P8）**

- HTTP 网关多跳图谱查询
- L1 原子自动图谱集成（实体 + `mentions` / `supersedes` / `related_to` 关系）
- `relation_kind` 列区分 asserted 与 derived 关系

🔵 **安全与质量（代码审查）**

- API Key 使用常量时间比较 `subtle::ConstantTimeEq`（时序攻击防护）
- 创建 `bounded_memory_fts` FTS5 表 + 同步触发器，支持有界记忆全文检索
- 消除 search、recall、bounded_memory list_entries 中的 N+1 查询（批量 IN 查询）
- 演化链循环检测（HashSet + 深度限制）
- 图谱存储通过 RAII `unchecked_transaction()` 管理事务
- LIKE 通配符转义和 FTS5 操作符注入防护
- 模型下载 SHA256 校验基础设施

> **历史变更日志**（v1.0.x – v2.4.1）：请参阅 [HISTORY_ZH.md](HISTORY_ZH.md)

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

下载解压后安装：

```bash
# Linux x64
tar xzf asuna-memory-linux-x64.tar.gz
sudo mv asuna-memory /usr/local/bin/
sudo mv libonnxruntime.so* /usr/local/lib/   # ONNX Runtime，语义搜索需要

# macOS
tar xzf asuna-memory-macos-apple-silicon.tar.gz
sudo mv asuna-memory /usr/local/bin/
sudo mv libonnxruntime.dylib /usr/local/lib/

# Windows: 解压 zip，将 asuna-memory.exe 和 onnxruntime.dll 放到 PATH 中
```

> **注意**：压缩包同时包含二进制和 ONNX Runtime 库。二进制会自动从同目录、`~/.asuna/lib/` 或系统标准路径发现 `libonnxruntime.so`。如果只移动二进制，需确保 `.so` 在上述路径之一，或设置 `ORT_DYLIB_PATH` 指向其绝对路径。

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

# 下载嵌入模型（首次安装需要，~300MB）
asuna-memory model-download

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

1. **ONNX Runtime（必需）**：语义搜索需要 ONNX Runtime 动态库（`onnxruntime.dll` / `libonnxruntime.so`）。从 GitHub Release 下载的预编译包已包含；源码构建需自行放置。若缺失，系统自动降级为纯关键词搜索。
2. **模型文件（推荐）**：语义搜索需要 `embeddinggemma-300m-q8` 模型（~300MB）。
   - **自动下载**（推荐）：运行 `asuna-memory model-download`，从 GitHub Release Assets 下载到 `~/.asuna/models/`
   - 手动放置：从 [HuggingFace](https://huggingface.co/onnx-community/embeddinggemma-300m-ONNX) 下载放到 `~/.asuna/models/embeddinggemma-300m-q8/`
   - Windows 下支持 `ASUNA_DEV_ROOT` 环境变量指定开发路径
   - 未找到时自动降级为关键词搜索

3. **数据目录**：默认为 `~/.asuna/`。首次运行会自动创建。
4. **Profile 隔离**：每个 profile 的数据独立存储在 `~/.asuna/profiles/{profile_id}/` 下。

---

## 系统架构

**协议**：MCP stdio · JSON-RPC 2.0  
**嵌入**：本地 ONNX（embeddinggemma-300m，768d）或第三方 API（可配置维度，默认 1024d）· INT8 量化

**多层记忆架构（Project Aegis）** — 详见下方[专门章节](#project-aegis--多层记忆架构)了解 L0-L5 完整说明。

**图谱层 (v1.3+)**：SQLite 表 `entities` + `relations`，由 agent 通过 `graph_assert` 累积；canonical 归一化（lowercase + trim + 折空白）；不调 LLM 也不做规则抽取。

### 事实层（Fact Layer）

- **对话存储**：每次对话以 JSONL 格式归档到 `conversations/YYYY/MM/DD/` 目录
- **索引**：SQLite 存储会话元数据和对话轮次摘要
- **全文检索**：FTS5 contentless 虚拟表，支持中文 unigram 分词（v1.1.3+ 完善的 schema 自动迁移）
- **向量检索**：sqlite-vec 扩展，INT8 量化向量（可配置维度，默认 1024d），save/import/rebuild 均自动写入
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
| `rebuild_index`     | 从 JSONL 文件后台异步重建索引          |
| `rebuild_status`    | 查询 rebuild_index 执行进度            |
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
| `graph_path` | 两节点最短路径（返回完整的 Entity/Edge 交替序列） |
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

## Project Aegis — 多层记忆架构

Project Aegis 将原始的双层架构（事实层 + 成长层）扩展为 6 层分层记忆系统（L0-L5），灵感来源于人类认知模型：

| 层级 | 名称 | 存储 | 说明 |
|------|------|------|------|
| L0 | 对话 | JSONL + SQLite `turns` | 原始对话轮次（现有事实层） |
| L1 | 原子 | SQLite `bounded_memory` | 通过 LLM 从对话中提取的原子事实 |
| L2 | 场景 | Markdown 文件 | 从相关 L1 原子聚合的场景块 |
| L3 | 画像 | `USER.md` | 从 L2 场景生成的用户画像 |
| L4 | 心智模型 | Markdown 文件 | 抽象认知框架（工作模式、决策标准） |
| L5 | 意图预测 | 内存 | 基于 L4 模式预测的未来需求 |

### 核心机制

- **演化链**（P3）：`supersedes_id` 指针链追踪事实如何随时间演变。`get_chain()` / `get_latest_version()` 带循环检测。
- **A-MAC 准入评分**（P4）：5 维评分（效用 / 新颖性 / 时效性 / 重要性 / 可信度）决定提取的事实是否值得存为长期记忆。
- **渐进式披露检索**（P5）：分层检索（L3→L2→L1→L0），带 token 预算控制——高层分配更少 token，低层填充剩余预算。
- **技能记忆**（P7）：追踪 agent 解题路径，识别重复模式（3+ 次），通过 LLM 自动生成 SOP。
- **图谱集成**（P8）：L1 原子自动创建图谱实体和 `mentions` / `supersedes` / `related_to` 关系。支持多跳查询。

### HTTP REST 网关（P1）

基于 axum 的 HTTP 服务器，用于与 agent 框架（Hermes、LangChain 等）集成：

```bash
asuna-memory gateway --port 8765
```

| 端点 | 方法 | 状态 | 说明 |
|------|------|------|------|
| `/health` | GET | ✅ | 健康检查 |
| `/stats` | GET | ✅ | 数据库统计 |
| `/capture` | POST | ✅ | 保存对话轮次 |
| `/recall` | POST | ✅ | 渐进式披露检索（L3→L2→L1→L0） |
| `/search` | POST | ✅ | 文本或多跳图谱搜索 |
| `/persona` | GET | ✅ | 读取用户画像 |
| `/offload` | POST | ✅ | 长文本存储到 refs/ 目录 |
| `/recall/:node_id` | GET | ✅ | 召回已卸载的文本 |
| `/graph/assert` | POST | ✅ | 写入实体-关系三元组 |
| `/graph/neighbors` | POST | ✅ | 查询 N 跳邻居 |
| `/session/end` | POST | ✅ | 记录会话结束时间戳 |

认证：可选 API Key，通过 `Authorization: Bearer <key>` 或 `X-API-Key` 头。设置 `gateway.auth_enabled = true` 并配置 `AMS_GATEWAY_API_KEY`。

CORS：当 `gateway.cors_origins` 为空且 auth 关闭时，网关**仅放行 localhost 来源**（阻止公网站点跨域读取你的记忆）。如需允许其它来源，请将 `cors_origins` 设为显式白名单，或启用 auth。

### Hermes 插件（P9）

[Hermes Agent](https://github.com/NousResearch/hermes-agent) 集成的 Python 插件。实现 Hermes `MemoryProvider` ABC。

```bash
# 复制插件到 Hermes 插件目录
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
mkdir -p "$HERMES_HOME/plugins/ams_memory"
cp hermes-plugin/ams_memory/* "$HERMES_HOME/plugins/ams_memory/"
pip3 install requests
```

在 `~/.hermes/config.yaml` 中激活：

```yaml
memory:
  provider: ams_memory
```

通过环境变量或 `~/.hermes/ams.json` 配置：

| 环境变量 | JSON 键 | 默认值 | 说明 |
|----------|---------|--------|------|
| `AMS_GATEWAY_URL` | `gateway_url` | `http://127.0.0.1:8765` | Gateway 地址 |
| `AMS_API_KEY` | `api_key` | *(空)* | 认证密钥 |
| `AMS_RECALL_TOP_K` | `recall_top_k` | `5` | 每次召回数量 |
| `AMS_AUTO_RECALL` | `auto_recall` | `true` | 自动召回 |
| `AMS_AUTO_STORE` | `auto_store` | `true` | 自动存储 |

### Docker 支持

```bash
docker build -t asuna-memory .
docker run -p 8765:8765 -v ~/.asuna:/root/.asuna asuna-memory
```

多阶段构建：Rust 编译 → Debian slim 运行时（预装 Python3 + Hermes 插件）。

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

# 启动 HTTP REST 网关
asuna-memory gateway --port 8765

# 环境检查
asuna-memory doctor

# 自动修复 DB/.md 不一致
asuna-memory doctor --fix

# 下载嵌入模型（首次安装，~300MB）
asuna-memory model-download

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

# 安全删除 turn（自动清理 FTS + 向量索引）
asuna-memory delete-turn <id>

# 只读 SQL 查询
asuna-memory sql "SELECT id, preview FROM turns LIMIT 5"
```

### 全局参数

| 参数        | 默认值                 | 说明         |
| ----------- | ---------------------- | ------------ |
| `--config`  | `~/.asuna/config.json` | 配置文件路径 |
| `--profile` | `default`              | 指定 profile |

---

## 配置文件

配置文件为 JSON 格式，默认路径 `~/.asuna/config.json`。不存在时使用内置默认值。
所有字段均为可选——只需覆盖你需要修改的部分。

### 最小配置（API 嵌入，VPS 推荐）

```json
{
  "embedding": {
    "dimensions": 1024,
    "batch_size": 10,
    "api_url": "https://dashscope.aliyuncs.com/api/v1",
    "api_key": "sk-your-key-here",
    "api_model": "text-embedding-v4"
  }
}
```

### 完整配置参考

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
    "security_scan": true,
    "atom_capacity_ratio": 0.3
  },
  "search": {
    "default_top_k": 5,
    "search_mode": "hybrid",
    "fts_enabled": true
  },
  "embedding": {
    "dimensions": 1024,
    "batch_size": 32,
    "api_url": "",
    "api_key": "",
    "api_model": "",
    "api_format": ""
  },
  "graph": {
    "enabled": true,
    "remind_on_save": true
  },
  "pipeline": {
    "enable_extraction": true,
    "every_n_turns": 5
  },
  "llm": {
    "base_url": "",
    "api_key": "",
    "model": ""
  },
  "gateway": {
    "auth_enabled": false,
    "api_key": "",
    "cors_origins": []
  }
}
```

### 嵌入字段说明

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `dimensions` | `1024` | 向量维度（`vec0` 表）。切换需要 `rebuild --full` |
| `batch_size` | `32` | 每次嵌入 API 调用的最大文本数。DashScope 限制为 10 |
| `api_url` | `""` | OpenAI 兼容的 base URL。与 `api_model` 同时设置时使用 API 后端 |
| `api_key` | `""` | API 密钥。也可读取 `AMS_EMBEDDING_API_KEY` 环境变量 |
| `api_model` | `""` | 模型名称（如 `text-embedding-v4`、`text-embedding-3-small`） |
| `api_format` | `""` | `"openai"` 或 `"dashscope"`。从 `api_url` 自动检测 |

**后端优先级**：API（`api_url` + `api_model` 已设置）→ 本地 ONNX → 禁用（仅关键词搜索）

### 嵌入模型提供方

嵌入后端根据配置自动检测：

1. **API 后端** — 当 `api_url` 和 `api_model` 均已设置时，使用 OpenAI 兼容或 DashScope HTTP API
2. **本地 ONNX** — 否则使用本地模型（需要模型文件 + `libonnxruntime.so`，~300MB 内存）
3. **禁用** — 两者都不可用时，降级为仅关键词搜索

**推荐：DashScope text-embedding-v4**（多语言中文最优，可配置维度）：

```json
{
  "embedding": {
    "dimensions": 1024,
    "batch_size": 10,
    "api_url": "https://dashscope.aliyuncs.com/api/v1",
    "api_key": "sk-your-key-here",
    "api_model": "text-embedding-v4"
  }
}
```

**OpenAI 兼容 API**（OpenAI、Ollama、vLLM、LiteLLM 等）：

```json
{
  "embedding": {
    "dimensions": 1024,
    "api_url": "https://api.example.com/v1",
    "api_key": "your-key-here",
    "api_model": "text-embedding-3-small"
  }
}
```

- `api_url`：OpenAI 兼容的 base URL，或 DashScope 原生 URL
- `api_key`：也可通过 `AMS_EMBEDDING_API_KEY` 环境变量设置。本地端点（如 Ollama）可留空
- `api_model`：API 端点识别的模型名称
- `api_format`：`"openai"`（默认）或 `"dashscope"`。从 `api_url` 自动检测——包含 "dashscope" 的 URL 自动使用 DashScope 格式
- `batch_size`：每次 API 调用的最大文本数（默认 32）。DashScope 限制为 10
- `dimensions`：向量维度（默认 1024）。所有数据**及嵌入模型**必须一致——本地 ONNX 模型（EmbeddingGemma，768）请设为 768，不匹配现在会显式报错。切换需要 `rebuild --full`

> **注意**：切换后端或更改 `dimensions` 后需要重建向量：`asuna-memory rebuild --full`

---

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
