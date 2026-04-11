use super::conversation::{SessionHeader, Turn};
use crate::index::db::Db;
use crate::index::vector::VectorStore;
use crate::util::time;
use std::path::{Path, PathBuf};

/// SessionStore: JSONL 文件 + SQLite 索引双写
pub struct SessionStore<'a> {
    conversations_dir: &'a Path,
    db: &'a Db,
}

#[derive(Debug)]
pub struct SaveStats {
    pub session_id: String,
    pub file_path: PathBuf,
    pub turns_saved: usize,
}

impl<'a> SessionStore<'a> {
    pub fn new(conversations_dir: &'a Path, db: &'a Db) -> Self {
        Self {
            conversations_dir,
            db,
        }
    }

    /// 保存会话（JSONL + SQLite 双写，无向量）
    pub fn save(&self, header: &SessionHeader, turns: &[Turn]) -> anyhow::Result<SaveStats> {
        self.save_with_embeddings(header, turns, None)
    }

    /// 保存会话（JSONL + SQLite 双写 + 向量索引）
    /// embeddings: 可选的向量列表，与 turns 一一对应
    pub fn save_with_embeddings(
        &self,
        header: &SessionHeader,
        turns: &[Turn],
        embeddings: Option<&[Vec<f32>]>,
    ) -> anyhow::Result<SaveStats> {
        // 1. 写入 JSONL
        let file_path = super::conversation::write_session(self.conversations_dir, header, turns)?;

        // 2. 计算时间范围
        let start_ts = time::ts_to_unix_ms(&header.start_time)?;
        let end_ts = turns
            .last()
            .map(|t| time::ts_to_unix_ms(&t.ts).unwrap_or(start_ts));

        // 计算总 tokens
        let total_tokens: i64 = turns
            .iter()
            .map(|t| {
                if let Some(ref meta) = t.metadata {
                    meta.get("usage")
                        .and_then(|u| {
                            u.get("input_tokens")
                                .and_then(|v| v.as_i64())
                                .zip(u.get("output_tokens").and_then(|v| v.as_i64()))
                        })
                        .map(|(inp, out)| inp + out)
                        .unwrap_or(0)
                } else {
                    0
                }
            })
            .sum();

        let now = time::now_unix_ms();
        let file_rel_path = file_path
            .strip_prefix(self.conversations_dir)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        let tags_json = if header.tags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&header.tags)?)
        };

        let conn = self.db.conn();

        // [FIX] 开始写入前，如果 session_id 已存在，必须先清理旧的 JSONL 文件
        // 由于文件名包含时间戳，start_time 变化会导致生成不同文件名的文件，
        // 如果不删除旧文件，rebuild 时会识别到两个具有相同 session_id 的文件，导致崩溃。
        if let Ok(old_path) = conn.query_row(
            "SELECT file_path FROM sessions WHERE session_id = ?1",
            rusqlite::params![header.session_id],
            |r| r.get::<_, String>(0),
        ) {
            let full_old_path = self.conversations_dir.join(&old_path);
            if full_old_path.exists() && full_old_path != file_path {
                let _ = std::fs::remove_file(full_old_path);
            }
        }

        // 清理数据库索引关联数据（INSERT OR REPLACE 只处理 sessions 表）
        conn.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            rusqlite::params![header.session_id],
        )?;
        conn.execute(
            "DELETE FROM vec_turns WHERE rowid NOT IN (SELECT id FROM turns)",
            [],
        )?; // 清理孤立向量
            // FTS 触发器会自动同步 DELETE

        // 3. 插入 session
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

        // 4. 插入 turns（+ 可选向量）
        let vec_store = VectorStore::new(self.db);
        for (i, turn) in turns.iter().enumerate() {
            let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
            let preview: String = turn.content.chars().take(200).collect();
            let char_count = turn.content.len() as i64;

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

            // 如果提供了 embedding，写入向量表
            if let Some(embs) = embeddings {
                if i < embs.len() {
                    let turn_id = conn.last_insert_rowid();
                    vec_store.insert(turn_id, &embs[i])?;
                }
            }
        }

        Ok(SaveStats {
            session_id: header.session_id.clone(),
            file_path,
            turns_saved: turns.len(),
        })
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
        let stats = store.save(&make_header(), &make_turns()).unwrap();

        assert_eq!(stats.session_id, "dual-write-test");
        assert_eq!(stats.turns_saved, 2);

        // 验证 SQLite 数据
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

        // 验证 JSONL 文件存在
        assert!(stats.file_path.exists());

        // 清理
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
