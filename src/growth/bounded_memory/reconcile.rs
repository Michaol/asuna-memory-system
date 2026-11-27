use super::format::{extract_body, normalize_crlf, split_entries, ENTRY_SEPARATOR};
use super::{
    bump_md_generation, BoundedMemory, ProvenanceInfo, ProvenanceReport, ReconcileReport,
    SplitReport,
};
use crate::util::time;

pub(super) fn matching_row_ids(
    conn: &rusqlite::Connection,
    target: &str,
    needle: &str,
    include_superseded: bool,
) -> anyhow::Result<Vec<i64>> {
    let needle_norm = super::format::normalize_crlf(needle);
    let sql = if include_superseded {
        "SELECT bm.id FROM bounded_memory bm
         WHERE bm.target = ?1
           AND INSTR(REPLACE(bm.content, char(13)||char(10), char(10)), ?2) > 0
           AND COALESCE(bm.memory_type, 'manual') != 'scenario'"
    } else {
        "SELECT bm.id FROM bounded_memory bm
         WHERE bm.target = ?1
           AND INSTR(REPLACE(bm.content, char(13)||char(10), char(10)), ?2) > 0
           AND COALESCE(bm.memory_type, 'manual') != 'scenario'
           AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![target, needle_norm], |r| {
        r.get::<_, i64>(0)
    })?;
    // F10: 行解码失败上抛（与 render_md_from_db / prepare_sync_atoms 同姿势）——
    // filter_map(Result::ok) 静默丢行会让 update/remove 漏掉受影响行。
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn fetch_session_paths(
    conn: &rusqlite::Connection,
    session_ids: &[String],
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut path_map = std::collections::HashMap::new();
    crate::index::sql_util::for_each_in_chunk(
        session_ids,
        |chunk, placeholders| -> anyhow::Result<()> {
            let sql = format!(
                "SELECT session_id, file_path FROM sessions WHERE session_id IN ({placeholders})"
            );
            let mut session_stmt = conn.prepare(&sql)?;
            let session_rows = session_stmt
                .query_map(rusqlite::params_from_iter(chunk), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
            // F10: 行解码失败上抛，不用 flatten() 静默丢行——溯源映射缺失会让
            // verify_provenance 把有源会话误报为 missing_source。
            for (sid, path) in session_rows.collect::<rusqlite::Result<Vec<_>>>()? {
                path_map.insert(sid, path);
            }
            Ok(())
        },
    )?;
    Ok(path_map)
}

struct BadRow {
    id: i64,
    content: String,
    created_at: i64,
    updated_at: i64,
    source_session: Option<String>,
    confidence: String,
    memory_type: String,
    supersedes_id: Option<i64>,
    source_turn_ids: Option<String>,
    confidence_score: Option<f64>,
    edited_at: Option<i64>,
}

fn query_bad_multi_entry_rows(
    conn: &rusqlite::Connection,
    target: &str,
) -> anyhow::Result<Vec<BadRow>> {
    let pat_full = format!("%{}%", ENTRY_SEPARATOR);
    let pat_trail = "%\n§";
    let pat_lead = "§\n%";
    // W2.3 / W7: 检测面先 CRLF→LF 并修剪边界空白后再匹配（覆盖 \n§\n、头尾截断与
    // 裸 "§" 行变体），与 has_entry_separator 口径保持一致。
    let norm_expr =
        "TRIM(REPLACE(bm.content, char(13)||char(10), char(10)), char(9)||char(10)||char(13)||' ')";
    let sql = format!(
        "SELECT bm.id, bm.content, bm.created_at, bm.updated_at, bm.source_session,
                COALESCE(bm.confidence, 'medium'), COALESCE(bm.memory_type, 'manual'),
                bm.supersedes_id, bm.source_turn_ids, bm.confidence_score, bm.edited_at
         FROM bounded_memory bm
         WHERE bm.target = ?1 AND COALESCE(bm.memory_type, 'manual') != 'scenario'
           AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
           AND (REPLACE(bm.content, char(13)||char(10), char(10)) LIKE ?2
                OR {norm_expr} LIKE ?2
                OR {norm_expr} LIKE ?3
                OR {norm_expr} LIKE ?4
                OR {norm_expr} = '§')"
    );
    let mut stmt = conn.prepare(&sql)?;
    let bad_rows = stmt
        .query_map(
            rusqlite::params![target, pat_full, pat_trail, pat_lead],
            |row| {
                Ok(BadRow {
                    id: row.get::<_, i64>(0)?,
                    content: row.get::<_, String>(1)?,
                    created_at: row.get::<_, i64>(2)?,
                    updated_at: row.get::<_, i64>(3)?,
                    source_session: row.get::<_, Option<String>>(4)?,
                    confidence: row.get::<_, String>(5)?,
                    memory_type: row.get::<_, String>(6)?,
                    supersedes_id: row.get::<_, Option<i64>>(7)?,
                    source_turn_ids: row.get::<_, Option<String>>(8)?,
                    confidence_score: row.get::<_, Option<f64>>(9)?,
                    edited_at: row.get::<_, Option<i64>>(10)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(bad_rows)
}

impl<'a> BoundedMemory<'a> {
    /// 查询指定 target 的所有记忆条目（含溯源信息）
    pub fn list_entries(&self, target: &str) -> anyhow::Result<Vec<ProvenanceInfo>> {
        self.target_file(target)?;
        let mut stmt = self.db.conn().prepare(
            // W6: 与其他所有读取面同口径——排除 scenario 行（其唯一读取面是
            // /recall L2）与已被取代的行，否则溯源报告把死行/场景行当现行条目，
            // total_entries / verified / missing_source 全部虚高。
            "SELECT target, content, source_session, COALESCE(confidence, 'medium'), created_at
             FROM bounded_memory
             WHERE target = ?1
               AND COALESCE(memory_type, 'manual') != 'scenario'
               AND NOT EXISTS (
                   SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bounded_memory.id
               )
             ORDER BY created_at DESC",
        )?;

        let rows = stmt.query_map(rusqlite::params![target], |row| {
            Ok(ProvenanceInfo {
                target: row.get(0)?,
                content: row.get(1)?,
                source_session: row.get(2)?,
                confidence: row.get(3)?,
                created_at: {
                    let ms: i64 = row.get(4)?;
                    time::unix_ms_to_iso(ms)
                },
                session_exists: false,
                session_file_path: None,
            })
        })?;

        let mut results: Vec<ProvenanceInfo> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let session_ids: Vec<String> = results
            .iter()
            .filter_map(|info| info.source_session.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if !session_ids.is_empty() {
            let path_map = fetch_session_paths(self.db.conn(), &session_ids)?;
            for info in &mut results {
                if let Some(ref sid) = info.source_session {
                    if let Some(path) = path_map.get(sid) {
                        info.session_exists = true;
                        info.session_file_path = Some(path.clone());
                    }
                }
            }
        }

        Ok(results)
    }

    /// 验证成长层记忆与事实层的一致性
    pub fn verify_provenance(&self, target: &str) -> anyhow::Result<ProvenanceReport> {
        let entries = self.list_entries(target)?;
        let total = entries.len();
        let verified = entries
            .iter()
            .filter(|e| e.source_session.is_some() && e.session_exists)
            .count();
        let missing = entries
            .iter()
            .filter(|e| e.source_session.is_some() && !e.session_exists)
            .count();
        let no_source = entries
            .iter()
            .filter(|e| e.source_session.is_none())
            .count();

        Ok(ProvenanceReport {
            target: target.to_string(),
            total_entries: total,
            verified,
            missing_source: missing,
            no_source,
            entries,
        })
    }

    /// 对比 .md 条目与 SQLite 行，返回差异报告。
    pub fn reconcile_check(&self, target: &str) -> anyhow::Result<ReconcileReport> {
        let md_content = self.read_disk(target)?;
        let md_body = extract_body(&md_content);
        let md_entries: Vec<&str> = if md_body.is_empty() {
            vec![]
        } else {
            md_body.split(ENTRY_SEPARATOR).collect()
        };

        let mut stmt = self.db.conn().prepare(
            "SELECT bm.content FROM bounded_memory bm
             WHERE bm.target = ?1 AND COALESCE(bm.memory_type, 'manual') != 'scenario'
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
             ORDER BY bm.created_at",
        )?;
        let db_entries: Vec<String> = stmt
            .query_map(rusqlite::params![target], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // W2.3: 两侧统一 CRLF→LF 再 trim 比较。extract_body 已把 .md 的 \r\n 归一，
        // 但历史 DB 行可能仍含 \r；不归一就会把同一条目判成 DIVERGED，doctor --fix
        // 回插 LF 版本制造永不收敛的重复。归一后差集只反映真实内容差异。
        let md_set: std::collections::HashSet<String> = md_entries
            .iter()
            .map(|e| normalize_crlf(e).trim().to_string())
            .collect();
        let db_set: std::collections::HashSet<String> = db_entries
            .iter()
            .map(|e| normalize_crlf(e).trim().to_string())
            .collect();

        let only_in_md: Vec<String> = md_set.difference(&db_set).cloned().collect();
        let only_in_db: Vec<String> = db_set.difference(&md_set).cloned().collect();

        Ok(ReconcileReport {
            target: target.to_string(),
            md_entry_count: md_entries.len(),
            db_entry_count: db_entries.len(),
            only_in_md,
            only_in_db,
        })
    }

    /// 无损合并 .md 和 SQLite（修复 DB/文件不一致）。
    pub fn reconcile_fix(&self, target: &str) -> anyhow::Result<usize> {
        // C4: 只拆 DB 坏行、**不**立即重建 .md——否则 rebuild 会用 DB 内容覆盖
        // 盘面，把 .md 独有的手工条目在 reinsert 保留之前就冲掉。拆分后再
        // reconcile_check（直读盘面）→ reinsert .md 独有条目 → 末尾统一重建。
        let split = self.split_multi_entry_rows_db_only(target)?;
        if split.bad_rows > 0 {
            tracing::info!(
                "reconcile_fix[{}]: 拆分 {} 条多条目坏行 -> 新增 {} 子条目, 跳过 {} 重复",
                target,
                split.bad_rows,
                split.sub_entries_created,
                split.duplicates_skipped
            );
        }

        let mut report = self.reconcile_check(target)?;
        report.only_in_md.sort();
        let (inserted, stale_skipped) =
            self.reinsert_md_only_entries(target, &report.only_in_md)?;

        let total = self.rebuild_md_from_db(target)?;
        tracing::info!(
            "reconcile_fix[{}]: .md独有 {} 条已入库, SQLite独有 {} 条已合并, 陈旧被取代内容跳过 {} 条, 总计 {} 条",
            target,
            inserted,
            report.only_in_db.len(),
            stale_skipped,
            total
        );
        Ok(total)
    }

    fn is_valid_reinsert_candidate(&self, target: &str, entry: &str) -> bool {
        if entry.trim().is_empty() {
            return false;
        }
        if let Err(e) = self.run_scan(entry) {
            tracing::warn!(
                "reconcile_fix[{}]: skipped entry failing security scan ({})",
                target,
                e
            );
            return false;
        }
        if super::format::has_entry_separator(entry) {
            tracing::warn!(
                "reconcile_fix[{}]: skipped entry containing entry separator",
                target
            );
            return false;
        }
        true
    }

    fn matches_superseded_entry(
        tx: &rusqlite::Transaction<'_>,
        target: &str,
        trimmed_entry: &str,
    ) -> anyhow::Result<bool> {
        let found: bool = tx.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM bounded_memory bm
                WHERE bm.target = ?1 AND TRIM(REPLACE(bm.content, char(13)||char(10), char(10)), char(9)||char(10)||char(13)||' ') = ?2
                  AND EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
             )",
            rusqlite::params![target, trimmed_entry],
            |r| r.get(0),
        )?;
        Ok(found)
    }

    fn reinsert_md_only_entries(
        &self,
        target: &str,
        entries: &[String],
    ) -> anyhow::Result<(usize, usize)> {
        let now = time::now_unix_ms();
        // 维度7: memory 目标的回插行标 'atom'（与 MEMORY.md 自动原子同一渲染/容量
        // 管线），但下面 INSERT 同时盖 edited_at——prepare_sync_atoms_to_md 跳过
        // edited_at 非空的行，故手写 .md 内容靠 edited_at 保护，不会因被标 atom 而
        // 遭自动驱逐/重写。user 目标标 'manual'（恒受保护）。
        let reinsert_type = if target == "memory" { "atom" } else { "manual" };
        let mut inserted = 0usize;
        let mut stale_skipped = 0usize;
        let tx = self.db.immediate_tx()?;
        for entry in entries {
            // W2.3: 回插入口同样 CRLF→LF 归一（only_in_md 经 extract_body 已是 LF，
            // 此处防御性再归一，保证入库行与 .md 渲染同形）。
            let entry = normalize_crlf(entry);
            if !self.is_valid_reinsert_candidate(target, &entry) {
                continue;
            }
            if Self::matches_superseded_entry(&tx, target, entry.trim())? {
                tracing::info!(
                    "reconcile_fix[{}]: stale superseded content in MEMORY.md; \
                     skipped re-mint, next .md rebuild will drop it",
                    target
                );
                stale_skipped += 1;
                continue;
            }
            // 维度2: active dup 复查必须在 IMMEDIATE 事务内重做（与 write 的
            // NEW-B-growth 同姿势）。report.only_in_md 来自 tx 外的 reconcile_check，
            // 解锁间隙里并发 doctor/gateway 可能已插入同内容活行；不复查就插重复。
            let active_dup: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM bounded_memory bm
                    WHERE bm.target = ?1 AND TRIM(REPLACE(bm.content, char(13)||char(10), char(10)), char(9)||char(10)||char(13)||' ') = ?2
                      AND COALESCE(bm.memory_type, 'manual') != 'scenario'
                      AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
                 )",
                rusqlite::params![target, entry.trim()],
                |r| r.get(0),
            )?;
            if active_dup {
                continue;
            }
            tx.execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, edited_at)
                 VALUES (?1, ?2, ?3, ?4, 'medium', ?5, ?4)",
                rusqlite::params![target, entry, now, now, reinsert_type],
            )?;
            inserted += 1;
        }
        // 维度4: doctor --fix 改库须留审计轨迹（与 write/update/remove 同姿势，
        // 事务内 log_action 随提交一并落库）。
        if inserted > 0 {
            crate::growth::audit::log_action(
                self.db,
                "reconcile_reinsert",
                target,
                &format!("reinserted={} stale_skipped={}", inserted, stale_skipped),
                None,
            )?;
        }
        tx.commit()?;
        // C8: 回插提交同样递增代际（随后 reconcile_fix 必定重建并刷新 .md）。
        bump_md_generation(&self.target_file(target)?);
        Ok((inserted, stale_skipped))
    }

    /// 拆分 target 中含多个 `§` 分隔符的坏行（一行多条目 -> 多行各一单条目）。
    /// 公共契约：拆分 DB 行**并**重建刷新 .md。
    pub fn split_multi_entry_rows(&self, target: &str) -> anyhow::Result<SplitReport> {
        let report = self.split_multi_entry_rows_db_only(target)?;
        if report.bad_rows > 0 {
            self.rebuild_md_from_db(target)?;
        }
        Ok(report)
    }

    /// C4: 仅拆分 DB 坏行（事务内拆分 + 提交 + 递增代际），**不**重建 .md。
    /// 供 `reconcile_fix` 在保留 .md 独有条目之前调用，避免提前覆盖盘面。
    fn split_multi_entry_rows_db_only(&self, target: &str) -> anyhow::Result<SplitReport> {
        use rusqlite::OptionalExtension;
        self.target_file(target)?;
        // W8: query_bad_multi_entry_rows 与后续拆分写入放进同一 IMMEDIATE 事务，防止并发漏/重拆。
        let tx = self.db.immediate_tx()?;
        let bad_rows = query_bad_multi_entry_rows(&tx, target)?;
        if bad_rows.is_empty() {
            return Ok(SplitReport {
                target: target.to_string(),
                bad_rows: 0,
                sub_entries_created: 0,
                duplicates_skipped: 0,
            });
        }

        let mut created = 0usize;
        let mut skipped = 0usize;
        let mut deleted = 0usize;

        {
            let mut exists_stmt = tx.prepare(
                "SELECT 1 FROM bounded_memory bm
                 WHERE bm.target = ?1 AND TRIM(REPLACE(bm.content, char(13)||char(10), char(10)), char(9)||char(10)||char(13)||' ') = ?2
                   AND COALESCE(bm.memory_type, 'manual') != 'scenario'
                   AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
                 LIMIT 1",
            )?;
            let mut insert_stmt = tx.prepare(
                "INSERT INTO bounded_memory
                    (target, content, created_at, updated_at, source_session,
                     confidence, memory_type, supersedes_id, source_turn_ids, confidence_score,
                     edited_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                         (SELECT id FROM bounded_memory WHERE id = ?8), ?9, ?10, ?11)",
            )?;
            let mut delete_stmt = tx.prepare("DELETE FROM bounded_memory WHERE id = ?1")?;
            let mut deref_stmt = tx.prepare(
                "UPDATE bounded_memory SET supersedes_id = NULL WHERE supersedes_id = ?1",
            )?;

            for row in &bad_rows {
                let sub_entries: Vec<String> = split_entries(&row.content);
                for (idx, sub) in sub_entries.iter().enumerate() {
                    // M7: 拆分子条目经与 reinsert 相同的安全扫描/分隔符校验，
                    // 不合格者跳过（计 skipped），坏行里的注入内容不被拆分洗入库。
                    if !self.is_valid_reinsert_candidate(target, sub) {
                        skipped += 1;
                        continue;
                    }
                    let exists: bool = exists_stmt
                        .query_row(rusqlite::params![target, sub.as_str()], |_| Ok(()))
                        .optional()?
                        .is_some();
                    if exists {
                        skipped += 1;
                        continue;
                    }
                    // M7: 仅首个拆分子条目（idx==0）继承原行 supersedes_id，其余
                    // 置 None——否则多条活行同时取代同一前驱，前驱会被重复级联
                    // 删除/引用混乱。
                    let inherit_supersedes = if idx == 0 { row.supersedes_id } else { None };
                    insert_stmt.execute(rusqlite::params![
                        target,
                        sub.as_str(),
                        row.created_at,
                        row.updated_at,
                        row.source_session,
                        row.confidence,
                        row.memory_type,
                        inherit_supersedes,
                        row.source_turn_ids,
                        row.confidence_score,
                        row.edited_at,
                    ])?;
                    created += 1;
                }
                let original_trimmed = row.content.trim();
                let changed = sub_entries.len() > 1
                    || sub_entries.first().is_none_or(|s| *s != original_trimmed);
                if changed {
                    tx.execute(
                        "DELETE FROM vec_bounded_memory WHERE id = ?1",
                        rusqlite::params![row.id],
                    )?;
                    tx.execute(
                        "DELETE FROM entities WHERE memory_atom_id = ?1",
                        rusqlite::params![row.id],
                    )?;
                    deref_stmt.execute(rusqlite::params![row.id])?;
                    delete_stmt.execute(rusqlite::params![row.id])?;
                    deleted += 1;
                }
            }
        }
        // 维度4 / M10: doctor --split 只要实际改库（新增或删除坏行）须留审计轨迹。
        if created > 0 || deleted > 0 {
            crate::growth::audit::log_action(
                self.db,
                "reconcile_split",
                target,
                &format!(
                    "bad_rows={} sub_entries_created={} duplicates_skipped={} rows_deleted={}",
                    bad_rows.len(),
                    created,
                    skipped,
                    deleted
                ),
                None,
            )?;
        }
        tx.commit()?;
        // C8: 拆分提交同样递增代际（公共入口随后重建 .md；reconcile_fix 在
        // 保留 .md 独有条目后于末尾统一重建）。
        bump_md_generation(&self.target_file(target)?);

        Ok(SplitReport {
            target: target.to_string(),
            bad_rows: bad_rows.len(),
            sub_entries_created: created,
            duplicates_skipped: skipped,
        })
    }
}

#[cfg(test)]
#[path = "reconcile_tests.rs"]
mod tests;
