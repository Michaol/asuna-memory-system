# v1.3.0 Graph Memory Layer Implementation Plan (SQLite Backend)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a graph memory layer using two new SQLite tables (`entities` + `relations`), with 4 new MCP tools and soft hints, without touching the fact/growth layers.

**Architecture:** New `src/graph/` module wraps two tables in the existing `memory.db`. canonical normalization (lowercase + trim + whitespace fold) is the only entity-identity logic. agent is the sole author of triples. No new dependencies — reuses existing `rusqlite` connection.

**Tech Stack:**

- Rust 2021 (existing toolchain)
- `rusqlite` (existing — schema additions only)
- `serde_json` for tool payloads
- `tempfile` (existing dev-dep)

**Design doc:** `docs/plans/2026-05-19-graph-layer-design.md`

**Note on backend choice:** Original plan used embedded Kuzu graph DB. Kuzu was archived 2025-10-10. We pivoted to SQLite tables: zero new deps, no archive risk, same use-cases via JOIN and recursive CTE. Lost Cypher language (so no `graph_query` free-form tool); kept the other 4 tools and all other design decisions.

---

## Phase 1 (P1) · Skeleton + Schema

Goal: New tables `entities` + `relations` live in `memory.db`. `GraphConfig` plumbed. `src/graph/` module exposes `canonicalize()` + a `Graph` wrapper around `&Db`. doctor shows row counts.

### Task 1.1: Schema additions

**Files:**

- Modify: `src/index/schema.rs`

**Step 1: Append two new tables to `SCHEMA_SQL`**

Add to the end of the `SCHEMA_SQL` constant (before the closing `"#`):

```sql
-- ════════════════════════════════════════════════
-- 图谱实体表 (entities) — v1.3.0
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS entities (
    canonical    TEXT    PRIMARY KEY,
    name         TEXT    NOT NULL,
    entity_type  TEXT    NOT NULL DEFAULT 'unknown',
    first_seen   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    source_turn  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_entities_type ON entities(entity_type);

-- ════════════════════════════════════════════════
-- 图谱关系表 (relations) — v1.3.0
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS relations (
    src_canonical TEXT    NOT NULL REFERENCES entities(canonical) ON DELETE CASCADE,
    rel_type      TEXT    NOT NULL,
    dst_canonical TEXT    NOT NULL REFERENCES entities(canonical) ON DELETE CASCADE,
    confidence    REAL    NOT NULL DEFAULT 0.5,
    source_turn   INTEGER,
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (src_canonical, rel_type, dst_canonical)
);
CREATE INDEX IF NOT EXISTS idx_relations_dst ON relations(dst_canonical, rel_type);
CREATE INDEX IF NOT EXISTS idx_relations_src_turn ON relations(source_turn);
```

**Step 2: Verify**

Run: `cargo test`
Expected: all 51 existing tests pass (new tables exist but no code touches them).

**Step 3: Commit**

```bash
git add src/index/schema.rs
git commit -m "feat(graph): add entities + relations tables (v1.3.0 schema)"
```

### Task 1.2: GraphConfig + graph_enabled helper

**Files:**

- Modify: `src/config.rs`

**Step 1: Add `GraphConfig` struct**

After `EmbeddingConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphConfig {
    pub enabled: bool,
    pub remind_on_save: bool,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            remind_on_save: true,
        }
    }
}
```

**Step 2: Add `graph` field to `Config`**

In `pub struct Config`, after `pub embedding: EmbeddingConfig,`:

```rust
    #[serde(default)]
    pub graph: GraphConfig,
```

**Step 3: Add `graph` to `impl Default for Config`**

After `embedding: EmbeddingConfig { ... },`:

```rust
            graph: GraphConfig::default(),
```

**Step 4: Verify**

Run: `cargo check`
Expected: clean build.

**Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add GraphConfig (enabled + remind_on_save)"
```

### Task 1.3: src/graph/mod.rs skeleton + canonicalize()

**Files:**

- Create: `src/graph/mod.rs`
- Create: `src/graph/canonical.rs`
- Modify: `src/main.rs` (`mod graph;`)

**Step 1: Write failing tests for canonicalize**

Create `src/graph/canonical.rs`:

```rust
//! Entity identity normalization for the graph layer.
//!
//! canonical 化字符串：lowercase + trim + 把连续空白折叠为单个空格。
//! 这是 v1.3.0 唯一的实体身份逻辑。
//! agent 是图谱内容的唯一作者；server 不做 fuzzy 匹配或语义合并。

pub fn canonicalize(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonicalize_basic_lowercase() {
        assert_eq!(canonicalize("Alice"), "alice");
        assert_eq!(canonicalize("ALICE SMITH"), "alice smith");
    }

    #[test]
    fn test_canonicalize_trims_and_folds_whitespace() {
        assert_eq!(canonicalize("  Alice  "), "alice");
        assert_eq!(canonicalize("Alice   Smith"), "alice smith");
        assert_eq!(canonicalize("Alice\tSmith"), "alice smith");
        assert_eq!(canonicalize("Alice\nSmith"), "alice smith");
    }

    #[test]
    fn test_canonicalize_chinese_passthrough() {
        // 中文不受 lowercase 影响；中文之间无空白时不补空白
        assert_eq!(canonicalize("亚丝娜"), "亚丝娜");
        assert_eq!(canonicalize("亚 丝 娜"), "亚 丝 娜");
        assert_eq!(canonicalize("亚丝娜  "), "亚丝娜");
    }

    #[test]
    fn test_canonicalize_empty() {
        assert_eq!(canonicalize(""), "");
        assert_eq!(canonicalize("   "), "");
        assert_eq!(canonicalize("\t\n"), "");
    }

    #[test]
    fn test_canonicalize_mixed() {
        assert_eq!(canonicalize("OpenAI Inc"), "openai inc");
        assert_eq!(canonicalize("Project / Asuna"), "project / asuna");
    }
}
```

**Step 2: Create module entry**

Create `src/graph/mod.rs`:

```rust
//! 图谱记忆层 (Graph Memory Layer)
//!
//! 三层正交架构中的第三层。事实层（SQLite sessions/turns）和成长层（Markdown）一字不动。
//! 图谱层用同一个 SQLite 数据库新增 `entities` + `relations` 两张表。
//!
//! agent 是图谱内容的唯一作者；server 不调 LLM 也不做规则抽取。

pub mod canonical;

pub use canonical::canonicalize;
```

**Step 3: Register in main**

In `src/main.rs`, after `mod fact;`:

```rust
mod graph;
```

**Step 4: Run tests**

Run: `cargo test graph::canonical`
Expected: 5 tests pass.

**Step 5: Run full suite**

Run: `cargo test`
Expected: 51 + 5 = 56 pass.

**Step 6: Commit**

```bash
git add src/graph/ src/main.rs
git commit -m "feat(graph): add src/graph/ module + canonicalize() with tests"
```

### Task 1.4: doctor shows entities/relations counts

**Files:**

- Modify: `src/main.rs`

**Step 1: Add SELECT COUNT queries to `cmd_doctor`**

After the existing `索引统计` line, add:

```rust
    let entity_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap_or(0);
    let relation_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap_or(0);
    let graph_status = if config.graph.enabled {
        format!("ENABLED ({} entities, {} relations)", entity_count, relation_count)
    } else {
        "DISABLED (config.graph.enabled = false)".to_string()
    };
    println!("图谱: {}", graph_status);
```

**Step 2: Manual smoke test**

Run: `cargo run --quiet -- doctor 2>&1 | grep 图谱`
Expected: `图谱: ENABLED (0 entities, 0 relations)` (in a fresh profile).

**Step 3: Verify tests**

Run: `cargo test`
Expected: 56 pass (no change to test surface).

**Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(graph): doctor shows entities/relations counts + ENABLED/DISABLED status"
```

**Phase 1 verification gate:**

- `cargo check` clean
- `cargo test` 56+/56+ pass
- `cargo run -- doctor` shows `图谱: ENABLED (0 entities, 0 relations)`
- Setting `graph.enabled = false` in config makes doctor show `图谱: DISABLED (...)`

---

## Phase 2 (P2) · Write Path

Goal: `Graph::assert_triples()` writes to `entities` + `relations` with proper MERGE semantics. Returns counts of created vs updated.

### Task 2.1: Triple types + assert_triples implementation

**Files:**

- Create: `src/graph/store.rs`
- Modify: `src/graph/mod.rs`

**Step 1: Define inputs/outputs**

Create `src/graph/store.rs`:

```rust
//! 图谱写路径：MERGE 三元组到 entities + relations。

use crate::graph::canonical::canonicalize;
use crate::index::db::Db;
use crate::util::time;
use serde::{Deserialize, Serialize};

/// 一条要断言的三元组（agent 传入）
#[derive(Debug, Clone, Deserialize)]
pub struct TripleInput {
    pub src: String,
    pub rel: String,
    pub dst: String,
    #[serde(default)]
    pub src_type: Option<String>,
    #[serde(default)]
    pub dst_type: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub source_turn: Option<i64>,
}

/// 写入完成后的统计
#[derive(Debug, Default, Serialize)]
pub struct AssertStats {
    pub entities_created: u32,
    pub entities_updated: u32,
    pub relations_created: u32,
    pub relations_updated: u32,
}

/// 图谱写入：在单个 SQLite 事务内执行
pub fn assert_triples(db: &Db, triples: &[TripleInput]) -> anyhow::Result<AssertStats> {
    if triples.is_empty() {
        anyhow::bail!("triples must be non-empty");
    }
    for (i, t) in triples.iter().enumerate() {
        if t.src.trim().is_empty() {
            anyhow::bail!("triple[{}].src is empty", i);
        }
        if t.rel.trim().is_empty() {
            anyhow::bail!("triple[{}].rel is empty", i);
        }
        if t.dst.trim().is_empty() {
            anyhow::bail!("triple[{}].dst is empty", i);
        }
        if let Some(c) = t.confidence {
            if !(0.0..=1.0).contains(&c) {
                anyhow::bail!("triple[{}].confidence={} out of range [0.0, 1.0]", i, c);
            }
        }
    }

    let conn = db.conn();
    let mut stats = AssertStats::default();
    let now = time::now_unix_ms();

    conn.execute_batch("BEGIN IMMEDIATE")?;

    let result: anyhow::Result<()> = (|| {
        for t in triples {
            let src_canon = canonicalize(&t.src);
            let dst_canon = canonicalize(&t.dst);
            if src_canon.is_empty() || dst_canon.is_empty() {
                anyhow::bail!("triple resolves to empty canonical after normalization");
            }

            let conf = t.confidence.unwrap_or(0.5);
            let src_type = t.src_type.as_deref().unwrap_or("unknown");
            let dst_type = t.dst_type.as_deref().unwrap_or("unknown");
            let source_turn = t.source_turn;

            // MERGE src entity
            let src_existed = entity_exists(conn, &src_canon)?;
            upsert_entity(conn, &src_canon, &t.src, src_type, source_turn, now, src_existed)?;
            if src_existed {
                stats.entities_updated += 1;
            } else {
                stats.entities_created += 1;
            }

            // MERGE dst entity (skip double-counting when src == dst)
            if dst_canon != src_canon {
                let dst_existed = entity_exists(conn, &dst_canon)?;
                upsert_entity(conn, &dst_canon, &t.dst, dst_type, source_turn, now, dst_existed)?;
                if dst_existed {
                    stats.entities_updated += 1;
                } else {
                    stats.entities_created += 1;
                }
            }

            // MERGE relation
            let rel_existed = relation_exists(conn, &src_canon, &t.rel, &dst_canon)?;
            upsert_relation(
                conn, &src_canon, &t.rel, &dst_canon, conf, source_turn, now, rel_existed,
            )?;
            if rel_existed {
                stats.relations_updated += 1;
            } else {
                stats.relations_created += 1;
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(stats)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn entity_exists(conn: &rusqlite::Connection, canonical: &str) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entities WHERE canonical = ?1",
        rusqlite::params![canonical],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

fn upsert_entity(
    conn: &rusqlite::Connection,
    canonical: &str,
    name: &str,
    entity_type: &str,
    source_turn: Option<i64>,
    now: i64,
    existed: bool,
) -> anyhow::Result<()> {
    if existed {
        // 仅刷新 last_seen；name / entity_type / source_turn 保留首次写入版本
        conn.execute(
            "UPDATE entities SET last_seen = ?1 WHERE canonical = ?2",
            rusqlite::params![now, canonical],
        )?;
    } else {
        conn.execute(
            "INSERT INTO entities
             (canonical, name, entity_type, first_seen, last_seen, source_turn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![canonical, name, entity_type, now, now, source_turn],
        )?;
    }
    Ok(())
}

fn relation_exists(
    conn: &rusqlite::Connection,
    src: &str,
    rel_type: &str,
    dst: &str,
) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM relations
         WHERE src_canonical = ?1 AND rel_type = ?2 AND dst_canonical = ?3",
        rusqlite::params![src, rel_type, dst],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

fn upsert_relation(
    conn: &rusqlite::Connection,
    src: &str,
    rel_type: &str,
    dst: &str,
    confidence: f64,
    source_turn: Option<i64>,
    now: i64,
    existed: bool,
) -> anyhow::Result<()> {
    if existed {
        // confidence 取 max；source_turn 不覆盖（首次写入 winner）
        conn.execute(
            "UPDATE relations
             SET confidence = MAX(confidence, ?1)
             WHERE src_canonical = ?2 AND rel_type = ?3 AND dst_canonical = ?4",
            rusqlite::params![confidence, src, rel_type, dst],
        )?;
    } else {
        conn.execute(
            "INSERT INTO relations
             (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![src, rel_type, dst, confidence, source_turn, now],
        )?;
    }
    Ok(())
}
```

**Step 2: Register module**

In `src/graph/mod.rs`:

```rust
pub mod store;
pub use store::{assert_triples, AssertStats, TripleInput};
```

**Step 3: Write integration tests**

Append to `src/graph/mod.rs`:

```rust
#[cfg(test)]
mod tests;
```

Create `src/graph/tests.rs`:

```rust
use crate::graph::{assert_triples, TripleInput};
use crate::index::db::Db;

fn fresh_db() -> Db {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db
}

fn t(src: &str, rel: &str, dst: &str) -> TripleInput {
    TripleInput {
        src: src.to_string(),
        rel: rel.to_string(),
        dst: dst.to_string(),
        src_type: None,
        dst_type: None,
        confidence: None,
        source_turn: None,
    }
}

#[test]
fn test_assert_basic_triples() {
    let db = fresh_db();
    let triples = vec![
        TripleInput {
            src: "Alice".to_string(),
            rel: "works_at".to_string(),
            dst: "OpenAI".to_string(),
            src_type: Some("person".to_string()),
            dst_type: Some("org".to_string()),
            confidence: Some(0.9),
            source_turn: Some(42),
        },
        t("Alice", "friend_of", "Bob"),
    ];
    let stats = assert_triples(&db, &triples).unwrap();
    assert_eq!(stats.entities_created, 3); // Alice, OpenAI, Bob
    assert_eq!(stats.entities_updated, 1); // Alice (second triple)
    assert_eq!(stats.relations_created, 2);
}

#[test]
fn test_assert_dedup_same_triple_canonical_insensitive() {
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "works_at", "OpenAI")]).unwrap();
    // Re-assert with different casing — must canonicalize to same
    let triples = vec![TripleInput {
        src: "alice".to_string(),
        rel: "works_at".to_string(),
        dst: "openai".to_string(),
        src_type: None,
        dst_type: None,
        confidence: Some(0.95),
        source_turn: None,
    }];
    let stats = assert_triples(&db, &triples).unwrap();
    assert_eq!(stats.entities_created, 0);
    assert_eq!(stats.relations_created, 0);
    assert_eq!(stats.relations_updated, 1);

    // Confidence should be MAX(0.5, 0.95) = 0.95
    let conf: f64 = db
        .conn()
        .query_row(
            "SELECT confidence FROM relations WHERE src_canonical='alice' AND rel_type='works_at' AND dst_canonical='openai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!((conf - 0.95).abs() < 1e-6, "confidence should be MAX, got {}", conf);
}

#[test]
fn test_assert_empty_rejected() {
    let db = fresh_db();
    assert!(assert_triples(&db, &[]).is_err());
}

#[test]
fn test_assert_invalid_confidence_rejected() {
    let db = fresh_db();
    let mut bad = t("a", "r", "b");
    bad.confidence = Some(1.5);
    assert!(assert_triples(&db, &[bad]).is_err());
}

#[test]
fn test_assert_invalid_empty_field_rejected() {
    let db = fresh_db();
    assert!(assert_triples(&db, &[t("", "r", "b")]).is_err());
    assert!(assert_triples(&db, &[t("a", "", "b")]).is_err());
    assert!(assert_triples(&db, &[t("a", "r", "")]).is_err());
}

#[test]
fn test_assert_transactional_rollback() {
    let db = fresh_db();
    // First triple is valid, second is invalid (out-of-range confidence)
    let triples = vec![
        t("Alice", "knows", "Bob"),
        TripleInput {
            src: "Carol".to_string(),
            rel: "knows".to_string(),
            dst: "Dave".to_string(),
            src_type: None,
            dst_type: None,
            confidence: Some(2.0), // invalid
            source_turn: None,
        },
    ];
    let result = assert_triples(&db, &triples);
    assert!(result.is_err());
    // Nothing should have been written (validation runs before transaction starts,
    // so technically rollback isn't even needed here — but verify state regardless)
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}
```

**Step 4: Run tests**

Run: `cargo test graph::`
Expected: 5 canonical tests + 6 assert tests = 11 pass.

**Step 5: Run full suite**

Run: `cargo test`
Expected: 51 baseline + 11 graph = 62 pass.

**Step 6: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): assert_triples — MERGE entities/relations + transactional + tests"
```

### Task 2.2: Performance budget sanity test

**Files:**

- Modify: `src/graph/tests.rs`

**Step 1: Add timing test**

Append:

```rust
#[test]
fn test_assert_performance_10_triples() {
    let db = fresh_db();
    let triples: Vec<TripleInput> = (0..10)
        .map(|i| TripleInput {
            src: format!("entity_{}", i),
            rel: "rel_test".to_string(),
            dst: format!("entity_{}", i + 100),
            src_type: None,
            dst_type: None,
            confidence: None,
            source_turn: Some(i as i64),
        })
        .collect();

    let start = std::time::Instant::now();
    assert_triples(&db, &triples).unwrap();
    let elapsed = start.elapsed();

    println!("10 triples write: {:?}", elapsed);
    // Budget: 10ms. Hard-fail at 50ms to catch real regressions.
    assert!(elapsed.as_millis() < 50, "10 triples took {:?}, over 10ms budget", elapsed);
}
```

**Step 2: Run**

Run: `cargo test graph::tests::test_assert_performance -- --nocapture`
Expected: PASS, prints timing under 50ms.

**Step 3: Commit**

```bash
git add src/graph/tests.rs
git commit -m "test(graph): perf budget for assert_triples (10 triples < 50ms)"
```

**Phase 2 verification gate:**

- 62+ tests pass
- 10-triples write < 50ms
- `cargo clippy --all-targets -- -D warnings` clean

---

## Phase 3 (P3) · Read Path

Goal: `graph_neighbors` (with hops via recursive CTE) and `graph_path` (shortest via BFS CTE) work and are correctly tested.

### Task 3.1: graph_neighbors

**Files:**

- Create: `src/graph/query.rs`
- Modify: `src/graph/mod.rs`

**Step 1: Write failing tests**

Append to `src/graph/tests.rs`:

```rust
use crate::graph::query::{neighbors, Direction, NeighborQuery};

#[test]
fn test_neighbors_1hop_out() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("Alice", "works_at", "OpenAI"),
        t("Alice", "friend_of", "Bob"),
    ]).unwrap();

    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    let canonicals: Vec<_> = result.iter().map(|n| n.canonical.as_str()).collect();
    assert!(canonicals.contains(&"openai"));
    assert!(canonicals.contains(&"bob"));
    assert_eq!(result.len(), 2);
    for n in &result {
        assert_eq!(n.distance, 1);
    }
}

#[test]
fn test_neighbors_filtered_by_rel_type() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("Alice", "works_at", "OpenAI"),
        t("Alice", "friend_of", "Bob"),
    ]).unwrap();

    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: Some("works_at".to_string()),
        direction: Direction::Out,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].canonical, "openai");
}

#[test]
fn test_neighbors_direction_in() {
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "works_at", "OpenAI")]).unwrap();
    let q = NeighborQuery {
        entity: "OpenAI".to_string(),
        rel_type: None,
        direction: Direction::In,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].canonical, "alice");
}

#[test]
fn test_neighbors_direction_both() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("Alice", "knows", "Bob"),
        t("Carol", "knows", "Alice"),
    ]).unwrap();
    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Both,
        hops: 1,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    let canonicals: Vec<_> = result.iter().map(|n| n.canonical.as_str()).collect();
    assert!(canonicals.contains(&"bob"));
    assert!(canonicals.contains(&"carol"));
}

#[test]
fn test_neighbors_2hop() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("Alice", "knows", "Bob"),
        t("Bob", "knows", "Carol"),
    ]).unwrap();
    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 2,
        limit: 50,
    };
    let result = neighbors(&db, &q).unwrap();
    let canonicals: Vec<_> = result.iter().map(|n| n.canonical.as_str()).collect();
    assert!(canonicals.contains(&"bob"));
    assert!(canonicals.contains(&"carol"));
}

#[test]
fn test_neighbors_invalid_hops() {
    let db = fresh_db();
    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 6,
        limit: 50,
    };
    assert!(neighbors(&db, &q).is_err());
}
```

**Step 2: Verify failure**

Run: `cargo test graph::tests::test_neighbors`
Expected: FAIL (module not yet implemented).

**Step 3: Implement**

Create `src/graph/query.rs`:

```rust
//! 图谱读路径：邻居 + 路径（基于 SQL JOIN / 递归 CTE）。

use crate::graph::canonical::canonicalize;
use crate::index::db::Db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Out,
    In,
    Both,
}

impl Default for Direction {
    fn default() -> Self {
        Direction::Both
    }
}

#[derive(Debug, Deserialize)]
pub struct NeighborQuery {
    pub entity: String,
    #[serde(default)]
    pub rel_type: Option<String>,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default = "default_hops")]
    pub hops: u32,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_hops() -> u32 { 1 }
fn default_limit() -> u32 { 50 }

#[derive(Debug, Serialize)]
pub struct Neighbor {
    pub canonical: String,
    pub name: String,
    pub entity_type: String,
    pub distance: u32,
}

const MAX_HOPS: u32 = 5;
const MAX_LIMIT: u32 = 200;

pub fn neighbors(db: &Db, q: &NeighborQuery) -> anyhow::Result<Vec<Neighbor>> {
    if !(1..=MAX_HOPS).contains(&q.hops) {
        anyhow::bail!("hops must be in 1..={}, got {}", MAX_HOPS, q.hops);
    }
    let limit = q.limit.min(MAX_LIMIT).max(1);
    let canon = canonicalize(&q.entity);
    if canon.is_empty() {
        return Ok(Vec::new());
    }

    // Use a recursive CTE for variable-hop expansion. Direction determines
    // whether we follow src->dst, dst<-src, or both.
    //
    // The CTE:
    //   - Anchor: the seed entity at distance 0
    //   - Recursive step: walk one edge respecting direction filter
    //   - Stop when distance >= hops
    //
    // Then we filter out the seed itself and return DISTINCT neighbors ordered by distance.

    let direction_clause = match q.direction {
        Direction::Out => "
            JOIN relations r ON r.src_canonical = visited.canonical
            JOIN entities e ON e.canonical = r.dst_canonical
        ",
        Direction::In => "
            JOIN relations r ON r.dst_canonical = visited.canonical
            JOIN entities e ON e.canonical = r.src_canonical
        ",
        Direction::Both => "
            JOIN relations r
              ON r.src_canonical = visited.canonical
              OR r.dst_canonical = visited.canonical
            JOIN entities e ON e.canonical = CASE
                WHEN r.src_canonical = visited.canonical THEN r.dst_canonical
                ELSE r.src_canonical
            END
        ",
    };

    let rel_filter = if q.rel_type.is_some() {
        " AND r.rel_type = ?"
    } else {
        ""
    };

    let sql = format!(
        "WITH RECURSIVE visited(canonical, distance) AS (
            SELECT ?, 0
            UNION
            SELECT e.canonical, visited.distance + 1
            FROM visited
            {direction_clause}
            WHERE visited.distance < ?
              {rel_filter}
        )
        SELECT DISTINCT v.canonical, e.name, e.entity_type, v.distance
        FROM visited v
        JOIN entities e ON e.canonical = v.canonical
        WHERE v.distance > 0
        ORDER BY v.distance, v.canonical
        LIMIT ?"
    );

    let conn = db.conn();
    let mut stmt = conn.prepare(&sql)?;

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(canon.clone()),
        Box::new(q.hops as i64),
    ];
    if let Some(rt) = &q.rel_type {
        params.push(Box::new(rt.clone()));
    }
    params.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(Neighbor {
            canonical: row.get(0)?,
            name: row.get(1)?,
            entity_type: row.get(2)?,
            distance: row.get::<_, i64>(3)? as u32,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
```

**Step 4: Register module**

In `src/graph/mod.rs`:

```rust
pub mod query;
```

**Step 5: Run tests**

Run: `cargo test graph::tests::test_neighbors`
Expected: 6 tests pass.

If 2-hop test fails, debug the recursive CTE — most likely cause is the `Direction::Both` JOIN expression being too clever. Simplify by doing 3 separate UNION'd CTEs (out / in / both as out+in).

**Step 6: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): neighbors() with N-hop recursive CTE + direction filter"
```

### Task 3.2: graph_path (shortest path)

**Files:**

- Modify: `src/graph/query.rs`

**Step 1: Write failing tests**

In `src/graph/tests.rs`:

```rust
use crate::graph::query::{path, PathResult};

#[test]
fn test_path_direct() {
    let db = fresh_db();
    assert_triples(&db, &[t("Alice", "knows", "Bob")]).unwrap();
    let p = path(&db, "Alice", "Bob", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 1);
}

#[test]
fn test_path_2hop() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("Alice", "knows", "Bob"),
        t("Bob", "works_at", "OpenAI"),
    ]).unwrap();
    let p = path(&db, "Alice", "OpenAI", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 2);
}

#[test]
fn test_path_not_found() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("Alice", "knows", "Bob"),
        t("Carol", "knows", "Dave"),
    ]).unwrap();
    let p = path(&db, "Alice", "Dave", 5).unwrap();
    assert!(!p.found);
}

#[test]
fn test_path_respects_max_hops() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("a", "r", "b"),
        t("b", "r", "c"),
        t("c", "r", "d"),
    ]).unwrap();
    // Path a->d is length 3
    let p = path(&db, "a", "d", 2).unwrap();
    assert!(!p.found, "should not find path within max_hops=2");
    let p = path(&db, "a", "d", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 3);
}

#[test]
fn test_path_invalid_max_hops() {
    let db = fresh_db();
    assert!(path(&db, "a", "b", 0).is_err());
    assert!(path(&db, "a", "b", 11).is_err());
}
```

**Step 2: Verify failure**

Run: `cargo test graph::tests::test_path`
Expected: FAIL.

**Step 3: Implement**

Add to `src/graph/query.rs`:

```rust
#[derive(Debug, Serialize)]
pub struct PathResult {
    pub found: bool,
    pub length: u32,
    pub path: Vec<PathStep>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PathStep {
    Entity { canonical: String, name: String },
    Edge { rel_type: String },
}

const MAX_PATH_HOPS: u32 = 10;

/// 在两节点间寻找最短路径（无向遍历）。
///
/// 使用 BFS 风格的递归 CTE，按 distance 升序枚举从 src 可达的节点，找到 dst 即停。
/// 返回 length；路径节点的完整序列由 v1.3.0 的 `path` 字段以空 Vec 返回——
/// 详细路径序列化是 v1.3.1 的优化项。`found`/`length` 已足够支撑 agent 使用场景。
pub fn path(db: &Db, src: &str, dst: &str, max_hops: u32) -> anyhow::Result<PathResult> {
    if !(1..=MAX_PATH_HOPS).contains(&max_hops) {
        anyhow::bail!("max_hops must be in 1..={}, got {}", MAX_PATH_HOPS, max_hops);
    }
    let src_c = canonicalize(src);
    let dst_c = canonicalize(dst);
    if src_c.is_empty() || dst_c.is_empty() || src_c == dst_c {
        return Ok(PathResult {
            found: src_c == dst_c && !src_c.is_empty(),
            length: 0,
            path: Vec::new(),
        });
    }

    let sql = "
        WITH RECURSIVE bfs(node, distance) AS (
            SELECT ?, 0
            UNION
            SELECT
                CASE
                    WHEN r.src_canonical = bfs.node THEN r.dst_canonical
                    ELSE r.src_canonical
                END,
                bfs.distance + 1
            FROM bfs
            JOIN relations r
              ON r.src_canonical = bfs.node OR r.dst_canonical = bfs.node
            WHERE bfs.distance < ?
        )
        SELECT MIN(distance) FROM bfs WHERE node = ?
    ";

    let conn = db.conn();
    let row: Option<i64> = conn
        .query_row(
            sql,
            rusqlite::params![src_c, max_hops as i64, dst_c],
            |r| r.get(0),
        )
        .ok();

    match row {
        Some(len) if len > 0 => Ok(PathResult {
            found: true,
            length: len as u32,
            path: Vec::new(),
        }),
        _ => Ok(PathResult {
            found: false,
            length: 0,
            path: Vec::new(),
        }),
    }
}
```

**Step 4: Run tests**

Run: `cargo test graph::tests::test_path`
Expected: 5 tests pass.

**Step 5: Run full**

Run: `cargo test`
Expected: 62 + 6 (neighbors) + 5 (path) = 73 pass.

**Step 6: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): path() via BFS recursive CTE (length only; path body v1.3.1)"
```

**Phase 3 verification gate:**

- 73+ tests pass
- `cargo clippy` clean
- 1-hop neighbors < 5ms (informal check)

---

## Phase 4 (P4) · Link Entity + MCP Tools + save_session hint

Goal: All 4 graph_* tools exposed. `save_session` returns `graph_pending`.

### Task 4.1: link_entity

**Files:**

- Modify: `src/graph/store.rs`

**Step 1: Write failing test**

In `src/graph/tests.rs`:

```rust
use crate::graph::store::link_entity;

#[test]
fn test_link_entity_rewires_outgoing() {
    let db = fresh_db();
    assert_triples(&db, &[
        t("Alice", "works_at", "OpenAI"),
        t("Alice", "knows", "Bob"),
    ]).unwrap();

    let rewired = link_entity(&db, "Alice", "Alice Smith").unwrap();
    assert!(rewired >= 2);

    // alice gone
    let n: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM entities WHERE canonical='alice'", [], |r| r.get(0)
    ).unwrap();
    assert_eq!(n, 0);

    // alice smith now has edges
    let n: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM relations WHERE src_canonical='alice smith'", [], |r| r.get(0)
    ).unwrap();
    assert_eq!(n, 2);
}

#[test]
fn test_link_entity_rewires_incoming() {
    let db = fresh_db();
    assert_triples(&db, &[t("Bob", "knows", "Alice")]).unwrap();
    link_entity(&db, "Alice", "Alice Smith").unwrap();
    let n: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM relations WHERE dst_canonical='alice smith'", [], |r| r.get(0)
    ).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn test_link_entity_merges_duplicates() {
    let db = fresh_db();
    // Both alice and alice_smith already have edge to OpenAI
    assert_triples(&db, &[
        t("Alice", "works_at", "OpenAI"),
        t("Alice Smith", "works_at", "OpenAI"),
    ]).unwrap();
    link_entity(&db, "Alice", "Alice Smith").unwrap();
    // Only one edge should remain (the duplicate gets merged)
    let n: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM relations WHERE src_canonical='alice smith' AND dst_canonical='openai'",
        [], |r| r.get(0)
    ).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn test_link_entity_same_canonical_rejected() {
    let db = fresh_db();
    assert!(link_entity(&db, "Alice", "alice").is_err());
}
```

**Step 2: Implement**

Add to `src/graph/store.rs`:

```rust
/// 把 `from` 实体的所有边重定向到 `to`，然后删除 `from` 节点。
/// 单事务；如果产生重复边则保留 `to` 侧（IGNORE 重复 INSERT）。
/// 返回重定向的边数。
pub fn link_entity(db: &Db, from: &str, to: &str) -> anyhow::Result<u32> {
    let from_c = canonicalize(from);
    let to_c = canonicalize(to);
    if from_c.is_empty() || to_c.is_empty() {
        anyhow::bail!("from/to canonicalize to empty");
    }
    if from_c == to_c {
        anyhow::bail!("from and to canonicalize to the same value: '{}'", from_c);
    }

    let conn = db.conn();
    let now = time::now_unix_ms();

    conn.execute_batch("BEGIN IMMEDIATE")?;

    let result: anyhow::Result<u32> = (|| {
        // Ensure target entity exists
        let to_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entities WHERE canonical = ?1",
            rusqlite::params![to_c],
            |r| r.get(0),
        )?;
        if to_exists == 0 {
            conn.execute(
                "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen, source_turn)
                 VALUES (?1, ?2, 'unknown', ?3, ?3, NULL)",
                rusqlite::params![to_c, to, now],
            )?;
        }

        // Count edges that will be rewired (before we touch anything)
        let edge_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM relations WHERE src_canonical = ?1 OR dst_canonical = ?1",
            rusqlite::params![from_c],
            |r| r.get(0),
        )?;

        // Step 1: copy out-edges (from -> X) to (to -> X), with INSERT OR IGNORE to merge duplicates
        conn.execute(
            "INSERT OR IGNORE INTO relations
             (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
             SELECT ?1, rel_type, dst_canonical, confidence, source_turn, created_at
             FROM relations WHERE src_canonical = ?2 AND dst_canonical <> ?1",
            rusqlite::params![to_c, from_c],
        )?;

        // Step 2: copy in-edges (X -> from) to (X -> to), with INSERT OR IGNORE
        conn.execute(
            "INSERT OR IGNORE INTO relations
             (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
             SELECT src_canonical, rel_type, ?1, confidence, source_turn, created_at
             FROM relations WHERE dst_canonical = ?2 AND src_canonical <> ?1",
            rusqlite::params![to_c, from_c],
        )?;

        // Step 3: delete the `from` entity — CASCADE will clean up its remaining edges
        conn.execute(
            "DELETE FROM entities WHERE canonical = ?1",
            rusqlite::params![from_c],
        )?;

        Ok(edge_count as u32)
    })();

    match result {
        Ok(n) => {
            conn.execute_batch("COMMIT")?;
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
```

**Step 3: Update `src/graph/mod.rs` exports**

```rust
pub use store::{assert_triples, link_entity, AssertStats, TripleInput};
```

**Step 4: Run tests**

Run: `cargo test graph::tests::test_link_entity`
Expected: 4 tests pass.

**Note on FK CASCADE:** Make sure `PRAGMA foreign_keys = ON` is set in `Db::open` and `Db::open_memory` — it is (v1.2.1 enabled it). Verify in test that CASCADE actually fires.

**Step 5: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): link_entity rewires edges + CASCADE deletes old + transactional"
```

### Task 4.2: pending_turn_ids helper

**Files:**

- Create or modify: `src/graph/query.rs`

**Step 1: Write failing test**

In `src/graph/tests.rs`:

```rust
use crate::graph::query::pending_turn_ids;

#[test]
fn test_pending_turn_ids_filters_referenced() {
    let db = fresh_db();
    // Need a turn record to satisfy FK on source_turn... actually source_turn has no FK
    // (we made it soft) so we can use arbitrary turn ids.
    let triples = vec![TripleInput {
        src: "a".to_string(),
        rel: "x".to_string(),
        dst: "b".to_string(),
        src_type: None,
        dst_type: None,
        confidence: None,
        source_turn: Some(10),
    }];
    assert_triples(&db, &triples).unwrap();

    let pending = pending_turn_ids(&db, &[10, 20, 30]).unwrap();
    assert_eq!(pending, vec![20, 30]);
}

#[test]
fn test_pending_turn_ids_empty_input() {
    let db = fresh_db();
    assert!(pending_turn_ids(&db, &[]).unwrap().is_empty());
}

#[test]
fn test_pending_turn_ids_no_relations() {
    let db = fresh_db();
    let pending = pending_turn_ids(&db, &[1, 2, 3]).unwrap();
    assert_eq!(pending, vec![1, 2, 3]);
}
```

**Step 2: Implement**

Add to `src/graph/query.rs`:

```rust
/// 返回 `turn_ids` 中未被任何 `relations.source_turn` 引用的子集。
/// `save_session` 用此构造 `graph_pending.turn_ids`。
pub fn pending_turn_ids(db: &Db, turn_ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    if turn_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Build IN (?, ?, ?) placeholders
    let placeholders: String = (0..turn_ids.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT DISTINCT source_turn FROM relations
         WHERE source_turn IN ({}) AND source_turn IS NOT NULL",
        placeholders
    );

    let conn = db.conn();
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = turn_ids.iter().map(|t| t as &dyn rusqlite::ToSql).collect();
    let referenced: std::collections::HashSet<i64> = stmt
        .query_map(params.as_slice(), |row| row.get(0))?
        .collect::<Result<_, _>>()?;

    Ok(turn_ids.iter().filter(|t| !referenced.contains(t)).copied().collect())
}
```

**Step 3: Export**

In `src/graph/mod.rs`:

```rust
pub use query::{neighbors, path, pending_turn_ids, Direction, Neighbor, NeighborQuery, PathResult};
```

**Step 4: Run tests**

Run: `cargo test graph::tests::test_pending`
Expected: 3 tests pass.

**Step 5: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): pending_turn_ids() for save_session graph_pending hint"
```

### Task 4.3: Expose 4 MCP tools

**Files:**

- Modify: `src/mcp/tools.rs`

**Step 1: Add tool definitions**

In `pub fn tool_definitions()`, append 4 new entries:

```rust
        json!({
            "name": "graph_assert",
            "description": "Write entity-relation triples to the graph memory layer. canonical-normalizes src/dst (lowercase + trim + whitespace fold). On duplicate triples, confidence is updated to MAX(existing, new); on duplicate entities, name and entity_type from first write are preserved.",
            "inputSchema": {
                "type": "object",
                "required": ["triples"],
                "properties": {
                    "triples": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "required": ["src", "rel", "dst"],
                            "properties": {
                                "src": {"type": "string"},
                                "rel": {"type": "string"},
                                "dst": {"type": "string"},
                                "src_type": {"type": "string"},
                                "dst_type": {"type": "string"},
                                "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                                "source_turn": {"type": "integer"}
                            }
                        }
                    },
                    "session_id": {"type": "string"}
                }
            }
        }),
        json!({
            "name": "graph_neighbors",
            "description": "Query N-hop neighbors of an entity. Supports rel_type filter and direction (out/in/both). hops in 1..=5.",
            "inputSchema": {
                "type": "object",
                "required": ["entity"],
                "properties": {
                    "entity": {"type": "string"},
                    "rel_type": {"type": "string"},
                    "direction": {"type": "string", "enum": ["out", "in", "both"], "default": "both"},
                    "hops": {"type": "integer", "minimum": 1, "maximum": 5, "default": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50}
                }
            }
        }),
        json!({
            "name": "graph_path",
            "description": "Find shortest path length between two entities (max_hops 1..=10). Returns found/length; full path serialization is a v1.3.1 polish.",
            "inputSchema": {
                "type": "object",
                "required": ["src", "dst"],
                "properties": {
                    "src": {"type": "string"},
                    "dst": {"type": "string"},
                    "max_hops": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5}
                }
            }
        }),
        json!({
            "name": "graph_link_entity",
            "description": "Merge alias: rewire all edges from `from` entity to `to` entity, then delete `from`. Irreversible. Duplicate edges after rewiring are merged automatically.",
            "inputSchema": {
                "type": "object",
                "required": ["from", "to"],
                "properties": {
                    "from": {"type": "string"},
                    "to": {"type": "string"},
                    "session_id": {"type": "string"}
                }
            }
        }),
```

**Step 2: Add routing**

In `ToolHandler::call`, add 4 arms (insert before the `_ =>` catch-all):

```rust
            "graph_assert" => self.graph_assert(args),
            "graph_neighbors" => self.graph_neighbors(args),
            "graph_path" => self.graph_path(args),
            "graph_link_entity" => self.graph_link_entity(args),
```

**Step 3: Implement handlers**

Add to `impl ToolHandler`:

```rust
    fn check_graph_enabled(&self) -> Result<(), String> {
        if !self.config.graph.enabled {
            return Err("graph disabled in config".to_string());
        }
        Ok(())
    }

    fn graph_assert(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let triples_value = args.get("triples")
            .ok_or("missing triples")?;
        let triples: Vec<crate::graph::TripleInput> =
            serde_json::from_value(triples_value.clone())
                .map_err(|e| format!("invalid triples: {}", e))?;
        let stats = crate::graph::assert_triples(&self.db, &triples)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "entities_created": stats.entities_created,
            "entities_updated": stats.entities_updated,
            "relations_created": stats.relations_created,
            "relations_updated": stats.relations_updated
        }))
    }

    fn graph_neighbors(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let q: crate::graph::NeighborQuery = serde_json::from_value(args.clone())
            .map_err(|e| format!("invalid query: {}", e))?;
        let neighbors = crate::graph::neighbors(&self.db, &q)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "neighbors": neighbors
        }))
    }

    fn graph_path(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let src = args["src"].as_str().ok_or("missing src")?;
        let dst = args["dst"].as_str().ok_or("missing dst")?;
        let max_hops = args["max_hops"].as_u64().unwrap_or(5) as u32;
        let result = crate::graph::path(&self.db, src, dst, max_hops)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "found": result.found,
            "length": result.length,
            "path": result.path
        }))
    }

    fn graph_link_entity(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let from = args["from"].as_str().ok_or("missing from")?;
        let to = args["to"].as_str().ok_or("missing to")?;
        let rewired = crate::graph::link_entity(&self.db, from, to)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "status": "ok",
            "edges_rewired": rewired,
            "old_entity_removed": crate::graph::canonicalize(from)
        }))
    }
```

**Step 4: Run all tests**

Run: `cargo test`
Expected: green (no regression in existing tests).

**Step 5: Commit**

```bash
git add src/mcp/tools.rs
git commit -m "feat(mcp): expose 4 graph_* tools (assert/neighbors/path/link_entity)"
```

### Task 4.4: save_session graph_pending field

**Files:**

- Modify: `src/mcp/tools.rs`

**Step 1: Modify `save_session` handler**

In `ToolHandler::save_session`, just before the final `Ok(json!({...}))`:

```rust
        // Append graph_pending hint if graph is enabled + remind_on_save
        let mut response = json!({
            "status": "ok",
            "session_id": stats.session_id,
            "file_path": stats.file_path.to_string_lossy(),
            "turns_saved": stats.turns_saved
        });

        if self.config.graph.enabled && self.config.graph.remind_on_save {
            if let Ok(turn_ids) = self.session_turn_ids(&stats.session_id) {
                if !turn_ids.is_empty() {
                    if let Ok(pending) = crate::graph::pending_turn_ids(&self.db, &turn_ids) {
                        if !pending.is_empty() {
                            response["graph_pending"] = json!({
                                "turn_ids": pending,
                                "hint": "These turns have no graph assertions yet. Call graph_assert with extracted triples (subject, relation, object) and source_turn=<id> to enable relationship queries."
                            });
                        }
                    }
                }
            }
        }

        Ok(response)
```

Replace the previous `Ok(json!({...}))` return with `response` construction shown above.

Add helper method on `ToolHandler`:

```rust
    fn session_turn_ids(&self, session_id: &str) -> Result<Vec<i64>, String> {
        let mut stmt = self.db.conn().prepare(
            "SELECT id FROM turns WHERE session_id = ?1 ORDER BY seq"
        ).map_err(|e| e.to_string())?;
        let rows = stmt.query_map([session_id], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }
```

**Step 2: Write e2e test**

Create `src/graph/e2e_test.rs`:

```rust
//! End-to-end tests: save_session + graph_assert flow

use crate::fact::conversation::{SessionHeader, Turn};
use crate::fact::session_store::SessionStore;
use crate::graph::{assert_triples, pending_turn_ids, TripleInput};
use crate::index::db::Db;
use tempfile::tempdir;

fn header(session_id: &str) -> SessionHeader {
    SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: session_id.to_string(),
        start_time: "2026-05-19T10:00:00+08:00".to_string(),
        profile_id: "default".to_string(),
        source: Some("e2e".to_string()),
        agent_model: None,
        title: None,
        tags: vec![],
    }
}

fn turn(seq: u32, role: &str, content: &str) -> Turn {
    Turn {
        ts: "2026-05-19T10:00:00+08:00".to_string(),
        seq,
        role: role.to_string(),
        content: content.to_string(),
        metadata: None,
    }
}

#[test]
fn test_save_then_pending_shrinks_as_triples_added() {
    let tmp = tempdir().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();

    let h = header("e2e-1");
    let turns = vec![
        turn(1, "user", "test question"),
        turn(2, "assistant", "test answer"),
    ];

    let store = SessionStore::new(tmp.path(), &db);
    store.save(&h, &turns, None).unwrap();

    // Fetch turn ids
    let turn_ids: Vec<i64> = db
        .conn()
        .prepare("SELECT id FROM turns WHERE session_id='e2e-1' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(turn_ids.len(), 2);

    // Initially: all turns pending
    let pending = pending_turn_ids(&db, &turn_ids).unwrap();
    assert_eq!(pending.len(), 2);

    // Assert a triple referencing the first turn
    assert_triples(&db, &[TripleInput {
        src: "user".to_string(),
        rel: "asked".to_string(),
        dst: "test_question".to_string(),
        src_type: None,
        dst_type: None,
        confidence: None,
        source_turn: Some(turn_ids[0]),
    }]).unwrap();

    // Now only the second turn is pending
    let pending = pending_turn_ids(&db, &turn_ids).unwrap();
    assert_eq!(pending, vec![turn_ids[1]]);
}
```

**Step 3: Register the test module**

In `src/graph/mod.rs`:

```rust
#[cfg(test)]
mod e2e_test;
```

**Step 4: Run**

Run: `cargo test`
Expected: all tests pass including new e2e.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat(mcp): save_session returns graph_pending when remind_on_save"
```

**Phase 4 verification gate:**

- All 4 graph tools listed in `tools/list`
- All callable from MCP stdio (manual smoke if possible)
- `save_session` returns `graph_pending` with correct turn_ids when graph empty
- Disabled mode returns friendly error

---

## Phase 5 (P5) · Doctor verbose + Docs + Release

### Task 5.1: doctor --verbose graph statistics

**Files:**

- Modify: `src/main.rs`

**Step 1: Add `--verbose` flag to Doctor subcommand**

Change `Commands::Doctor` to:

```rust
    /// 测试配置
    Doctor {
        /// 显示图谱覆盖率等额外诊断
        #[arg(long)]
        verbose: bool,
    },
```

Update the match arm:

```rust
        Some(Commands::Doctor { verbose }) => cmd_doctor(&config, &db, &db_path, verbose)?,
```

Update `cmd_doctor` signature:

```rust
fn cmd_doctor(
    config: &config::Config,
    db: &index::db::Db,
    db_path: &std::path::Path,
    verbose: bool,
) -> anyhow::Result<()> {
```

**Step 2: Append verbose graph stats**

At the end of `cmd_doctor`, just before `Ok(())`:

```rust
    if verbose && config.graph.enabled {
        let covered: i64 = db.conn().query_row(
            "SELECT COUNT(DISTINCT source_turn) FROM relations
             WHERE source_turn IS NOT NULL",
            [],
            |r| r.get(0),
        ).unwrap_or(0);
        let coverage_pct = if turn_count > 0 {
            (covered as f64 / turn_count as f64 * 100.0).round() as i64
        } else {
            0
        };
        let dangling: i64 = db.conn().query_row(
            "SELECT COUNT(DISTINCT r.source_turn) FROM relations r
             WHERE r.source_turn IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.id = r.source_turn)",
            [],
            |r| r.get(0),
        ).unwrap_or(0);
        println!(
            "图谱覆盖率: {}% ({}/{} turns)",
            coverage_pct, covered, turn_count
        );
        println!("图谱悬空引用: {}", dangling);
    }
```

(Make sure `turn_count` is in scope — it's set earlier in `cmd_doctor`.)

**Step 3: Smoke test**

Run: `cargo run --quiet -- doctor --verbose`
Expected: extra lines for 图谱覆盖率 / 图谱悬空引用.

**Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(doctor): --verbose shows graph coverage + dangling refs"
```

### Task 5.2: README + for_ai.md updates

**Files:**

- Modify: `README.md`
- Modify: `README_EN.md`
- Modify: `for_ai.md`

**Step 1: README.md updates**

In the "系统架构" section, after the existing table, add:

```markdown
**图谱层 (v1.3+)**: SQLite 表 `entities` + `relations`，由 agent 通过 `graph_assert` 累积；canonical 归一化（lowercase + trim + 折空白）；不调 LLM 也不规则抽取。
```

After "MCP 工具列表" table, add a new section:

```markdown
## 图谱记忆 (v1.3+)

图谱是事实层和成长层之外的第三层，复用同一个 SQLite 数据库新增两张表。agent 是图谱的唯一作者；server 不调 LLM。`canonical` 归一化处理大小写漂移，但不做语义合并（"Alice" 和 "Alice Smith" 是两个节点）。

### 新增 MCP 工具

| 工具 | 用途 |
|---|---|
| `graph_assert` | 写实体-关系三元组 |
| `graph_neighbors` | 查 N-hop 邻居（rel_type / direction / hops） |
| `graph_path` | 两节点最短路径长度 |
| `graph_link_entity` | 别名合并（不可逆） |

### 软提示

`save_session` 在图谱启用且 `remind_on_save = true` 时返回 `graph_pending: { turn_ids, hint }`，列出尚未被任何 relation 引用的 turn_id。关闭：`graph.remind_on_save = false`。

### 整体禁用

`graph.enabled = false` 时所有 `graph_*` 工具返回 `"graph disabled in config"`，事实/成长层完全不受影响。
```

In the "升级指南" section, insert at the top:

```markdown
### 从 v1.2.1 升级到 v1.3.0

v1.3.0 加入了第三个记忆层——图谱层。事实层和成长层一字不动；旧数据完全兼容。

```bash
# 1. 替换二进制文件
# 2. 启动 → init_schema 自动建 entities + relations 表
asuna-memory doctor
# 预期看到：图谱: ENABLED (0 entities, 0 relations)
```

**v1.3.0 Changelog:**

- **新：图谱记忆层** — 同 SQLite 数据库内的 entities + relations 表，canonical 归一化
- **新：4 个 MCP 工具** — `graph_assert` / `graph_neighbors` / `graph_path` / `graph_link_entity`
- **新：`save_session` 软提示** — 返回 `graph_pending` 字段列出未图谱化的 turn_id
- **doctor --verbose** — 显示图谱覆盖率和悬空引用统计
- **零新依赖** — 复用 `rusqlite`；二进制体积不变
```

**Step 2: README_EN.md — mirror the same changes in English**

(Same structure; translate Chinese text.)

**Step 3: for_ai.md — full tool specifications**

In `§ 3 Tools`, append:

````markdown
### 3.10 `graph_assert`

Write entity-relation triples to the graph layer. canonical-normalizes src/dst.

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

Params:
- `triples` (required, non-empty array). Each triple:
  - `src` / `rel` / `dst` (required strings, non-empty)
  - `src_type` / `dst_type` (optional, free string, default `'unknown'`)
  - `confidence` (optional, 0..=1, default 0.5)
  - `source_turn` (optional INT64, recommended for provenance)
- `session_id` (optional)

Semantics: single SQLite transaction. Existing entities keep their first-written `name`/`entity_type`; only `last_seen` refreshes. Existing relations have `confidence` updated to `MAX(existing, new)`.

### 3.11 `graph_neighbors`

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

- `direction` ∈ `out` / `in` / `both` (default `both`)
- `hops` ∈ 1..=5 (default 1)
- `limit` (default 50, max 200)

### 3.12 `graph_path`

```json
{
  "name": "graph_path",
  "arguments": {"src": "Alice", "dst": "OpenAI", "max_hops": 5}
}
```

- Returns `{found: bool, length: u32, path: []}`. Path body is empty in v1.3.0 (v1.3.1 polish).

### 3.13 `graph_link_entity`

```json
{
  "name": "graph_link_entity",
  "arguments": {"from": "alice", "to": "alice smith", "session_id": "uuid"}
}
```

Rewires all edges from `from` to `to`, then deletes `from`. Irreversible. Duplicate edges after rewiring are merged.
````

In `§ 4 Usage Patterns`, add a "Pattern: Graph-aware memory":

```markdown
### Pattern: Graph-aware memory

After each save_session, check the returned `graph_pending.turn_ids`. For each unreferenced turn, extract `(subject, relation, object)` triples and call `graph_assert` with `source_turn=<id>`. The graph layer becomes useful only as you write to it.
```

In `§ 9 Behavioral Contracts`, add 3 lines:

```markdown
- **Graph as third layer**: `entities` + `relations` tables in the same `memory.db`. Independent of fact/growth layers.
- **canonical normalization**: lowercase + trim + whitespace fold is the only entity-identity logic. "Alice" and "Alice Smith" remain separate nodes unless `graph_link_entity` is called.
- **Confidence is MAX-merge**: re-asserting the same triple with higher confidence updates the stored value; lower confidence is ignored.
```

**Step 4: Lint check**

Run: `npx --yes markdownlint-cli README.md README_EN.md for_ai.md`
Expected: clean (or only pre-existing warnings).

**Step 5: Commit**

```bash
git add README.md README_EN.md for_ai.md
git commit -m "docs: add v1.3.0 graph layer chapter + upgrade guide"
```

### Task 5.3: Bump version

**Files:**

- Modify: `Cargo.toml`

**Step 1: Bump to 1.3.0**

In `Cargo.toml`:

```toml
version = "1.3.0"
```

**Step 2: Refresh Cargo.lock**

Run: `cargo check`
Expected: Cargo.lock version updates.

**Step 3: Full validation**

Run:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

All three must pass.

**Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "release: bump version to 1.3.0"
```

### Task 5.4: Tag + push

**Step 1: Push**

```bash
git push origin main
```

**Step 2: Tag**

```bash
git tag -a v1.3.0 -m "v1.3.0 — graph memory layer (SQLite entities + relations)"
git push origin v1.3.0
```

Triggers release workflow.

**Step 3: Watch**

```bash
gh run watch <run-id>
```

Verify 4 artifacts produced; release published on GitHub.

**Phase 5 verification gate (release ready):**

- All tests pass (~80+ tests)
- clippy clean
- 4 CI artifacts produced
- doctor shows graph stats
- README, README_EN, for_ai.md all updated
- Zero regression in v1.2.1 functionality

---

## Risk Register

| Risk | Trigger | Mitigation |
|---|---|---|
| Recursive CTE slow on large graphs | neighbors test times out | KISS: limit `hops ≤ 5`; performance gate fails the test loudly |
| `Direction::Both` JOIN expression bugs | neighbors test fails for `both` | Fallback: 3 UNION'd CTEs (out + in) |
| FK `ON DELETE CASCADE` doesn't fire | link_entity test leaves orphan edges | Verify `PRAGMA foreign_keys = ON` is enforced |
| `source_turn` soft FK accumulates dangling | doctor verbose shows growing count | Acceptable; report only, no automatic cleanup |
| canonical edge-case (unicode quirks) | Chinese tests fail | Tests cover lowercase passthrough; if NFC normalization needed, v1.3.1 |
| agent doesn't use graph = dead feature | Real-world adoption | Soft hint + doctor visibility; if 0 usage, deprecate in v1.4 |

---

## What's NOT in v1.3.0

- Entity embedding / fuzzy linking → v1.4
- Rule-based extraction fallback → never (would invalidate "no LLM" commitment)
- Coverage threshold warning (tier 2) → only doctor --verbose
- Forced double-write (tier 3) → never
- Free-form SQL queries (`graph_sql`) → v1.4 if needed
- `rebuild_index` rebuilding graph → never (agent is source of truth)
- Cross-profile graph sharing → never
- search_sessions auto-using graph seeds → v1.4
- Path body serialization (full node/edge sequence) → v1.3.1 polish
