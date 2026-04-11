use crate::fact::conversation;
use crate::index::db::Db;
use crate::util::time;
use std::path::Path;

/// 重建统计
#[derive(Debug, serde::Serialize)]
pub struct RebuildStats {
    pub sessions_processed: usize,
    pub turns_indexed: usize,
    pub errors: Vec<String>,
}

/// 一致性检查结果
#[derive(Debug, serde::Serialize)]
pub struct ConsistencyResult {
    pub jsonl_count: usize,
    pub db_session_count: usize,
    pub in_sync: bool,
}

/// 从 JSONL 文件重建索引（传入 conversations 目录）
pub fn rebuild_from_jsonl(conversations_dir: &Path, db: &Db) -> anyhow::Result<RebuildStats> {
    let conn = db.conn();

    // 1. 清空所有索引表
    conn.execute_batch(
        "DELETE FROM turns;
         DELETE FROM sessions;
         DELETE FROM vec_turns;
         INSERT INTO turns_fts(turns_fts) VALUES('delete-all');",
    )?;
    // FTS5 自动通过 content= 表同步

    // 2. 遍历所有 JSONL 文件
    let files = conversation::list_sessions(conversations_dir);
    let mut stats = RebuildStats {
        sessions_processed: 0,
        turns_indexed: 0,
        errors: Vec::new(),
    };

    for file_path in &files {
        match conversation::read_session(file_path) {
            Ok((header, turns)) => {
                let start_ts = match time::ts_to_unix_ms(&header.start_time) {
                    Ok(ts) => ts,
                    Err(e) => {
                        stats
                            .errors
                            .push(format!("{}: 时间解析失败: {}", file_path.display(), e));
                        continue;
                    }
                };

                let end_ts = turns.last().and_then(|t| time::ts_to_unix_ms(&t.ts).ok());

                let total_tokens: i64 = turns
                    .iter()
                    .map(|t| {
                        t.metadata
                            .as_ref()
                            .and_then(|m| m.get("usage"))
                            .and_then(|u| {
                                let inp =
                                    u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                let out =
                                    u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                if inp + out > 0 {
                                    Some(inp + out)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0)
                    })
                    .sum();

                let file_rel_path = file_path
                    .strip_prefix(conversations_dir)
                    .unwrap_or(file_path)
                    .to_string_lossy()
                    .to_string();

                let tags_json = if header.tags.is_empty() {
                    None
                } else {
                    serde_json::to_string(&header.tags).ok()
                };

                let now = time::now_unix_ms();

                // 插入 session (使用 REPLACE 确保原子性，尽管上面已经 DELETE)
                if let Err(e) = conn.execute(
                    "INSERT OR REPLACE INTO sessions
                     (session_id, start_ts, end_ts, file_path, title, profile_id, source, agent_model,
                      turn_count, total_tokens, tags, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    rusqlite::params![
                        header.session_id, start_ts, end_ts, file_rel_path,
                        header.title,
                        header.profile_id, header.source, header.agent_model,
                        turns.len() as i64, total_tokens, tags_json, now, now,
                    ],
                ) {
                    stats.errors.push(format!("{}: session 插入失败: {}", file_path.display(), e));
                    continue;
                }

                // 插入 turns
                for turn in &turns {
                    let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
                    let preview: String = turn.content.chars().take(200).collect();
                    let char_count = turn.content.len() as i64;

                    if let Err(e) = conn.execute(
                        "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            header.session_id, turn.seq as i64, ts_ms,
                            turn.role, preview, char_count,
                        ],
                    ) {
                        stats.errors.push(format!("{}: turn {} 插入失败: {}", file_path.display(), turn.seq, e));
                    } else {
                        stats.turns_indexed += 1;
                    }
                }

                stats.sessions_processed += 1;
            }
            Err(e) => {
                stats
                    .errors
                    .push(format!("{}: 解析失败: {}", file_path.display(), e));
            }
        }
    }

    // 手动重建 FTS 索引（不使用 FTS5 内置 rebuild，因为它绕过触发器，无法应用中文分词）
    let _ = conn.execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", []);
    let fts_count = conn.execute(
        "INSERT INTO turns_fts(rowid, preview) SELECT id, tokenize_zh(preview) FROM turns WHERE preview IS NOT NULL",
        [],
    ).unwrap_or(0);
    tracing::info!("FTS 索引重建：已分词并索引 {} 条记录", fts_count);

    tracing::info!(
        "索引重建完成: {} 个会话, {} 轮对话, {} 个错误",
        stats.sessions_processed,
        stats.turns_indexed,
        stats.errors.len(),
    );

    Ok(stats)
}

/// 检查 JSONL 与 SQLite 索引的一致性
pub fn check_consistency(conversations_dir: &Path, db: &Db) -> anyhow::Result<ConsistencyResult> {
    let jsonl_files = conversation::list_sessions(conversations_dir);

    let db_count: usize = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| {
            r.get::<_, i64>(0).map(|v| v as usize)
        })?;

    Ok(ConsistencyResult {
        jsonl_count: jsonl_files.len(),
        db_session_count: db_count,
        in_sync: jsonl_files.len() == db_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::conversation::{SessionHeader, Turn};

    #[test]
    fn test_rebuild_from_jsonl() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_rebuild_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // 写入两个测试会话
        let header1 = SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: "rebuild-1".to_string(),
            start_time: "2026-04-01T10:00:00.000+08:00".to_string(),
            profile_id: "default".to_string(),
            source: Some("test".to_string()),
            agent_model: None,
            title: None,
            tags: vec![],
        };
        let turns1 = vec![Turn {
            ts: "2026-04-01T10:00:05.000+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "你好".to_string(),
            metadata: None,
        }];

        let header2 = SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: "rebuild-2".to_string(),
            start_time: "2026-04-02T14:00:00.000+08:00".to_string(),
            profile_id: "default".to_string(),
            source: Some("test".to_string()),
            agent_model: None,
            title: None,
            tags: vec![],
        };
        let turns2 = vec![
            Turn {
                ts: "2026-04-02T14:00:05.000+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "关于 Rust".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-02T14:00:10.000+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "Rust 很好".to_string(),
                metadata: None,
            },
        ];

        conversation::write_session(&tmp, &header1, &turns1).unwrap();
        conversation::write_session(&tmp, &header2, &turns2).unwrap();

        // 重建
        let stats = rebuild_from_jsonl(&tmp, &db).unwrap();
        assert_eq!(stats.sessions_processed, 2);
        assert_eq!(stats.turns_indexed, 3);
        assert!(stats.errors.is_empty());

        // 一致性检查
        let consistency = check_consistency(&tmp, &db).unwrap();
        assert_eq!(consistency.jsonl_count, 2);
        assert_eq!(consistency.db_session_count, 2);
        assert!(consistency.in_sync);

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
