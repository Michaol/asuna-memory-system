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
