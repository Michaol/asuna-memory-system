use super::jsonl_io::{append_jsonl_turns, AppendFilePlan};
use super::legacy_files::legacy_jsonl_candidates;
use super::{
    execute_turn_insert, run_in_transaction, turn_ts_ms_or_warn, SaveStats, SessionStore,
    INSERT_TURN_SQL,
};
use crate::fact::conversation::{self, SessionHeader, Turn};
use crate::index::vector::VectorStore;
use crate::util::time;
use std::path::{Path, PathBuf};

struct AppendTurnsBatch<'a> {
    session_id: &'a str,
    turns: &'a [Turn],
    max_seq: i64,
    start_ts: i64,
    embeddings: Option<&'a [Vec<f32>]>,
}

impl SessionStore<'_> {
    /// 追加保存（REST /capture 的历史语义，J33 自 http.rs 内联事务逐位迁移）。
    ///
    /// 与 [`Self::save_overwrite`] 的差异（均为显式契约，J41 契约测试钉住）：
    /// - 不删 turns；seq 事务内从 COALESCE(MAX(seq),0)+1 续排，输入 turn.seq
    ///   被忽略（/capture 请求体本就没有 seq 字段）；
    /// - sessions 最小列集：新会话 INSERT（start_ts/file_path/turn_count/
    ///   created_at/updated_at），已有会话 UPDATE 累加 turn_count、刷新
    ///   updated_at 与 file_path。title/source/total_tokens/end_ts 保持原值
    ///   （新行为 schema 默认）——与旧 /capture 逐位一致；
    /// - J33b：file_path 写**真实 JSONL 相对路径**（旧 /capture 写
    ///   `gateway://{id}` 伪 URI，破坏 rebuild/cleanup/溯源对该列的使用）；
    /// - 向量统一走 VectorStore::insert（J33f：旧 /capture 的手写 vec_int8
    ///   SQL 与之逐字段等价，已删）；写失败仅 warn 不回滚（M1：数据优先），
    ///   与 Overwrite 的“失败即回滚”不同；
    /// - JSONL 增量追加且 best-effort（W3），无同路径旧文件清理（路径派生自
    ///   session 的 start_ts，同一会话多次追加稳定指向同一文件）；但跨版本
    ///   升级会话例外——U5 改名让旧命名文件逃过全部清理，首写时先迁移后删除
    ///   （见 [`Self::prior_jsonl_for_append`]）。
    pub(super) fn save_append(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
        batch_id: Option<&str>,
    ) -> anyhow::Result<(SaveStats, bool)> {
        // 1. 计算目标 JSONL 路径（与旧 archive_session_jsonl 一致：仅由 header 推导）
        let (file_path, file_rel_path) = self.session_paths(header)?;

        let start_ts = time::ts_to_unix_ms(&header.start_time)?;
        let now = time::now_unix_ms();

        let conn = self.db.conn();
        let vec_store = VectorStore::new(self.db);

        // 锁跨度（C3 复审）：候选枚举 / legacy 文件读取 / 目标文件全量 seq 扫描都是
        // O(文件) 的 IO，**必须在写事务外**做。否则 BEGIN IMMEDIATE 跨整段文件 IO
        // + fsync：并发 MCP 重建线程 / CLI 写入超 busy_timeout=5000 即 SQLITE_BUSY，
        // 进程内所有 handler 阻塞在 Mutex<Db> 上（60s 超时回 408 而 detached 写仍
        // 完成 → 重复 turn）。事务内只保留**写**。此处预读 prior_db 作快照，与原
        // 实现（commit 后才读）相比并不更晚，跨进程并发仍由单进程红线约束。
        use rusqlite::OptionalExtension;
        let prior_db: Option<(String, i64)> = conn
            .query_row(
                "SELECT file_path, start_ts FROM sessions WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let candidates = legacy_jsonl_candidates(
            self.conversations_dir,
            header,
            prior_db.as_ref().map(|(p, ts)| (p.as_str(), *ts)),
        );
        let (prior_turns, stale_files, legacy_header) =
            self.prior_jsonl_for_append(header, &file_path, &candidates);
        // M7: 迁移旧文件时把当前 header 缺失的元数据（title/profile_id/source/
        // agent_model/tags）从旧 header 补齐，升级迁移不丢弃旧会话头字段。
        let merged_header;
        let header = match legacy_header {
            Some(lh) => {
                merged_header = merge_missing_header_fields(header, &lh);
                &merged_header
            }
            None => header,
        };
        // C5: JSONL 是真相源，而 `delete-turn` 有意只删 DB 行、保留 JSONL 行。
        // 仅按 DB MAX(seq) 分配会与文件里已存在的 seq 撞号，rebuild 的"保留先见者"
        // 去重会把新捕获的 turn 永久丢弃。故新 seq 必须同时尊重 DB 与 JSONL。
        // W27a: 单遍扫描同时得到 (max_seq, 重放候选)，替代两次全量文件读。
        let scan = conversation::scan_jsonl_for_append(&file_path, turns);
        let jsonl_max_seq = scan.max_seq.max(
            prior_turns
                .iter()
                .map(|t| i64::from(t.seq))
                .max()
                .unwrap_or(0),
        );

        // 整批重放检测（COMMIT 失败后的重试幂等）：JSONL 已写成功但 DB 事务失败时，
        // 重试会以更高 seq 再写一份相同内容，而 rebuild 只按 seq 去重 → 真相源静默
        // 重复。命中重放则复用文件里已有的 seq 区间并跳过追加。判据收窄到"尾部整批
        // 按序全等且 seq 连续"，不吞合法的相同 turn；且不依赖 DB 失败后状态（SQLite
        // 官方不保证 COMMIT 类错误后已自动回滚）。
        let replay_candidate = scan.replay_first_seq;

        // 2. 事务：若携带 batch_id，在同一事务顶部 INSERT OR IGNORE 占位；
        //    重复命中直接早退返回首存 turns_saved，否则与 session/turns/向量同提交或同回滚。
        enum AppendTxOutcome {
            DedupHit(usize),
            Written {
                any_vec_failed: bool,
                stale_files: Vec<PathBuf>,
            },
        }
        let outcome = run_in_transaction(self.db, || {
            if let Some(saved) =
                try_claim_capture_dedup(conn, &header.session_id, batch_id, turns.len(), now)?
            {
                return Ok(AppendTxOutcome::DedupHit(saved));
            }
            let _existing = upsert_append_session(
                conn,
                &header.session_id,
                &file_rel_path,
                start_ts,
                turns.len(),
                now,
            )?;
            let db_max_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| r.get(0),
            )?;
            // C5: 仅当尾批起始 seq **严格领先于 DB MAX(seq)**（且尾批 seq 连续，
            // 由 scan 保证）时才认定为 COMMIT 失败留下的重放。否则 delete-turn /
            // 部分缺口后 first <= db_max_seq 会复用撞号 seq 区间，重复 seq 或复活
            // 已删 turn；用户合法连发两次相同批次（DB 已追平）同样 first <= db_max。
            let replay_base_seq = replay_candidate.filter(|&first| i64::from(first) > db_max_seq);
            let max_seq = match replay_base_seq {
                Some(first) => i64::from(first) - 1,
                None => db_max_seq.max(jsonl_max_seq),
            };
            let any_vec_failed = self.insert_append_turns(
                conn,
                &vec_store,
                &AppendTurnsBatch {
                    session_id: &header.session_id,
                    turns,
                    max_seq,
                    start_ts,
                    embeddings,
                },
            )?;
            // C3: JSONL 真相源写入与 DB 写入同事务。旧实现在 commit 之后写且失败
            // 只 warn，于是 /capture 返回 200、dedup 占位已提交——同 batch_id 重试
            // 直接返回缓存值而永不补写 JSONL，后续 rebuild 从 JSONL 重建时这些
            // turn 消失（静默且不可恢复）。同事务后失败即整体回滚，客户端可安全重试。
            let stale_files = self.append_jsonl_in_tx(
                header,
                turns,
                &file_path,
                max_seq,
                AppendFilePlan {
                    prior_turns,
                    stale_files,
                    replay: replay_base_seq.is_some(),
                },
            )?;
            Ok(AppendTxOutcome::Written {
                any_vec_failed,
                stale_files,
            })
        })?;

        let (any_vec_failed, stale_files) = match outcome {
            AppendTxOutcome::DedupHit(saved) => {
                return Ok((
                    SaveStats {
                        session_id: header.session_id.clone(),
                        file_path,
                        turns_saved: saved,
                        vectors_skipped: false,
                    },
                    true,
                ));
            }
            AppendTxOutcome::Written {
                any_vec_failed,
                stale_files,
            } => (any_vec_failed, stale_files),
        };

        // C6: legacy 旧文件删除严格放在事务 COMMIT **之后**——COMMIT 失败整体回滚
        // 时，pre-U5 旧文件必须原样保留（它们可能是该会话唯一的真相源副本）。
        for stale in stale_files {
            if let Err(e) = std::fs::remove_file(&stale) {
                tracing::warn!(
                    target: "asuna_memory::fact::session_store",
                    "升级旧 JSONL 清理失败 {}: {}",
                    stale.display(),
                    e
                );
            }
        }

        Ok((
            SaveStats {
                session_id: header.session_id.clone(),
                file_path,
                turns_saved: turns.len(),
                vectors_skipped: any_vec_failed,
            },
            false,
        ))
    }

    fn insert_append_turns(
        &self,
        conn: &rusqlite::Connection,
        vec_store: &VectorStore<'_>,
        batch: &AppendTurnsBatch<'_>,
    ) -> anyhow::Result<bool> {
        let mut any_vec_failed = false;
        let mut stmt = conn.prepare_cached(INSERT_TURN_SQL)?;
        for (i, turn) in batch.turns.iter().enumerate() {
            let seq = batch.max_seq + (i as i64) + 1;
            let ts_ms = turn_ts_ms_or_warn(&turn.ts, batch.start_ts, batch.session_id, seq);
            let preview = self.preview_of(&turn.content);
            let char_count = turn.content.chars().count() as i64;
            execute_turn_insert(
                &mut stmt,
                batch.session_id,
                seq,
                ts_ms,
                &turn.role,
                &preview,
                char_count,
            )?;
            any_vec_failed |= try_insert_turn_vec(conn, vec_store, batch.embeddings, i);
        }
        Ok(any_vec_failed)
    }

    /// C3: 追加 JSONL 真相源。**在写事务内调用**——返回 Err 使整个 Append
    /// （turns + capture_dedup 占位）回滚，调用方可安全重试。
    /// C6: 只负责写 JSONL，返回待删除的 legacy 旧文件清单，由调用方在 COMMIT
    /// **之后**删除（COMMIT 失败回滚时旧文件必须原样保留）。
    fn append_jsonl_in_tx(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        file_path: &Path,
        max_seq: i64,
        plan: AppendFilePlan,
    ) -> anyhow::Result<Vec<PathBuf>> {
        if plan.replay {
            // 整批重放：文件里已有逐条全等的这一批（上次 COMMIT 失败留下的），
            // DB 侧已复用同一 seq 区间补齐，不再追加，避免真相源重复。
            tracing::info!(
                target: "asuna_memory::fact::session_store",
                "JSONL 整批重放 detected for session {}：复用已有 seq 区间，跳过追加",
                header.session_id
            );
        } else {
            let mut jsonl_turns = plan.prior_turns;
            jsonl_turns.extend(turns.iter().enumerate().map(|(i, t)| Turn {
                ts: t.ts.clone(),
                seq: u32::try_from(max_seq + (i as i64) + 1).unwrap_or(u32::MAX),
                role: t.role.clone(),
                content: t.content.clone(),
                metadata: t.metadata.clone(),
            }));

            append_jsonl_turns(file_path, header, &jsonl_turns).map_err(|e| {
                anyhow::anyhow!(
                    "JSONL append failed for session {}: {}",
                    header.session_id,
                    e
                )
            })?;
        }
        Ok(plan.stale_files)
    }
}

/// M7: 从 legacy header 补齐当前 header 缺失的元数据（首见优先）。仅在当前字段
/// 缺失/为空时填入，不覆盖当前 header 已有的值；session_id/start_time 等身份字段
/// 不在补齐范围（路径与身份已由当前 header 确定）。
fn merge_missing_header_fields(base: &SessionHeader, legacy: &SessionHeader) -> SessionHeader {
    let mut h = base.clone();
    if h.title.is_none() {
        h.title = legacy.title.clone();
    }
    if h.profile_id.is_empty() {
        h.profile_id = legacy.profile_id.clone();
    }
    if h.source.is_none() {
        h.source = legacy.source.clone();
    }
    if h.agent_model.is_none() {
        h.agent_model = legacy.agent_model.clone();
    }
    if h.tags.is_empty() {
        h.tags = legacy.tags.clone();
    }
    h
}

fn upsert_append_session(
    conn: &rusqlite::Connection,
    session_id: &str,
    file_rel_path: &str,
    start_ts: i64,
    turns_len: usize,
    now: i64,
) -> anyhow::Result<Option<(i64, String)>> {
    use rusqlite::OptionalExtension;
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT start_ts, file_path FROM sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if existing.is_none() {
        conn.execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            rusqlite::params![session_id, start_ts, file_rel_path, turns_len as i64, now],
        )?;
    } else {
        conn.execute(
            "UPDATE sessions SET turn_count = turn_count + ?1, updated_at = ?2, file_path = ?3
             WHERE session_id = ?4",
            rusqlite::params![turns_len as i64, now, file_rel_path, session_id],
        )?;
    }
    Ok(existing)
}

fn try_insert_turn_vec(
    conn: &rusqlite::Connection,
    vec_store: &VectorStore<'_>,
    embeddings: Option<&[Vec<f32>]>,
    idx: usize,
) -> bool {
    let Some(embs) = embeddings else {
        return false;
    };
    let Some(vec) = embs.get(idx) else {
        // M10: 短切片（生产不可达，embedder 保证等长）透出跳过标志
        return true;
    };
    let turn_id = conn.last_insert_rowid();
    if let Err(e) = vec_store.insert(turn_id, vec) {
        tracing::warn!(
            target: "asuna_memory::fact::session_store",
            "vec_turns insert failed for turn {}: {}",
            turn_id,
            e
        );
        return true;
    }
    false
}

/// W10: 同一事务内的 capture_dedup 幂等占位（原 session_store/dedup.rs，
/// 唯一调用方即本文件的 save_append，就地折叠）。
pub(super) fn try_claim_capture_dedup(
    conn: &rusqlite::Connection,
    session_id: &str,
    batch_id: Option<&str>,
    turns_len: usize,
    now: i64,
) -> anyhow::Result<Option<usize>> {
    let Some(bid) = batch_id else {
        return Ok(None);
    };
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO capture_dedup (session_id, batch_id, turns_saved, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![session_id, bid, turns_len as i64, now],
    )?;
    if inserted > 0 {
        return Ok(None);
    }
    let saved: i64 = conn.query_row(
        "SELECT turns_saved FROM capture_dedup WHERE session_id = ?1 AND batch_id = ?2",
        rusqlite::params![session_id, bid],
        |r| r.get(0),
    )?;
    Ok(Some(saved.max(0) as usize))
}
