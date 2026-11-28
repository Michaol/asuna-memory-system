# Asuna Memory System

> AI Agent 长期记忆系统 — MCP Server & HTTP Gateway

[![Quality gate status](https://sonarcloud.io/api/project_badges/measure?project=Michaol_asuna-memory-system&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=Michaol_asuna-memory-system)

> SonarCloud 徽章由 CI 产出：GitHub Actions 的 `sonar` job 在每次 push 到 main 时扫描（配置：[sonar-project.properties](sonar-project.properties)）。强制门禁为 GitHub Actions：fmt / clippy `-D warnings` / cargo test / 插件 pytest，外加 Docker 构建冒烟——见 [.github/workflows/ci.yml](.github/workflows/ci.yml)。

[English](README.md) | [AI Agent 安装指南](for_ai.md) | [历史变更日志](HISTORY_ZH.md)

## 升级指南

### 从 v3.0.0 升级到 v3.1.0

v3.1.0 是一次并发 + 构建正确性版本：新增**跨进程多写者支持**（同一 profile 上 1 个 gateway + N 个 MCP `serve` + CLI 命令真并发），修复**无 AVX 发布构建基线**（发布二进制不再内嵌无守卫 AVX2 C 代码、在无 AVX 的 CPU 上 SIGILL），并为 DashScope qwen3.7 模型调优**嵌入批量**。完整变更日志见 [HISTORY_ZH.md](HISTORY_ZH.md)。

**Breaking（必读）**：(1) v3.0.0 的 **1024 维锁定**仍然生效——`embedding.dimensions != 1024` 在加载时报错；迁移 pre-v3.0 向量索引时执行一次 `asuna-memory rebuild --full`。(2) **Overwrite 失败契约变更**：`save_session` / CLI import 现在事务外预写 JSONL、事务内 rename；预写/rename 失败会原子回滚 DB（残余的 rename 成功但 COMMIT 失败窗口由下次保存或 `rebuild --full` 抹平）。(3) **多写者版本下界**：同一 profile 的全部并发写进程必须 >= v3.1.0；新旧混跑不受多写者保证覆盖。(4) `/capture` 遇 SQLITE_BUSY/LOCKED 现在最多重试 2 次后返回 **503 Service Unavailable**（可重试），而非 500。(5) 沿用 v2.7.0：非空 `AMS_GATEWAY_API_KEY` 环境变量仍隐含启用 auth；回环调试如需关闭请显式设置 `AMS_GATEWAY_AUTH_ENABLED=false`。

升级步骤：替换二进制；**无 schema 迁移**（`user_version = 6` 不变）；仅迁移 pre-v3.0 向量时才需 `asuna-memory rebuild --full`。网络文件系统仍被禁止（SQLite WAL）；并发写者须共享 `embedding.dimensions`。源码构建需要 Rust **1.89+**。

更早版本（v3.0.0 及以前）的升级指南见 [HISTORY_ZH.md](HISTORY_ZH.md)。

### 架构：Project Aegis

Project Aegis 是 6 层分层长期记忆系统（L0-L5），集成 axum HTTP REST 网关、Hermes Agent 插件、知识图谱与 JSON-RPC 2.0 MCP stdio 服务器。

🟢 **多层记忆（L0-L5）**

- **L0 对话层**：JSONL 真相归档 + SQLite `turns` + `turns_fts`（jieba 中文分词，纯标点查询降级 `LIKE`）
- **L1 原子提取（P3）**：基于无锁 JSONL 完整对话的 LLM 原子事实提取、批处理 A-MAC 准入评分、`supersedes_id` 置信度门控演化链，以及 `bounded_memory_fts` + `vec_bounded_memory` RRF 混合召回
- **L2 场景聚合（P5，可选）**：复用 Phase 3 原子嵌入向量做聚类 + LLM 摘要，写入 `bounded_memory`（`memory_type='scenario'`）与 `memory/scenarios/*.md` 镜像
- **L3 画像生成（P5，可选）**：从 L2 场景生成 `memory/persona.md`，支持 DB 多行 `target='user'` 串联回退与 1200 字符 / 40% 预算保护
- **L4 心智模型 & L5 意图预测（P6，可选）**：单次批处理 JSON 生成 `memory/mental_models/*.md` 与 `memory/intent/*.md`（含单篇解析失败回退），`/recall` 强制 7 天新鲜度门控

🟢 **HTTP REST 网关（P1）**

- 基于 axum 的 11 端点网关，全部阻塞 DB/FS 处理器迁至 `spawn_blocking`，配备 60s 请求超时与 Ctrl+C / SIGTERM 优雅停机
- SHA-256 常量时间 `Authorization: Bearer`（大小写无关）/ `X-API-Key` 认证；CORS 默认仅放行 localhost/loopback 来源（无论是否启用认证；显式 `gateway.cors_origins` 可覆盖）；全部 CORS 配置显式放行 `Authorization`、`Content-Type`、`X-API-Key` 请求头
- `/session/end` 具备单会话去重与有限并发信号量（`MAX_CONCURRENT_PIPELINES = 4`）

🟢 **Hermes 插件 + Docker（P9）**

- Python `AMSMemoryProvider` 采用单 FIFO 有界队列（128）后台线程与线程隔离 `requests.Session`
- 多阶段 Rust 1.89 Docker 构建（非 root 运行、动态端口健康检查、无模型下载逻辑）

🟢 **图谱增强（P8）**

- 有界 BFS 图遍历——HTTP `/search`（多跳）+ `/graph/neighbors` 与 MCP `graph_neighbors` 访问节点上限 `10_000`（`MAX_VISITED_NODES` / `MAX_NEIGHBOR_VISITED`）；MCP `graph_path` 节点展开上限 `5_000`（`MAX_PATH_RESULTS`）
- L1 原子自动生成 `mentions` / `supersedes` / `from_session` / `related_to` 关系

🔵 **安全与存储完整性**

- 逐向量 absmax INT8 量化（`127.0 / max_abs`）与非有限向量拦截
- `Db::immediate_tx()` RAII 写事务（`PRAGMA user_version = 6`）、窄化 FTS 更新触发器与 L1–L5/JSONL 统一 `util::atomic_write`
- BiDi/Unicode Tag 不可见字符剥离、全角归一化、凭据脱敏、XML 不可信数据定界与 `asuna-memory sql` SQLite 只读 Authorizer
- 写侧安全扫描覆盖 L1–L5 与 `reconcile_fix`，JSONL 归档与 `/offload` 强制 Canonical Containment 校验

> **历史变更日志**（v1.0.x – v2.7.3）：请参阅 [HISTORY_ZH.md](HISTORY_ZH.md)

---

## 安装方式

### 方式一：从 GitHub Release 下载（推荐）

前往 [Releases](https://github.com/Michaol/asuna-memory-system/releases) 下载对应平台的预编译压缩包（含二进制、`NOTICE.md`、`LICENSES/` 与 `SHA256SUMS.txt` 校验）：

| 平台                | 文件                                      |
| ------------------- | ----------------------------------------- |
| Windows x86_64      | `asuna-memory-windows-x64.zip`            |
| Linux x86_64        | `asuna-memory-linux-x64.tar.gz`           |
| Linux ARM64         | `asuna-memory-linux-arm64.tar.gz`         |
| macOS Apple Silicon | `asuna-memory-macos-apple-silicon.tar.gz` |

解压后将二进制放入 `PATH`：

```bash
# Linux x64 / macOS
tar xzf asuna-memory-linux-x64.tar.gz
sudo mv asuna-memory /usr/local/bin/

# Windows: 解压 asuna-memory-windows-x64.zip，将 asuna-memory.exe 放到 PATH 中
```

### 方式二：从源码构建

前置要求：**Rust 1.89+**（与 `Cargo.toml` 的 `rust-version` 一致），无需外部数据库（SQLite 已内嵌）。

```bash
git clone https://github.com/Michaol/asuna-memory-system.git
cd asuna-memory-system
cargo build --locked --release
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

> **AI Agent 注意**：如果你是 AI Agent，需要自动安装并接入此系统，请阅读 [for_ai.md](for_ai.md)。

### 安装注意事项

1. **在线嵌入 API（启用语义/混合检索可选配）**：在配置中设置 1024 维在线 API（`embedding.api_url` + `embedding.api_model` + `AMS_EMBEDDING_API_KEY`）。未配置时系统自动工作在纯关键词模式。
2. **数据目录**：默认为 `~/.asuna/`。首次运行自动创建。
3. **Profile 隔离**：每个 profile 数据独立存储在 `~/.asuna/profiles/{profile_id}/` 下。

---

## 系统架构

**协议**：MCP stdio · JSON-RPC 2.0  
**嵌入**：1024 维在线 API（OpenAI 兼容或 DashScope）· 逐向量 absmax INT8 量化 · 未配置时降级纯关键词检索

**多层记忆架构（Project Aegis）** — 详见下方[专门章节](#project-aegis--多层记忆架构)。

**图谱层 (v1.3+)**：SQLite 表 `entities` + `relations`，由 agent 通过 `graph_assert` 写入或 L1 自动集成派生；`canonical` 归一化（lowercase + trim + 折空白）。

### 事实层（Fact Layer）

- **对话存储**：每次对话以 JSONL 归档到 `conversations/YYYY/MM/DD/`
- **索引**：SQLite WAL 存储会话元数据与轮次摘要（`user_version = 6`）
- **全文检索**：FTS5 虚拟表（`turns_fts`、`bounded_memory_fts`）使用内联 `JiebaTokenizer`（`tokenize='jieba'`，与查询侧 OR 回退共用进程内唯一 `jieba-rs 0.11` 词典；完整保留 `.`/`_`/`-` ASCII 复合词，纯中文分词不变，出处见 [NOTICE.md](NOTICE.md)）与安全多词 `AND`/`OR` 查询构建
- **向量检索**：sqlite-vec 扩展，1024 维 absmax INT8 量化向量（`vec_turns`、`vec_bounded_memory`）
- **混合搜索**：确定性 Reciprocal Rank Fusion (RRF) 融合语义 + 关键词候选

### 成长层（Growth Layer）

- **有界记忆**：`MEMORY.md`（AI 知识记忆，2200 字符上限）和 `USER.md`（用户画像，1375 字符上限）
- **条目分隔**：使用 `§` 分隔符区分不同条目
- **安全扫描**：L1–L5 与 `reconcile_fix` 持久化前自动拦截 prompt injection、凭据泄露与不可见 Unicode
- **溯源追踪**：每条记忆可追溯到原始对话 session

---

## MCP 工具列表

| 工具名                 | 说明                                   |
| ---------------------- | -------------------------------------- |
| `save_session`         | 保存完整对话到事实层（含自动生成向量） |
| `search_sessions`      | 多维度检索历史对话                     |
| `memory_write`         | 向成长记忆写入新条目                   |
| `memory_update`        | 通过子串匹配更新记忆条目               |
| `memory_remove`        | 删除记忆条目                           |
| `memory_read`          | 读取当前成长记忆全文                   |
| `user_profile`         | 读写用户画像                           |
| `rebuild_index`        | 从 JSONL 文件后台异步重建索引          |
| `rebuild_status`       | 查询 rebuild_index 执行进度            |
| `memory_provenance`    | 验证成长记忆的溯源信息                 |
| `graph_assert`         | 写实体-关系三元组                      |
| `graph_neighbors`      | 查 N-hop 邻居                          |
| `graph_path`           | 两节点有界 BFS 最短路径                |
| `graph_link_entity`    | 别名合并（`from` -> `to`）             |
| `graph_prune_dangling` | 清理悬空 `source_turn` 引用            |

详细参数说明见 [for_ai.md](for_ai.md)。

---

## 图谱记忆 (v1.3+)

图谱层复用同一个 SQLite 数据库的 `entities` + `relations` 表。agent 通过 `graph_assert` 写入实体关系；`canonical` 归一化处理大小写和空白漂移，显式别名合并通过 `graph_link_entity` 完成。

### 软提示

`save_session` 在 `graph.enabled && graph.remind_on_save` 时返回 `graph_pending: { turn_ids, hint }`，列出尚未被 relation 引用的 turn_id；可在 `config.json` 设 `graph.remind_on_save = false` 关闭。

### 整体禁用

`graph.enabled = false` 时：
- MCP `graph_*` 工具返回 `"graph disabled in config"`；
- HTTP REST `/graph/*` 与多跳 `/search` 返回 `403 Forbidden`；
- 会话后 L1/L2/L3-L5 提取与整合管道直接跳过。事实层与手动成长记忆不受影响。

### 诊断

`asuna-memory doctor` 默认显示 `图谱: ENABLED (N entities, M relations)`。加 `--verbose` 还显示图谱覆盖率与悬空引用数。

---

## Project Aegis — 多层记忆架构

Project Aegis 将长期记忆组织为 6 层分层架构（L0-L5）：

| 层级 | 名称     | 存储                                                                  | 说明                                              |
| ---- | -------- | --------------------------------------------------------------------- | ------------------------------------------------- |
| L0   | 对话     | JSONL + SQLite `turns` + `turns_fts` + `vec_turns`                    | 原始对话轮次与混合检索                            |
| L1   | 原子     | SQLite `bounded_memory` + `bounded_memory_fts` + `vec_bounded_memory` | LLM 提取的原子事实（FTS+向量 RRF 混合召回）       |
| L2   | 场景     | SQLite 行（`memory_type='scenario'`）+ `memory/scenarios/*.md` 镜像   | 复用 Phase 3 原子向量聚类的场景块（可选开启）     |
| L3   | 画像     | `memory/persona.md`（生成物）；`USER.md` / `bounded_memory`           | L2 场景生成的用户画像（1200 字符 / 40% 预算保护） |
| L4   | 心智模型 | `memory/mental_models/*.md`                                           | 抽象认知框架（单次批处理生成）                    |
| L5   | 意图预测 | `memory/intent/*.md`                                                  | 预测未来需求与话题（单次批处理生成）              |

### 核心机制

- **演化链（P3）**：`supersedes_id` 指针链配合置信度门控与全部读取面过滤，已取代事实不会复活。
- **A-MAC 准入评分（P4）**：5 维评分合并为单次 JSON Utility LLM 调用（含单项回退）。
- **渐进式披露检索（P5）**：按 `L3 -> L4 -> L5 -> L2 -> L1 -> L0` 分层填充 token 预算，查询向量在 DB 锁外预计算。
- **图谱集成（P8）**：L1 原子自动创建图谱实体与 `mentions` / `supersedes` / `from_session` 关系。

### HTTP REST 网关（P1）

```bash
asuna-memory gateway --port 8765
```

| 端点               | 方法 | 说明                                                |
| ------------------ | ---- | --------------------------------------------------- |
| `/health`          | GET  | 健康检查（`{"status":"ok","version":"3.1.0"}`）     |
| `/stats`           | GET  | 数据库统计                                          |
| `/capture`         | POST | 保存对话轮次                                        |
| `/recall`          | POST | 渐进式披露检索（`L3->L4->L5->L2->L1->L0`）          |
| `/search`          | POST | 文本（`hybrid`/`semantic`/`keyword`）或多跳图谱检索 |
| `/persona`         | GET  | 读取用户画像（`USER.md` -> `persona.md` -> DB）     |
| `/offload`         | POST | 长文本存储到 `refs/` 目录                           |
| `/recall/*node_id` | GET  | 召回已卸载文本（支持含 `/` 的通配路径）             |
| `/graph/assert`    | POST | 写入实体-关系三元组（支持数字或字符串 confidence）  |
| `/graph/neighbors` | POST | 查询 N 跳邻居                                       |
| `/session/end`     | POST | 记录会话结束并触发有界异步管道                      |

认证（除 `/health` 外的所有端点）：可选 API Key，通过大小写无关 `Authorization: Bearer <key>` 或 `X-API-Key` 请求头。设置非空 `AMS_GATEWAY_API_KEY` 会自动启用 auth（仅本机调试如需禁用可显式设 `AMS_GATEWAY_AUTH_ENABLED=false`）。非回环地址绑定（`gateway.bind_host` / `AMS_GATEWAY_BIND_HOST`）必须开启认证，否则拒绝启动。

CORS：`gateway.cors_origins` 为空时仅放行 localhost/loopback 来源——启用认证时同样收窄（v4 加固，原为 `Any`）。如需放行 Web 客户端，请显式配置来源列表（或 `*`）。全部 CORS 配置均显式放行 `Authorization`、`Content-Type` 与 `X-API-Key` 请求头。

### Hermes 插件（P9）

[Hermes Agent](https://github.com/NousResearch/hermes-agent) 的 Python 插件，仅依赖 `requests`（CI 通过 `hermes-plugin/requirements-ci.txt` 全哈希锁定）。

```bash
HERMES_HOME="${HERMES_HOME:-$HOME/.hermes}"
mkdir -p "$HERMES_HOME/plugins/ams_memory"
cp hermes-plugin/ams_memory/*.py "$HERMES_HOME/plugins/ams_memory/"
python3 -m pip install --require-hashes --only-binary :all: -r hermes-plugin/requirements.txt
```

在 `$HERMES_HOME/config.yaml` 中启用：

```yaml
memory:
  provider: ams_memory
```

### Docker 支持

```bash
docker build -t asuna-memory .

# 先创建宿主数据目录：若 ~/.asuna 不存在，dockerd 会以 root:root 创建它，
# 容器内非 root 的 asuna 用户（uid 1000）无法写入——首跑 entrypoint doctor 即失败。
mkdir -p ~/.asuna

docker run -p 127.0.0.1:8765:8765 -v ~/.asuna:/home/asuna/.asuna \
  -e AMS_GATEWAY_BIND_HOST=0.0.0.0 -e AMS_GATEWAY_API_KEY=your-secret-key \
  asuna-memory
```

多阶段构建：Rust 1.89 编译 -> Debian slim 运行时（非 root `asuna` 用户、`/opt/venv` + Hermes 插件预装）。入口脚本提取 `--config`/`--profile`（空格与 `=` 两种写法）并转发给启动前的 `asuna-memory doctor` 检查，再启动 `gateway`（`AMS_GATEWAY_PORT`，默认 `8765`）；运行时镜像随附 `/app/NOTICE.md` 与 `/app/LICENSES/`。

> **bind-mount 权属**：`mkdir -p ~/.asuna` 让宿主目录归当前宿主用户所有——仅当 uid 与镜像内 `asuna`（默认 `1000`）一致时容器才可写。宿主 uid 不同时，请用 `--build-arg UID=$(id -u) --build-arg GID=$(id -g)` 重新构建，或改用命名卷（`-v ams-data:/home/asuna/.asuna`）。`hermes-plugin/docker-compose.yml` 使用命名卷，不受此问题影响。

---

## JSONL 文件格式

`import` 与 `rebuild` 读取 JSONL 文件：**1 行 Header + N 行 Turn**，每行一个 JSON 对象。

- Header 字段：`v`（`1`）、`type`（`"session_header"`）、`session_id`（非空且 ≤255 字符、无路径/控制字符/`:`）、`start_time`（ISO 8601）、`profile_id`（仅 ASCII 字母/数字/`_`/`-`）、可选 `source`/`agent_model`/`title`/`tags`。
- Turn 字段：`ts`（ISO 8601）、`seq`（1 起递增）、`role`（`user`/`assistant`/`tool_call`/`system`）、`content`、可选附加扁平字段。

## CLI 命令

```bash
asuna-memory serve
asuna-memory gateway --port 8765
asuna-memory doctor [--verbose|--fix|--split-entries]
asuna-memory list-profiles
asuna-memory list-sessions --last-days 7 --limit 20
asuna-memory search "Rust async" --mode hybrid --top-k 5
asuna-memory rebuild [--full]
asuna-memory import session.jsonl
asuna-memory export <session_id>
asuna-memory delete-turn <id>
asuna-memory sql "SELECT id, preview FROM turns LIMIT 5"
```

---

## 配置文件

JSON 格式，默认 `~/.asuna/config.json`。任意子集或 `{}` 均可启动，优先级 `config.json > 环境变量 > 默认值`（例外：`AMS_GATEWAY_AUTH_ENABLED` 显式覆盖 auth 开关；`llm.*` 支持 `AMS_LLM_*` 与 `OPENAI_*` 回退，空 `llm.model` 默认使用 `"deepseek-v3"`）。

```json
{
  "embedding": {
    "dimensions": 1024,
    "batch_size": 20,
    "api_url": "https://dashscope.aliyuncs.com/api/v1",
    "api_key": "sk-your-key-here",
    "api_model": "qwen3.7-text-embedding-flash"
  },
  "llm": {
    "base_url": "https://api.deepseek.com/v1",
    "api_key": "sk-your-llm-key",
    "model": "deepseek-chat"
  }
}
```

> **端点说明**：共享域名 `dashscope.aliyuncs.com` 实测可调用 `qwen3.7-text-embedding-flash`（2026-11-28 验证）。该域名自 2026-09-30 起进入维护状态，官方建议新部署迁移到业务空间专属域名 `https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/api/v1`（`{WorkspaceId}` 见百炼控制台「业务空间管理」API Host 列；专属域名仅接受该业务空间的 API Key，否则返回 401）。两种域名原生路径相同，AMS 均自动识别。
>
> **模型说明**：`qwen3.7-text-embedding-flash` 默认 1024 维（另支持 768/512/256，AMS v3.0.0 锁定 1024），原生格式单请求最多 20 条文本（v3/v4 为 10 条），AMS 按模型自动钳制 `batch_size`。若改用 `compatible-mode` 兼容端点，官方未明确 flash 的 `dimensions` 参数与批量上限支持，AMS 保守按 10 条钳制，建议优先使用原生格式并实测一次语义检索。

| 嵌入配置字段 | 默认值 | 说明                                                               |
| ------------ | ------ | ------------------------------------------------------------------ |
| `dimensions` | `1024` | v3.0.0 锁定 `1024`；显式配置非 1024 值会在加载时报错并提示 `rebuild --full` |
| `batch_size` | `32`   | 单次 API 调用最大文本数（`1..=1024`；DashScope 自动钳制：qwen3.7-text-embedding 系列原生格式 `20`，其余 DashScope 模型/兼容模式 `10`） |
| `api_url`    | `""`   | OpenAI 兼容或 DashScope 原生 URL（含业务空间专属域名 `*.maas.aliyuncs.com`） |
| `api_key`    | `""`   | 也可由环境变量 `AMS_EMBEDDING_API_KEY` 提供                        |
| `api_model`  | `""`   | 模型名称（如 `qwen3.7-text-embedding-flash`、`text-embedding-v4`、`text-embedding-3-small`） |
| `api_format` | `""`   | `"openai"` 或 `"dashscope"`；DashScope 原生 URL（共享或专属域名）自动检测，`compatible-mode` 保持 OpenAI 格式 |

---

## 数据目录结构

```text
~/.asuna/
├── config.json
└── profiles/
    └── default/
        ├── memory.db
        ├── conversations/2026/04/10/20260410T100200_a1b2c3d4.jsonl
        ├── refs/
        └── memory/
            ├── MEMORY.md
            ├── USER.md
            ├── persona.md
            ├── scenarios/
            ├── mental_models/
            └── intent/
```

---

## 安全机制

`scan_content` 会在检测前先剥离不可见 BiDi / Unicode Tag / 软连字符等控制字符并做全角符号归一化，识别中英文 Prompt Injection、凭据泄露（OpenAI / GitHub / AWS / PEM / PGP 私钥块）与零宽字符，覆盖 **L1–L5** 与手工写入全部写面：

- **硬拦截门（写入/保存前拒绝或跳过）**：成长记忆手工读写（`memory_write`、`memory_update`、`reconcile_fix`）、L1 提取原子（跳过并审计 `security_scan_skip`）、L2 场景标题与摘要、L3 `persona.md` 五大字段、L4 心智模型条目、L5 意图预测条目、`graph_assert`、`graph_link_entity` 与 `/offload`。
- **软审计（原文落库、审计标记）**：通过 `/capture`、`save_session` 与 CLI `import` 写入的 L0 原始对话轮次作为事实证据保留原文，同时命中项记录至 `audit_log`（`security_scan_flag`）。
- **不可信数据定界与脱敏**：LLM 提取/准入/整合 prompt 使用 XML 不可信数据定界符隔离历史对话；全部召回出口（`/recall` 横幅、插件 `<recalled_memories>` 与分隔符转义）明确声明召回记忆是数据而非指令；日志与审计表对检测到的凭据执行脱敏。

---

## License

MIT

第三方许可声明：内联 FTS5 `JiebaTokenizer`（`src/index/jieba_tokenizer.rs`）与 FTS5 v2 FFI 垫片（`src/index/fts5_ffi.rs`）改编自 MIT 许可的 `sqlite-simple-tokenizer` ——详见 [NOTICE.md](NOTICE.md) 与 [LICENSES/sqlite-simple-tokenizer-MIT](LICENSES/sqlite-simple-tokenizer-MIT)。
