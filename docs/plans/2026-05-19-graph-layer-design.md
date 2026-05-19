# v1.3.0 设计文档 — 图谱记忆层（Graph Memory Layer）

**日期**：2026-05-19
**目标版本**：v1.3.0
**作者**：brainstorming 会话产物，待 writing-plans 转化为实现计划

**版本变更说明**：原计划基于 Kuzu 内嵌图数据库；Kuzu 项目已于 2025-10-10 归档，**改用 SQLite 表方案**（rusqlite，零新依赖）。其余设计要点保持不变。

---

## 总览

v1.3.0 给 Asuna 加一个图谱记忆层，**复用现有 SQLite 数据库**新增两张表（`entities` + `relations`）。事实层（`sessions` / `turns` / FTS / vec）和成长层（Markdown）一字不动。agent 是图谱内容的唯一作者——server 端不会调 LLM 也不做规则抽取。如果 agent 不主动断言图，图就是空的；server 只通过 `save_session` 返回里的 `graph_pending` 字段轻度提示 agent 去补。

### 为什么是 SQLite 表方案

- **零新依赖**：Asuna 已经在用 rusqlite。永不会被第三方库归档拖死。
- **二进制零增长**：相比 Kuzu (+10MB)，体积不变。
- **跨平台稳定**：4 个 release target 全部已验证稳定。
- **schema 完全可读**：用户可以 `sqlite3 memory.db` 直接看图层数据。
- **取舍**：
  - 失去 Cypher 查询语言（砍掉 `graph_query` MCP 工具）
  - 多跳路径查询用递归 CTE 实现（~10 万边内可接受）
  - 失去自动属性图理论模型（但对 agent 实际使用无影响）

### 三层正交

```text
┌─────────────────────────────────────────────────────────────┐
│  MCP Server (stdio · JSON-RPC 2.0)                          │
├──────────────┬──────────────────────┬───────────────────────┤
│ 成长层 / MD   │ 事实层 / SQLite      │ 图谱层 / SQLite (新)   │
│              │                      │                       │
│ MEMORY.md    │ sessions + turns     │ entities (节点)        │
│ USER.md      │ turns_fts (FTS5)     │ relations (边)         │
│ 安全扫描      │ vec_turns (int8 768) │ source_turn → turns.id │
│ 溯源追踪      │ bounded_memory       │ canonical 主键        │
└──────────────┴──────────────────────┴───────────────────────┘
                          ↑ 不动 ↑          ↑ 新建表 ↑
```

### 关键不变量

- 事实层永远是真理之源。turns 是图层 `source_turn` 外键的指向终点。
- 图层失败 / 为空时，所有现有 MCP 工具行为不变。
- `entities` 和 `relations` 表都用 `CREATE TABLE IF NOT EXISTS`：v1.2.1 数据库自动升级，零迁移。
- 图谱**不参与** rebuild。rebuild 只重建 SQLite 索引；图谱由 agent 累积。

### 数据流（写）

```text
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
        SQLite tx：INSERT OR IGNORE entities + INSERT OR IGNORE relations + UPDATE confidence
```

### 数据流（读）

```text
agent → search_sessions(query=...)            ← 现有，不变（vec + FTS）

agent → graph_neighbors(entity, rel?, hops?)  ← 新工具：SQL JOIN
agent → graph_path(src, dst, max_hops?)        ← 新工具：递归 CTE
```

读路径**不自动联动**：search_sessions 不会自动喂图谱种子。留作 v1.4 增量。

---

## SQLite Schema

两张新表加到现有 `memory.db`（`src/index/schema.rs::SCHEMA_SQL`）：

```sql
-- ════════════════════════════════════════════════
-- 图谱实体表 (entities)
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS entities (
    canonical    TEXT    PRIMARY KEY,            -- lowercase+trim+折空白
    name         TEXT    NOT NULL,               -- 原始字面（首次写入版本）
    entity_type  TEXT    NOT NULL DEFAULT 'unknown',
    first_seen   INTEGER NOT NULL,               -- unix ms
    last_seen    INTEGER NOT NULL,
    source_turn  INTEGER                         -- 软外键 → turns(id)
);
CREATE INDEX IF NOT EXISTS idx_entities_type ON entities(entity_type);

-- ════════════════════════════════════════════════
-- 图谱关系表 (relations)
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS relations (
    src_canonical TEXT    NOT NULL REFERENCES entities(canonical) ON DELETE CASCADE,
    rel_type      TEXT    NOT NULL,
    dst_canonical TEXT    NOT NULL REFERENCES entities(canonical) ON DELETE CASCADE,
    confidence    REAL    NOT NULL DEFAULT 0.5,
    source_turn   INTEGER,                       -- 软外键 → turns(id)
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (src_canonical, rel_type, dst_canonical)
);
CREATE INDEX IF NOT EXISTS idx_relations_dst ON relations(dst_canonical, rel_type);
CREATE INDEX IF NOT EXISTS idx_relations_src_turn ON relations(source_turn);
```

### Schema 要点

1. **canonical 是 PK** — 字符串主键。INSERT OR IGNORE 语义天然幂等。
2. **`entity_type` 默认 `'unknown'`** — agent 不强制填类型；server 不做枚举校验。
3. **relations 复合 PK** — `(src, rel_type, dst)` 三元组级别唯一；重复 INSERT 会被 IGNORE，confidence 通过单独 UPDATE 取 max。
4. **`source_turn` 不加 FK 约束** — turns 表存在但不强制引用有效性（agent 可能引用不存在的 turn_id；doctor 报告悬空）。
5. **ON DELETE CASCADE** — 删除 entity 时所有出入边自动清理。仅在 `graph_link_entity` 内部使用。
6. **没有 embedding 字段** — 纯字符串。实体向量化留 v1.4。
7. **没有 `updated_at`** — 仅 `created_at`。重复 assert 不刷新时间戳，仅刷新 confidence。

### 初始化与迁移

- v1.3.0 binary 首次连上 v1.2.1 数据库：`init_schema()` 跑 `CREATE TABLE IF NOT EXISTS` → 空表自动建好。
- 零破坏：旧 v1.2.1 binary 能继续读 v1.3.0 数据库（只是看不到新表）。
- 不需要 `rebuild`。

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

## 4 个 MCP 工具

| 工具 | 用途 |
|---|---|
| `graph_assert` | 批量写三元组 |
| `graph_neighbors` | 查 N-hop 邻居 |
| `graph_path` | 两节点最短路径 |
| `graph_link_entity` | 别名合并 |

**对比原 Kuzu 计划，砍掉了 `graph_query`** —— SQLite 无 Cypher 引擎，agent 想要复杂查询应回到 `graph_neighbors` / `graph_path` 的参数化版本。如果未来发现这是个瓶颈，v1.4 可考虑加 `graph_sql`（只读 SQL 子查询）。

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

- 全部三元组在单个 SQLite 事务内提交，任意失败 ROLLBACK
- canonical 化两端节点
- entities：`INSERT OR IGNORE`（不存在则创建，存在则保留 `entity_type` 首次写入版本，仅刷新 `last_seen`）
- relations：`INSERT OR IGNORE` + 单独 `UPDATE confidence = MAX(confidence, ?)`

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

实现：

- 1-hop = 单次 JOIN
- 2-hop+ = 递归 CTE
- `direction=both` = UNION 入边 + 出边

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
- 实现：递归 CTE，BFS 风格找最短路径（首次命中即返回）

返回：

```json
{
  "status": "ok",
  "found": true,
  "length": 2,
  "path": [
    {"canonical": "alice", "name": "Alice"},
    {"rel_type": "friend_of"},
    {"canonical": "bob", "name": "Bob"},
    {"rel_type": "works_at"},
    {"canonical": "openai", "name": "OpenAI"}
  ]
}
```

未找到 → `{"status": "ok", "found": false}`。

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

实现（单事务）：

1. 把所有 `relations.src_canonical = $from` 改为 `$to`
2. 把所有 `relations.dst_canonical = $from` 改为 `$to`
3. 合并新产生的重复行（`INSERT OR IGNORE` + DELETE old）
4. `DELETE FROM entities WHERE canonical = $from`（CASCADE 自动清理任何剩余边）

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

- 实现：写完 turns 后用一条 SQL 查 "本 session turn_ids 中，没有任何 `relations.source_turn` 命中的"
- 单 SELECT，~1ms 级
- 关闭时该字段不出现

---

## 错误处理与降级

### 启动期降级矩阵

| 故障 | 行为 |
|---|---|
| `graph.enabled = false` | 跳过 schema 中的 entities/relations 表创建；图工具返回 `"graph disabled in config"` |
| schema 创建失败（极罕见） | warn! + 图工具返回 `"graph schema unavailable: <error>"`；其余正常 |
| 磁盘满 | 写失败；读仍工作 |

注意：**SQLite 表方案天然不会"加载失败"** —— rusqlite 已是 Asuna 现有依赖，不存在"找不到库"的可能。所以降级路径比 Kuzu 简单。

### 运行时错误

| 错误 | 返回 |
|---|---|
| 参数缺失 / 类型错 / 越界 | `isError: true` + 字段名 |
| `confidence` 不在 [0,1] | `"confidence must be in [0.0, 1.0]"` |
| `hops` 越界 | `"hops must be in 1..=5"` |
| 行数截断（neighbors 超 limit） | 正常返回，截到 limit |
| SQLite 内部错误 | 透传 + `isError: true` |

---

## `doctor` 命令扩展

默认输出新增 2 行：

```text
图谱: OK (47 entities, 89 relations)
图谱状态: ENABLED
```

降级时：

```text
图谱: DISABLED (config.graph.enabled = false)
```

`doctor --verbose` 额外显示：

```text
图谱覆盖率: 73% (64/87 turns 至少被 1 条 relation 引用)
图谱悬空引用: 0 (relations.source_turn 全部命中 turns 表)
```

---

## 测试策略

### 单元测试

| 文件 | 覆盖点 |
|---|---|
| `src/graph/canonical.rs` | `canonicalize()` 表驱动 |
| `src/graph/store.rs` | INSERT OR IGNORE 去重、confidence MAX 合并 |
| `src/graph/query.rs` | neighbors / path 的边界、CTE 正确性 |

### 集成测试（`src/graph/tests.rs`）

10 个场景：

1. `graph_assert` 写入 3 三元组 → 节点 / 边数正确
2. 同 src/rel/dst 重复 assert → 仅 confidence 更新
3. canonical 归一化：`Alice` / `alice` / ` Alice ` 同一节点
4. `graph_neighbors` 1-hop vs 2-hop 行为差异
5. `graph_neighbors` direction filter（out/in/both）
6. `graph_path` found vs not-found
7. `graph_path` 找到的长度等于实际最短
8. `graph_link_entity` 重定向 N 条边 + 删除旧节点 + 不留悬空
9. `pending_turn_ids` 正确返回未被引用的子集
10. `graph.enabled = false` 时所有图工具返回 disabled

### e2e 测试（`src/graph/e2e_test.rs`）

- save_session → 检查 `graph_pending`
- graph_assert → graph_pending 缩减
- 全 turn 被引用后 graph_pending 消失
- rebuild + graph 数据保留（rebuild 不动图）

### 测试隔离

复用现有 `Db::open_memory()` —— 每个测试一个独立内存数据库。

### 性能预算

| 操作 | 预算 |
|---|---|
| save_session 增量（graph_pending） | < 5ms |
| graph_assert（10 triples） | < 10ms |
| graph_neighbors（1-hop, limit 50） | < 5ms |
| graph_neighbors（2-hop） | < 20ms |
| graph_path（max_hops 5） | < 100ms |
| Kuzu 启动开销 | **0**（复用现有连接） |

超预算 50%+ → `tracing::warn!`。**v1.3.0 不写 perf 测试**。

---

## 文件改动清单

**新增 (5)**：

```text
src/graph/mod.rs
src/graph/canonical.rs    — canonicalize() function
src/graph/store.rs        — entities + relations CRUD
src/graph/query.rs        — neighbors + path with CTE
src/graph/tests.rs        — unit + integration tests
src/graph/e2e_test.rs     — end-to-end save_session + graph flow
```

**修改 (5)**：

```text
src/main.rs                — mod graph; + doctor 输出
src/mcp/tools.rs           — +4 工具
src/fact/session_store.rs  — 仍保持图层无关
src/index/schema.rs        — 加 entities + relations 两张表
src/config.rs              — GraphConfig
```

**文档 (3)**：

```text
README.md / README_EN.md   — 架构表 + 图谱小节 + 升级指南
for_ai.md                  — 4 工具签名 + 用例 + 不变量
```

**总规模**：~500 行 Rust + ~150 行 Markdown + ~300 行测试 ≈ **950 行**（比 Kuzu 方案少 ~30%）。

---

## 任务表（5 Phase · ~3 工作日）

| Phase | 任务 | 工时 | 验收 |
|---|---|---|---|
| **P1 · 骨架** | 加 schema（entities + relations）、GraphConfig、`src/graph/mod.rs` + canonicalize、doctor 显示统计 | 0.5 d | `cargo check`+`cargo test graph` 通过 |
| **P2 · 写入** | `graph_assert`（含 MERGE 逻辑）、单测+集成测 1–3 | 0.5 d | 10 triples 写入 ≤ 10ms |
| **P3 · 读取** | `graph_neighbors`（含递归 CTE）+ `graph_path`、集成测 4–7 | 1 d | 1-hop ≤ 5ms，path ≤ 100ms |
| **P4 · 修正+联动** | `graph_link_entity`、`save_session.graph_pending`、config wiring、4 个 MCP 工具暴露、集成测 8–10 | 0.5 d | save→assert 联动正确 |
| **P5 · 收尾** | doctor `--verbose`、README/for_ai 文档、bump v1.3.0、e2e 全跑、release tag | 0.5 d | `cargo test` 全绿；release tag |

每 Phase 末尾 commit 一次。

---

## 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 递归 CTE 在大图上慢 | 中 | 低 | 限定 `max_hops ≤ 10`；v1.4 加索引 |
| 复合 PK 写性能 | 低 | 低 | SQLite 复合 PK 已优化；写量级小 |
| canonical 化 unicode 边界 | 低 | 低 | 表驱动测试覆盖中英混合 |
| agent 不用图谱 = 死功能 | 中 | 高 | 软提示 + doctor 可见性；不用就 v1.4 砍 |
| FK CASCADE 误删 | 低 | 中 | CASCADE 仅用于 `graph_link_entity`；不影响 turns |

**比 Kuzu 方案少的风险**：

- ✅ 不再有"Kuzu 编译失败"
- ✅ 不再有"ARM64 不兼容"
- ✅ 不再有"unsafe transmute lifetime"
- ✅ 不再有"二进制 +10MB"

---

## 不在 v1.3.0 中（明确不做）

- 实体向量化 / 自动同义合并 → v1.4
- 规则抽取 fallback → 永久砍
- 图谱覆盖率阈值警告（档 2）→ 仅 `doctor --verbose` 显示统计
- 强制双写（档 3）→ 永久不做
- Cypher / 自由查询语言 → v1.4 可考虑 `graph_sql`（只读 SQL）
- `rebuild_index` 重建图谱 → 不做。图谱的真理之源是 agent 的累积断言
- 跨 profile 共享图谱 → 不做，与 profile 一对一隔离
- search_sessions 自动联动图谱（hybrid 三路融合）→ v1.4

---

## 下一步

按 brainstorming 流程：

1. 本文档 commit 到 `docs/plans/2026-05-19-graph-layer-design.md`
2. invoke `writing-plans` skill 生成可执行 implementation plan
3. 用户确认 plan 后开始 P1
