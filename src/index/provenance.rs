use std::collections::{HashMap, HashSet};

/// 全量重建 `DELETE FROM turns` 前快照旧 `(id, session_id, seq)` 到 `temp.tmp_turn_id_old`，
/// 供阶段末按 `(session_id, seq)` 把 `entities`/`relations`/`bounded_memory` 软引用映射回新 `turns.id`。
pub(crate) fn snapshot_all_turn_ids(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.tmp_turn_id_old;
         CREATE TEMP TABLE tmp_turn_id_old (
             id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL
         );
         INSERT INTO tmp_turn_id_old SELECT id, session_id, seq FROM turns;",
    )?;
    Ok(())
}

/// 全量重建写回后执行：将指向旧 `turns.id` 的软引用按 `(session_id, seq)` 配对映射到新 `id`，
/// 映射不到的引用置 `NULL` 或从 `source_turn_ids` JSON 数组中丢弃，并清理临时快照表。
pub(crate) fn remap_all_turn_refs(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    // GROUP BY + MIN 防御病态数据：JSONL 中重复 session_id 会产生重复的
    // (session_id, seq)，不加聚合将触发 old_id 主键冲突。
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.tmp_turn_id_remap;
         CREATE TEMP TABLE tmp_turn_id_remap (old_id INTEGER PRIMARY KEY, new_id INTEGER NOT NULL);
         INSERT INTO tmp_turn_id_remap
             SELECT o.id, MIN(n.id)
             FROM tmp_turn_id_old o
             JOIN turns n ON n.session_id = o.session_id AND n.seq = o.seq
             GROUP BY o.id;",
    )?;

    conn.execute_batch(
        "UPDATE entities
            SET source_turn = (SELECT new_id FROM tmp_turn_id_remap WHERE old_id = entities.source_turn)
          WHERE source_turn IS NOT NULL;
         UPDATE relations
            SET source_turn = (SELECT new_id FROM tmp_turn_id_remap WHERE old_id = relations.source_turn)
          WHERE source_turn IS NOT NULL;",
    )?;

    let remap: HashMap<i64, i64> = {
        let mut stmt = conn.prepare("SELECT old_id, new_id FROM tmp_turn_id_remap")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>()?
    };
    let bm_rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, source_turn_ids FROM bounded_memory WHERE source_turn_ids IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    rewrite_bounded_memory_rows(conn, bm_rows, &remap, None)?;

    conn.execute_batch("DROP TABLE temp.tmp_turn_id_old; DROP TABLE temp.tmp_turn_id_remap;")?;
    Ok(())
}

/// 单会话覆盖写入后执行：仅重映射或清理属于本次 `stale_turns` 的旧 `turns.id` 引用，
/// 保持其他会话或原本未涉及的引用（包括非 `stale_turns` 的未知 ID）原样不动。
pub(crate) fn remap_session_turn_refs(
    conn: &rusqlite::Connection,
    session_id: &str,
    stale_turns: &[(i64, i64)],
) -> anyhow::Result<()> {
    let (remap, stale_set) = collect_turn_id_remap(conn, session_id, stale_turns)?;
    remap_graph_turn_refs(conn, stale_turns, &remap)?;
    let bm_rows = query_affected_bounded_memory_rows(conn, stale_turns)?;
    rewrite_bounded_memory_rows(conn, bm_rows, &remap, Some(&stale_set))
}

fn collect_turn_id_remap(
    conn: &rusqlite::Connection,
    session_id: &str,
    stale_turns: &[(i64, i64)],
) -> anyhow::Result<(HashMap<i64, i64>, HashSet<i64>)> {
    let mut seq_to_new: HashMap<i64, i64> = HashMap::new();
    let mut stmt = conn.prepare("SELECT seq, id FROM turns WHERE session_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map(rusqlite::params![session_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (seq, new_id) = row?;
        seq_to_new.entry(seq).or_insert(new_id);
    }

    let mut remap: HashMap<i64, i64> = HashMap::new();
    let mut stale_set: HashSet<i64> = HashSet::with_capacity(stale_turns.len());
    for &(old_id, seq) in stale_turns {
        stale_set.insert(old_id);
        if let Some(&new_id) = seq_to_new.get(&seq) {
            remap.insert(old_id, new_id);
        }
    }
    Ok((remap, stale_set))
}

fn remap_graph_turn_refs(
    conn: &rusqlite::Connection,
    stale_turns: &[(i64, i64)],
    remap: &HashMap<i64, i64>,
) -> anyhow::Result<()> {
    // W26b: temp remap 表 + 两条集合式 UPDATE 取代 2*N 逐行更新；
    // 映射不到的旧 id 置 NULL（与逐行 remap.get → None 语义一致）。
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.tmp_session_remap;
         CREATE TEMP TABLE tmp_session_remap (old_id INTEGER PRIMARY KEY, new_id INTEGER);",
    )?;
    {
        let mut ins_stmt = conn
            .prepare("INSERT OR REPLACE INTO tmp_session_remap (old_id, new_id) VALUES (?1, ?2)")?;
        for &(old_id, _) in stale_turns {
            ins_stmt.execute(rusqlite::params![old_id, remap.get(&old_id).copied()])?;
        }
    }
    conn.execute_batch(
        "UPDATE entities
            SET source_turn = (SELECT new_id FROM tmp_session_remap WHERE old_id = entities.source_turn)
          WHERE source_turn IN (SELECT old_id FROM tmp_session_remap);
         UPDATE relations
            SET source_turn = (SELECT new_id FROM tmp_session_remap WHERE old_id = relations.source_turn)
          WHERE source_turn IN (SELECT old_id FROM tmp_session_remap);
         DROP TABLE IF EXISTS temp.tmp_session_remap;",
    )?;
    Ok(())
}

fn query_affected_bounded_memory_rows(
    conn: &rusqlite::Connection,
    stale_turns: &[(i64, i64)],
) -> anyhow::Result<HashMap<i64, String>> {
    let stale_ids: Vec<i64> = stale_turns.iter().map(|&(id, _)| id).collect();
    let mut bm_rows = HashMap::new();
    super::sql_util::for_each_in_chunk(&stale_ids, |chunk, placeholders| -> anyhow::Result<()> {
        let sql = format!(
            "SELECT id, source_turn_ids FROM bounded_memory \
             WHERE source_turn_ids IS NOT NULL AND json_valid(source_turn_ids) \
               AND EXISTS (SELECT 1 FROM json_each(source_turn_ids) WHERE value IN ({placeholders}))"
        );
        // W26b: 动态占位符 SQL 每 chunk 形状不同，prepare_cached 只会污染缓存——
        // 用一次性 prepare。
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (bm_id, json) = row?;
            bm_rows.entry(bm_id).or_insert(json);
        }
        Ok(())
    })?;
    Ok(bm_rows)
}

fn rewrite_bounded_memory_rows(
    conn: &rusqlite::Connection,
    bm_rows: impl IntoIterator<Item = (i64, String)>,
    remap: &HashMap<i64, i64>,
    stale_scope: Option<&HashSet<i64>>,
) -> anyhow::Result<()> {
    let mut update_stmt =
        conn.prepare_cached("UPDATE bounded_memory SET source_turn_ids = ?1 WHERE id = ?2")?;
    for (bm_id, json) in bm_rows {
        let Ok(parsed) = serde_json::from_str::<Vec<i64>>(&json) else {
            continue; // 非 id 数组（遗留/手工数据）：保持原样
        };
        if let Some(stale_set) = stale_scope {
            if !parsed.iter().any(|id| stale_set.contains(id)) {
                continue;
            }
        }
        let updated: Vec<i64> = parsed
            .iter()
            .filter_map(|old| match stale_scope {
                Some(stale_set) if !stale_set.contains(old) => Some(*old),
                _ => remap.get(old).copied(),
            })
            .collect();
        if updated != parsed {
            let new_json = serde_json::to_string(&updated)?;
            update_stmt.execute(rusqlite::params![new_json, bm_id])?;
        }
    }
    Ok(())
}
