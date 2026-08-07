use crate::fact::conversation;
use crate::index::db::Db;
use crate::util::time;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    full_rebuild: bool,
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

    let result = rebuild_from_jsonl_with_callback(conversations_dir, db, embedder, Some(&callback), full_rebuild);

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
    full_rebuild: bool,
) -> anyhow::Result<RebuildStats> {
    rebuild_from_jsonl_with_callback(conversations_dir, db, embedder, None, full_rebuild)
}

/// 带进度回调的重建实现
///
/// 分两阶段执行：
/// 1. **元数据 + FTS 阶段**（单事务，快）：清理表、插入 sessions/turns、重建 FTS 索引
/// 2. **向量嵌入阶段**（分批事务）：批次大小由 `embedder.batch_size()` 决定，支持断点续传
///
/// **断点续传**：检查 vec_turns 中已有的 turn_id，跳过已索引的向量。
/// 崩溃后重跑时，只需嵌入剩余部分。
///
/// **增量模式**：当 `full_rebuild=false` 且 DB 中已有数据时，自动跳过 Phase 1，
/// 直接进入 Phase 2 继续未完成的向量嵌入。这避免了重新处理已完成的元数据阶段。
pub fn rebuild_from_jsonl_with_callback(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    on_progress: Option<&ProgressFn>,
    full_rebuild: bool,
) -> anyhow::Result<RebuildStats> {
    let conn = db.conn();

    // 判断是否使用增量模式（跳过 Phase 1）
    let incremental = !full_rebuild && should_do_incremental_rebuild(db, conversations_dir);

    let mut stats = if incremental {
        tracing::info!("增量模式：DB 中已有数据，跳过 Phase 1（元数据+FTS），直接进入 Phase 2（向量嵌入）");
        // 从现有 DB 读取统计信息
        let session_count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)).unwrap_or(0);
        let turn_count: i64 = conn.query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0)).unwrap_or(0);
        RebuildStats {
            sessions_processed: session_count as usize,
            turns_indexed: turn_count as usize,
            vectors_indexed: 0,
            vectors_skipped: 0,
            errors: Vec::new(),
        }
    } else {
        // ── Phase 1: 元数据 + FTS（单事务，快） ──
        tracing::info!("完整重建模式：执行 Phase 1（元数据+FTS）");
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let stats_result = rebuild_metadata(conversations_dir, db);
        match stats_result {
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
        }
    };

    // ── Phase 2: 向量嵌入（分批事务，支持断点续传） ──
    if let Some(emb) = embedder {
        match rebuild_vectors(db, emb, on_progress) {
            Ok((indexed, skipped, failed, errs)) => {
                stats.vectors_indexed = indexed;
                stats.vectors_skipped = skipped;
                if failed > 0 {
                    // Surface partial/total vector failures so an all-failing index
                    // (e.g. embedding dimension mismatch) is not reported as success.
                    stats.errors.push(format!(
                        "向量阶段: {} 条 turn 未能索引，语义/混合搜索将不完整",
                        failed
                    ));
                    stats.errors.extend(errs);
                }
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

/// 判断是否应该使用增量重建模式
///
/// 条件：
/// 1. DB 中已有 sessions 数据（非首次运行）
/// 2. JSONL 文件数量与 DB 中的 sessions 数量一致
///
/// 如果数量不一致，说明 JSONL 发生了变化，需要完整重建。
fn should_do_incremental_rebuild(db: &Db, conversations_dir: &Path) -> bool {
    let conn = db.conn();

    // 检查 DB 中是否有数据
    let session_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);

    if session_count == 0 {
        tracing::info!("增量检测：DB 为空，需要完整重建");
        return false;
    }

    // 比较 JSONL 数量与 DB 数量
    let jsonl_count = conversation::list_sessions(conversations_dir).len();
    let db_count = session_count as usize;

    if jsonl_count != db_count {
        tracing::info!(
            "增量检测：JSONL 数量 ({}) 与 DB 数量 ({}) 不一致，需要完整重建",
            jsonl_count,
            db_count
        );
        return false;
    }

    tracing::info!(
        "增量检测：JSONL 与 DB 数量一致 ({} sessions)，可以增量重建",
        db_count
    );
    true
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
                index_session_file(conn, conversations_dir, file_path, &header, &turns, &mut stats);
            }
            Err(e) => {
                stats.errors.push(format!("{}: 解析失败: {}", file_path.display(), e));
            }
        }
    }

    // 3. 一次性收集所有 (turn_id, preview) 对，供 FTS 重建使用
    let turn_rows = load_turn_previews(conn)?;

    // 4. 手动重建 FTS 索引（覆盖 turns_ai 触发器的写入）
    rebuild_fts_rows(conn, &turn_rows)?;

    tracing::info!(
        "Phase 1 完成: {} 个会话, {} 轮对话, {} 条 FTS, {} 个错误",
        stats.sessions_processed,
        stats.turns_indexed,
        turn_rows.len(),
        stats.errors.len(),
    );

    Ok(stats)
}

/// 处理单个 JSONL 会话文件：插入 session 与其所有 turns
///
/// 从 `rebuild_metadata` 的文件循环中提取。原循环内的 `continue`（时间解析失败、
/// session 插入失败时跳过本文件）在函数内等价转换为 `return`。
fn index_session_file(
    conn: &rusqlite::Connection,
    conversations_dir: &Path,
    file_path: &Path,
    header: &conversation::SessionHeader,
    turns: &[conversation::Turn],
    stats: &mut RebuildStats,
) {
    let start_ts = match time::ts_to_unix_ms(&header.start_time) {
        Ok(ts) => ts,
        Err(e) => {
            stats.errors.push(format!("{}: 时间解析失败: {}", file_path.display(), e));
            return;
        }
    };

    let end_ts = turns.last().and_then(|t| time::ts_to_unix_ms(&t.ts).ok());

    let total_tokens: i64 = session_total_tokens(turns);

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
        return;
    }

    // 插入 turns（turns_ai 触发器会同步写 FTS，下方手动段覆盖）
    insert_session_turns(conn, file_path, &header.session_id, start_ts, turns, stats);

    stats.sessions_processed += 1;
}

/// 汇总会话所有 turn 的 token 数（usage.input_tokens + output_tokens）
fn session_total_tokens(turns: &[conversation::Turn]) -> i64 {
    turns
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
        .sum()
}

/// 插入单个会话的所有 turns，失败仅记录不中断
fn insert_session_turns(
    conn: &rusqlite::Connection,
    file_path: &Path,
    session_id: &str,
    start_ts: i64,
    turns: &[conversation::Turn],
    stats: &mut RebuildStats,
) {
    for turn in turns {
        let ts_ms = time::ts_to_unix_ms(&turn.ts).unwrap_or(start_ts);
        let preview: String = turn.content.chars().take(200).collect();
        let char_count = turn.content.chars().count() as i64;

        if let Err(e) = conn.execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id, turn.seq as i64, ts_ms,
                turn.role, preview, char_count,
            ],
        ) {
            stats.errors.push(format!("{}: turn {} 插入失败: {}", file_path.display(), turn.seq, e));
        } else {
            stats.turns_indexed += 1;
        }
    }
}

/// 一次性收集所有 (turn_id, preview) 对（Phase 1 FTS 重建与 Phase 2 向量嵌入共用）
fn load_turn_previews(conn: &rusqlite::Connection) -> anyhow::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, preview FROM turns WHERE preview IS NOT NULL")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let collected: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(collected?)
}

/// 手动重建 FTS 索引（覆盖 turns_ai 触发器的写入）
fn rebuild_fts_rows(conn: &rusqlite::Connection, turn_rows: &[(i64, String)]) -> anyhow::Result<()> {
    let _ = conn.execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", []);
    for (id, preview) in turn_rows {
        // jieba tokenizer 在 FTS5 引擎内自动分词，无需预处理
        conn.execute(
            "INSERT INTO turns_fts(rowid, preview) VALUES (?1, ?2)",
            rusqlite::params![id, preview],
        )?;
    }
    Ok(())
}

/// Phase 2: 向量嵌入（分批事务，支持断点续传）
///
/// 返回 (已索引数, 跳过数, 失败 turn 数, 错误样本)。失败仅记录不中断，由调用方
/// 汇总到 stats.errors，避免"全部插入失败却报告成功"的静默退化。
fn rebuild_vectors(
    db: &Db,
    embedder: &crate::embedder::LazyEmbedder,
    on_progress: Option<&ProgressFn>,
) -> anyhow::Result<(usize, usize, usize, Vec<String>)> {
    let conn = db.conn();

    // 1. 收集所有待索引的 turns
    let turn_rows = load_turn_previews(conn)?;

    if turn_rows.is_empty() {
        tracing::info!("Phase 2: 无 turns 需要索引");
        return Ok((0, 0, 0, Vec::new()));
    }

    // 2. 查询已有向量（断点续传）
    let existing_ids = load_existing_vector_ids(conn)?;

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
        return Ok((skipped, skipped, 0, Vec::new()));
    }

    let total_to_index = pending.len();
    tracing::info!(
        "Phase 2 开始: {} 条待嵌入 (跳过 {} 条已存在)",
        total_to_index,
        skipped
    );

    // 3. 两级分批：嵌入批（从 embedder 配置读取）+ 事务批（减少 fsync）
    //    - 嵌入批大小由 API 限制（DashScope=10, OpenAI 可更大）
    //    - 每 10 个嵌入批一个事务（共享一次 COMMIT）
    let vec_store = crate::index::vector::VectorStore::new(db);
    let mut vectors_indexed = skipped;
    let mut failed_count = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let total = total_to_index + skipped;
    let embed_batch_size = embedder.batch_size().max(1);
    let tx_batch_size = embed_batch_size * 10; // 10 embed batches per DB transaction
    let num_db_batches = pending.len().div_ceil(tx_batch_size);

    for (tx_idx, db_chunk) in pending.chunks(tx_batch_size).enumerate() {
        process_vector_db_batch(
            conn,
            embedder,
            &vec_store,
            db_chunk,
            embed_batch_size,
            tx_idx,
            num_db_batches,
            total,
            &mut vectors_indexed,
            &mut failed_count,
            &mut errors,
            on_progress,
        )?;
    }

    tracing::info!(
        "Phase 2 完成: {} 条向量已索引 ({} 条跳过)",
        vectors_indexed - skipped,
        skipped
    );

    Ok((vectors_indexed, skipped, failed_count, errors))
}

/// 查询 vec_turns 中已有的 rowid（断点续传）
fn load_existing_vector_ids(conn: &rusqlite::Connection) -> anyhow::Result<HashSet<i64>> {
    let mut stmt = conn.prepare("SELECT rowid FROM vec_turns")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// 向量错误样本保留上限
const MAX_ERR_SAMPLES: usize = 5;

/// 处理单个事务批：BEGIN → 分批嵌入插入 → COMMIT → 进度日志/回调
///
/// 从 `rebuild_vectors` 的事务批循环中提取，BEGIN/COMMIT 失败时通过 `?` 原样上抛。
#[allow(clippy::too_many_arguments)] // extraction boundary from rebuild_vectors
fn process_vector_db_batch(
    conn: &rusqlite::Connection,
    embedder: &crate::embedder::LazyEmbedder,
    vec_store: &crate::index::vector::VectorStore<'_>,
    db_chunk: &[(i64, String)],
    embed_batch_size: usize,
    tx_idx: usize,
    num_db_batches: usize,
    total: usize,
    vectors_indexed: &mut usize,
    failed_count: &mut usize,
    errors: &mut Vec<String>,
    on_progress: Option<&ProgressFn>,
) -> anyhow::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;

    // 事务内分多个嵌入批次
    for embed_chunk in db_chunk.chunks(embed_batch_size) {
        embed_and_insert_chunk(embedder, vec_store, embed_chunk, vectors_indexed, failed_count, errors);
    }

    conn.execute_batch("COMMIT")?;

    tracing::info!(
        "向量嵌入进度: {}/{} (事务批 {}/{})",
        *vectors_indexed,
        total,
        tx_idx + 1,
        num_db_batches
    );

    if let Some(cb) = on_progress {
        cb(*vectors_indexed, total);
    }

    Ok(())
}

/// 嵌入一个批次的文本并逐条插入向量；失败仅记录不中断
///
/// 原内层循环中的 `continue`（嵌入失败时跳过本批）在函数内等价转换为 `return`。
fn embed_and_insert_chunk(
    embedder: &crate::embedder::LazyEmbedder,
    vec_store: &crate::index::vector::VectorStore<'_>,
    embed_chunk: &[(i64, String)],
    vectors_indexed: &mut usize,
    failed_count: &mut usize,
    errors: &mut Vec<String>,
) {
    let texts: Vec<&str> = embed_chunk.iter().map(|(_, p)| p.as_str()).collect();
    let embeddings = match embedder.embed_documents(&texts) {
        Ok(embs) => embs,
        Err(e) => {
            tracing::warn!("批量嵌入失败: {}", e);
            *failed_count += texts.len();
            if errors.len() < MAX_ERR_SAMPLES {
                errors.push(format!("批量嵌入失败 ({} 条): {}", texts.len(), e));
            }
            return;
        }
    };

    for ((turn_id, _), embedding) in embed_chunk.iter().zip(embeddings.iter()) {
        match vec_store.insert(*turn_id, embedding) {
            Ok(_) => *vectors_indexed += 1,
            Err(e) => {
                tracing::warn!("向量插入失败 turn_id={}: {}", turn_id, e);
                *failed_count += 1;
                if errors.len() < MAX_ERR_SAMPLES {
                    errors.push(format!("向量插入失败 turn_id={}: {}", turn_id, e));
                }
            }
        }
    }
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
        let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
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
        let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
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
        let _ = rebuild_from_jsonl_with_progress(&tmp, &db, None, &progress, true).unwrap();
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
