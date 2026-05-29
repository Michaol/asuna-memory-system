# AMS "Project Aegis" — 功能与技术规格书

> 版本：v2.0.0 目标规格
> 日期：2026-05-29
> 状态：规划中

---

## 一、记忆层级

| 层级 | 名称 | 存储形式 | 内容 |
|------|------|---------|------|
| 短期 | Context Offload | `refs/*.md` + Mermaid `.mmd` | 长任务日志压缩，node_id 回溯 |
| L0 | Conversation | JSONL + SQLite `turns` | 原始对话全文 |
| L1 | Atom | SQLite `bounded_memory` | 原子事实/偏好/决策/关系 |
| L2 | Scenario | `memory/scenarios/*.md` | 场景块（相关 L1 聚合） |
| L3 | Persona | `memory/persona.md` | 用户画像（偏好/身份/工作流/技术栈） |
| L4 | Mental Model | `memory/mental_models/*.md` | 认知框架（工作流模式/决策框架/沟通风格） |
| L5 | Intent | `memory/intents/*.md` | 意图预测（可能话题/预期需求） |
| 技能 | Skill | `memory/skills/*.md` | 自动生成的 SOP（触发条件+步骤+成功率） |
| 图谱 | Graph | SQLite `entities` + `relations` | 实体关系三元组 + 记忆节点化 |

### 存储目录结构

```
~/.asuna/profiles/{profile_id}/
├── memory.db                        -- SQLite（L0 turns + L1 atoms + 图谱）
├── conversations/                   -- JSONL（L0 原始对话）
│   └── 2026/05/29/
│       └── 20260529T100200_abc.jsonl
├── memory/
│   ├── MEMORY.md                    -- 手动记忆（现有，兼容）
│   ├── USER.md                      -- 手动用户画像（现有，兼容）
│   ├── persona.md                   -- L3 自动生成画像
│   ├── scenarios/                   -- L2 场景块
│   │   ├── rust-dev-preferences.md
│   │   └── mcp-protocol-decisions.md
│   ├── mental_models/               -- L4 心智模型
│   │   ├── workflow-patterns.md
│   │   ├── decision-framework.md
│   │   └── communication-style.md
│   ├── intents/                     -- L5 意图预测
│   │   ├── likely-next-topics.md
│   │   └── anticipated-needs.md
│   └── skills/                      -- 技能记忆
│       ├── fix-rust-compile-errors.md
│       └── setup-mcp-server.md
└── refs/                            -- 短期记忆 offload
    └── task_001/
        ├── canvas.mmd
        ├── step_001.md
        └── step_002.md
```

---

## 二、核心机制

### Evolution Chain（记忆演化链）

记忆更新不覆盖旧值，通过 `supersedes` 指针形成时间因果链。检索任一节点自动带出完整历史轨迹。

```
memory_v1 --supersedes--> memory_v2 --supersedes--> memory_v3
```

解决的问题：向量搜索在偏好翻转场景失效（"喜欢甜食" → "控糖" 语义相似但含义相反），结构指针捕获这种"翻转"关系。

### A-MAC 5 维准入评分

决定"该不该存这条记忆"的结构化评分：

| 维度 | 计算方式 | 需要 LLM | 权重 |
|------|---------|---------|------|
| Utility | LLM 评分 (0-1) | ✅ | 0.35 |
| Confidence | 对话文本可信度 | ❌ 规则 | 0.20 |
| Novelty | 与已有 L1 的向量距离 | ❌ 规则 | 0.20 |
| Recency | 时间衰减函数 | ❌ 规则 | 0.10 |
| TypePrior | 类型权重（decision > fact） | ❌ 规则 | 0.15 |

阈值：`score > 0.6` → 存储。5 个维度中 4 个纯规则计算（<65ms），只有 Utility 需要一次 LLM 调用。

### 渐进披露检索

```
recall(query, token_budget=2000)
  ↓
1. L3 Persona    → 最多 200 token
2. L2 Scenarios  → 每块最多 300 token，总最多 600 token
3. L1 Atoms      → 每条最多 100 token，总最多 500 token
4. L0 Conversation → 每条最多 500 token，剩余预算
  ↓
超出预算时从低层截断（L0 优先被截断）
```

### 异步双路径

- **快路径**：毫秒级，用户说话就写 L0，不阻塞对话
- **慢路径**：后台异步，提取 L1→聚合 L2→生成 L3→抽象 L4→预测 L5

### 向量去重 + 冲突检测

- cosine similarity > 0.95 → 跳过（重复事实）
- 语义相似但内容矛盾 → 创建新版本 + supersedes 链

### 熔断降级

| 故障 | 降级策略 |
|------|---------|
| 连续 5 次失败 | 60 秒冷却期 |
| LLM API 不可用 | Lite 模式（L0 存储 + BM25/向量检索，零 LLM 调用） |
| 嵌入模型加载失败 | 纯 BM25 检索 |
| Gateway HTTP 超时 | 5 秒超时，跳过注入不阻塞对话 |

### 记忆图谱化

L1 Atom 入库时自动创建图谱关系：

- `[atom_42] --mentions--> [entity:Rust]`
- `[atom_42] --supersedes--> [atom_38]`
- `[atom_42] --from_session--> [session:abc-123]`
- `[atom_42] --related_to--> [atom_45]`（向量相似度高时）

Multi-hop 查询示例："关于 Rust 的所有决策" → 从 entity:Rust 出发 → mentions 关系 → 过滤 type=decision → supersedes 链只返回最新版本。

### 技能自动生成

1. 追踪 Agent 解决同类问题的历史路径
2. 3+ 次重复模式 → 触发 LLM 抽象
3. 生成 SOP Markdown（触发条件 + 步骤 + 成功率 + 使用次数）

---

## 三、Transport 层

| 模式 | 命令 | 协议 | 目标平台 |
|------|------|------|---------|
| MCP stdio | `asuna-memory serve` | JSON-RPC 2.0 | Cursor/Windsurf/Cline/任意 MCP 客户端 |
| HTTP Gateway | `asuna-memory gateway` | REST + JSON | Hermes / 任意 HTTP 客户端 |

两者复用同一个 Rust 核心（ToolHandler + MemoryEngine），只是 transport adapter 不同。

---

## 四、REST API（Gateway）

| Method | Path | 功能 |
|--------|------|------|
| POST | `/capture` | 捕获对话轮次（自动触发 L1 提取） |
| POST | `/recall` | 渐进披露召回（自动注入 system prompt） |
| POST | `/search` | 显式搜索（含 Evolution Chain 展开） |
| GET | `/persona` | 获取用户画像 |
| POST | `/offload` | 长文本 → Mermaid + node_ids |
| GET | `/recall/{node_id}` | 回溯 offload 原文 |
| POST | `/graph/assert` | 图谱断言 |
| POST | `/graph/neighbors` | 图谱 N-hop 查询 |
| POST | `/session/end` | 触发异步聚合管道 |
| GET | `/health` | 健康检查 |
| GET | `/stats` | 统计信息 |

---

## 五、MCP 工具

### 保留 v1.3.1 全部 15 个工具

| 工具 | 层 | 说明 |
|------|----|------|
| `save_session` | L0 | 保存对话 |
| `search_sessions` | L0-L3 | 多层搜索（增强：渐进披露） |
| `memory_write` | L1 | 手动写入记忆 |
| `memory_update` | L1 | 更新记忆（增强：Evolution Chain） |
| `memory_remove` | L1 | 删除记忆 |
| `memory_read` | L1-L3 | 读取记忆（增强：含画像和场景） |
| `user_profile` | L3 | 用户画像读写（升级为 L3 Persona） |
| `rebuild_index` | 索引 | 异步重建 |
| `rebuild_status` | 索引 | 重建进度 |
| `memory_provenance` | L1 | 溯源验证 |
| `graph_assert` | 图谱 | 三元组断言 |
| `graph_neighbors` | 图谱 | N-hop 查询 |
| `graph_path` | 图谱 | 最短路径 |
| `graph_link_entity` | 图谱 | 别名合并 |
| `graph_prune_dangling` | 图谱 | 清理悬空引用 |

---

## 六、CLI 命令

| 命令 | 说明 | 来源 |
|------|------|------|
| `serve` | 启动 MCP 服务器 | v1.0 |
| `gateway` | 启动 HTTP Gateway | P1 新增 |
| `doctor [--fix]` | 环境检查 + 自动修复 + 迁移状态 | v1.2 / v1.3.1 增强 |
| `model-download` | 下载嵌入模型 | v1.3.1 |
| `rebuild` | 重建索引 | v1.0 |
| `search` | 搜索对话 | v1.0 |
| `list-sessions` | 列出会话 | v1.0 |
| `list-profiles` | 列出 Profile | v1.0 |
| `import` | 导入 JSONL | v1.0 |
| `export` | 导出会话 | v1.0 |
| `delete-turn` | 安全删除 turn | v1.3.1 |
| `sql` | 只读 SQL 查询 | v1.3.1 |
| `export-memory` | 导出全部记忆为 JSON/Markdown | P3 新增 |
| `forget --topic/--session` | 删除特定主题/会话的所有记忆 | P3 新增 |

---

## 七、Hermes 集成

| 组件 | 说明 |
|------|------|
| **AMSMemoryProvider** | Hermes MemoryProvider 子类（Python ~200 行薄壳） |
| **GatewaySupervisor** | 管理 Rust Gateway 子进程（启动/watchdog/崩溃恢复） |
| **AMSClient** | HTTP client（调 Gateway REST API） |
| **自动召回** | `prefetch()` → 每轮对话前注入 `<memory-context>` |
| **自动捕获** | `sync_turn()` → 每轮对话后 fire-and-forget |
| **会话结束** | `on_session_end()` → 触发异步聚合管道 |
| **显式工具** | `ams_memory_search` / `ams_conversation_search` |
| **LLM 透传** | 从 Hermes 环境变量读取 LLM 配置（`OPENAI_BASE_URL`/`API_KEY`/`MODEL`） |
| **崩溃恢复** | watchdog 每 5 秒健康检查，崩溃后自动重启，从 SQLite WAL 恢复 |
| **异步任务持久化** | 提取任务写入 `pending_tasks` 表，重启后继续执行 |
| **Docker 一体化** | `docker run hermes-ams` 一条命令启动 |

### 零配置设计

- **Embedding**：本地 EmbeddingGemma ONNX（`model-download` 自动下载）
- **LLM 提取**：从 Hermes 环境变量透传，不需要用户配置
- **存储**：默认 `~/.asuna/`，自动创建
- **config.json**：完全可选，不存在时用 Default
- **Lite 模式**：`pipeline.enable_extraction: false` 或 LLM 不可用时自动降级

### 安装方式

通过 `for_ai.md` 引导 AI Agent 执行 `install.sh`（从 GitHub Release 下载 Python/YAML 文件 + 对应平台二进制），不需要 git clone 整个仓库。

---

## 八、检索引擎

| 组件 | 技术 |
|------|------|
| 全文检索 | FTS5 contentless + 中文 unigram（`tokenize_zh` UDF） |
| 向量检索 | sqlite-vec INT8 量化（768 维 EmbeddingGemma） |
| 融合算法 | Reciprocal Rank Fusion (RRF, k=60) |
| BM25 分词 | jieba（中文）/ 标准（英文），可配置 `bm25.language` |
| 渐进披露 | L3→L2→L1→L0 逐层，Token Budget 控制（默认 2000 token） |
| Evolution Chain | 检索结果自动展开 supersedes 链 |
| 图谱 re-ranking | L4/L5 匹配时提升 L1 结果权重（×1.5 / ×1.3） |

---

## 九、AI 管道

| 阶段 | 触发条件 | LLM 调用 | 延迟 |
|------|---------|---------|------|
| L1 提取 | 每 5 轮（`pipeline.every_n_turns`）或空闲 600 秒 | ✅ 事实提取 | 异步 |
| A-MAC 准入 | L1 提取时 | ✅ Utility 评分（其余 4 维规则） | <65ms(规则) + LLM |
| 向量去重 | L1 写入时 | ❌ cosine similarity | <10ms |
| 冲突检测 | L1 写入时 | ❌ 语义匹配 + 矛盾判定 | <20ms |
| L2 聚合 | L1 聚类触发（向量相似度 > 阈值） | ✅ 场景摘要 | 异步 |
| L3 画像 | 每 50 条新 L1（`persona.trigger_every_n`） | ✅ 画像生成 | 异步 |
| L4 心智 | 每天或每 100 条 L1 | ✅ 框架抽象 | 异步 |
| L5 意图 | L4 更新后 | ✅ 意图预测 | 异步 |
| 技能提取 | 3+ 次重复模式 | ✅ SOP 生成 | 异步 |

### LLM 成本估算

- 每次提取：~2000 token 输入 + 500 token 输出
- 频率：每 5 轮触发一次
- 日均（20 轮对话）：~4 次 LLM 调用 ≈ 10K token/天
- Lite 模式：0 LLM 调用

---

## 十、隐私与安全

| 功能 | 说明 |
|------|------|
| 隐私分级 | capture 时标记 `"privacy": "private"` → 只存 L0，不参与 L2-L6 聚合 |
| 选择性遗忘 | `forget --topic "xxx"` / `forget --session "id"` |
| 数据导出 | `export-memory` 导出全部记忆为 JSON/Markdown |
| 保留策略 | `privacy.l0_retention_days: 90`（L0 自动清理），L1+ 永久保留 |
| Profile 隔离 | 所有记忆按 `profile_id` 隔离在独立目录 |
| 安全扫描 | 写入前检测 prompt injection / 凭据泄露 / 不可见 Unicode |

---

## 十一、技术栈

| 组件 | 技术 | 说明 |
|------|------|------|
| 核心语言 | Rust (edition 2021) | 性能 + 安全 |
| HTTP 框架 | axum 0.7 + tower-http | CORS 支持 |
| 数据库 | SQLite WAL | 并发读写 + 崩溃恢复 |
| 向量索引 | sqlite-vec | INT8 量化，768 维 |
| 全文索引 | FTS5 contentless | 中文 unigram 分词 |
| 嵌入模型 | EmbeddingGemma 300M Q8 | ONNX Runtime，本地推理 |
| LLM 接口 | ureq (OpenAI-compatible) | 复用 Hermes LLM 配置 |
| CLI 框架 | clap 4 (derive) | 派生宏 |
| 日志 | tracing + tracing-subscriber | 结构化日志 |
| 序列化 | serde + serde_json | JSON 序列化 |
| 正则 | regex-lite | 安全扫描（轻量） |
| 时间 | chrono | ISO 8601 / Unix 毫秒 |
| Hermes 适配器 | Python 3.8+ | ~200 行薄壳 |
| 部署 | 单二进制 / Docker | 零外部依赖 |

---

## 十二、关键指标目标

| 指标 | 目标值 | 对标 |
|------|--------|------|
| LongMemEval | >80% | Hy-Memory 85.2% |
| PersonaMem | >70% | TencentDB 76% |
| 写入延迟（L0） | <5ms | TencentDB 12.3s/千token |
| 召回延迟 | <100ms | — |
| Token 节省 | 省 40-60%（vs 原文） | TencentDB 省 61% |
| 存储效率 | ~80 条/用户，~130 token/条 | Hy-Memory 同 |
| LLM 调用频率 | 每 5 轮 1 次，日均 ~10K token | — |
| 首次启动到可用 | <3 分钟（含模型下载） | — |
| Gateway 启动 | <50ms | Rust 单二进制 |
| 内存占用 | ~30MB + ~200MB（ONNX 加载后） | — |

---

## 十三、Phase 路线图

| Phase | 内容 | 工作量 | 累计 | 版本 |
|-------|------|--------|------|------|
| P1 | Transport 层重构 (MCP + HTTP Gateway) | 2 周 | 2 周 | v1.4.0-alpha |
| P2 | 短期记忆 (Context Offload + Mermaid) | 2 周 | 4 周 | |
| P3 | L0-L1 + Evolution Chain | 2 周 | 6 周 | **v1.4.0** |
| P4 | A-MAC 5 维准入评分 | 1 周 | 7 周 | v1.5.0-alpha |
| P5 | L2-L3 场景聚合 + 画像增强 | 2 周 | 9 周 | **v1.5.0** |
| P6 | L4-L5 心智模型 + 意图预测 | 2 周 | 11 周 | v1.6.0-alpha |
| P7 | 技能记忆 (SOP 自动生成) | 2 周 | 13 周 | |
| P8 | 记忆图谱化 | 2 周 | 15 周 | **v1.6.0** |
| P9 | Hermes MemoryProvider 插件 + Docker | 1 周 | 16 周 | **v2.0.0** |

**总计：16 周（约 4 个月）**

### 可并行 Phase

- P2（短期记忆）∥ P3（L0-L1）— 无依赖
- P10（已砍）原可与 P5-P8 并行

### 关键交付点

```
第 6 周 ──── v1.4.0 基础可用
              自动捕获 + 自动召回 + 短期压缩 + Evolution Chain

第 9 周 ──── v1.5.0 智能记忆
              A-MAC 准入 + 渐进披露检索

第 15 周 ─── v1.6.0 知识系统
              L4/L5 + SOP 生成 + 记忆图谱化

第 16 周 ─── v2.0.0 Hermes 深度集成
              MemoryProvider 插件 + Docker 一体化 + 零配置
```

---

## 十四、数据迁移策略

从 v1.3.1 升级到 v1.4.0+ 时：

| 现有数据 | 迁移方式 |
|----------|---------|
| JSONL 对话 | 不变，自动识别为 L0 |
| MEMORY.md/USER.md 条目 | 自动标记 `memory_type='manual'` |
| SQLite bounded_memory | ALTER TABLE 新增列（全部有 DEFAULT） |
| 图谱 entities/relations | 保持不变，P8 时新增 `memory_atom_id` 列 |
| 嵌入向量 | 不变，vec_turns 表兼容 |

`asuna-memory doctor` 检测并报告迁移状态，`doctor --fix` 执行迁移。

---

## 十五、配置参考

所有配置项完全可选，不存在 config.json 时使用默认值。

```jsonc
{
  "data_dir": "~/.asuna",
  "profile_id": "default",

  // 嵌入（本地模型，零配置）
  "embedding": {
    "model_name": "embeddinggemma-300m-q8",
    "dimensions": 768,
    "batch_size": 32
  },

  // LLM 提取（从环境变量透传，通常不需要手动配置）
  "llm": {
    "base_url": "",       // env: AMS_LLM_BASE_URL
    "api_key": "",        // env: AMS_LLM_API_KEY
    "model": "deepseek-v3" // env: AMS_LLM_MODEL
  },

  // 提取管道
  "pipeline": {
    "enable_extraction": true,
    "every_n_turns": 5,
    "idle_timeout_seconds": 600,
    "l2_min_interval_seconds": 900,
    "enable_warmup": true
  },

  // A-MAC 准入
  "admission": {
    "enabled": true,
    "threshold": 0.6,
    "weights": [0.35, 0.20, 0.20, 0.10, 0.15]
  },

  // 召回
  "recall": {
    "strategy": "hybrid",
    "max_results": 5,
    "token_budget": 2000,
    "timeout_ms": 5000
  },

  // 画像
  "persona": {
    "trigger_every_n": 50
  },

  // 隐私
  "privacy": {
    "l0_retention_days": 90,
    "l1_retention_days": 0,
    "auto_cleanup": true
  },

  // 图谱
  "graph": {
    "enabled": true,
    "remind_on_save": true
  },

  // Gateway
  "gateway": {
    "port": 0,
    "auth_key": null
  }
}
```
