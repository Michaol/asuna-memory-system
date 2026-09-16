use super::conversation::{SessionHeader, Turn};
use crate::index::db::Db;
use crate::index::vector::VectorStore;
use crate::util::time;
use std::path::{Path, PathBuf};

/// SessionStore: JSONL 文件 + SQLite 索引双写
pub struct SessionStore<'a> {
    conversations_dir: &'a Path,
    db: &'a Db,
    /// 单条 turn preview 截取的最大 Unicode 字符数
    preview_length: usize,
}

#[derive(Debug)]
pub struct SaveStats {
    pub session_id: String,
    pub file_path: PathBuf,
    pub turns_saved: usize,
    /// true 表示 embedder 已配置但 embed_documents 失败：会话数据照常落库，
    /// 向量本次跳过（派生数据，可通过 rebuild 后补）。
    pub vectors_skipped: bool,
}

/// 保存模式（J33 收敛：REST /capture 与 MCP save_session 共用同一持久化实现，
/// 两个入口的语义差异由模式显式区分，不再是两份各自漂移的 SQL）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    /// 覆盖（MCP save_session / CLI import）：DELETE 本 session 全部 turns +
    /// INSERT OR REPLACE sessions 全列（title/source/tags/total_tokens/end_ts…）；
    /// seq 取输入 turn.seq；JSONL 全量重写并清理旧路径文件；
    /// 向量写失败随事务回滚（数据与索引要么都新要么都旧）。
    Overwrite,
    /// 追加（REST /capture）：seq 从 COALESCE(MAX(seq),0)+1 事务内续排（输入
    /// turn.seq 被忽略）；sessions 走最小列集（新会话 INSERT / 已有会话
    /// turn_count 累加 + file_path 刷新）；向量写失败仅 warn 不回滚（M1）；
    /// JSONL 增量追加且 best-effort（失败只记日志，W3），不做旧文件清理。
    /// 完整差异见 `SessionStore::save_append`。
    Append,
}

impl<'a> SessionStore<'a> {
    pub fn new(conversations_dir: &'a Path, db: &'a Db) -> Self {
        Self {
            conversations_dir,
            db,
            preview_length: 200,
        }
    }

    /// 注入 preview_length 配置（与 Config.conversation.preview_length 联动）
    pub fn with_preview_length(mut self, n: usize) -> Self {
        // 至少留 1 字符，避免退化为空 preview
        self.preview_length = n.max(1);
        self
    }

    fn preview_of(&self, content: &str) -> String {
        content.chars().take(self.preview_length).collect()
    }

    /// 保存会话（JSONL + SQLite 双写，可自动生成向量）
    ///
    /// 降级契约：向量是可后补的派生数据。embedder 已配置但 `embed_documents`
    /// 失败（网络 / 限流 / quota 等）时，本次保存**不失败**——会话数据照常
    /// 落库（DB + JSONL），向量跳过，并在 `SaveStats::vectors_skipped` 中置位，
    /// 调用方应提示用户稍后运行 rebuild 补齐。仅 DB / JSONL 写入失败才返回 Err。
    pub fn save(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embedder: Option<&crate::embedder::LazyEmbedder>,
    ) -> anyhow::Result<SaveStats> {
        if let Some(emb) = embedder {
            let previews: Vec<String> = turns.iter().map(|t| self.preview_of(&t.content)).collect();
            let preview_refs: Vec<&str> = previews.iter().map(|s| s.as_str()).collect();
            // 文档侧使用 Document 前缀，避免与 query 侧前缀错配导致召回率下降
            match emb.embed_documents(&preview_refs) {
                Ok(embeddings) => self.save_with_embeddings(header, turns, Some(&embeddings)),
                Err(e) => {
                    tracing::warn!(
                        "会话 {} 嵌入失败，本次跳过向量（可稍后 rebuild 补齐）: {}",
                        header.session_id,
                        e
                    );
                    let mut stats = self.save_with_embeddings(header, turns, None)?;
                    stats.vectors_skipped = true;
                    Ok(stats)
                }
            }
        } else {
            self.save_with_embeddings(header, turns, None)
        }
    }

    /// 保存会话（JSONL + SQLite 双写 + 向量索引），显式指定保存模式。
    ///
    /// 顺序契约（两种模式共用，J33 收敛时自 http.rs 的内联事务与
    /// session_store 的覆盖路径合并而来）：
    /// 1. 计算 path
    /// 2. 读旧 path 用于清理（仅 Overwrite）
    /// 3. 事务内写 DB
    /// 4. commit
    /// 5. commit 成功后才写 JSONL
    /// 6. 删除旧 JSONL（仅 Overwrite，且不同路径才删）
    ///
    /// 这样保证：
    /// - DB tx 失败：JSONL 完全未触动；
    /// - JSONL 写盘失败：DB 已更新但磁盘缺失（rebuild 时该 session 直接缺席，下次 save 会覆盖）。
    ///
    /// 旧实现先写 JSONL，DB 失败会留残骸——更糟。
    ///
    /// 两种模式的语义差异（turns 处置 / sessions 列集 / seq / 向量与 JSONL
    /// 的失败粒度）见 [`SaveMode`] 与各分支文档注释；J41 跨入口契约测试
    /// （`transport::http` 测试模块与下方 store 级测试）钉住这些差异是有意的。
    pub fn save_with_embeddings_mode(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
        mode: SaveMode,
    ) -> anyhow::Result<SaveStats> {
        match mode {
            SaveMode::Overwrite => self.save_overwrite(header, turns, embeddings),
            SaveMode::Append => self.save_append(header, turns, embeddings),
        }
    }

    /// 覆盖保存（MCP save_session / CLI import 的历史语义，逐位保持）。
    /// [`Self::save_with_embeddings_mode`] 的薄包装，签名保留以稳定调用方。
    pub fn save_with_embeddings(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        self.save_with_embeddings_mode(header, turns, embeddings, SaveMode::Overwrite)
    }

    fn save_overwrite(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        // 1. 计算目标 JSONL 路径（仅计算，不写盘）
        let file_path = super::conversation::compute_session_path(self.conversations_dir, header)?;
        let file_rel_path = file_path
            .strip_prefix(self.conversations_dir)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        // 2. 计算元信息
        let start_ts = time::ts_to_unix_ms(&header.start_time)?;
        let end_ts = turns
            .last()
            .map(|t| time::ts_to_unix_ms(&t.ts).unwrap_or(start_ts));
        let total_tokens: i64 = turns.iter().map(|t| turn_tokens(t.metadata.as_ref())).sum();
        let now = time::now_unix_ms();
        let tags_json = if header.tags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&header.tags)?)
        };

        let conn = self.db.conn();

        // 3. 读旧 JSONL 路径（用于 commit 后清理；事务外只读，安全）
        let old_jsonl_path: Option<PathBuf> = conn
            .query_row(
                "SELECT file_path FROM sessions WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .map(|p| self.conversations_dir.join(p));

        // 4. 进入事务：所有 DB 改动包裹其中
        let vec_store = VectorStore::new(self.db);
        run_in_transaction(conn, || {
            // 先收集本 session 现有 turn 的 rowid（必须在 DELETE 之前，同一事务
            // 内快照一致）。用于精确删除它们的向量——旧实现每存一次就对全库做
            // `rowid NOT IN (SELECT id FROM turns)` 全表扫描，代价与库规模成正比；
            // 而本次操作只可能孤立本 session 的向量（turns AUTOINCREMENT 不复用
            // id）。全库级孤儿清理由 rebuild（整体清空 vec_turns）与 doctor 的
            // 只读检测兜底。
            let stale_turn_ids: Vec<i64> = {
                let mut stmt = conn.prepare("SELECT id FROM turns WHERE session_id = ?1")?;
                let ids = stmt
                    .query_map(rusqlite::params![header.session_id], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                ids
            };

            // 清理旧索引（INSERT OR REPLACE 只覆盖 sessions 表）
            conn.execute(
                "DELETE FROM turns WHERE session_id = ?1",
                rusqlite::params![header.session_id],
            )?;
            // 清理本 session 旧 turn 的向量（vec0 支持 rowid 等值删除）
            for id in &stale_turn_ids {
                conn.execute(
                    "DELETE FROM vec_turns WHERE rowid = ?1",
                    rusqlite::params![id],
                )?;
            }

            // 写入 session
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
                    now,
                    now,
                ],
            )?;

            // 写入 turns + 可选向量
            self.write_turn_rows(conn, header, turns, start_ts, embeddings, &vec_store)
        })?;

        // 5. DB commit 成功后写 JSONL
        super::conversation::write_session_at(&file_path, header, turns)?;

        // 6. 清理旧 JSONL（不同路径才删，且不要因清理失败而拒绝整体成功）
        cleanup_old_jsonl(old_jsonl_path, &file_path);

        Ok(SaveStats {
            session_id: header.session_id.clone(),
            file_path,
            turns_saved: turns.len(),
            vectors_skipped: false,
        })
    }

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
    /// - JSONL 增量追加且 best-effort（W3），无旧文件清理（路径派生自 session
    ///   的 start_ts，同一会话多次追加稳定指向同一文件）。
    fn save_append(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        // 1. 计算目标 JSONL 路径（与旧 archive_session_jsonl 一致：仅由 header 推导）
        let file_path = super::conversation::compute_session_path(self.conversations_dir, header)?;
        let file_rel_path = file_path
            .strip_prefix(self.conversations_dir)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        let start_ts = time::ts_to_unix_ms(&header.start_time)?;
        let now = time::now_unix_ms();

        let conn = self.db.conn();
        let vec_store = VectorStore::new(self.db);

        // 2. 事务：session 行（INSERT/UPDATE）+ turns 续排 + 向量。
        //    返回本次续排的起点（max_seq），用于 commit 后构造 JSONL 行。
        //    会话存在性判断在事务内完成（调用方持 DB mutex，与旧 handler
        //    在事务前读 sessions.start_ts 的结果一致）。
        let max_seq: i64 = run_in_transaction(conn, || {
            let existing_start_ts: Option<i64> = conn
                .query_row(
                    "SELECT start_ts FROM sessions WHERE session_id = ?1",
                    rusqlite::params![header.session_id],
                    |r| r.get(0),
                )
                .ok();

            if existing_start_ts.is_none() {
                conn.execute(
                    "INSERT INTO sessions (session_id, start_ts, file_path, turn_count, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    rusqlite::params![
                        header.session_id,
                        start_ts,
                        file_rel_path,
                        turns.len() as i64,
                        now,
                    ],
                )?;
            } else {
                conn.execute(
                    "UPDATE sessions SET turn_count = turn_count + ?1, updated_at = ?2, file_path = ?3
                     WHERE session_id = ?4",
                    rusqlite::params![turns.len() as i64, now, file_rel_path, header.session_id],
                )?;
            }

            let max_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| r.get(0),
            )?;

            for (i, turn) in turns.iter().enumerate() {
                let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
                let preview = self.preview_of(&turn.content);
                let char_count = turn.content.chars().count() as i64;
                let seq = max_seq + (i as i64) + 1;

                conn.execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        header.session_id,
                        seq,
                        ts_ms,
                        turn.role,
                        preview,
                        char_count,
                    ],
                )?;

                // J33f：向量统一走 VectorStore::insert（SQL 与旧 /capture 手写版
                // 逐字段一致）。失败仅 warn（M1 数据优先契约，见函数文档）。
                if let Some(embs) = embeddings {
                    if i < embs.len() {
                        let turn_id = conn.last_insert_rowid();
                        if let Err(e) = vec_store.insert(turn_id, &embs[i]) {
                            tracing::warn!("vec_turns insert failed for turn {}: {}", turn_id, e);
                        }
                    }
                }
            }
            Ok(max_seq)
        })?;

        // 3. DB commit 成功后写 JSONL（顺序契约），增量追加 + best-effort。
        //    JSONL 行的 seq 与事务内续排一致（旧实现用同一 max_seq 在
        //    archive_session_jsonl 独立重算），u32 截断语义保持。
        let jsonl_turns: Vec<Turn> = turns
            .iter()
            .enumerate()
            .map(|(i, t)| Turn {
                ts: t.ts.clone(),
                seq: u32::try_from(max_seq + (i as i64) + 1).unwrap_or(u32::MAX),
                role: t.role.clone(),
                content: t.content.clone(),
                metadata: None,
            })
            .collect();

        if let Err(e) = append_jsonl_turns(&file_path, header, &jsonl_turns) {
            // W3: log JSONL failures instead of silent discard —— but never fail the save
            tracing::warn!(
                "JSONL append failed for session {}: {}",
                header.session_id,
                e
            );
        }

        Ok(SaveStats {
            session_id: header.session_id.clone(),
            file_path,
            turns_saved: turns.len(),
            vectors_skipped: false,
        })
    }

    /// 写入 turns 行 + 可选向量（事务内调用）
    fn write_turn_rows(
        &self,
        conn: &rusqlite::Connection,
        header: &SessionHeader,
        turns: &[Turn],
        start_ts: i64,
        embeddings: Option<&[Vec<f32>]>,
        vec_store: &VectorStore<'_>,
    ) -> anyhow::Result<()> {
        for (i, turn) in turns.iter().enumerate() {
            let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
            let preview = self.preview_of(&turn.content);
            let char_count = turn.content.chars().count() as i64;

            conn.execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    header.session_id,
                    turn.seq as i64,
                    ts_ms,
                    turn.role,
                    preview,
                    char_count,
                ],
            )?;

            if let Some(embs) = embeddings {
                if i < embs.len() {
                    let turn_id = conn.last_insert_rowid();
                    vec_store.insert(turn_id, &embs[i])?;
                }
            }
        }
        Ok(())
    }
}

/// 单条 turn 的 token 总数（usage.input_tokens + usage.output_tokens，缺失时为 0）
fn turn_tokens(metadata: Option<&serde_json::Value>) -> i64 {
    metadata
        .and_then(|m| {
            let u = m.get("usage")?;
            let inp = u.get("input_tokens")?.as_i64()?;
            let out = u.get("output_tokens")?.as_i64()?;
            Some(inp + out)
        })
        .unwrap_or(0)
}

/// 清理旧 JSONL（不同路径才删，且不要因清理失败而拒绝整体成功）
fn cleanup_old_jsonl(old_jsonl_path: Option<PathBuf>, file_path: &Path) {
    let Some(old) = old_jsonl_path else {
        return;
    };
    if !old.exists() || old == file_path {
        return;
    }
    if let Err(e) = std::fs::remove_file(&old) {
        tracing::warn!("旧 JSONL 清理失败 {}: {}", old.display(), e);
    }
}

/// Append turn lines to a JSONL file. Creates the file with header if it doesn't exist,
/// otherwise appends lines only. Ensures parent directory exists.
///
/// J33 收敛时自 transport/http.rs（旧 /capture 的归档路径）逐位迁入，
/// 仅供 [`SaveMode::Append`] 使用；Overwrite 模式走
/// `conversation::write_session_at` 全量重写。
fn append_jsonl_turns(
    jsonl_path: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<()> {
    use std::io::Write;

    // Ensure parent directory exists (W2 fix)
    if let Some(parent) = jsonl_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(jsonl_path)?;

    // Check file size AFTER opening (avoids TOCTOU race between exists() and open())
    if file.metadata().map(|m| m.len() == 0).unwrap_or(true) {
        // Empty or new file: write header line first
        writeln!(file, "{}", serde_json::to_string(header)?)?;
    }

    for turn in turns {
        writeln!(file, "{}", serde_json::to_string(turn)?)?;
    }

    Ok(())
}

/// 事务封装：成功 COMMIT，任何 Err 触发 ROLLBACK。
/// 使用 SQL 级别 BEGIN/COMMIT/ROLLBACK 以兼容 `&Connection`（Rc 共享场景）。
fn run_in_transaction<F, T>(conn: &rusqlite::Connection, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T>,
{
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(val) => {
            conn.execute_batch("COMMIT")?;
            Ok(val)
        }
        Err(e) => {
            // 尽力回滚；即使 rollback 失败也返回原始错误
            if let Err(rb_err) = conn.execute_batch("ROLLBACK") {
                tracing::error!("回滚失败: {} (原始错误: {})", rb_err, e);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::conversation::SessionHeader;

    fn make_header() -> SessionHeader {
        SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: "dual-write-test".to_string(),
            start_time: "2026-04-10T14:30:00.000+08:00".to_string(),
            profile_id: "default".to_string(),
            source: Some("test".to_string()),
            agent_model: None,
            title: None,
            tags: vec!["test".to_string()],
        }
    }

    fn make_turns() -> Vec<Turn> {
        vec![
            Turn {
                ts: "2026-04-10T14:30:05.000+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "测试双写".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-10T14:30:10.000+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "收到".to_string(),
                metadata: Some(serde_json::json!({
                    "usage": {"input_tokens": 5, "output_tokens": 2}
                })),
            },
        ]
    }

    #[test]
    fn test_session_store_dual_write() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_dual_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let store = SessionStore::new(&tmp, &db);
        let stats = store.save(&make_header(), &make_turns(), None).unwrap();

        assert_eq!(stats.session_id, "dual-write-test");
        assert_eq!(stats.turns_saved, 2);
        assert!(!stats.vectors_skipped, "无 embedder 时不应报告跳过向量");

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let turn_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turn_count, 2);

        assert!(stats.file_path.exists());

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 验证：JSONL 写入失败（目录不可写）时 DB 必须保持一致 —— 但因为我们的实现是
    /// "先 commit 再 JSONL"，DB 会更新而 JSONL 缺失。这是可接受的（缺 JSONL 不破坏 DB）。
    /// 反向（旧实现）会留下残骸，已废弃。
    #[test]
    fn test_preview_length_config() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_preview_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db).with_preview_length(5);

        let header = make_header();
        let turns = vec![Turn {
            ts: "2026-04-10T14:30:05.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "一二三四五六七八九十".to_string(),
            metadata: None,
        }];
        store.save(&header, &turns, None).unwrap();

        let preview: String = db
            .conn()
            .query_row("SELECT preview FROM turns LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(preview, "一二三四五");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// C8/U4/U16 降级契约：API embedder 指向不可达端点（127.0.0.1:9 discard 端口，
    /// 连接必然被拒，离线确定性）时，save() 必须 Ok 且会话数据完整落库，
    /// 仅向量缺失并在 SaveStats 中置位警告。
    /// 注意：embed_batch 对 Transport 错误会重试 3 次（1s + 2s 退避），测试约 3-4s。
    #[test]
    fn test_save_degrades_when_embedder_fails() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_embed_fail_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        let mut emb_cfg = crate::config::Config::default().embedding;
        emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
        emb_cfg.api_model = "unreachable-test".to_string();
        let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg, None)
            .expect("配置了 api_url + api_model，API embedder 应构造成功");

        let store = SessionStore::new(&tmp, &db);
        let stats = store
            .save(&make_header(), &make_turns(), Some(&embedder))
            .expect("嵌入失败不得阻断 save()");

        assert!(stats.vectors_skipped, "嵌入失败必须置位 vectors_skipped");
        assert_eq!(stats.turns_saved, 2);

        let session_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(session_count, 1, "sessions 行必须落库");

        let turn_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turn_count, 2, "turns 行必须落库");

        assert!(stats.file_path.exists(), "JSONL 文件必须写盘");

        let vec_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM vec_turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_count, 0, "向量应为空（本次跳过）");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// J13: save 的向量清理必须限定在本 session——覆盖式重存时旧 turn 的向量行
    /// 被精确删除，其他 session 的向量行（哨兵）不得被波及。
    #[test]
    fn test_save_vec_cleanup_scoped_to_session() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_vecscope_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db);

        let unit_vec = |val: f32| {
            let mut v = vec![0.0f32; db.dimensions()];
            v[0] = val;
            v
        };

        // 哨兵：另一个 session 的 turn + 向量，必须全程存活
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES ('sentinel-sess', 0, 'sentinel.jsonl', 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('sentinel-sess', 1, 0, 'user', 'sentinel')",
                [],
            )
            .unwrap();
        let sentinel_turn_id = db.conn().last_insert_rowid();
        let bytes = crate::embedder::onnx::quantize_to_int8(&unit_vec(9.0));
        db.conn()
            .execute(
                "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![sentinel_turn_id, bytes],
            )
            .unwrap();

        let header = make_header();
        let turns = make_turns();

        // 第一次 save（带向量）
        store
            .save_with_embeddings(&header, &turns, Some(&[unit_vec(1.0), unit_vec(2.0)]))
            .unwrap();
        let first_ids: Vec<i64> = db
            .conn()
            .prepare("SELECT id FROM turns WHERE session_id = 'dual-write-test'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(first_ids.len(), 2);

        let mut vec_rowids: Vec<i64> = db
            .conn()
            .prepare("SELECT rowid FROM vec_turns")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        vec_rowids.sort_unstable();
        let mut expected = vec![sentinel_turn_id];
        expected.extend(first_ids.iter());
        expected.sort_unstable();
        assert_eq!(vec_rowids, expected, "首次 save：哨兵 + 本 session 向量");

        // 第二次 save（覆盖语义）：旧 turn 向量必须清掉，哨兵必须保留
        store
            .save_with_embeddings(&header, &turns, Some(&[unit_vec(3.0), unit_vec(4.0)]))
            .unwrap();
        let new_ids: Vec<i64> = db
            .conn()
            .prepare("SELECT id FROM turns WHERE session_id = 'dual-write-test'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        let mut vec_rowids: Vec<i64> = db
            .conn()
            .prepare("SELECT rowid FROM vec_turns")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        vec_rowids.sort_unstable();
        let mut expected = vec![sentinel_turn_id];
        expected.extend(new_ids.iter());
        expected.sort_unstable();
        assert_eq!(
            vec_rowids, expected,
            "重存后仅剩哨兵 + 新 turn 向量；旧 turn 向量必须被精确清理"
        );
        for old in &first_ids {
            assert!(
                !vec_rowids.contains(old),
                "stale vector rowid {} must be gone",
                old
            );
        }
        assert!(
            vec_rowids.contains(&sentinel_turn_id),
            "sentinel (other session) vector must survive"
        );

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// J41 store-level 契约（Append 侧）：同 session 两次 Append → seq 从
    /// MAX(seq) 续排且连续、turn_count 累加、vec_turns 行数 = 总 turns 且
    /// rowid 与 turns.id 一一对应（J33f：统一走 VectorStore::insert）、
    /// file_path 为真实 JSONL 相对路径（J33b：无 gateway:// 伪 URI）且文件
    /// 存在、JSONL 为 header + 全部 5 行（含续排后的 seq）。
    #[test]
    fn test_append_mode_twice_accumulates_with_vectors() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_append2_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db);

        let unit_vec = |val: f32| {
            let mut v = vec![0.0f32; db.dimensions()];
            v[0] = val;
            v
        };
        let header = make_header();

        let stats1 = store
            .save_with_embeddings_mode(
                &header,
                &make_turns(),
                Some(&[unit_vec(1.0), unit_vec(2.0)]),
                SaveMode::Append,
            )
            .unwrap();
        assert_eq!(stats1.turns_saved, 2);

        let batch2 = vec![Turn {
            ts: "2026-04-10T14:31:00.000+08:00".to_string(),
            seq: 99, // Append 模式必须忽略输入 seq（/capture 请求体本无 seq）
            role: "user".to_string(),
            content: "追加轮次".to_string(),
            metadata: None,
        }];
        let stats2 = store
            .save_with_embeddings_mode(&header, &batch2, Some(&[unit_vec(3.0)]), SaveMode::Append)
            .unwrap();
        assert_eq!(stats2.turns_saved, 1);

        // seq 连续：1,2,3（第二批不得是 99）
        let seqs: Vec<i64> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT seq FROM turns ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(seqs, vec![1, 2, 3], "Append 必须从 MAX(seq) 续排");

        // sessions：turn_count 累加，file_path 真实且文件存在
        let (turn_count, file_path): (i64, String) = db
            .conn()
            .query_row(
                "SELECT turn_count, file_path FROM sessions WHERE session_id = ?1",
                rusqlite::params![header.session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(turn_count, 3, "turn_count 必须累加");
        assert!(
            !file_path.contains("gateway://"),
            "J33b: file_path 不得再写伪 URI, got {file_path}"
        );
        let jsonl = tmp.join(&file_path);
        assert!(jsonl.exists(), "file_path 必须指向真实落盘的 JSONL");

        // JSONL：header + 3 行，行内 seq 与 DB 续排一致
        let (hdr, jsonl_turns) = super::super::conversation::read_session(&jsonl).unwrap();
        assert_eq!(hdr.session_id, header.session_id);
        assert_eq!(jsonl_turns.len(), 3);
        assert_eq!(
            jsonl_turns.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        // 向量：行数 = 总 turns，且 rowid 集合 == 本 session 的 turns.id 集合
        let turn_ids: Vec<i64> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT id FROM turns WHERE session_id = ?1 ORDER BY id")
                .unwrap();
            stmt.query_map(rusqlite::params![header.session_id], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let vec_ids: Vec<i64> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT rowid FROM vec_turns ORDER BY rowid")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(vec_ids, turn_ids, "Append 向量 rowid 必须逐 turn 对应");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// J41 store-level 契约（Overwrite 侧）：同 session 两次 Overwrite →
    /// turns 行数不翻倍、S8 定点清理后 vec_turns 仅剩最新一轮、turn_count
    /// 为全量覆盖值（非累加）、JSONL 全量重写（行数不叠加）、file_path 真实。
    #[test]
    fn test_overwrite_mode_twice_no_row_doubling() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_ovr2_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let store = SessionStore::new(&tmp, &db);

        let unit_vec = |val: f32| {
            let mut v = vec![0.0f32; db.dimensions()];
            v[0] = val;
            v
        };
        let header = make_header();
        let turns = make_turns();

        store
            .save_with_embeddings_mode(
                &header,
                &turns,
                Some(&[unit_vec(1.0), unit_vec(2.0)]),
                SaveMode::Overwrite,
            )
            .unwrap();
        let first_ids: Vec<i64> = {
            let mut stmt = db.conn().prepare("SELECT id FROM turns").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        store
            .save_with_embeddings_mode(
                &header,
                &turns,
                Some(&[unit_vec(3.0), unit_vec(4.0)]),
                SaveMode::Overwrite,
            )
            .unwrap();

        let turn_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turn_count, 2, "Overwrite 两次后 turns 行数不得翻倍");

        let vec_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM vec_turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vec_count, 2, "S8 定点清理：仅剩最新一轮的向量");
        let live_ids: Vec<i64> = {
            let mut stmt = db.conn().prepare("SELECT id FROM turns").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        for old in &first_ids {
            assert!(
                !live_ids.contains(old) && !has_vec_row(&db, *old),
                "旧 turn 行与其向量都必须被覆盖清掉 (rowid {old})"
            );
        }

        let (sessions_count, session_turn_count, file_path): (i64, i64, String) = db
            .conn()
            .query_row(
                "SELECT COUNT(*), (SELECT turn_count FROM sessions WHERE session_id = 'dual-write-test'),
                        (SELECT file_path FROM sessions WHERE session_id = 'dual-write-test')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(sessions_count, 1);
        assert_eq!(session_turn_count, 2, "Overwrite 语义为全量覆盖而非累加");
        assert!(!file_path.contains("gateway://"));

        // JSONL 全量重写：header + 2 行（不是两次保存叠加的 5 行）
        let jsonl = tmp.join(&file_path);
        let (_, jsonl_turns) = super::super::conversation::read_session(&jsonl).unwrap();
        assert_eq!(jsonl_turns.len(), 2);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 辅助：vec_turns 中是否存在指定 rowid（S8 定点清理断言用）
    fn has_vec_row(db: &Db, rowid: i64) -> bool {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM vec_turns WHERE rowid = ?1",
                rusqlite::params![rowid],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
    }
}
