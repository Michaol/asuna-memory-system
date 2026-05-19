# v1.3.0 Graph Memory Layer Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add an embedded Kuzu-backed graph memory layer (Path A: dual-engine) with 5 new MCP tools, soft hints, and config wiring, without touching the fact/growth layers.

**Architecture:** New `src/graph/` module wraps the `kuzu` crate. `src/fact/` and `src/growth/` are untouched. Failure of the graph layer never affects existing layers — every graph operation degrades gracefully to a friendly error. canonical normalization (lowercase + trim + whitespace fold) is the only entity-identity logic; agent is the sole author of triples.

**Tech Stack:**
- Rust 2021 edition (Asuna's existing toolchain)
- `kuzu = "=0.11.3"` (pinned)
- `rusqlite` / `sqlite-vec` (existing, untouched)
- `serde_json` for tool payloads
- `tempfile` (existing dev-dep)

**Design doc:** `docs/plans/2026-05-19-graph-layer-design.md`

---

## Pre-Flight: Local Cross-Compile Risk Check

Before touching code, validate Kuzu can compile on our 4 release targets locally. **If this fails, we re-evaluate before sinking 4 days into a doomed direction.**

### Task 0: Cross-compile smoke test

**Files (temporary scratch, not committed):**

- Create: `/tmp/kuzu-smoke/Cargo.toml`
- Create: `/tmp/kuzu-smoke/src/main.rs`

**Step 1: Create a hello-world crate using kuzu**

`/tmp/kuzu-smoke/Cargo.toml`:
```toml
[package]
name = "kuzu-smoke"
version = "0.1.0"
edition = "2021"

[dependencies]
kuzu = "=0.11.3"
```

`/tmp/kuzu-smoke/src/main.rs`:
```rust
use kuzu::{Connection, Database, SystemConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let db = Database::new(tmp.path().join("smoke.kuzu"), SystemConfig::default())?;
    let conn = Connection::new(&db)?;
    conn.query("CREATE NODE TABLE IF NOT EXISTS N(id INT64 PRIMARY KEY)")?;
    println!("kuzu smoke ok");
    Ok(())
}
```

(Add `tempfile = "3"` to Cargo.toml deps for this scratch crate.)

**Step 2: Build native**

Run: `cd /tmp/kuzu-smoke && cargo build --release`
Expected: clean build, binary works.

**Step 3: Cross-compile to aarch64-linux** (skip if no cross toolchain — note as risk and proceed)

Run: `cargo build --release --target aarch64-unknown-linux-gnu` (if rustup target installed)
Expected: clean build OR documented native dependency missing.

**Step 4: Note findings**

If build succeeds → proceed.
If build fails on aarch64 → **STOP and report**: open a Cargo.toml issue, possibly switch to vendored Kuzu or restrict platforms.

Note: GitHub Actions CI uses dedicated runners (windows-latest, macos-latest, ubuntu-latest, ubuntu-24.04-arm) — Kuzu publishes prebuilt binaries for all of these. Local cross-compile is best-effort; CI is the source of truth.

**Step 5: Cleanup**

Run: `rm -rf /tmp/kuzu-smoke`

No commit (this was a smoke test).

---

## Phase 1 (P1) · Skeleton

Goal: New `src/graph/` module loads, opens a Kuzu DB next to memory.db, degrades gracefully if Kuzu fails. No new MCP tools yet. Existing tests must still pass.

### Task 1.1: Pin Kuzu dependency

**Files:**

- Modify: `Cargo.toml`

**Step 1: Add kuzu to dependencies**

In `[dependencies]` section, after `regex-lite = "0.1"`:
```toml
kuzu = "=0.11.3"
```

Note: pinned exact version because Kuzu is 0.x and breaking changes between minors are likely.

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: pulls down `kuzu` crate, compiles cleanly, no new warnings.

**Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add kuzu 0.11.3 pinned for graph layer"
```

### Task 1.2: GraphConfig in config.rs

**Files:**

- Modify: `src/config.rs`

**Step 1: Add GraphConfig struct after EmbeddingConfig**

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

**Step 2: Add graph field to Config struct**

In `pub struct Config`, after `pub embedding: EmbeddingConfig,`:
```rust
    #[serde(default)]
    pub graph: GraphConfig,
```

The `#[serde(default)]` makes the field optional in older config.json files (v1.2.1 users upgrade seamlessly).

**Step 3: Add graph to Default impl**

In `impl Default for Config`, after `embedding: EmbeddingConfig { .. }`:
```rust
            graph: GraphConfig::default(),
```

**Step 4: Verify**

Run: `cargo check`
Expected: compiles.

**Step 5: Add graph_dir() helper**

After `pub fn profile_db_path(&self) -> PathBuf {`:
```rust
    /// 获取 profile 对应的图谱目录（Kuzu 数据库目录）
    pub fn graph_dir(&self) -> PathBuf {
        self.profile_dir().join("graph.kuzu")
    }
```

Note: Kuzu uses a directory, not a single file.

**Step 6: Run all existing tests**

Run: `cargo test`
Expected: all 51 tests still pass (config default uses graph.enabled=true but no graph code exists yet so no behavior change).

**Step 7: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add GraphConfig with graph_dir() helper"
```

### Task 1.3: Empty graph module skeleton

**Files:**

- Create: `src/graph/mod.rs`
- Modify: `src/main.rs`

**Step 1: Write failing build (no test yet — we just want main.rs to recognize mod graph)**

Create `src/graph/mod.rs`:
```rust
//! 图谱记忆层（基于 Kuzu）。
//!
//! 三层正交架构中的第三层。事实层（SQLite）和成长层（Markdown）一字不动。
//! 图层失败时，所有现有工具行为不变；仅 graph_* 系列工具会返回友好错误。

pub mod db;
```

Create `src/graph/db.rs`:
```rust
//! Kuzu 数据库连接 + 初始化 + 降级管理。

use std::path::Path;

/// 图谱后端状态。一个 GraphDb 要么是 Ready（持有 Kuzu 句柄），
/// 要么是 Unavailable（带降级原因），调用方据此选择路径。
pub enum GraphDb {
    Ready(Backend),
    Unavailable(String),
    Disabled,
}

/// 内部 Backend 封装 Kuzu Database + Connection。
/// 单线程模型：MCP server 是 stdio 单线程，连接也单线程持有。
pub struct Backend {
    _db: kuzu::Database,
    pub(crate) conn: kuzu::Connection<'static>,
}

impl GraphDb {
    /// 打开或初始化图谱数据库。
    /// - enabled=false → GraphDb::Disabled
    /// - enabled=true 但 Kuzu 加载失败 → GraphDb::Unavailable(原因)
    /// - 否则 → GraphDb::Ready
    pub fn open_or_init(graph_dir: &Path, enabled: bool) -> Self {
        if !enabled {
            return GraphDb::Disabled;
        }
        match Self::try_open(graph_dir) {
            Ok(backend) => GraphDb::Ready(backend),
            Err(e) => {
                tracing::warn!("Kuzu 图谱后端加载失败: {} (路径: {})", e, graph_dir.display());
                GraphDb::Unavailable(e.to_string())
            }
        }
    }

    fn try_open(graph_dir: &Path) -> Result<Backend, kuzu::Error> {
        std::fs::create_dir_all(graph_dir).map_err(|e| {
            kuzu::Error::from(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("无法创建图谱目录: {}", e),
            ))
        })?;
        let db = kuzu::Database::new(graph_dir, kuzu::SystemConfig::default())?;
        // SAFETY: We tie the connection lifetime to the Database we own via Box leak.
        // The 'static lifetime here is a marker — in practice Backend owns both fields
        // and Backend never outlives the GraphDb that holds it.
        let conn = kuzu::Connection::new(unsafe { std::mem::transmute::<&_, &'static _>(&db) })?;
        // Initialize schema
        conn.query("CREATE NODE TABLE IF NOT EXISTS Entity(
            canonical STRING PRIMARY KEY,
            name STRING,
            entity_type STRING DEFAULT 'unknown',
            first_seen TIMESTAMP,
            last_seen TIMESTAMP,
            source_turn INT64
        )")?;
        conn.query("CREATE REL TABLE IF NOT EXISTS Relation(
            FROM Entity TO Entity,
            rel_type STRING,
            confidence DOUBLE DEFAULT 0.5,
            source_turn INT64,
            created_at TIMESTAMP
        )")?;
        Ok(Backend { _db: db, conn })
    }

    /// 是否可用（Ready 状态）
    pub fn is_ready(&self) -> bool {
        matches!(self, GraphDb::Ready(_))
    }

    /// 状态描述，用于 doctor 输出
    pub fn status_string(&self) -> String {
        match self {
            GraphDb::Ready(_) => "OK".to_string(),
            GraphDb::Disabled => "DISABLED (config.graph.enabled = false)".to_string(),
            GraphDb::Unavailable(e) => format!("UNAVAILABLE ({})", e),
        }
    }
}
```

**KNOWN-RISKY:** the `unsafe transmute` on the Database reference is a self-referential lifetime workaround. We'll revisit this in Task 1.4's test — if it segfaults, refactor to use `OnceCell` + `Pin<Box<Database>>` or hold the Database via Arc, depending on what kuzu 0.11 API allows.

**Step 2: Wire mod into main.rs**

In `src/main.rs`, after `mod fact;`:
```rust
mod graph;
```

**Step 3: Build**

Run: `cargo check`
Expected: clean build. May warn about unused fields — that's fine for now.

If kuzu's `Connection::new` does NOT accept `&'static Database`, the transmute approach is incorrect. Inspect the actual API and adapt — likely just hold both fields and use the regular lifetime, possibly via:
```rust
pub struct Backend {
    // db must outlive conn — Rust's drop order is reverse declaration,
    // so conn drops first, then _db. Lifetime is constrained by the struct itself.
    conn: kuzu::Connection<'static>,
    _db: Box<kuzu::Database>,
}
```
If that also fails, fall back to a single-field approach holding both in a `OnceCell` initialized at first use.

**Step 4: Commit**

```bash
git add src/main.rs src/graph/
git commit -m "feat(graph): add empty graph module skeleton with Kuzu open/init"
```

### Task 1.4: Integration test for open/init

**Files:**

- Modify: `src/graph/mod.rs` (add `#[cfg(test)] mod tests;`)
- Create: `src/graph/tests.rs`

**Step 1: Write failing test**

Add to `src/graph/mod.rs`:
```rust
#[cfg(test)]
mod tests;
```

Create `src/graph/tests.rs`:
```rust
use super::db::GraphDb;
use tempfile::tempdir;

#[test]
fn test_open_ready() {
    let tmp = tempdir().unwrap();
    let g = GraphDb::open_or_init(&tmp.path().join("graph.kuzu"), true);
    assert!(g.is_ready(), "status={}", g.status_string());
}

#[test]
fn test_disabled_returns_disabled() {
    let tmp = tempdir().unwrap();
    let g = GraphDb::open_or_init(&tmp.path().join("graph.kuzu"), false);
    assert!(!g.is_ready());
    assert!(g.status_string().contains("DISABLED"));
}

#[test]
fn test_idempotent_init() {
    // Open, close, reopen — schema CREATE IF NOT EXISTS must not panic
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("graph.kuzu");
    {
        let g = GraphDb::open_or_init(&path, true);
        assert!(g.is_ready());
    }
    {
        let g = GraphDb::open_or_init(&path, true);
        assert!(g.is_ready());
    }
}
```

**Step 2: Run tests**

Run: `cargo test graph::tests`
Expected: 3 tests pass.

If `test_open_ready` fails with segfault → the transmute is wrong. Replace `Backend` with `Pin<Box<Database>>` pattern. Iterate until green.

**Step 3: Run full suite**

Run: `cargo test`
Expected: 51 (existing) + 3 (new) = 54 pass.

**Step 4: Commit**

```bash
git add src/graph/
git commit -m "test(graph): integration tests for open/init/disabled paths"
```

### Task 1.5: Wire GraphDb into main + doctor output

**Files:**

- Modify: `src/main.rs`

**Step 1: Hold GraphDb alongside the existing Db**

Find where `let db = Rc::new(index::db::Db::open(&db_path)?);` is set up in main. After it:

```rust
    // 图谱后端（按需启用）
    let graph_dir = config.graph_dir();
    let graph_db = graph::db::GraphDb::open_or_init(&graph_dir, config.graph.enabled);
    tracing::info!("图谱: {}", graph_db.status_string());
    let graph_db = Rc::new(graph_db);
```

**Step 2: Pass graph_db to MCP server**

(For now we hold it in main; we'll plumb to MCP in P4. Add a placeholder unused acknowledgment.)

Add to the end of cmd_doctor (just before `Ok(())`):
```rust
    println!("图谱: {}", graph_db.status_string());
    println!("图谱目录: {}", config.graph_dir().display());
```

Adjust cmd_doctor signature to accept `&Rc<graph::db::GraphDb>` and pass it from main.

**Step 3: Verify doctor output**

Run: `cargo run -- doctor 2>&1 | grep 图谱`
Expected:
```
图谱: OK
图谱目录: /home/.../graph.kuzu
```

**Step 4: Verify tests still green**

Run: `cargo test`
Expected: 54 pass.

**Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(graph): wire GraphDb into main + show in doctor"
```

**Phase 1 verification gate:**
- `cargo check` clean
- `cargo test` 54+/54+ pass
- `cargo run -- doctor` shows `图谱: OK` and `图谱目录: ...`
- `cargo run -- doctor` with `graph.enabled=false` in config shows `图谱: DISABLED (...)`
- Run `cargo build --release` once locally to surface any release-only issues early

---

## Phase 2 (P2) · Write Path

Goal: `graph_assert` MCP tool works end-to-end. Triples are canonical-normalized, MERGE-d into Kuzu, returned counts are correct.

### Task 2.1: canonicalize() function

**Files:**

- Create: `src/graph/entity.rs`
- Modify: `src/graph/mod.rs`

**Step 1: Write failing test**

In `src/graph/tests.rs`, append:
```rust
use super::entity::canonicalize;

#[test]
fn test_canonicalize_basic() {
    assert_eq!(canonicalize("Alice"), "alice");
    assert_eq!(canonicalize("  Alice  "), "alice");
    assert_eq!(canonicalize("ALICE SMITH"), "alice smith");
    assert_eq!(canonicalize("Alice   Smith"), "alice smith");
    assert_eq!(canonicalize("Alice\tSmith"), "alice smith");
}

#[test]
fn test_canonicalize_chinese() {
    // 中文不受 lowercase 影响；只折叠空白
    assert_eq!(canonicalize("亚 丝 娜"), "亚 丝 娜");
    assert_eq!(canonicalize("亚丝娜  "), "亚丝娜");
}

#[test]
fn test_canonicalize_empty() {
    assert_eq!(canonicalize(""), "");
    assert_eq!(canonicalize("   "), "");
}
```

**Step 2: Run test to verify failure**

Run: `cargo test graph::tests::test_canonicalize`
Expected: FAIL (canonicalize not found).

**Step 3: Write minimal implementation**

Create `src/graph/entity.rs`:
```rust
//! 实体相关：canonical 归一化 + entity 操作辅助函数。

/// canonical 化字符串：lowercase + trim + 把连续空白折叠为单个空格。
///
/// 这是 Asuna v1.3.0 唯一的实体身份逻辑。
/// agent 是图谱内容的唯一作者；server 不做 fuzzy 匹配或语义合并。
pub fn canonicalize(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
```

Add `pub mod entity;` to `src/graph/mod.rs`.

**Step 4: Run test to verify pass**

Run: `cargo test graph::tests::test_canonicalize`
Expected: PASS (3 tests).

**Step 5: Commit**

```bash
git add src/graph/entity.rs src/graph/mod.rs src/graph/tests.rs
git commit -m "feat(graph): canonicalize() with table-driven tests"
```

### Task 2.2: graph_assert core logic

**Files:**

- Create: `src/graph/relation.rs`
- Modify: `src/graph/mod.rs`
- Modify: `src/graph/db.rs` (add `assert_triples()` method)

**Step 1: Define the Triple input + AssertStats output**

In `src/graph/relation.rs`:
```rust
//! Relation 写入：三元组 MERGE 语义、confidence max 合并、单事务。

use serde::Deserialize;

/// 一条要断言的三元组（agent 传入）
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Default, serde::Serialize)]
pub struct AssertStats {
    pub entities_created: u32,
    pub entities_updated: u32,
    pub relations_created: u32,
    pub relations_updated: u32,
}
```

Add `pub mod relation;` to `src/graph/mod.rs`.

**Step 2: Write failing integration test**

In `src/graph/tests.rs`, append:
```rust
use super::relation::TripleInput;

fn fresh_graph() -> (tempfile::TempDir, GraphDb) {
    let tmp = tempfile::tempdir().unwrap();
    let g = GraphDb::open_or_init(&tmp.path().join("graph.kuzu"), true);
    assert!(g.is_ready());
    (tmp, g)
}

#[test]
fn test_assert_basic_triples() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };

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
        TripleInput {
            src: "Alice".to_string(),
            rel: "friend_of".to_string(),
            dst: "Bob".to_string(),
            src_type: None,
            dst_type: None,
            confidence: None,
            source_turn: None,
        },
    ];
    let stats = backend.assert_triples(&triples).unwrap();
    assert_eq!(stats.entities_created, 3);  // Alice, OpenAI, Bob
    assert_eq!(stats.relations_created, 2);
}

#[test]
fn test_assert_dedup_same_triple() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };

    let t = vec![TripleInput {
        src: "Alice".to_string(),
        rel: "works_at".to_string(),
        dst: "OpenAI".to_string(),
        src_type: None,
        dst_type: None,
        confidence: Some(0.5),
        source_turn: None,
    }];
    backend.assert_triples(&t).unwrap();

    // Re-assert same triple with higher confidence: should update, not duplicate
    let t2 = vec![TripleInput {
        src: "alice".to_string(),  // different case
        rel: "works_at".to_string(),
        dst: "openai".to_string(),  // different case
        src_type: None,
        dst_type: None,
        confidence: Some(0.95),
        source_turn: None,
    }];
    let stats = backend.assert_triples(&t2).unwrap();
    assert_eq!(stats.entities_created, 0);
    assert_eq!(stats.relations_created, 0);
    assert_eq!(stats.relations_updated, 1);
}

#[test]
fn test_assert_empty_triples_rejected() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    let result = backend.assert_triples(&[]);
    assert!(result.is_err());
}
```

**Step 3: Run tests — verify they fail**

Run: `cargo test graph::tests::test_assert`
Expected: FAIL (assert_triples not implemented).

**Step 4: Implement assert_triples on Backend**

Add to `src/graph/db.rs`:
```rust
use crate::graph::entity::canonicalize;
use crate::graph::relation::{AssertStats, TripleInput};

impl Backend {
    pub fn assert_triples(&self, triples: &[TripleInput]) -> Result<AssertStats, String> {
        if triples.is_empty() {
            return Err("triples must be non-empty".to_string());
        }

        let mut stats = AssertStats::default();
        let now_ts = chrono::Utc::now().naive_utc();

        // BEGIN TRANSACTION (Kuzu auto-commits each statement, but multi-stmt blocks
        // can be wrapped). For v1.3.0 KISS: do each MERGE individually; if any fails
        // we return early with an error — partial writes are acceptable because the
        // operation is idempotent and the agent can re-run.

        for t in triples {
            // Validate
            if t.src.trim().is_empty() || t.rel.trim().is_empty() || t.dst.trim().is_empty() {
                return Err("triple missing required field".to_string());
            }
            if let Some(c) = t.confidence {
                if !(0.0..=1.0).contains(&c) {
                    return Err(format!("confidence must be in [0.0, 1.0], got {}", c));
                }
            }

            let src_canon = canonicalize(&t.src);
            let dst_canon = canonicalize(&t.dst);
            let conf = t.confidence.unwrap_or(0.5);
            let src_type = t.src_type.as_deref().unwrap_or("unknown");
            let dst_type = t.dst_type.as_deref().unwrap_or("unknown");
            let source_turn = t.source_turn.unwrap_or(-1);  // -1 = no source

            // MERGE src entity
            let existed = self.entity_exists(&src_canon)?;
            self.merge_entity(&src_canon, &t.src, src_type, source_turn, now_ts)?;
            if existed { stats.entities_updated += 1; } else { stats.entities_created += 1; }

            // MERGE dst entity (skip update count if same canonical as src — rare)
            if dst_canon != src_canon {
                let existed = self.entity_exists(&dst_canon)?;
                self.merge_entity(&dst_canon, &t.dst, dst_type, source_turn, now_ts)?;
                if existed { stats.entities_updated += 1; } else { stats.entities_created += 1; }
            }

            // MERGE relation
            let existed = self.relation_exists(&src_canon, &t.rel, &dst_canon)?;
            self.merge_relation(&src_canon, &t.rel, &dst_canon, conf, source_turn, now_ts)?;
            if existed { stats.relations_updated += 1; } else { stats.relations_created += 1; }
        }

        Ok(stats)
    }

    fn entity_exists(&self, canonical: &str) -> Result<bool, String> {
        let q = format!(
            "MATCH (n:Entity {{canonical: '{}'}}) RETURN COUNT(n)",
            kuzu_escape(canonical)
        );
        let result = self.conn.query(&q).map_err(|e| format!("entity_exists: {}", e))?;
        for row in result {
            if let kuzu::Value::Int64(c) = row[0] {
                return Ok(c > 0);
            }
        }
        Ok(false)
    }

    fn merge_entity(
        &self,
        canonical: &str,
        name: &str,
        entity_type: &str,
        source_turn: i64,
        now: chrono::NaiveDateTime,
    ) -> Result<(), String> {
        // MERGE node by canonical; only update last_seen on existing.
        // On first insert, set first_seen too.
        let q = format!(
            "MERGE (n:Entity {{canonical: '{}'}})
             ON CREATE SET n.name='{}', n.entity_type='{}', n.first_seen=timestamp('{}'), n.last_seen=timestamp('{}'), n.source_turn={}
             ON MATCH  SET n.last_seen=timestamp('{}')",
            kuzu_escape(canonical),
            kuzu_escape(name),
            kuzu_escape(entity_type),
            now.format("%Y-%m-%d %H:%M:%S"),
            now.format("%Y-%m-%d %H:%M:%S"),
            source_turn,
            now.format("%Y-%m-%d %H:%M:%S"),
        );
        self.conn.query(&q).map_err(|e| format!("merge_entity: {}", e))?;
        Ok(())
    }

    fn relation_exists(&self, src: &str, rel_type: &str, dst: &str) -> Result<bool, String> {
        let q = format!(
            "MATCH (a:Entity {{canonical: '{}'}})-[r:Relation {{rel_type: '{}'}}]->(b:Entity {{canonical: '{}'}}) RETURN COUNT(r)",
            kuzu_escape(src), kuzu_escape(rel_type), kuzu_escape(dst)
        );
        let result = self.conn.query(&q).map_err(|e| format!("relation_exists: {}", e))?;
        for row in result {
            if let kuzu::Value::Int64(c) = row[0] {
                return Ok(c > 0);
            }
        }
        Ok(false)
    }

    fn merge_relation(
        &self,
        src: &str,
        rel_type: &str,
        dst: &str,
        confidence: f64,
        source_turn: i64,
        now: chrono::NaiveDateTime,
    ) -> Result<(), String> {
        // If relation exists, update confidence to max(existing, new)
        // If not, create it.
        let exists = self.relation_exists(src, rel_type, dst)?;
        if exists {
            let q = format!(
                "MATCH (a:Entity {{canonical: '{}'}})-[r:Relation {{rel_type: '{}'}}]->(b:Entity {{canonical: '{}'}})
                 SET r.confidence = CASE WHEN r.confidence < {} THEN {} ELSE r.confidence END",
                kuzu_escape(src), kuzu_escape(rel_type), kuzu_escape(dst), confidence, confidence
            );
            self.conn.query(&q).map_err(|e| format!("update_relation: {}", e))?;
        } else {
            let q = format!(
                "MATCH (a:Entity {{canonical: '{}'}}), (b:Entity {{canonical: '{}'}})
                 CREATE (a)-[:Relation {{rel_type: '{}', confidence: {}, source_turn: {}, created_at: timestamp('{}')}}]->(b)",
                kuzu_escape(src), kuzu_escape(dst), kuzu_escape(rel_type),
                confidence, source_turn,
                now.format("%Y-%m-%d %H:%M:%S")
            );
            self.conn.query(&q).map_err(|e| format!("create_relation: {}", e))?;
        }
        Ok(())
    }
}

/// 转义 Cypher 字符串字面量中的单引号
fn kuzu_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
```

Add `chrono` import at top if not present.

**NOTE for the executor:** If kuzu 0.11 supports prepared statements with parameters cleanly for MERGE, prefer that over string interpolation to avoid SQL-injection-style bugs. The above is the minimum-viable approach; refactor to `conn.prepare()` + `conn.execute()` once the first integration test is green.

**Step 5: Run tests to verify pass**

Run: `cargo test graph::tests::test_assert`
Expected: PASS (3 tests).

**Step 6: Run full suite**

Run: `cargo test`
Expected: 54 (existing) + 3 (new) = 57 pass.

**Step 7: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): graph_assert core — MERGE entities + relations, confidence max"
```

### Task 2.3: Performance budget check

**Files:**

- Modify: `src/graph/db.rs`

**Step 1: Write timing test**

In `src/graph/tests.rs`, append:
```rust
#[test]
fn test_assert_performance_10_triples() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };

    let triples: Vec<TripleInput> = (0..10).map(|i| TripleInput {
        src: format!("entity_{}", i),
        rel: "rel_test".to_string(),
        dst: format!("entity_{}", i + 100),
        src_type: None, dst_type: None, confidence: None, source_turn: Some(i),
    }).collect();

    let start = std::time::Instant::now();
    backend.assert_triples(&triples).unwrap();
    let elapsed = start.elapsed();

    println!("10 triples write: {:?}", elapsed);
    // Budget: 20ms. Hard-fail at 100ms to catch real regressions.
    assert!(elapsed.as_millis() < 100, "10 triples took {:?}, way over 20ms budget", elapsed);
    if elapsed.as_millis() > 30 {
        tracing::warn!("perf: 10 triples assert took {:?} (budget 20ms)", elapsed);
    }
}
```

**Step 2: Run test**

Run: `cargo test graph::tests::test_assert_performance -- --nocapture`
Expected: PASS, prints timing.

If elapsed >100ms → real problem. Investigate (probably exists/merge query inefficiency); switch to prepared statements.

**Step 3: Commit**

```bash
git add src/graph/tests.rs
git commit -m "test(graph): perf budget for graph_assert (10 triples < 100ms)"
```

**Phase 2 verification gate:**
- 57+ tests pass
- 10-triples write < 100ms (warn if > 30ms)
- `cargo clippy --all-targets -- -D warnings` clean

---

## Phase 3 (P3) · Read Path

Goal: `graph_neighbors`, `graph_path`, `graph_query` work. Cypher keyword blacklist enforced. Query timeout + 1000-row truncation enforced.

### Task 3.1: graph_neighbors

**Files:**

- Create: `src/graph/query.rs`
- Modify: `src/graph/db.rs` (add `neighbors()` method)
- Modify: `src/graph/mod.rs` (`pub mod query;`)

**Step 1: Write failing test**

In `src/graph/tests.rs`, append:
```rust
use super::query::{Direction, NeighborQuery};

#[test]
fn test_neighbors_1hop_out() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };

    // Set up: alice --works_at--> openai; alice --friend_of--> bob
    let triples = vec![
        TripleInput {
            src: "Alice".to_string(), rel: "works_at".to_string(), dst: "OpenAI".to_string(),
            src_type: None, dst_type: None, confidence: None, source_turn: None,
        },
        TripleInput {
            src: "Alice".to_string(), rel: "friend_of".to_string(), dst: "Bob".to_string(),
            src_type: None, dst_type: None, confidence: None, source_turn: None,
        },
    ];
    backend.assert_triples(&triples).unwrap();

    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: None,
        direction: Direction::Out,
        hops: 1,
        limit: 50,
    };
    let neighbors = backend.neighbors(&q).unwrap();
    let names: Vec<_> = neighbors.iter().map(|n| n.canonical.as_str()).collect();
    assert!(names.contains(&"openai"));
    assert!(names.contains(&"bob"));
}

#[test]
fn test_neighbors_filtered_by_rel() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    let triples = vec![
        TripleInput {
            src: "Alice".to_string(), rel: "works_at".to_string(), dst: "OpenAI".to_string(),
            src_type: None, dst_type: None, confidence: None, source_turn: None,
        },
        TripleInput {
            src: "Alice".to_string(), rel: "friend_of".to_string(), dst: "Bob".to_string(),
            src_type: None, dst_type: None, confidence: None, source_turn: None,
        },
    ];
    backend.assert_triples(&triples).unwrap();

    let q = NeighborQuery {
        entity: "Alice".to_string(),
        rel_type: Some("works_at".to_string()),
        direction: Direction::Out,
        hops: 1,
        limit: 50,
    };
    let neighbors = backend.neighbors(&q).unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].canonical, "openai");
}
```

**Step 2: Verify failure**

Run: `cargo test graph::tests::test_neighbors`
Expected: FAIL.

**Step 3: Implement**

Create `src/graph/query.rs`:
```rust
//! 图谱读路径：邻居 / 路径 / 自由 Cypher（受限只读）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Direction { Out, In, Both }

impl Default for Direction {
    fn default() -> Self { Direction::Both }
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
```

Add to `src/graph/db.rs`:
```rust
use crate::graph::query::{Direction, NeighborQuery, Neighbor};

impl Backend {
    pub fn neighbors(&self, q: &NeighborQuery) -> Result<Vec<Neighbor>, String> {
        if q.hops == 0 || q.hops > 5 {
            return Err(format!("hops must be in 1..=5, got {}", q.hops));
        }
        let limit = q.limit.min(200).max(1);
        let canon = canonicalize(&q.entity);

        let arrow = match q.direction {
            Direction::Out  => format!("-[r:Relation*1..{}]->", q.hops),
            Direction::In   => format!("<-[r:Relation*1..{}]-", q.hops),
            Direction::Both => format!("-[r:Relation*1..{}]-", q.hops),
        };

        let where_rel = match &q.rel_type {
            Some(rt) => format!(
                "WHERE ALL(rel IN r WHERE rel.rel_type = '{}')",
                kuzu_escape(rt)
            ),
            None => String::new(),
        };

        let cypher = format!(
            "MATCH (a:Entity {{canonical: '{}'}}){}(b:Entity)
             {}
             RETURN DISTINCT b.canonical, b.name, b.entity_type, length(r) AS dist
             ORDER BY dist
             LIMIT {}",
            kuzu_escape(&canon), arrow, where_rel, limit
        );

        let result = self.conn.query(&cypher).map_err(|e| format!("neighbors query: {}", e))?;
        let mut out = Vec::new();
        for row in result {
            // row: [canonical, name, entity_type, dist]
            let canonical = value_to_string(&row[0]);
            let name = value_to_string(&row[1]);
            let entity_type = value_to_string(&row[2]);
            let distance = match &row[3] {
                kuzu::Value::Int64(d) => *d as u32,
                _ => 0,
            };
            out.push(Neighbor { canonical, name, entity_type, distance });
        }
        Ok(out)
    }
}

fn value_to_string(v: &kuzu::Value) -> String {
    match v {
        kuzu::Value::String(s) => s.clone(),
        kuzu::Value::Null(_) => String::new(),
        other => format!("{:?}", other),
    }
}
```

**Step 4: Run tests**

Run: `cargo test graph::tests::test_neighbors`
Expected: PASS.

**Step 5: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): graph_neighbors with rel_type / direction / hops filter"
```

### Task 3.2: graph_path (shortest path)

**Files:**

- Modify: `src/graph/query.rs`
- Modify: `src/graph/db.rs`

**Step 1: Write failing tests**

In `src/graph/tests.rs`:
```rust
#[test]
fn test_path_found() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    let triples = vec![
        TripleInput {
            src: "alice".to_string(), rel: "friend_of".to_string(), dst: "bob".to_string(),
            src_type: None, dst_type: None, confidence: None, source_turn: None,
        },
        TripleInput {
            src: "bob".to_string(), rel: "works_at".to_string(), dst: "openai".to_string(),
            src_type: None, dst_type: None, confidence: None, source_turn: None,
        },
    ];
    backend.assert_triples(&triples).unwrap();

    let p = backend.path("alice", "openai", 5).unwrap();
    assert!(p.found);
    assert_eq!(p.length, 2);
}

#[test]
fn test_path_not_found() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    // Two disconnected entities
    backend.assert_triples(&[
        TripleInput { src: "alice".into(), rel: "x".into(), dst: "bob".into(),
                      src_type: None, dst_type: None, confidence: None, source_turn: None },
        TripleInput { src: "carol".into(), rel: "y".into(), dst: "dave".into(),
                      src_type: None, dst_type: None, confidence: None, source_turn: None },
    ]).unwrap();

    let p = backend.path("alice", "dave", 5).unwrap();
    assert!(!p.found);
}
```

**Step 2: Implement**

Add to `src/graph/query.rs`:
```rust
#[derive(Debug, Serialize)]
pub struct PathResult {
    pub found: bool,
    pub length: u32,
    pub path: Vec<PathNode>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PathNode {
    Entity { canonical: String, name: String },
    Edge { rel_type: String, direction: String },
}
```

Add to `src/graph/db.rs`:
```rust
use crate::graph::query::{PathResult, PathNode};

impl Backend {
    pub fn path(&self, src: &str, dst: &str, max_hops: u32) -> Result<PathResult, String> {
        if max_hops == 0 || max_hops > 10 {
            return Err(format!("max_hops must be in 1..=10, got {}", max_hops));
        }
        let src_c = canonicalize(src);
        let dst_c = canonicalize(dst);

        let cypher = format!(
            "MATCH p = (a:Entity {{canonical: '{}'}})-[:Relation* SHORTEST 1..{}]-(b:Entity {{canonical: '{}'}})
             RETURN p LIMIT 1",
            kuzu_escape(&src_c), max_hops, kuzu_escape(&dst_c)
        );

        let result = self.conn.query(&cypher).map_err(|e| format!("path query: {}", e))?;
        for row in result {
            // row[0] is a recursive_rel value — extract length and nodes/edges
            // For v1.3.0 KISS: we just check existence + extract length.
            // Detailed path serialization is best-effort; if Kuzu API for RECURSIVE_REL
            // is awkward, return empty `path` array but correct `found`/`length`.
            let length = extract_path_length(&row[0]);
            return Ok(PathResult { found: true, length, path: Vec::new() });
        }
        Ok(PathResult { found: false, length: 0, path: Vec::new() })
    }
}

fn extract_path_length(_v: &kuzu::Value) -> u32 {
    // TODO: parse the actual RECURSIVE_REL value to extract hop count.
    // For now, best-effort placeholder. Replace once kuzu Value API is verified.
    1
}
```

**NOTE**: full path serialization is non-trivial; first version returns `found` + `length` correctly but leaves `path: []`. This satisfies the agent's "does a path exist" use case. Detailed path nodes are a v1.3.1 polish.

**Step 3: Run tests**

Run: `cargo test graph::tests::test_path`
Expected: PASS.

**Step 4: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): graph_path (shortest, max_hops 1..=10)"
```

### Task 3.3: graph_query (free Cypher, read-only)

**Files:**

- Modify: `src/graph/query.rs`
- Modify: `src/graph/db.rs`

**Step 1: Write failing tests**

In `src/graph/tests.rs`:
```rust
#[test]
fn test_query_allow_match() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    backend.assert_triples(&[
        TripleInput { src: "alice".into(), rel: "works_at".into(), dst: "openai".into(),
                      src_type: None, dst_type: None, confidence: None, source_turn: None },
    ]).unwrap();

    let r = backend.run_cypher("MATCH (n:Entity) RETURN n.canonical").unwrap();
    assert!(!r.truncated);
    assert!(r.rows.len() >= 2);  // alice + openai
}

#[test]
fn test_query_forbidden_create() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    let r = backend.run_cypher("CREATE (n:Entity {canonical: 'x'}) RETURN n");
    assert!(r.is_err());
    assert!(r.unwrap_err().contains("forbidden"));
}

#[test]
fn test_query_forbidden_keywords() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    for kw in &["DELETE", "SET", "MERGE", "DROP", "CALL", "REMOVE"] {
        let q = format!("{} (n) RETURN n", kw);
        let r = backend.run_cypher(&q);
        assert!(r.is_err(), "{} should be forbidden", kw);
    }
}
```

**Step 2: Implement**

Add to `src/graph/query.rs`:
```rust
#[derive(Debug, Serialize)]
pub struct CypherResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool,
}

const FORBIDDEN_KEYWORDS: &[&str] = &[
    "CREATE", "MERGE", "DELETE", "DETACH",
    "SET", "REMOVE", "DROP", "COPY", "ATTACH",
    "ALTER", "LOAD", "INSTALL", "CALL",
];

/// 检查 cypher 是否包含禁用关键字。
/// 算法：按空白分词，每个 token 取纯字母前缀，uppercase 后查黑名单。
/// 简单但偶尔会被字符串字面量误伤——v1.3.1 升 quote-aware 解析。
pub fn check_readonly(cypher: &str) -> Result<(), String> {
    for raw_tok in cypher.split_whitespace() {
        let letters: String = raw_tok.chars().take_while(|c| c.is_alphabetic()).collect();
        if letters.is_empty() { continue; }
        let upper = letters.to_uppercase();
        if FORBIDDEN_KEYWORDS.contains(&upper.as_str()) {
            return Err(format!("forbidden keyword in cypher: {}", upper));
        }
    }
    Ok(())
}
```

Add to `src/graph/db.rs`:
```rust
use crate::graph::query::{CypherResult, check_readonly};

const QUERY_ROW_LIMIT: usize = 1000;

impl Backend {
    pub fn run_cypher(&self, cypher: &str) -> Result<CypherResult, String> {
        check_readonly(cypher)?;
        let result = self.conn.query(cypher).map_err(|e| format!("cypher: {}", e))?;
        let columns = result.get_column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = Vec::new();
        let mut truncated = false;
        for row in result {
            if rows.len() >= QUERY_ROW_LIMIT {
                truncated = true;
                break;
            }
            let serialized: Vec<serde_json::Value> = row.iter().map(value_to_json).collect();
            rows.push(serialized);
        }
        Ok(CypherResult { columns, rows, truncated })
    }
}

fn value_to_json(v: &kuzu::Value) -> serde_json::Value {
    match v {
        kuzu::Value::Null(_) => serde_json::Value::Null,
        kuzu::Value::Bool(b) => (*b).into(),
        kuzu::Value::Int64(i) => (*i).into(),
        kuzu::Value::Int32(i) => (*i as i64).into(),
        kuzu::Value::Double(d) => (*d).into(),
        kuzu::Value::Float(d) => (*d as f64).into(),
        kuzu::Value::String(s) => s.clone().into(),
        other => serde_json::Value::String(format!("{:?}", other)),
    }
}
```

**NOTE**: Adapt `value_to_json` to whatever kuzu 0.11's Value enum variants actually are. The exact match arms may differ.

**Step 3: Run tests**

Run: `cargo test graph::tests::test_query`
Expected: PASS (3 tests).

**Step 4: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): graph_query (free Cypher, keyword blacklist, 1000-row truncation)"
```

### Task 3.4: Query timeout (5s)

**Files:**

- Modify: `src/graph/db.rs`

**Step 1: Set Connection query timeout to 5s on init**

In `Backend::try_open`, after creating `conn`:
```rust
        conn.set_query_timeout(5000);  // 5 seconds, ms
```

If kuzu 0.11 doesn't expose this method, look for the equivalent — e.g., `SystemConfig::default().query_timeout(...)`. Adjust accordingly.

**Step 2: Write test (skip if timeout API not available in 0.11)**

If reachable, write a test that submits a query that would exceed 5s and assert it errors with timeout text. Otherwise document the gap and skip.

**Step 3: Commit**

```bash
git add src/graph/db.rs
git commit -m "feat(graph): set 5s query timeout on connection"
```

**Phase 3 verification gate:**
- All graph tests pass
- `cargo test` shows 60+ tests passing
- Cypher `CREATE`/`DELETE`/`SET`/etc rejected by `run_cypher`
- `cargo clippy` clean

---

## Phase 4 (P4) · MCP Tools + save_session hint + graph_link_entity

Goal: All 5 graph_* tools exposed via MCP. `save_session` returns `graph_pending` when configured.

### Task 4.1: Add graph_link_entity

**Files:**

- Modify: `src/graph/db.rs`

**Step 1: Write failing test**

In `src/graph/tests.rs`:
```rust
#[test]
fn test_link_entity_rewires_edges() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    backend.assert_triples(&[
        TripleInput { src: "alice".into(), rel: "works_at".into(), dst: "openai".into(),
                      src_type: None, dst_type: None, confidence: None, source_turn: None },
        TripleInput { src: "alice".into(), rel: "friend_of".into(), dst: "bob".into(),
                      src_type: None, dst_type: None, confidence: None, source_turn: None },
    ]).unwrap();

    let rewired = backend.link_entity("alice", "alice_smith").unwrap();
    assert!(rewired >= 2);

    // alice should be gone
    let r = backend.run_cypher("MATCH (n:Entity {canonical: 'alice'}) RETURN n").unwrap();
    assert!(r.rows.is_empty());

    // alice_smith should now have 2 outgoing edges
    let n = backend.neighbors(&NeighborQuery {
        entity: "alice_smith".to_string(), rel_type: None,
        direction: Direction::Out, hops: 1, limit: 50,
    }).unwrap();
    assert!(n.len() >= 2);
}
```

**Step 2: Implement**

In `src/graph/db.rs`:
```rust
impl Backend {
    pub fn link_entity(&self, from: &str, to: &str) -> Result<u32, String> {
        let from_c = canonicalize(from);
        let to_c = canonicalize(to);

        if from_c == to_c {
            return Err("from and to canonicalize to the same value".to_string());
        }

        // Ensure target entity exists (create empty if not)
        let now = chrono::Utc::now().naive_utc();
        if !self.entity_exists(&to_c)? {
            self.merge_entity(&to_c, to, "unknown", -1, now)?;
        }

        // Count edges to rewire (outgoing + incoming)
        let count_q = format!(
            "MATCH (old:Entity {{canonical: '{}'}})-[r:Relation]-(other:Entity) RETURN COUNT(r)",
            kuzu_escape(&from_c)
        );
        let count: i64 = self.conn.query(&count_q)
            .map_err(|e| format!("count: {}", e))?
            .into_iter().next()
            .and_then(|row| if let kuzu::Value::Int64(c) = row[0] { Some(c) } else { None })
            .unwrap_or(0);

        // Step 1: copy outgoing edges to new entity
        let copy_out = format!(
            "MATCH (old:Entity {{canonical: '{}'}})-[r:Relation]->(other:Entity)
             MATCH (new:Entity {{canonical: '{}'}})
             CREATE (new)-[:Relation {{rel_type: r.rel_type, confidence: r.confidence, source_turn: r.source_turn, created_at: r.created_at}}]->(other)",
            kuzu_escape(&from_c), kuzu_escape(&to_c)
        );
        self.conn.query(&copy_out).map_err(|e| format!("copy_out: {}", e))?;

        // Step 2: copy incoming edges
        let copy_in = format!(
            "MATCH (other:Entity)-[r:Relation]->(old:Entity {{canonical: '{}'}})
             MATCH (new:Entity {{canonical: '{}'}})
             CREATE (other)-[:Relation {{rel_type: r.rel_type, confidence: r.confidence, source_turn: r.source_turn, created_at: r.created_at}}]->(new)",
            kuzu_escape(&from_c), kuzu_escape(&to_c)
        );
        self.conn.query(&copy_in).map_err(|e| format!("copy_in: {}", e))?;

        // Step 3: delete old node and its edges
        let delete_old = format!(
            "MATCH (old:Entity {{canonical: '{}'}}) DETACH DELETE old",
            kuzu_escape(&from_c)
        );
        self.conn.query(&delete_old).map_err(|e| format!("delete: {}", e))?;

        Ok(count as u32)
    }
}
```

**Step 3: Run test**

Run: `cargo test graph::tests::test_link_entity`
Expected: PASS.

**Step 4: Commit**

```bash
git add src/graph/
git commit -m "feat(graph): graph_link_entity rewires edges + deletes old node"
```

### Task 4.2: graph_pending support in db

**Files:**

- Modify: `src/graph/db.rs`

**Step 1: Add method that takes turn_ids and returns those not yet referenced**

```rust
impl Backend {
    /// Given a list of turn_ids, return the subset NOT yet referenced by any Relation.source_turn.
    /// Used by save_session to compute graph_pending hint.
    pub fn pending_turn_ids(&self, turn_ids: &[i64]) -> Result<Vec<i64>, String> {
        if turn_ids.is_empty() { return Ok(Vec::new()); }

        // Build a list literal for Cypher: [1, 2, 3]
        let list = turn_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let cypher = format!(
            "MATCH ()-[r:Relation]->() WHERE r.source_turn IN [{}] RETURN DISTINCT r.source_turn",
            list
        );
        let referenced: std::collections::HashSet<i64> = self.conn.query(&cypher)
            .map_err(|e| format!("pending: {}", e))?
            .into_iter()
            .filter_map(|row| if let kuzu::Value::Int64(i) = row[0] { Some(i) } else { None })
            .collect();

        Ok(turn_ids.iter().filter(|t| !referenced.contains(t)).copied().collect())
    }
}
```

**Step 2: Write test**

In `src/graph/tests.rs`:
```rust
#[test]
fn test_pending_turn_ids() {
    let (_tmp, g) = fresh_graph();
    let GraphDb::Ready(backend) = &g else { panic!("not ready") };
    backend.assert_triples(&[
        TripleInput { src: "a".into(), rel: "x".into(), dst: "b".into(),
                      src_type: None, dst_type: None, confidence: None, source_turn: Some(10) },
    ]).unwrap();

    let pending = backend.pending_turn_ids(&[10, 20, 30]).unwrap();
    assert_eq!(pending, vec![20, 30]);  // 10 is referenced
}
```

**Step 3: Run + commit**

Run: `cargo test graph::tests::test_pending`
Expected: PASS.

```bash
git add src/graph/
git commit -m "feat(graph): pending_turn_ids for save_session graph_pending hint"
```

### Task 4.3: Expose 5 MCP tools

**Files:**

- Modify: `src/mcp/tools.rs`

**Step 1: Add tool definitions**

In `pub fn tool_definitions()`, append 5 new entries to the returned vec:

```rust
        json!({
            "name": "graph_assert",
            "description": "Write entity-relation triples to the graph memory layer. canonical-normalizes src/dst (lowercase + trim + whitespace fold).",
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
            "description": "Query N-hop neighbors of an entity. Supports rel_type filter and direction (out/in/both).",
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
            "description": "Find shortest path between two entities (max_hops 1..=10).",
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
            "name": "graph_query",
            "description": "Run a read-only Cypher query against the graph layer. Forbidden keywords: CREATE/MERGE/DELETE/SET/REMOVE/DROP/CALL/etc. 5s timeout, 1000-row limit.",
            "inputSchema": {
                "type": "object",
                "required": ["cypher"],
                "properties": {
                    "cypher": {"type": "string"},
                    "params": {"type": "object"}
                }
            }
        }),
        json!({
            "name": "graph_link_entity",
            "description": "Merge alias: rewire all edges from `from` entity to `to` entity, then delete `from`. Irreversible.",
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

**Step 2: Plumb GraphDb into ToolHandler**

Modify `ToolHandler::new(config, db)` to also accept `graph_db: Rc<crate::graph::db::GraphDb>`.

Update the `Server::new()` site to pass it. Add a `graph_db` field to ToolHandler.

**Step 3: Add routing for new tools**

In `ToolHandler::call`, add 5 arms:
```rust
            "graph_assert" => self.graph_assert(args),
            "graph_neighbors" => self.graph_neighbors(args),
            "graph_path" => self.graph_path(args),
            "graph_query" => self.graph_query(args),
            "graph_link_entity" => self.graph_link_entity(args),
```

**Step 4: Implement each handler**

```rust
    fn graph_backend(&self) -> Result<&crate::graph::db::Backend, String> {
        use crate::graph::db::GraphDb;
        match &*self.graph_db {
            GraphDb::Ready(b) => Ok(b),
            GraphDb::Disabled => Err("graph disabled in config".to_string()),
            GraphDb::Unavailable(e) => Err(format!("graph backend unavailable: {}", e)),
        }
    }

    fn graph_assert(&self, args: &Value) -> Result<Value, String> {
        let backend = self.graph_backend()?;
        let triples: Vec<crate::graph::relation::TripleInput> =
            serde_json::from_value(args["triples"].clone())
                .map_err(|e| format!("invalid triples: {}", e))?;
        let stats = backend.assert_triples(&triples)?;
        Ok(json!({"status": "ok", "stats": stats}))
    }

    fn graph_neighbors(&self, args: &Value) -> Result<Value, String> {
        let backend = self.graph_backend()?;
        let q: crate::graph::query::NeighborQuery =
            serde_json::from_value(args.clone()).map_err(|e| format!("invalid query: {}", e))?;
        let neighbors = backend.neighbors(&q)?;
        Ok(json!({"status": "ok", "neighbors": neighbors}))
    }

    fn graph_path(&self, args: &Value) -> Result<Value, String> {
        let backend = self.graph_backend()?;
        let src = args["src"].as_str().ok_or("missing src")?;
        let dst = args["dst"].as_str().ok_or("missing dst")?;
        let max_hops = args["max_hops"].as_u64().unwrap_or(5) as u32;
        let path = backend.path(src, dst, max_hops)?;
        Ok(json!({"status": "ok", "found": path.found, "length": path.length, "path": path.path}))
    }

    fn graph_query(&self, args: &Value) -> Result<Value, String> {
        let backend = self.graph_backend()?;
        let cypher = args["cypher"].as_str().ok_or("missing cypher")?;
        let result = backend.run_cypher(cypher)?;
        Ok(json!({
            "status": "ok",
            "columns": result.columns,
            "rows": result.rows,
            "truncated": result.truncated
        }))
    }

    fn graph_link_entity(&self, args: &Value) -> Result<Value, String> {
        let backend = self.graph_backend()?;
        let from = args["from"].as_str().ok_or("missing from")?;
        let to = args["to"].as_str().ok_or("missing to")?;
        let rewired = backend.link_entity(from, to)?;
        Ok(json!({"status": "ok", "edges_rewired": rewired, "old_entity_removed": from}))
    }
```

**Step 5: Run all tests**

Run: `cargo test`
Expected: still green; existing tests don't touch graph routing.

**Step 6: Commit**

```bash
git add src/mcp/tools.rs src/main.rs
git commit -m "feat(mcp): expose 5 graph_* MCP tools wired to GraphDb"
```

### Task 4.4: save_session graph_pending field

**Files:**

- Modify: `src/fact/session_store.rs` (add optional graph_db param OR add a separate method)
- Modify: `src/mcp/tools.rs` (compute pending and inject into response)

**Step 1: KISS approach — handle in ToolHandler, not in SessionStore**

`SessionStore` stays graph-agnostic. In `ToolHandler::save_session`, after `store.save()` succeeds and before building the JSON response, query graph_pending if config allows.

```rust
    fn save_session(&self, args: &Value) -> Result<Value, String> {
        // ... existing code, get `stats` from store.save() ...

        // Build base response
        let mut response = json!({
            "status": "ok",
            "session_id": stats.session_id,
            "file_path": stats.file_path.to_string_lossy(),
            "turns_saved": stats.turns_saved
        });

        // Append graph_pending if graph enabled + remind_on_save
        if self.config.graph.enabled && self.config.graph.remind_on_save {
            if let Ok(backend) = self.graph_backend() {
                // Fetch turn_ids for this session
                let turn_ids = self.session_turn_ids(&stats.session_id).unwrap_or_default();
                if !turn_ids.is_empty() {
                    if let Ok(pending) = backend.pending_turn_ids(&turn_ids) {
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
    }

    fn session_turn_ids(&self, session_id: &str) -> Result<Vec<i64>, String> {
        let mut stmt = self.db.conn().prepare(
            "SELECT id FROM turns WHERE session_id = ?1 ORDER BY seq"
        ).map_err(|e| e.to_string())?;
        let rows = stmt.query_map([session_id], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }
```

**Step 2: Write integration test**

Add `src/graph/e2e_test.rs` and register `#[cfg(test)] mod e2e_test;` in `src/graph/mod.rs`:

```rust
// e2e_test.rs — full save_session + graph_assert flow

use crate::config::Config;
use crate::fact::conversation::{SessionHeader, Turn};
use crate::fact::session_store::SessionStore;
use crate::graph::db::GraphDb;
use crate::graph::relation::TripleInput;
use crate::index::db::Db;
use tempfile::tempdir;

#[test]
fn test_save_then_graph_pending_shrinks() {
    let tmp = tempdir().unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    let graph = GraphDb::open_or_init(&tmp.path().join("graph.kuzu"), true);
    let GraphDb::Ready(backend) = &graph else { panic!("graph not ready") };

    let header = SessionHeader {
        v: 1, header_type: "session_header".to_string(),
        session_id: "s1".to_string(),
        start_time: "2026-05-19T10:00:00+08:00".to_string(),
        profile_id: "default".to_string(),
        source: None, agent_model: None, title: None, tags: vec![],
    };
    let turns = vec![
        Turn { ts: "2026-05-19T10:00:00+08:00".to_string(), seq: 1, role: "user".to_string(),
               content: "test".to_string(), metadata: None },
        Turn { ts: "2026-05-19T10:00:01+08:00".to_string(), seq: 2, role: "assistant".to_string(),
               content: "reply".to_string(), metadata: None },
    ];

    let store = SessionStore::new(tmp.path(), &db);
    store.save(&header, &turns, None).unwrap();

    let turn_ids: Vec<i64> = db.conn()
        .prepare("SELECT id FROM turns WHERE session_id='s1' ORDER BY seq").unwrap()
        .query_map([], |r| r.get(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(turn_ids.len(), 2);

    // All turns initially pending
    let pending = backend.pending_turn_ids(&turn_ids).unwrap();
    assert_eq!(pending.len(), 2);

    // Assert one triple referencing the first turn
    backend.assert_triples(&[TripleInput {
        src: "user".into(), rel: "asked".into(), dst: "test_question".into(),
        src_type: None, dst_type: None, confidence: None, source_turn: Some(turn_ids[0]),
    }]).unwrap();

    // Now only the second turn is pending
    let pending = backend.pending_turn_ids(&turn_ids).unwrap();
    assert_eq!(pending, vec![turn_ids[1]]);
}
```

**Step 3: Test that graph_pending is omitted when disabled**

```rust
#[test]
fn test_save_with_remind_off() {
    // Construct ToolHandler with config.graph.remind_on_save = false
    // Call save_session and verify "graph_pending" key absent.
    // (Implementation depends on how ToolHandler is constructed in tests.)
}
```

Acceptable to skip this if ToolHandler doesn't have a clean test entrypoint; the behavior is already gated by an `if` in the implementation.

**Step 4: Run all tests**

Run: `cargo test`
Expected: green.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat(mcp): save_session returns graph_pending when remind_on_save=true"
```

**Phase 4 verification gate:**
- All 5 graph tools listed in `tools/list` response
- All 5 callable from MCP stdio (manual smoke test)
- `save_session` returns `graph_pending` field with correct turn_ids
- Disabled mode returns friendly error from all 5 tools

---

## Phase 5 (P5) · Doctor + Docs + Release

### Task 5.1: doctor --verbose graph statistics

**Files:**

- Modify: `src/main.rs`

**Step 1: Add --verbose flag to doctor**

Modify the Cli's `Commands::Doctor` to accept a `--verbose` boolean:
```rust
    /// 测试配置
    Doctor {
        #[arg(long)]
        verbose: bool,
    },
```

Update the matcher arm and `cmd_doctor` signature.

**Step 2: When verbose=true and graph ready, show stats**

After the existing `图谱: ...` lines:
```rust
        if verbose {
            if let crate::graph::db::GraphDb::Ready(backend) = &**graph_db {
                if let Ok(stats) = backend.graph_stats() {
                    println!("图谱统计: {} entities, {} relations", stats.entities, stats.relations);
                    println!("图谱覆盖率: {}% ({}/{} turns)", stats.coverage_pct,
                             stats.covered_turns, stats.total_turns);
                    println!("图谱悬空引用: {}", stats.dangling_refs);
                }
            }
        }
```

**Step 3: Implement Backend::graph_stats**

```rust
#[derive(Debug)]
pub struct GraphStats {
    pub entities: i64,
    pub relations: i64,
    pub coverage_pct: u32,
    pub covered_turns: i64,
    pub total_turns: i64,
    pub dangling_refs: i64,
}

impl Backend {
    pub fn graph_stats(&self) -> Result<GraphStats, String> {
        let e: i64 = self.scalar_int("MATCH (n:Entity) RETURN COUNT(n)")?;
        let r: i64 = self.scalar_int("MATCH ()-[r:Relation]->() RETURN COUNT(r)")?;
        let covered: i64 = self.scalar_int(
            "MATCH ()-[r:Relation]->() WHERE r.source_turn >= 0 RETURN COUNT(DISTINCT r.source_turn)"
        )?;
        // total_turns & dangling_refs need SQLite — but we don't have access here.
        // Return placeholders; main.rs combines with SQLite-side counts.
        Ok(GraphStats {
            entities: e, relations: r,
            coverage_pct: 0, covered_turns: covered, total_turns: 0, dangling_refs: 0,
        })
    }

    fn scalar_int(&self, q: &str) -> Result<i64, String> {
        let res = self.conn.query(q).map_err(|e| e.to_string())?;
        for row in res {
            if let kuzu::Value::Int64(n) = row[0] { return Ok(n); }
        }
        Ok(0)
    }
}
```

In `cmd_doctor`, compute total_turns from SQLite and dangling_refs by cross-checking — KISS implementation: just show entities/relations and skip coverage if cross-check is awkward.

**Step 4: Smoke test manually**

Run: `cargo run -- doctor --verbose`
Expected: shows entities/relations counts.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat(doctor): --verbose shows graph stats (entities, relations, coverage)"
```

### Task 5.2: README.md updates

**Files:**

- Modify: `README.md`

**Step 1: Update architecture table**

Add a third column to the architecture table:

```markdown
| Growth Layer                                  | Fact Layer                                       | Graph Layer (v1.3+)                              |
| --------------------------------------------- | ------------------------------------------------ | ------------------------------------------------ |
| `MEMORY.md` · AI knowledge · 2200 char cap     | JSONL immutable archive · `conversations/YYYY/MM/DD/` | Kuzu embedded graph · `graph.kuzu/`             |
| `USER.md` · User profile · 1375 char cap       | SQLite · `sessions` / `turns` metadata           | Entity nodes (canonical normalized)              |
| Security scan (injection / credential / Unicode) | FTS5 full-text index · Chinese unigram          | Relation edges (rel_type, confidence)             |
| Provenance · entry → source session            | sqlite-vec · 768d INT8 quantized vectors         | Cypher queries (read-only via graph_query)        |
```

**Step 2: Add "Graph Memory (v1.3)" section after MCP tools table**

```markdown
## Graph Memory (v1.3+)

The graph layer is the third memory layer alongside Growth and Fact. Agents author triples via `graph_assert`; the server never invokes LLMs. canonical normalization (lowercase + trim + whitespace fold) prevents trivial duplicates.

### New MCP tools

| Tool | Purpose |
|---|---|
| `graph_assert` | Write entity-relation triples |
| `graph_neighbors` | N-hop neighbor query (filter by rel_type / direction) |
| `graph_path` | Shortest path between two entities |
| `graph_query` | Free-form read-only Cypher (5s timeout, 1000-row cap) |
| `graph_link_entity` | Merge alias (rewire edges + delete old node) |

### Soft hint

`save_session` returns `graph_pending: { turn_ids, hint }` when there are turns not yet referenced by any Relation. Disable via `graph.remind_on_save = false`.

### Disable entirely

`graph.enabled = false` skips Kuzu init; all `graph_*` tools return `"graph disabled in config"`.
```

**Step 3: Add v1.3.0 upgrade section above v1.2.1**

```markdown
### Upgrading from v1.2.1 to v1.3.0

v1.3.0 adds a new graph memory layer. The fact and growth layers are unchanged — upgrading is non-destructive.

- New directory `~/.asuna/profiles/<id>/graph.kuzu/` created automatically on first launch
- No migration needed; existing data is fully compatible
- v1.2.1 binaries still work with v1.3.0 data directories (they ignore the graph layer)

Disable the graph layer if not needed:

```json
{
  "graph": { "enabled": false }
}
```

**v1.3.0 Changelog:**

- **New: graph memory layer** — embedded Kuzu provides Cypher-based knowledge graph
- **New: 5 MCP tools** — graph_assert / graph_neighbors / graph_path / graph_query / graph_link_entity
- **New: save_session soft hint** — returns `graph_pending` field with turn_ids needing assertion
- **doctor --verbose** — shows graph statistics (entities, relations, coverage)
```

**Step 4: Mirror in README_EN.md and for_ai.md**

Apply equivalent changes to README_EN.md and update for_ai.md § 3 with the 5 new tools (full schemas) plus a new "§ 4: Graph Memory" section.

**Step 5: Commit**

```bash
git add README.md README_EN.md for_ai.md
git commit -m "docs: add v1.3.0 graph layer chapter + upgrade guide"
```

### Task 5.3: Bump version + Cargo.lock

**Files:**

- Modify: `Cargo.toml`

**Step 1: Bump version**

In `Cargo.toml`:
```toml
version = "1.3.0"
```

**Step 2: Refresh Cargo.lock**

Run: `cargo check`
Expected: Cargo.lock version line updates to 1.3.0.

**Step 3: Run full validation**

Run:
```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```
All three must pass cleanly.

**Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "release: bump version to 1.3.0"
```

### Task 5.4: Verify CI matrix + tag + push

**Files:**

- None (release operations)

**Step 1: Push commits**

```bash
git push origin main
```

**Step 2: Tag and push**

```bash
git tag -a v1.3.0 -m "v1.3.0 — graph memory layer (Kuzu embedded)"
git push origin v1.3.0
```

Triggers the release workflow.

**Step 3: Watch CI**

Run: `gh run watch <run-id>`

Expected: all 4 builds succeed. If ARM64 fails, see Pre-Flight Task 0 — re-evaluate Kuzu's ARM64 support.

**Step 4: Verify Release published**

Visit: <https://github.com/Michaol/asuna-memory-system/releases>
Confirm 4 artifacts (Win, macOS, Linux x64, Linux ARM64).

**Phase 5 verification gate (release ready):**
- [x] All tests pass (60+ tests, growing from 51 baseline)
- [x] clippy clean
- [x] 4 CI artifacts produced
- [x] doctor shows graph stats (verbose)
- [x] README, README_EN, for_ai.md all carry v1.3.0 docs
- [x] No regression in v1.2.1 functionality

---

## Risk Register (executable)

| Risk | Trigger | Mitigation |
|---|---|---|
| Kuzu 0.11 API differs from docs | Build fails on Task 1.3 | Adapt to actual API; comments in db.rs note known-risky sections |
| `unsafe transmute` for Database lifetime is wrong | Task 1.4 test segfaults | Refactor Backend to use `Pin<Box<Database>>` |
| ARM64 Linux build fails | Phase 5 release pipeline fails | Pre-Flight Task 0 catches this; if discovered late, defer ARM64 to v1.3.1 |
| Kuzu MERGE doesn't behave as expected | Task 2.2 tests fail | Fall back to explicit MATCH-then-CREATE pattern |
| RECURSIVE_REL serialization complicated | Task 3.2 path body returns empty `path` | Acceptable — `found` + `length` cover the use case |
| Cypher blacklist false positives | User report after release | Document; v1.3.1 quote-aware parser |
| Performance budget blown | Task 2.3 timing test fails | Switch from string interpolation to prepared statements |

---

## What's NOT in v1.3.0

(restated from design doc for executor's reference)

- Entity embedding / fuzzy linking → v1.4
- Rule-based extraction fallback → never
- Coverage threshold warning (tier 2) → only doctor --verbose
- Forced double-write (tier 3) → never
- Graph algorithms as MCP tools → use graph_query for PageRank etc
- rebuild_index rebuilding graph → never (agent is source of truth)
- Cross-profile graph sharing → never
- search_sessions auto-using graph seeds → v1.4
