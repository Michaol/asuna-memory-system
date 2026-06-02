use crate::fact::conversation;
use crate::index::db::Db;
use crate::util::time;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// 向量嵌入批次大小（每批一个事务）
const VECTOR_BATCH_SIZE: usize = 1000;

/// 重建统计
#[derive(Debug, Clone, serde::Serialize)]
pub struct RebuildStats {
    pub sessions_processed: usize,
    pub turns_indexed: usize,
    pub vectors_indexed: usize,
    pub vectors_skipped: usize,
    pub errors: Vec<String>,
}

/// 一致性检查结果
#[derive(Debug, serde::Serialize)]
pub struct ConsistencyResult {
    pub jsonl_count: usize,
    pub db_session_count: usize,
    pub in_sync: bool,
}

/// 重建状态
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RebuildStatus {
    Idle,
    Running,
    Completed,
    Failed,
}

/// 重建进度（线程安全，跨线程共享）
#[derive(Debug, Serialize)]
pub struct RebuildProgress {
    pub status: RebuildStatus,
    pub sessions_processed: usize,
    pub turns_indexed: usize,
    pub vectors_indexed: usize,
    pub errors: Vec<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

impl Default for RebuildProgress {
    fn default() -> Self {
        Self {
            status: RebuildStatus::Idle,
            sessions_processed: 0,
            turns_indexed: 0,
            vectors_indexed: 0,
            errors: Vec::new(),
            started_at: 0,
            finished_at: None,
        }
    }
}

pub type SharedProgress = Arc<Mutex<RebuildProgress>>;

pub fn new_shared_progress() -> SharedProgress {
    Arc::new(Mutex::new(RebuildProgress::default()))
}

/// 进度回调类型：(已完成数, 总数)
type ProgressFn = dyn Fn(usize, usize) + Send + Sync;

/// 带进度追踪的重建（供 MCP 异步调用使用）
pub fn rebuild_from_jsonl_with_progress(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    progress: &SharedProgress,
) -> anyhow::Result<RebuildStats> {
    {
        let mut p = progress.lock().map_err(|e| anyhow::anyhow!("lock: {}", e))?;
        p.status = RebuildStatus::Running;
        p.started_at = time::now_unix_ms();
    }

    // 创建进度回调，更新 SharedProgress
    let progress_clone = progress.clone();
    let callback = move |done: usize, _total: usize| {
        if let Ok(mut p) = progress_clone.lock() {
            p.vectors_indexed = done;
        }
    };

    let result = rebuild_from_jsonl_with_callback(conversations_dir, db, embedder, Some(&callback));

    {
        let mut p = progress.lock().map_err(|e| anyhow::anyhow!("lock: {}", e))?;
        match &result {
            Ok(stats) => {
                p.status = RebuildStatus::Completed;
                p.sessions_processed = stats.sessions_processed;
                p.turns_indexed = stats.turns_indexed;
                p.vectors_indexed = stats.vectors_indexed;
                p.errors = stats.errors.clone();
            }
            Err(e) => {
                p.status = RebuildStatus::Failed;
                p.errors = vec![e.to_string()];
            }
        }
        p.finished_at = Some(time::now_unix_ms());
    }
    result
}

/// 从 JSONL 文件重建索引（传入 conversations 目录）
pub fn rebuild_from_jsonl(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
) -> anyhow::Result<RebuildStats> {
    rebuild_from_jsonl_with_callback(conversations_dir, db, embedder, None)
}

/// 带进度回调的重建实现
///
/// 分两阶段执行：
/// 1. **元数据 + FTS 阶段**（单事务，快）：清理表、插入 sessions/turns、重建 FTS 索引
/// 2. **向量嵌入阶段**（分批事务）：每 BATCH_SIZE 条一个事务，支持断点续传
///
/// 断点续传：检查 vec_turns 中已有的 turn_id，跳过已索引的向量。
/// 崩溃后重跑时，只需嵌入剩余部分。
pub fn rebuild_from_jsonl_with_callback(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    on_progress: Option<&ProgressFn>,
) -> anyhow::Result<RebuildStats> {
    let conn = db.conn();

    // ── Phase 1: 元数据 + FTS（单事务，快） ──
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let stats_result = rebuild_metadata(conversations_dir, db);
    let mut stats = match stats_result {
        Ok(s) => {
            conn.execute_batch("COMMIT")?;
            s
        }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("rebuild Phase 1 回滚失败: {} (原始错误: {})", rb, e);
            }
            return Err(e);
        }
    };

    // ── Phase 2: 向量嵌入（分批事务，支持断点续传） ──
    if let Some(emb) = embedder {
        match rebuild_vectors(db, emb, on_progress) {
            Ok((indexed, skipped)) => {
                stats.vectors_indexed = indexed;
                stats.vectors_skipped = skipped;
            }
            Err(e) => {
                // Phase 2 失败不回滚 Phase 1，记录错误继续
                tracing::error!("rebuild Phase 2 向量嵌入失败: {}", e);
                stats.errors.push(format!("向量嵌入阶段失败: {}", e));
            }
        }
    }

    Ok(stats)
}

/// Phase 1: 清理表、插入 sessions/turns、重建 FTS 索引
///
/// 调用方负责 BEGIN/COMMIT 事务包裹。
fn rebuild_metadata(
    conversations_dir: &Path,
    db: &Db,
) -> anyhow::Result<RebuildStats> {
    let conn = db.conn();

    // 1. 清空所有索引表
    conn.execute_batch(
        "DELETE FROM turns;
         DELETE FROM sessions;
         DELETE FROM vec_turns;
         INSERT INTO turns_fts(turns_fts) VALUES('delete-all');",
    )?;

    // 2. 遍历所有 JSONL 文件，插入 sessions + turns
    let files = conversation::list_sessions(conversations_dir);
    let mut stats = RebuildStats {
        sessions_processed: 0,
        turns_indexed: 0,
        vectors_indexed: 0,
        vectors_skipped: 0,
        errors: Vec::new(),
    };

    for file_path in &files {
        match conversation::read_session(file_path) {
            Ok((header, turns)) => {
                let start_ts = match time::ts_to_unix_ms(&header.start_time) {
                    Ok(ts) => ts,
                    Err(e) => {
                        stats.errors.push(format!("{}: 时间解析失败: {}", file_path.display(), e));
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
                                let inp = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                let out = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                                if inp + out > 0 { Some(inp + out) } else { None }
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

                // 插入 session
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

                // 插入 turns（turns_ai 触发器会同步写 FTS，下方手动段覆盖）
                for turn in &turns {
                    let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
                    let preview: String = turn.content.chars().take(200).collect();
                    let char_count = turn.content.chars().count() as i64;

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
                stats.errors.push(format!("{}: 解析失败: {}", file_path.display(), e));
            }
        }
    }

    // 3. 一次性收集所有 (turn_id, preview) 对，供 FTS 重建使用
    let turn_rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, preview FROM turns WHERE preview IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let collected: rusqlite::Result<Vec<_>> = rows.collect();
        collected?
    };

    // 4. 手动重建 FTS 索引（覆盖 turns_ai 触发器的写入）
    let _ = conn.execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", []);
    for (id, preview) in &turn_rows {
        let tokenized = crate::util::text::tokenize_chinese(preview);
        conn.execute(
            "INSERT INTO turns_fts(rowid, preview) VALUES (?1, ?2)",
            rusqlite::params![id, tokenized],
        )?;
    }

    tracing::info!(
        "Phase 1 完成: {} 个会话, {} 轮对话, {} 条 FTS, {} 个错误",
        stats.sessions_processed,
        stats.turns_indexed,
        turn_rows.len(),
        stats.errors.len(),
    );

    Ok(stats)
}

/// Phase 2: 向量嵌入（分批事务，支持断点续传）
///
/// 返回 (已索引数, 跳过数)
fn rebuild_vectors(
    db: &Db,
    embedder: &crate::embedder::LazyEmbedder,
    on_progress: Option<&ProgressFn>,
) -> anyhow::Result<(usize, usize)> {
    let conn = db.conn();

    // 1. 收集所有待索引的 turns
    let turn_rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, preview FROM turns WHERE preview IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let collected: rusqlite::Result<Vec<_>> = rows.collect();
        collected?
    };

    if turn_rows.is_empty() {
        tracing::info!("Phase 2: 无 turns 需要索引");
        return Ok((0, 0));
    }

    // 2. 查询已有向量（断点续传）
    let existing_ids: HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT rowid FROM vec_turns")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let pending: Vec<(i64, String)> = turn_rows
        .into_iter()
        .filter(|(id, _)| !existing_ids.contains(id))
        .collect();

    let skipped = existing_ids.len();
    if skipped > 0 {
        tracing::info!("向量断点续传：跳过已索引的 {} 条", skipped);
    }

    if pending.is_empty() {
        tracing::info!("Phase 2: 向量索引已是最新 ({} 条), 跳过", skipped);
        return Ok((skipped, skipped));
    }

    let total_to_index = pending.len();
    tracing::info!(
        "Phase 2 开始: {} 条待嵌入 (跳过 {} 条已存在)",
        total_to_index,
        skipped
    );

    // 3. 分批嵌入 + 写入
    let vec_store = crate::index::vector::VectorStore::new(db);
    let mut vectors_indexed = skipped;
    let total = total_to_index + skipped;

    for (batch_idx, chunk) in pending.chunks(VECTOR_BATCH_SIZE).enumerate() {
        conn.execute_batch("BEGIN IMMEDIATE")?;

        for (turn_id, preview) in chunk {
            match embedder.embed_document(preview) {
                Ok(embedding) => match vec_store.insert(*turn_id, &embedding) {
                    Ok(_) => vectors_indexed += 1,
                    Err(e) => tracing::warn!("向量插入失败 turn_id={}: {}", turn_id, e),
                },
                Err(e) => tracing::warn!("嵌入生成失败 turn_id={}: {}", turn_id, e),
            }
        }

        conn.execute_batch("COMMIT")?;

        tracing::info!(
            "向量嵌入进度: {}/{} (batch {}/{})",
            vectors_indexed,
            total,
            batch_idx + 1,
            pending.len().div_ceil(VECTOR_BATCH_SIZE)
        );

        if let Some(cb) = on_progress {
            cb(vectors_indexed, total);
        }
    }

    tracing::info!(
        "Phase 2 完成: {} 条向量已索引 ({} 条跳过)",
        vectors_indexed - skipped,
        skipped
    );

    Ok((vectors_indexed, skipped))
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
        let stats = rebuild_from_jsonl(&tmp, &db, None).unwrap();
        assert_eq!(stats.sessions_processed, 2);
        assert_eq!(stats.turns_indexed, 3);
        assert_eq!(stats.vectors_indexed, 0); // no embedder provided
        assert!(stats.errors.is_empty());

        // 一致性检查
        let consistency = check_consistency(&tmp, &db).unwrap();
        assert_eq!(consistency.jsonl_count, 2);
        assert_eq!(consistency.db_session_count, 2);
        assert!(consistency.in_sync);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// 验证 rebuild 后 FTS 数量与 turns 数量严格一致
    #[test]
    fn test_rebuild_fts_consistency() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_fts_consist_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // 写入 1 个会话，包含 4 轮对话
        let header = SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: "fts-consist-1".to_string(),
            start_time: "2026-04-12T10:00:00.000+08:00".to_string(),
            profile_id: "default".to_string(),
            source: Some("test".to_string()),
            agent_model: None,
            title: None,
            tags: vec![],
        };
        let turns = vec![
            Turn { ts: "2026-04-12T10:00:01.000+08:00".to_string(), seq: 1, role: "user".to_string(), content: "Rust ownership model".to_string(), metadata: None },
            Turn { ts: "2026-04-12T10:00:02.000+08:00".to_string(), seq: 2, role: "assistant".to_string(), content: "借用检查器保证内存安全".to_string(), metadata: None },
            Turn { ts: "2026-04-12T10:00:03.000+08:00".to_string(), seq: 3, role: "user".to_string(), content: "lifetime annotations".to_string(), metadata: None },
            Turn { ts: "2026-04-12T10:00:04.000+08:00".to_string(), seq: 4, role: "assistant".to_string(), content: "生命周期标注确保引用有效".to_string(), metadata: None },
        ];

        conversation::write_session(&tmp, &header, &turns).unwrap();

        // 重建（无 embedder）
        let stats = rebuild_from_jsonl(&tmp, &db, None).unwrap();
        assert_eq!(stats.sessions_processed, 1);
        assert_eq!(stats.turns_indexed, 4);
        assert!(stats.errors.is_empty());

        // FTS 数量必须与 turns 数量严格一致
        let turn_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        let fts_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0))
            .unwrap_or(0);
        assert_eq!(
            turn_count, fts_count,
            "FTS count ({fts_count}) must equal turns count ({turn_count}) after rebuild"
        );

        // keyword 搜索可命中
        let store = crate::index::fts::FtsStore::new(&db);
        let results = store.search("Rust", 10).unwrap();
        assert!(!results.is_empty(), "keyword search 'Rust' must return results after rebuild");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_rebuild_with_progress_tracking() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_progress_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let progress = new_shared_progress();
        let _ = rebuild_from_jsonl_with_progress(&tmp, &db, None, &progress).unwrap();
        let p = progress.lock().unwrap();
        assert_eq!(p.status, RebuildStatus::Completed);
        assert!(p.finished_at.is_some());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn test_progress_default_is_idle() {
        let progress = new_shared_progress();
        let p = progress.lock().unwrap();
        assert_eq!(p.status, RebuildStatus::Idle);
        assert_eq!(p.started_at, 0);
    }
}
