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
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("graph assert 回滚失败: {} (原始错误: {})", rb, e);
            }
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

#[allow(clippy::too_many_arguments)]
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

/// 把 `from` 实体的所有边重定向到 `to`，然后删除 `from` 节点。
/// 单事务；如果产生重复边则保留 `to` 侧（IGNORE 重复 INSERT）。
/// 返回重定向前 `from` 实体上的边数（in + out）。
///
/// 行为：
/// - canonical 化 from/to
/// - from == to canonical → 报错（无意义操作）
/// - 若 to 不存在，先创建一个空 entity（entity_type='unknown'）
/// - 复制 from 的所有出边到 to（INSERT OR IGNORE 自动合并重复）
/// - 复制 from 的所有入边到 to（同上）
/// - DELETE entities WHERE canonical = from（CASCADE 清理任何剩余边）
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

        // 复制出边 (from→X) → (to→X)，跳过自环（dst == to）；INSERT OR IGNORE 自动合并重复
        conn.execute(
            "INSERT OR IGNORE INTO relations
             (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
             SELECT ?1, rel_type, dst_canonical, confidence, source_turn, created_at
             FROM relations WHERE src_canonical = ?2 AND dst_canonical <> ?1",
            rusqlite::params![to_c, from_c],
        )?;

        // 复制入边 (X→from) → (X→to)，跳过自环（src == to）
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

        Ok(edge_count as u32)
    })();

    match result {
        Ok(n) => {
            conn.execute_batch("COMMIT")?;
            Ok(n)
        }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("graph link_entity 回滚失败: {} (原始错误: {})", rb, e);
            }
            Err(e)
        }
    }
}
