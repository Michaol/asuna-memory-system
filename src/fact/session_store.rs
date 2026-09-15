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

    /// 保存会话（JSONL + SQLite 双写 + 向量索引）
    ///
    /// 顺序：
    /// 1. 计算 path
    /// 2. 读旧 path 用于清理
    /// 3. 事务内写 DB
    /// 4. commit
    /// 5. commit 成功后才写 JSONL
    /// 6. 删除旧 JSONL（若不同路径）
    ///
    /// 这样保证：
    /// - DB tx 失败：JSONL 完全未触动；
    /// - JSONL 写盘失败：DB 已更新但磁盘缺失（rebuild 时该 session 直接缺席，下次 save 会覆盖）。
    ///
    /// 旧实现先写 JSONL，DB 失败会留残骸——更糟。
    pub fn save_with_embeddings(
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
}
