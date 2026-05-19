# v1.3.0 设计文档 — 图谱记忆层（Graph Memory Layer）

**日期**：2026-05-19
**目标版本**：v1.3.0
**作者**：brainstorming 会话产物，待 writing-plans 转化为实现计划

---

## 总览

v1.3.0 给 Asuna 加一个独立的图谱记忆层，由内嵌 [Kuzu](https://kuzudb.com/) 提供完整 Cypher 查询能力。事实层（SQLite）和成长层（Markdown）**一字不动**。agent 是图谱内容的唯一作者——server 端不会调 LLM 也不做规则抽取。如果 agent 不主动断言图，图就是空的；server 只通过 `save_session` 返回里的 `graph_pending` 字段轻度提示 agent 去补。

### 三层正交

```
┌─────────────────────────────────────────────────────────────┐
│  MCP Server (stdio · JSON-RPC 2.0)                          │
├──────────────┬──────────────────────┬───────────────────────┤
│ 成长层 / MD   │ 事实层 / SQLite      │ 图谱层 / Kuzu (新)     │
│              │                      │                       │
│ MEMORY.md    │ sessions + turns     │ Entity (节点)         │
│ USER.md      │ turns_fts (FTS5)     │ Relation (边)         │
│ 安全扫描      │ vec_turns (int8 768) │ Cypher 查询           │
│ 溯源追踪      │ bounded_memory       │ source_turn → turns.id│
└──────────────┴──────────────────────┴───────────────────────┘
                          ↑ 不动 ↑          ↑ 新建 ↑
```

### 关键不变量

- 事实层永远是真理之源。turns 是图层 `source_turn` 字段的指向终点。
- 图层失败 / 为空时，所有现有 MCP 工具行为不变。
- 删除 `~/.asuna/profiles/<id>/graph.kuzu/` 目录 = 完全回到 v1.2.1 行为。
- 图谱**不参与** rebuild。rebuild 只重建 SQLite 索引；图谱由 agent 累积。

### 数据流（写）

```
agent → save_session(turns)
        ↓
        SQLite tx (sessions/turns/FTS/vec)
        ↓
        JSONL 落盘
        ↓
        响应 + graph_pending: [turn_ids]   ← 新增字段（配置可关）

agent → graph_assert(triples=[...], source_turn=42)  ← 新工具
        ↓
        canonical 归一化（lowercase + trim + 折空白）
        ↓
        Kuzu tx：MERGE Entity nodes + MERGE Relation edges
```

### 数据流（读）

```
agent → search_sessions(query=...)            ← 现有，不变（vec + FTS）

agent → graph_neighbors(entity, rel?, hops?)  ← 新工具
agent → graph_path(src, dst, max_hops?)        ← 新工具
agent → graph_query(cypher)                    ← 新工具（受限只读）
```

读路径**不自动联动**：search_sessions 不会自动喂图谱种子。留作 v1.4 增量。

---

## Kuzu Schema

数据库目录：`~/.asuna/profiles/<id>/graph.kuzu/`（与 `memory.db` 同级）。

```cypher
-- 实体节点表
CREATE NODE TABLE Entity(
    canonical    STRING PRIMARY KEY,        -- lowercase+trim+折空白
    name         STRING,                    -- 原始字面（首次写入版本）
    entity_type  STRING DEFAULT 'unknown',  -- 'person'|'org'|'concept'|...
    first_seen   TIMESTAMP,
    last_seen    TIMESTAMP,
    source_turn  INT64                      -- 软外键 → SQLite turns.id
);

-- 关系边表
CREATE REL TABLE Relation(
    FROM Entity TO Entity,
    rel_type     STRING,
    confidence   DOUBLE DEFAULT 0.5,
    source_turn  INT64,
    created_at   TIMESTAMP
);
```

### Schema 要点

1. **canonical 是主键** — Kuzu 允许字符串 PK，MERGE 语义天然幂等。
2. **`entity_type` 默认 `'unknown'`** — agent 不强制填类型；server 不做枚举校验（KISS）。
3. **Relation 没有 PK** — 用 MERGE 语义去重；同三元组重复写入更新 confidence（取 max）。
4. **`source_turn` 软外键** — Kuzu 无跨库 FK。`doctor --verbose` 报告悬空引用。
5. **没有 embedding** — canonical 归一化纯字符串。实体向量化留 v1.4。
6. **没有 `updated_at`** — 仅 `created_at`。

### 初始化与迁移

- 首次启动：`Db::open` 后调用 `GraphDb::open_or_init`，目录不存在则建空 + 跑 `CREATE NODE/REL TABLE IF NOT EXISTS`。
- 失败不致命：Kuzu 加载失败时 warn! 日志，server 继续，`graph_*` 工具返回 `"graph backend unavailable"`。
- v1.2.1 → v1.3.0 升级零迁移：老用户首次启动自动建空 `graph.kuzu/`。
- 旧 v1.2.1 binary 能继续读 v1.3.0 数据目录（忽略 `graph.kuzu/`）。

### 配置（新增）

```jsonc
{
  "graph": {
    "enabled": true,            // 总开关
    "remind_on_save": true      // save_session 是否返回 graph_pending
  }
}
```

`graph` 整体缺省 = 默认值。档 2/档 3 开关 v1.3.0 不暴露。

---

## 5 个 MCP 工具

| 工具 | 用途 |
|---|---|
| `graph_assert` | 批量写三元组 |
| `graph_neighbors` | 查 N-hop 邻居 |
| `graph_path` | 两节点最短路径 |
| `graph_query` | 直接跑受限 Cypher |
| `graph_link_entity` | 别名合并 |

### `graph_assert`

```json
{
  "name": "graph_assert",
  "arguments": {
    "triples": [
      {
        "src": "Alice Smith",
        "rel": "works_at",
        "dst": "OpenAI",
        "src_type": "person",
        "dst_type": "org",
        "confidence": 0.9,
        "source_turn": 42
      }
    ],
    "session_id": "uuid"
  }
}
```

- 全部三元组在单个 Kuzu 事务内提交，任意失败全部回滚
- canonical 化两端节点
- MERGE Entity 不改 type（首次 winner）
- MERGE Relation：同三元组重复 → confidence 取 max

返回：

```json
{
  "status": "ok",
  "entities_created": 3,
  "entities_updated": 1,
  "relations_created": 2,
  "relations_updated": 0
}
```

### `graph_neighbors`

```json
{
  "name": "graph_neighbors",
  "arguments": {
    "entity": "Alice Smith",
    "rel_type": "works_at",
    "direction": "out",
    "hops": 1,
    "limit": 50
  }
}
```

- `direction` ∈ `out` / `in` / `both`（默认 `both`）
- `hops` ∈ 1..=5（默认 1）
- `limit` 默认 50，max 200

返回：

```json
{
  "status": "ok",
  "neighbors": [
    {"canonical": "openai", "name": "OpenAI", "type": "org", "distance": 1}
  ]
}
```

### `graph_path`

```json
{
  "name": "graph_path",
  "arguments": {
    "src": "Alice",
    "dst": "OpenAI",
    "max_hops": 5
  }
}
```

- `max_hops` ∈ 1..=10（默认 5）

返回：

```json
{
  "status": "ok",
  "found": true,
  "length": 2,
  "path": [
    {"canonical": "alice", "name": "Alice"},
    {"rel_type": "friend_of", "direction": "out"},
    {"canonical": "bob", "name": "Bob"},
    {"rel_type": "works_at", "direction": "out"},
    {"canonical": "openai", "name": "OpenAI"}
  ]
}
```

未找到 → `{"status": "ok", "found": false}`。

### `graph_query`

```json
{
  "name": "graph_query",
  "arguments": {
    "cypher": "MATCH (a:Entity)-[r:Relation]-(b:Entity) WHERE a.canonical = 'alice' RETURN b.name, r.rel_type",
    "params": {}
  }
}
```

**只读限制**：禁止下列关键字（分词后纯字母 uppercase 比对）：

```rust
const FORBIDDEN_KEYWORDS: &[&str] = &[
    "CREATE", "MERGE", "DELETE", "DETACH",
    "SET", "REMOVE", "DROP", "COPY", "ATTACH",
    "ALTER", "LOAD", "INSTALL", "CALL",
];
```

**超时 5s，返回行数上限 1000（超出截断）**。

返回：

```json
{
  "status": "ok",
  "columns": ["b.name", "r.rel_type"],
  "rows": [["Bob", "friend_of"]],
  "truncated": false
}
```

### `graph_link_entity`

```json
{
  "name": "graph_link_entity",
  "arguments": {
    "from": "alice",
    "to": "alice smith",
    "session_id": "uuid"
  }
}
```

- 把所有指向 / 从 `from` 出发的边重定向到 `to`
- 删除 `from` 节点
- 不可逆（审计日志记录）

返回：

```json
{
  "status": "ok",
  "edges_rewired": 7,
  "old_entity_removed": "alice"
}
```

### `save_session` 副作用变更

返回值新增 `graph_pending` 字段（受 `graph.remind_on_save` 控制）：

```json
{
  "status": "ok",
  "session_id": "...",
  "turns_saved": 5,
  "graph_pending": {
    "turn_ids": [101, 102, 103, 104, 105],
    "hint": "These turns have no graph assertions yet. Call graph_assert with extracted triples (subject, relation, object) and source_turn=<id> to enable relationship queries."
  }
}
```

- 实现：写完 turns 后用 Kuzu 反查 "本 session turn_ids 中没有 Relation.source_turn 命中"
- 一次 Kuzu 查询，~1ms 级
- 关闭时该字段不出现

---

## 错误处理与降级

### 启动期降级矩阵

| 故障 | 行为 |
|---|---|
| Kuzu 库加载失败 | warn! + 图工具返回 `"graph backend unavailable"`；其余正常 |
| `graph.enabled = false` | 跳过 Kuzu 初始化；图工具返回 `"graph disabled in config"` |
| `graph.kuzu/` 损坏 | warn! + 同上 |
| 磁盘满 | 写失败；读仍工作 |

### 运行时错误

| 错误 | 返回 |
|---|---|
| 参数缺失 / 类型错 / 越界 | `isError: true` + 字段名 |
| Cypher 写关键字 | `"forbidden keyword in cypher: CREATE"` |
| Cypher 超时 | `"query timeout exceeded"` |
| 行数截断 | 正常返回 + `truncated: true` |
| Kuzu 内部错误 | 透传 + `isError: true` |

---

## `doctor` 命令扩展

默认输出新增 2 行：

```
图谱: OK (47 entities, 89 relations)
图谱目录: /home/user/.asuna/profiles/default/graph.kuzu
```

降级时：

```
图谱: DISABLED (config.graph.enabled = false)
图谱: UNAVAILABLE (Kuzu 加载失败: <error>)
```

`doctor --verbose` 额外显示：

```
图谱覆盖率: 73% (64/87 turns 至少被 1 条 relation 引用)
图谱悬空引用: 0 (Relation.source_turn 全部命中 turns 表)
```

---

## 测试策略

### 单元测试

| 文件 | 覆盖点 |
|---|---|
| `src/graph/entity.rs` | `canonicalize()` 表驱动 |
| `src/graph/db.rs` | `open_or_init`、降级路径 |
| `src/graph/relation.rs` | MERGE 去重、confidence 取 max |
| `src/graph/query.rs` | 黑名单 / 超时 / 截断 |

### 集成测试（`src/graph/tests.rs`）

10 个场景：

1. `graph_assert` 写入 3 三元组 → 节点 / 边数正确
2. 同 src/rel/dst 重复 assert → 仅 confidence 更新
3. canonical 归一化：`Alice` / `alice` / ` Alice ` 同一节点
4. `graph_neighbors` 1-hop vs 2-hop 行为差异
5. `graph_path` found vs not-found
6. `graph_query` 合法查询正确返回
7. `graph_query` 写关键字 → forbidden
8. `graph_query` >1000 行 → `truncated: true`
9. `graph_link_entity` 重定向 N 条边 + 删除旧节点
10. `graph.enabled = false` 时所有图工具返回 disabled

### e2e 测试（`src/graph/e2e_test.rs`）

- save_session → 检查 `graph_pending`
- graph_assert → graph_pending 缩减
- 全 turn 被引用后 graph_pending 消失
- rebuild + graph 数据保留（rebuild 不动图）

### Kuzu 测试隔离

`tempfile::tempdir()` 每个测试独立目录；测试结束显式 `drop(db)` 后清理。

### 性能预算

| 操作 | 预算 |
|---|---|
| save_session 增量（graph_pending） | < 5ms |
| graph_assert（10 triples） | < 20ms |
| graph_neighbors（1-hop, limit 50） | < 10ms |
| graph_path（max_hops 5） | < 50ms |
| graph_query（中等复杂度） | < 100ms |
| Kuzu 启动开销 | < 100ms |

超预算 50%+ → `tracing::warn!`。**v1.3.0 不写 perf 测试**。

---

## 文件改动清单

**新增 (7)**：

```
src/graph/mod.rs
src/graph/db.rs
src/graph/entity.rs
src/graph/relation.rs
src/graph/query.rs
src/graph/tests.rs
src/graph/e2e_test.rs
```

**修改 (5)**：

```
Cargo.toml                 — + kuzu (locked minor version)
src/main.rs                — mod graph; + doctor 输出
src/mcp/tools.rs           — +5 工具
src/fact/session_store.rs  — graph_pending 字段
src/config.rs              — GraphConfig
```

**文档 (4)**：

```
README.md / README_EN.md   — 架构表 + 图谱小节 + 升级指南
for_ai.md                  — 5 工具签名 + 用例 + 不变量
```

**总规模**：~800 行 Rust + ~200 行 Markdown + ~400 行测试 ≈ **1400 行**。

---

## 任务表（5 Phase · ~4 工作日）

| Phase | 任务 | 工时 | 验收 |
|---|---|---|---|
| **P1 · 骨架** | 加 kuzu 依赖、`src/graph/mod.rs` 骨架、`GraphDb::open_or_init` + 降级、db 启停单测 | 0.5 d | `cargo check`+`cargo test graph::db` 通过 |
| **P2 · 写入** | `canonicalize()`、`graph_assert`、单测+集成测 1–3 | 1.0 d | 10 triples 写入 ≤ 20ms |
| **P3 · 读取** | `graph_neighbors` / `graph_path` / `graph_query`（黑名单+超时+截断）、集成测 4–8 | 1.0 d | 1-hop ≤ 10ms |
| **P4 · 修正+联动** | `graph_link_entity`、`save_session.graph_pending`、config wiring、集成测 9–10 | 0.5 d | save→assert 联动正确 |
| **P5 · 收尾** | doctor `--verbose`、README/for_ai 文档、bump v1.3.0、e2e 全跑 | 1.0 d | `cargo test` 全绿；release tag |

每 Phase 末尾 commit 一次。

---

## 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| Kuzu 在 ARM64 Linux 编译失败 | 中 | 高 | P1 本地 cross-compile 验证 |
| Kuzu 0.x API breaking | 中 | 中 | Cargo.toml 锁 `kuzu = "=0.x.y"` |
| Cypher 黑名单被绕过 | 低 | 中 | agent 是受信方；v1.3.0 不担保完美 |
| 二进制体积 +10MB | 高 | 低 | README 注明 |
| Windows 上文件锁未释放 | 中 | 低 | 测试结束显式 drop |
| 大图上 `graph_pending` 慢 | 低 | 低 | < 1万 turns 用户 OK |
| 黑名单误伤合法查询 | 低 | 低 | v1.3.1 升 quote-aware |
| Windows 上 Kuzu 路径问题 | 中 | 中 | P1 本地 Windows 验证 |
| agent 不用图谱 = 死功能 | 中 | 高 | 软提示 + doctor 可见性；不用就 v1.4 砍 |

---

## 不在 v1.3.0 中（明确不做）

- 实体向量化 / 自动同义合并 → v1.4
- 规则抽取 fallback → 永久砍
- 图谱覆盖率阈值警告（档 2）→ 仅 `doctor --verbose` 显示统计
- 强制双写（档 3）→ 永久不做
- 图算法 MCP 工具 → 通过 `graph_query` 调用，不主动暴露
- `rebuild_index` 重建图谱 → 不做。图谱的真理之源是 agent 的累积断言
- 跨 profile 共享图谱 → 不做，与 profile 一对一隔离
- 图层备份/导出工具 → `cp -r graph.kuzu/` 即备份
- search_sessions 自动联动图谱（hybrid 三路融合）→ v1.4

---

## 验收清单（release 前）

- [ ] `cargo test` 100% 通过（新增 ~15 个 graph 测试）
- [ ] `cargo clippy --all-targets -- -D warnings` 干净
- [ ] `cargo build --release` 4 平台全成功（Win/Linux x64/Linux ARM64/macOS）
- [ ] v1.2.1 数据目录被 v1.3.0 打开 → 自动建空图谱目录
- [ ] 删除 `graph.kuzu/` 后 v1.3.0 启动 → 自动重建
- [ ] `graph.enabled = false` → 图工具返回友好错误
- [ ] README / for_ai 示例 JSON 可拷贝可用
- [ ] doctor 输出图谱状态（OK / DISABLED / UNAVAILABLE）
- [ ] release workflow 4 artifact 全产出

---

## 下一步

按 brainstorming 流程：

1. 本文档 commit 到 `docs/plans/2026-05-19-graph-layer-design.md`
2. invoke `writing-plans` skill 生成可执行 implementation plan
3. 用户确认 plan 后开始 P1
