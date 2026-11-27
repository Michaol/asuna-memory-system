use super::legacy_files::{legacy_jsonl_candidates, remove_verified_legacy_jsonl};
use super::{
    execute_turn_insert, run_in_transaction, turn_ts_ms_or_warn, SaveStats, SessionStore,
    INSERT_TURN_SQL,
};
use crate::fact::conversation::{self, SessionHeader, Turn};
use crate::index::vector::VectorStore;
use crate::util::time;

impl SessionStore<'_> {
    pub(super) fn save_overwrite(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        // 1. 计算目标 JSONL 路径（仅计算，不写盘）
        let (file_path, file_rel_path) = self.session_paths(header)?;

        // 2. 计算元信息
        let start_ts = time::ts_to_unix_ms(&header.start_time)?;
        // M19: end_ts 取所有 turn 时间戳的最大值，不假定 turns 按 ts 有序
        // （MCP/CLI 导入顺序由调用方决定）；空 turns → None（保持原语义）。
        // 坏 ts 的回落 warn 在 write_turn_rows 逐行入库时发出，此处不重复 warn。
        let end_ts = conversation::session_end_ts(turns, start_ts);
        let total_tokens: i64 = conversation::session_total_tokens(turns);
        let now = time::now_unix_ms();
        let tags_json = if header.tags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&header.tags)?)
        };

        let conn = self.db.conn();

        // 3. 进入事务：所有 DB 改动包裹其中。旧 JSONL 路径 + start_ts 一并
        //    在事务内读（同 save_append，W15 消除跨进程 TOCTOU），值随事务
        //    结果传出，供 commit 后清理与 U5 升级缝隙的旧命名文件定位。
        let vec_store = VectorStore::new(self.db);
        let (any_vec_skipped, prior_db) = run_in_transaction(self.db, || {
            use rusqlite::OptionalExtension;
            // M7: 一并取 created_at——覆盖已有会话时保留首次创建时间。
            let prior_db: Option<(String, i64, i64)> = conn
                .query_row(
                    "SELECT file_path, start_ts, created_at FROM sessions WHERE session_id = ?1",
                    rusqlite::params![header.session_id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;

            let mut stale_stmt = conn.prepare("SELECT id, seq FROM turns WHERE session_id = ?1")?;
            let stale_turns: Vec<(i64, i64)> = stale_stmt
                .query_map(rusqlite::params![header.session_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<(i64, i64)>>>()?;

            // W28f: 集合式删除本 session 旧向量（须在删 turns 前，子查询才
            // 看得到 id），替代逐 id 循环 DELETE。
            conn.execute(
                "DELETE FROM vec_turns WHERE rowid IN (SELECT id FROM turns WHERE session_id = ?1)",
                rusqlite::params![header.session_id],
            )?;
            conn.execute(
                "DELETE FROM turns WHERE session_id = ?1",
                rusqlite::params![header.session_id],
            )?;

            // M7: 覆盖已有会话时保留 prior created_at（首次创建时间），新会话用 now。
            let created_at = prior_db.as_ref().map(|(_, _, c)| *c).unwrap_or(now);
            conn.execute(
                "INSERT OR REPLACE INTO sessions
                 (session_id, start_ts, end_ts, file_path, title, profile_id, source, agent_model,
                  turn_count, total_tokens, tags, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    header.session_id,
                    start_ts,
                    end_ts,
                    file_rel_path,
                    header.title,
                    header.profile_id,
                    header.source,
                    header.agent_model,
                    turns.len() as i64,
                    total_tokens,
                    tags_json,
                    created_at,
                    now,
                ],
            )?;

            let skipped =
                self.write_turn_rows(conn, header, turns, start_ts, embeddings, &vec_store)?;

            if !stale_turns.is_empty() {
                crate::index::provenance::remap_session_turn_refs(
                    conn,
                    &header.session_id,
                    &stale_turns,
                )?;
            }
            Ok((skipped, prior_db))
        })?;

        // 4. DB commit 成功后写 JSONL
        conversation::write_session_at(&file_path, header, turns)?;

        // 5. 清理同 session 旧 JSONL（先验证 canonical path 位于 conversations_dir 内且 header.session_id 匹配）
        remove_verified_legacy_jsonl(
            self.conversations_dir,
            &legacy_jsonl_candidates(
                self.conversations_dir,
                header,
                prior_db.as_ref().map(|(p, ts, _)| (p.as_str(), *ts)),
            ),
            &file_path,
            &header.session_id,
        );

        Ok(SaveStats {
            session_id: header.session_id.clone(),
            file_path,
            turns_saved: turns.len(),
            vectors_skipped: any_vec_skipped,
        })
    }

    /// 写入 turns 行 + 可选向量（事务内调用）。返回是否有 turn 因 embeddings
    /// 切片短于 turns 而跳过向量（M10：照 Append any_vec_failed 透出标志）。
    /// 向量 insert 失败仍 `?` 回滚（Overwrite 契约不变）。
    fn write_turn_rows(
        &self,
        conn: &rusqlite::Connection,
        header: &SessionHeader,
        turns: &[Turn],
        start_ts: i64,
        embeddings: Option<&[Vec<f32>]>,
        vec_store: &VectorStore<'_>,
    ) -> anyhow::Result<bool> {
        let mut insert_stmt = conn.prepare_cached(INSERT_TURN_SQL)?;
        let mut any_vec_skipped = false;
        // M7: 按 seq 去重，保留先见者（与 rebuild / insert_session_turns 同口径）——
        // 输入 turns 含重复 seq 时只入库首条，避免 UNIQUE/主键冲突或静默覆盖。
        let mut seen_seqs: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for (i, turn) in turns.iter().enumerate() {
            if !seen_seqs.insert(turn.seq) {
                continue;
            }
            let ts_ms = turn_ts_ms_or_warn(&turn.ts, start_ts, &header.session_id, turn.seq as i64);
            let preview = self.preview_of(&turn.content);
            let char_count = turn.content.chars().count() as i64;

            execute_turn_insert(
                &mut insert_stmt,
                &header.session_id,
                turn.seq as i64,
                ts_ms,
                &turn.role,
                &preview,
                char_count,
            )?;

            if let Some(embs) = embeddings {
                if i < embs.len() {
                    let turn_id = conn.last_insert_rowid();
                    vec_store.insert(turn_id, &embs[i])?;
                } else {
                    // M10: 短切片（生产不可达，embedder 保证等长）透出跳过标志
                    any_vec_skipped = true;
                }
            }
        }
        Ok(any_vec_skipped)
    }
}
