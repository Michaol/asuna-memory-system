//! 图谱写路径：MERGE 三元组到 entities + relations。

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
/// - rel_type 仅 trim 两端空白（M7：不折叠大小写/内部空白，关系标签由 agent 定义）
/// - entities：不存在则 INSERT；存在则只刷新 last_seen（name/entity_type 保留首次写入版本）
/// - relations：三元组（src_canonical, rel_type, dst_canonical）唯一；
///   存在时 confidence = MAX(existing, new)，其他字段不覆盖
/// - 整体单事务；任意错误 ROLLBACK
pub fn assert_triples(db: &Db, triples: &[TripleInput]) -> anyhow::Result<AssertStats> {
    // Validate first (before opening transaction)
    validate_triples(triples)?;

    let conn = db.conn();
    let mut stats = AssertStats::default();
    let now = time::now_unix_ms();

    let tx = db.immediate_tx()?;

    for t in triples {
        merge_triple(conn, t, now, &mut stats)?;
    }

    tx.commit()?;
    tracing::debug!(
        "assert_triples: {} triples → entities +{}/upd{} , relations +{}/upd{}",
        triples.len(),
        stats.entities_created,
        stats.entities_updated,
        stats.relations_created,
        stats.relations_updated
    );
    Ok(stats)
}

/// 事务前校验：非空批、src/rel/dst trim 后非空、confidence（若提供）在 [0.0, 1.0] 内。
fn validate_triples(triples: &[TripleInput]) -> anyhow::Result<()> {
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
            if !c.is_finite() || !(0.0..=1.0).contains(&c) {
                anyhow::bail!("triple[{}].confidence={} out of range [0.0, 1.0]", i, c);
            }
        }
    }
    Ok(())
}

/// 处理单条 triple：canonical 化并 MERGE src/dst entity 与 relation，累计计数到 stats。
fn merge_triple(
    conn: &rusqlite::Connection,
    t: &TripleInput,
    now: i64,
    stats: &mut AssertStats,
) -> anyhow::Result<()> {
    // H2: session 键是无损写入的，必须经解析器（先精确后规范化），否则会新建一个
    // 折叠后的 disconnected 实体，而不是挂到真实会话节点上。
    let src_canon = crate::graph::query::resolve_entity_canonical(conn, &t.src)?;
    let dst_canon = crate::graph::query::resolve_entity_canonical(conn, &t.dst)?;
    if src_canon.is_empty() || dst_canon.is_empty() {
        anyhow::bail!("triple resolves to empty canonical after normalization");
    }

    let conf = t.confidence.unwrap_or(0.5);
    let src_name = t.src.trim();
    let dst_name = t.dst.trim();
    let src_type = t
        .src_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let dst_type = t
        .dst_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let source_turn = t.source_turn;

    // MERGE src entity（单语句 + 一次 changes() 判断 created vs updated）
    let src_created = upsert_entity(conn, &src_canon, src_name, src_type, source_turn, now)?;
    if src_created {
        stats.entities_created += 1;
    } else {
        stats.entities_updated += 1;
    }

    // MERGE dst entity（src == dst 时跳过，避免重复计数）
    if dst_canon != src_canon {
        let dst_created = upsert_entity(conn, &dst_canon, dst_name, dst_type, source_turn, now)?;
        if dst_created {
            stats.entities_created += 1;
        } else {
            stats.entities_updated += 1;
        }
    }

    // MERGE relation（M7: rel 在此汇聚点 trim，MCP/HTTP 双入口共用；
    // "knows " 与 "knows" 否则成 PK 不同的两条边，MAX 合并被拆散）
    let rel_type = t.rel.trim();
    let rel_created = upsert_relation(
        conn,
        &src_canon,
        rel_type,
        &dst_canon,
        conf,
        source_turn,
        now,
    )?;
    if rel_created {
        stats.relations_created += 1;
    } else {
        stats.relations_updated += 1;
    }
    Ok(())
}

/// 写入 entity；存在则仅刷新 last_seen，name/entity_type/source_turn 保留首次写入版本。
///
/// last_seen 目前是 dormant 列（全仓只写不读，见 mod.rs）：断言层在此刷新它，
/// 派生层（graph_integration）的 INSERT OR IGNORE 不刷新——差异有意保留，
/// 不为无读者的列给派生热路径多加一次 UPDATE。
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
    let mut insert_stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO entities
         (canonical, name, entity_type, first_seen, last_seen, source_turn)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
    )?;
    let inserted = insert_stmt.execute(rusqlite::params![
        canonical,
        name,
        entity_type,
        now,
        source_turn
    ])?;
    if inserted == 0 {
        let mut update_stmt =
            conn.prepare_cached("UPDATE entities SET last_seen = ?1 WHERE canonical = ?2")?;
        update_stmt.execute(rusqlite::params![now, canonical])?;
        Ok(false)
    } else {
        Ok(true)
    }
}

/// 写入 relation；存在则 confidence = MAX(existing, new)，其他字段不覆盖。
///
/// 不写 relation_kind：走 schema 默认 'asserted'（断言层）。读路径只遍历
/// 'asserted'/'derived' 白名单（策略见 mod.rs），故本函数的边恒可见。
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
    let mut insert_stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO relations
         (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let inserted = insert_stmt.execute(rusqlite::params![
        src,
        rel_type,
        dst,
        confidence,
        source_turn,
        now
    ])?;
    if inserted == 0 {
        // 已存在：confidence 取 max
        let mut update_stmt = conn.prepare_cached(
            "UPDATE relations
             SET confidence = MAX(confidence, ?1)
             WHERE src_canonical = ?2 AND rel_type = ?3 AND dst_canonical = ?4",
        )?;
        update_stmt.execute(rusqlite::params![confidence, src, rel_type, dst])?;
        Ok(false)
    } else {
        Ok(true)
    }
}

/// 把 `from` 实体的所有边重定向到 `to`，然后删除 `from` 节点。
/// 单事务；如果产生重复边则合并为 `MAX(confidence)`，同时保留原有的 `relation_kind`。
///
/// 返回值：重定向**前** `from` 实体上的边数（含冲突时被合并的重复边，
/// 含将被自环过滤掉的边）。MCP 响应中称为 `edges_rewired`，但严格来说是
/// "因 link 操作触发处理的边数"。
///
/// 行为：
/// - canonical 化 from/to；为空或同名时报错
/// - 若 `from` 不存在则直接静默 no-op，返回 0（不创建孤儿 `to` 实体）
/// - 若 `from` 存在且 `to` 不存在，则创建继承 `from` 属性（`entity_type`、`source_turn`）
///   的实体 `to`（`name` 用调用方原始字面）。**不继承 `memory_atom_id`**（W17）：
///   合并目标是共享概念节点，原子驱逐的 `DELETE … WHERE memory_atom_id` 级联
///   不得波及它；已存在的 `to` 同样不被补填 `from` 的 atom id。
/// - 复制 from 的出边到 to（`ON CONFLICT DO UPDATE SET confidence = MAX(...)`，过滤 dst==to 防自环）
/// - 复制 from 的入边到 to（同上，过滤 src==to 防自环）
/// - `from` 上的自环边 `(from, rel, from)` 会被 CASCADE 一并删除，**不会**重写为
///   `(to, rel, to)`（v1.3.0 设计取舍）
/// - DELETE entities WHERE canonical=from（CASCADE 清理任何剩余边）
pub fn link_entity(db: &Db, from: &str, to: &str) -> anyhow::Result<u32> {
    use rusqlite::OptionalExtension;
    let from_c = crate::graph::query::resolve_entity_canonical(db.conn(), from)?;
    let to_c = crate::graph::query::resolve_entity_canonical(db.conn(), to)?;
    let to_name = to.trim();
    if from_c.is_empty() || to_c.is_empty() {
        anyhow::bail!("from/to canonicalize to empty");
    }
    if from_c == to_c {
        anyhow::bail!("from and to canonicalize to the same value: '{}'", from_c);
    }

    let conn = db.conn();
    let now = time::now_unix_ms();

    let tx = db.immediate_tx()?;

    // 先检查 from 实体是否存在并读取其来源/类型元数据；不存在时直接作为真 no-op 返回 0
    // W17: 不再读取/传播 memory_atom_id——概念节点不得暴露于原子级联删除。
    let Some((from_type, from_source_turn)): Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT entity_type, source_turn FROM entities WHERE canonical = ?1",
            rusqlite::params![from_c],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    else {
        tx.commit()?;
        return Ok(0);
    };
    let propagated_type = {
        let trimmed = from_type.trim();
        if trimmed.is_empty() {
            "unknown"
        } else {
            trimmed
        }
    };

    // 确保 to 实体存在（不存在则继承 from 的类型/来源元数据，已存在则仅补填缺失的
    // NULL/'unknown' 字段）。W17: memory_atom_id 两个分支都不写——新建目标为 NULL，
    // 已有目标保留自己的值（含 NULL），绝不承接 from 的 atom id。
    let to_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entities WHERE canonical = ?1",
        rusqlite::params![to_c],
        |r| r.get(0),
    )?;
    if to_exists == 0 {
        conn.execute(
            "INSERT INTO entities
             (canonical, name, entity_type, first_seen, last_seen, source_turn)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            rusqlite::params![to_c, to_name, propagated_type, now, from_source_turn],
        )?;
    } else {
        conn.execute(
            "UPDATE entities
             SET source_turn = COALESCE(source_turn, ?1),
                 entity_type = CASE WHEN entity_type = 'unknown' THEN ?2 ELSE entity_type END
             WHERE canonical = ?3",
            rusqlite::params![from_source_turn, propagated_type, to_c],
        )?;
    }

    // 统计要重定向的边数（in + out，剔除自环重复）
    let edge_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM relations
         WHERE src_canonical = ?1 OR dst_canonical = ?1",
        rusqlite::params![from_c],
        |r| r.get(0),
    )?;

    // 复制出边 (from→X) → (to→X)，跳过自环（dst == to），重复边保留更大 confidence，并保留 relation_kind
    conn.execute(
        "INSERT INTO relations
         (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at, relation_kind)
         SELECT ?1, rel_type, dst_canonical, confidence, source_turn, created_at, relation_kind
         FROM relations WHERE src_canonical = ?2 AND dst_canonical <> ?1
         ON CONFLICT(src_canonical, rel_type, dst_canonical) DO UPDATE
             SET confidence = MAX(relations.confidence, excluded.confidence)",
        rusqlite::params![to_c, from_c],
    )?;

    // 复制入边 (X→from) → (X→to)，跳过自环（src == to），重复边保留更大 confidence，并保留 relation_kind
    conn.execute(
        "INSERT INTO relations
         (src_canonical, rel_type, dst_canonical, confidence, source_turn, created_at, relation_kind)
         SELECT src_canonical, rel_type, ?1, confidence, source_turn, created_at, relation_kind
         FROM relations WHERE dst_canonical = ?2 AND src_canonical <> ?1
         ON CONFLICT(src_canonical, rel_type, dst_canonical) DO UPDATE
             SET confidence = MAX(relations.confidence, excluded.confidence)",
        rusqlite::params![to_c, from_c],
    )?;

    // 删除 from 实体，CASCADE 会清理所有剩余边（包括没被复制成功的）
    conn.execute(
        "DELETE FROM entities WHERE canonical = ?1",
        rusqlite::params![from_c],
    )?;

    tx.commit()?;
    tracing::debug!(
        "link_entity: merged '{}' into '{}', {} edges redirected",
        from_c,
        to_c,
        edge_count
    );
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
    let tx = db.immediate_tx()?;

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

#[cfg(test)]
#[path = "store_unit_tests.rs"]
mod tests;
