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
///
/// `relations_updated` 计数命中已有三元组的次数（无论 `MAX(confidence)` 是否实际改变了存储值）。
/// 同理 `entities_updated` 计数命中已有 canonical 的次数（仅刷新 last_seen）。
#[derive(Debug, Default, Serialize)]
pub struct AssertStats {
    pub entities_created: u32,
    pub entities_updated: u32,
    pub relations_created: u32,
    pub relations_updated: u32,
}

/// 图谱写入：在单个 SQLite 事务内执行
///
/// 行为：
/// - canonical 化 src/dst（lowercase + trim + 折空白）
/// - entities：不存在则 INSERT；存在则只刷新 last_seen（name/entity_type 保留首次写入版本）
/// - relations：三元组（src_canonical, rel_type, dst_canonical）唯一；
///   存在时 confidence = MAX(existing, new)，其他字段不覆盖
/// - 整体单事务；任意错误 ROLLBACK
pub fn assert_triples(db: &Db, triples: &[TripleInput]) -> anyhow::Result<AssertStats> {
    // Validate first (before opening transaction)
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

    // Use unchecked_transaction for RAII-based rollback on error/panic.
    // unchecked_transaction uses DEFERRED by default; for write-heavy workloads,
    // we manually upgrade to IMMEDIATE via PRAGMA or accept the minor risk of
    // SQLITE_BUSY on concurrent writers (single-writer in practice via Mutex).
    let tx = conn.unchecked_transaction()?;

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

        // MERGE src entity（单语句 + 一次 changes() 判断 created vs updated）
        let src_created =
            upsert_entity(conn, &src_canon, &t.src, src_type, source_turn, now)?;
        if src_created {
            stats.entities_created += 1;
        } else {
            stats.entities_updated += 1;
        }

        // MERGE dst entity（src == dst 时跳过，避免重复计数）
        if dst_canon != src_canon {
            let dst_created =
                upsert_entity(conn, &dst_canon, &t.dst, dst_type, source_turn, now)?;
            if dst_created {
                stats.entities_created += 1;
            } else {
                stats.entities_updated += 1;
            }
        }

        // MERGE relation
        let rel_created =
            upsert_relation(conn, &src_canon, &t.rel, &dst_canon, conf, source_turn, now)?;
        if rel_created {
            stats.relations_created += 1;
        } else {
            stats.relations_updated += 1;
        }
    }

    // Explicitly commit; if we get here without error, all operations succeeded.
    // If any operation above returned Err, the `?` operator exits early and
    // the Transaction's Drop will automatically ROLLBACK.
    tx.commit()?;
    Ok(stats)
}

/// 写入 entity；存在则仅刷新 last_seen，name/entity_type/source_turn 保留首次写入版本。
///
/// 实现：`INSERT OR IGNORE` 优先（重复时静默丢弃）→ `conn.execute()` 返回值（即 changes）
/// 判断是否真的插入了行 → 若未插入则单独发一次 `UPDATE last_seen`。
///
/// 性能：新建场景 1 次 SQL（vs v1.3.0 之前的 exists+INSERT 2 次）；
/// 已存在场景 2 次 SQL（vs 之前的 exists+UPDATE 2 次，持平）。
/// 写多于改的 agent 场景整体减半。
///
/// 返回 `true` = 新建；`false` = 更新已有。
fn upsert_entity(
    conn: &rusqlite::Connection,
    canonical: &str,
    name: &str,
    entity_type: &str,
    source_turn: Option<i64>,
    now: i64,
) -> anyhow::Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO entities
         (canonical, name, entity_type, first_seen, last_seen, source_turn)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![canonical, name, entity_type, now, source_turn],
    )?;
    if inserted == 0 {
        conn.execute(
            "UPDATE entities SET last_seen = ?1 WHERE canonical = ?2",
            rusqlite::params![now, canonical],
        )?;
        Ok(false)
    } else {
        Ok(true)
    }
}

/// 写入 relation；存在则 confidence = MAX(existing, new)，其他字段不覆盖。
///
/// 实现同 `upsert_entity`：先 `INSERT OR IGNORE` 试图插入；若被忽略则发 UPDATE 提升 confidence。
///
/// 返回 `true` = 新建；`false` = 更新已有。
fn upsert_relation(
    conn: &rusqlite::Connection,
    src: &str,
    rel_type: &str,
    dst: &str,
    confidence: f64,
    source_turn: Option<i64>,
    now: i64,
) -> anyhow::Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO relations
         (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![src, rel_type, dst, confidence, source_turn, now],
    )?;
    if inserted == 0 {
        // 已存在：confidence 取 max
        conn.execute(
            "UPDATE relations
             SET confidence = MAX(confidence, ?1)
             WHERE src_canonical = ?2 AND rel_type = ?3 AND dst_canonical = ?4",
            rusqlite::params![confidence, src, rel_type, dst],
        )?;
        Ok(false)
    } else {
        Ok(true)
    }
}

/// 把 `from` 实体的所有边重定向到 `to`，然后删除 `from` 节点。
/// 单事务；如果产生重复边则保留 `to` 侧现有边（confidence 不做 MAX 合并，v1.4 再优化）。
///
/// 返回值：重定向**前** `from` 实体上的边数（含被 INSERT OR IGNORE 丢弃的重复，
/// 含将被自环过滤掉的边）。MCP 响应中称为 `edges_rewired`，但严格来说是
/// "因 link 操作触发处理的边数"。
///
/// 行为：
/// - canonical 化 from/to；为空或同名时报错
/// - 若 `to` 不存在则创建空 entity（entity_type='unknown'，name 用调用方原始字面）
/// - 若 `from` 不存在则静默 no-op，返回 0
/// - 复制 from 的出边到 to（INSERT OR IGNORE 合并重复，过滤 dst==to 防自环）
/// - 复制 from 的入边到 to（同上，过滤 src==to 防自环）
/// - `from` 上的自环边 `(from, rel, from)` 会被 CASCADE 一并删除，**不会**重写为
///   `(to, rel, to)`（v1.3.0 设计取舍）
/// - DELETE entities WHERE canonical=from（CASCADE 清理任何剩余边）
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

    let tx = conn.unchecked_transaction()?;

    // 确保 to 实体存在（不存在则创建为 unknown 类型）
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

    // 统计要重定向的边数（in + out，剔除自环重复）
    let edge_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM relations
         WHERE src_canonical = ?1 OR dst_canonical = ?1",
        rusqlite::params![from_c],
        |r| r.get(0),
    )?;

    // 复制出边 (from→X) → (to→X)，跳过自环（dst == to）；
    // 重复时 INSERT OR IGNORE 保留 to 侧现有边（confidence 不 MAX 合并，v1.4 再优化）
    conn.execute(
        "INSERT OR IGNORE INTO relations
         (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
         SELECT ?1, rel_type, dst_canonical, confidence, source_turn, created_at
         FROM relations WHERE src_canonical = ?2 AND dst_canonical <> ?1",
        rusqlite::params![to_c, from_c],
    )?;

    // 复制入边 (X→from) → (X→to)，跳过自环（src == to）；INSERT OR IGNORE 同上
    conn.execute(
        "INSERT OR IGNORE INTO relations
         (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
         SELECT src_canonical, rel_type, ?1, confidence, source_turn, created_at
         FROM relations WHERE dst_canonical = ?2 AND src_canonical <> ?1",
        rusqlite::params![to_c, from_c],
    )?;

    // 删除 from 实体，CASCADE 会清理所有剩余边（包括没被复制成功的）
    conn.execute(
        "DELETE FROM entities WHERE canonical = ?1",
        rusqlite::params![from_c],
    )?;

    tx.commit()?;
    Ok(edge_count as u32)
}

/// 清理悬空 source_turn 引用：把 relations.source_turn 指向已被删除 turn 的字段置 NULL。
///
/// 不删除 relation 本身——三元组的语义价值独立于来源 turn 的存活状态。
/// 仅清理失效引用，让 `doctor --verbose` 的悬空计数归零。
///
/// 返回值：被清理的 relation 行数（即原本 source_turn 非 NULL 但已悬空的行数）。
/// entities.source_turn 也同步清理但不计入返回值。
pub fn prune_dangling_refs(db: &Db) -> anyhow::Result<u32> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;

    let rel_pruned = conn.execute(
        "UPDATE relations
         SET source_turn = NULL
         WHERE source_turn IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.id = source_turn)",
        [],
    )?;
    conn.execute(
        "UPDATE entities
         SET source_turn = NULL
         WHERE source_turn IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.id = source_turn)",
        [],
    )?;

    tx.commit()?;
    Ok(rel_pruned as u32)
}
