use crate::fact::conversation;
use crate::index::db::Db;
use crate::util::time;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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
        let mut p = progress
            .lock()
            .map_err(|e| anyhow::anyhow!("lock: {}", e))?;
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

    let result = rebuild_from_jsonl_with_callback(
        conversations_dir,
        db,
        embedder,
        Some(&callback),
        full_rebuild,
    );

    {
        let mut p = progress
            .lock()
            .map_err(|e| anyhow::anyhow!("lock: {}", e))?;
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

    // C6 guard: Phase 1 unconditionally clears vec_turns，而无 embedder 时 Phase 2
    // 不会重建向量——旧实现静默清空后仍报告成功。不能改成保留 vec_turns：Phase 1
    // 重插 turns 后 AUTOINCREMENT 产生全新 id（sqlite_sequence 不重置），旧向量行
    // 会全部变成孤儿/错键。必须在 Phase 1 的 DELETE 之前计数，且只在真正进入
    // Phase 1（完整重建路径）时报告。
    let vectors_will_be_wiped = embedder.is_none()
        && !incremental
        && conn
            .query_row(
                // 走 vec0 的 rowids 影子表计数（与 main.rs doctor / http.rs /stats /
                // vector.rs count() 的惯例一致，避免对虚拟表全量扫描）
                "SELECT COUNT(*) FROM vec_turns_rowids",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
    if vectors_will_be_wiped {
        tracing::warn!(
            "无嵌入后端且 vec_turns 非空：Phase 1 将清空向量索引，Phase 2 无法重建；\
             语义/混合搜索将降级为关键词"
        );
    }

    let mut stats = if incremental {
        tracing::info!(
            "增量模式：DB 中已有数据，跳过 Phase 1（元数据+FTS），直接进入 Phase 2（向量嵌入）"
        );
        // 从现有 DB 读取统计信息
        let session_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap_or(0);
        let turn_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap_or(0);
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
            Ok(mut s) => {
                conn.execute_batch("COMMIT")?;
                if vectors_will_be_wiped {
                    // 写入 stats.errors 以便 CLI（cmd_rebuild 打印 errors）与
                    // MCP rebuild_status（回传 stats.errors）都可见
                    s.errors.push(
                        "无嵌入后端：向量索引已清空且本次无法重建；语义/混合搜索将降级为关键词。\
                         请配置 embedding API 或下载本地模型后重跑 rebuild"
                            .to_string(),
                    );
                }
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
/// 2. JSONL ↔ DB 两级一致（session_id 集合 + 每会话轮数，见 jsonl_db_sessions_in_sync）
///
/// 任一不一致说明 JSONL 发生了变化，需要完整重建。
fn should_do_incremental_rebuild(db: &Db, conversations_dir: &Path) -> bool {
    // 检查 DB 中是否有数据
    let session_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);

    if session_count == 0 {
        tracing::info!("增量检测：DB 为空，需要完整重建");
        return false;
    }

    if !jsonl_db_sessions_in_sync(db, conversations_dir) {
        return false;
    }

    tracing::info!(
        "增量检测：JSONL 与 DB 一致 ({} sessions)，可以增量重建",
        session_count
    );
    true
}

/// JSONL ↔ DB 会话状态两级对比：session_id 集合一致 + 每会话 turn 数一致。
/// 任一不一致（含 JSONL 文件解析失败、JSONL 内重复 session_id、DB 读取失败）
/// 返回 false，要求完整重建。旧的"文件数 == sessions 行数"对比会漏检等量换代
/// （JSONL 换成了不同 session 的同数量文件）。
///
/// rebuild/doctor 均为离线诊断命令，逐文件 read_session 全解析的成本可接受；
/// conversation.rs 无轻量 header 读取 API，不为此新增。
fn jsonl_db_sessions_in_sync(db: &Db, conversations_dir: &Path) -> bool {
    let conn = db.conn();

    let db_sessions: HashSet<String> = collect_rows(conn, "SELECT session_id FROM sessions", |r| {
        r.get::<_, String>(0)
    })
    .into_iter()
    .collect();

    let db_turn_counts: HashMap<String, i64> = collect_rows(
        conn,
        "SELECT session_id, COUNT(*) FROM turns GROUP BY session_id",
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )
    .into_iter()
    .collect();

    // JSONL 侧：全解析取 session_id 与每文件 turn 数
    let mut jsonl_sessions: HashSet<String> = HashSet::new();
    let mut jsonl_turn_counts: HashMap<String, usize> = HashMap::new();
    for file in conversation::list_sessions(conversations_dir) {
        let (header, turns) = match conversation::read_session(&file) {
            Ok(v) => v,
            Err(e) => {
                tracing::info!(
                    "增量检测：JSONL 解析失败（{}: {}），需要完整重建",
                    file.display(),
                    e
                );
                return false;
            }
        };
        if !jsonl_sessions.insert(header.session_id.clone()) {
            tracing::info!(
                "增量检测：JSONL 中 session_id 重复（{}），需要完整重建",
                header.session_id
            );
            return false;
        }
        jsonl_turn_counts.insert(header.session_id, turns.len());
    }

    if jsonl_sessions != db_sessions {
        tracing::info!(
            "增量检测：session_id 集合不一致（JSONL {} 个 vs DB {} 个），需要完整重建",
            jsonl_sessions.len(),
            db_sessions.len()
        );
        return false;
    }
    for (sid, jsonl_count) in &jsonl_turn_counts {
        let db_count = db_turn_counts.get(sid).copied().unwrap_or(0);
        if *jsonl_count as i64 != db_count {
            tracing::info!(
                "增量检测：会话 {} 轮数不一致（JSONL {} vs DB {}），需要完整重建",
                sid,
                jsonl_count,
                db_count
            );
            return false;
        }
    }
    true
}

/// 执行只读查询并收集全部行；任何失败返回空集合（调用方按"不一致"处理，
/// 触发完整重建，安全侧）。
fn collect_rows<T>(
    conn: &rusqlite::Connection,
    sql: &str,
    map: impl Fn(&rusqlite::Row) -> rusqlite::Result<T>,
) -> Vec<T> {
    match conn.prepare(sql).and_then(|mut s| {
        let rows = s.query_map([], |r| map(r))?;
        rows.collect::<rusqlite::Result<Vec<T>>>()
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("增量检测：DB 读取失败（{}），按不一致处理", e);
            Vec::new()
        }
    }
}

/// Phase 1: 清理表、插入 sessions/turns、重建 FTS 索引
///
/// 调用方负责 BEGIN/COMMIT 事务包裹。
fn rebuild_metadata(conversations_dir: &Path, db: &Db) -> anyhow::Result<RebuildStats> {
    let conn = db.conn();

    // 0. C9: 在 DELETE 前快照旧 turn 身份 (id, session_id, seq)。重插后 turns 会
    //    获得全新 AUTOINCREMENT id，entities/relations.source_turn 与
    //    bounded_memory.source_turn_ids 这些软引用会悬空，需在重插后按身份 remap。
    //    选择 (session_id, seq) 作为身份键：seq 是 JSONL 内的稳定序号，且与写入时
    //    一致（insert_session_turns 用 turn.seq）。
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.tmp_turn_id_old;
         CREATE TEMP TABLE tmp_turn_id_old (
             id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL
         );
         INSERT INTO tmp_turn_id_old SELECT id, session_id, seq FROM turns;",
    )?;

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
                index_session_file(
                    conn,
                    conversations_dir,
                    file_path,
                    &header,
                    &turns,
                    &mut stats,
                );
            }
            Err(e) => {
                stats
                    .errors
                    .push(format!("{}: 解析失败: {}", file_path.display(), e));
            }
        }
    }

    // C9: 全部重插完成后，把图谱/有界记忆中的旧 turn 软引用 remap 到新 id
    remap_turn_references(conn)?;

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

/// C9: 把 entities/relations.source_turn 与 bounded_memory.source_turn_ids 中指向
/// 旧 turns.id 的软引用 remap 到重插后的新 id（按 (session_id, seq) 身份配对）。
/// 映射不到的引用置 NULL（软引用设计允许）或从 source_turn_ids JSON 数组中丢弃。
/// 在 Phase 1 既有事务内执行（rebuild_metadata 由调用方 BEGIN/COMMIT 包裹）。
fn remap_turn_references(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    // 旧 id → 新 id 映射表（SQL 集合操作填充，entities/relations 可能较大）。
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

    // 相关子查询：映射不到时结果为 NULL，同时完成置 NULL 兜底
    conn.execute_batch(
        "UPDATE entities
            SET source_turn = (SELECT new_id FROM tmp_turn_id_remap WHERE old_id = entities.source_turn)
          WHERE source_turn IS NOT NULL;
         UPDATE relations
            SET source_turn = (SELECT new_id FROM tmp_turn_id_remap WHERE old_id = relations.source_turn)
          WHERE source_turn IS NOT NULL;",
    )?;

    // bounded_memory.source_turn_ids 是 JSON 数组文本（serde_json::to_string(&Vec<i64>)
    // 写入，见 memory/l1.rs store_atoms）。行数通常小（有界记忆），Rust 侧映射可接受。
    let remap: HashMap<i64, i64> = {
        let mut stmt = conn.prepare("SELECT old_id, new_id FROM tmp_turn_id_remap")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let bm_rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, source_turn_ids FROM bounded_memory WHERE source_turn_ids IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (id, json) in bm_rows {
        let parsed: Vec<i64> = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(_) => continue, // 非 id 数组（遗留/手工数据）：保持原样
        };
        let mapped: Vec<i64> = parsed
            .iter()
            .filter_map(|old| remap.get(old).copied())
            .collect();
        if mapped != parsed {
            let new_json = serde_json::to_string(&mapped)?;
            conn.execute(
                "UPDATE bounded_memory SET source_turn_ids = ?1 WHERE id = ?2",
                rusqlite::params![new_json, id],
            )?;
        }
    }

    conn.execute_batch("DROP TABLE temp.tmp_turn_id_old; DROP TABLE temp.tmp_turn_id_remap;")?;
    Ok(())
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
            stats
                .errors
                .push(format!("{}: 时间解析失败: {}", file_path.display(), e));
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
    ) {
        stats
            .errors
            .push(format!("{}: session 插入失败: {}", file_path.display(), e));
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
                    if inp + out > 0 {
                        Some(inp + out)
                    } else {
                        None
                    }
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
                session_id,
                turn.seq as i64,
                ts_ms,
                turn.role,
                preview,
                char_count,
            ],
        ) {
            stats.errors.push(format!(
                "{}: turn {} 插入失败: {}",
                file_path.display(),
                turn.seq,
                e
            ));
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
fn rebuild_fts_rows(
    conn: &rusqlite::Connection,
    turn_rows: &[(i64, String)],
) -> anyhow::Result<()> {
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
    //    - 每 10 个嵌入批一个事务（共享一次 COMMIT）；C7：该事务只做毫秒级
    //      INSERT——嵌入与量化已在事务外完成（见 process_vector_db_batch）
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

/// 处理单个事务批：先事务外嵌入+量化，再 BEGIN → 批量 INSERT → COMMIT
///
/// C7：BEGIN IMMEDIATE 立刻持上 SQLite 写锁——旧实现嵌入（网络 I/O，单批含
/// 重试最坏 ~33s）在事务内进行，rebuild 期间其他连接（MCP 主线程
/// save_session、网关 /capture 的独立连接）等过 busy_timeout 后必然
/// SQLITE_BUSY。现在事务作用域只覆盖纯 INSERT（毫秒级），事务内零网络调用。
/// BEGIN/COMMIT 失败仍通过 `?` 原样上抛。
#[allow(clippy::too_many_arguments)] // extraction boundary from rebuild_vectors
fn process_vector_db_batch(
    conn: &rusqlite::Connection,
    embedder: &crate::embedder::LazyEmbedder,
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
    // 1) 事务外：本事务批的全部嵌入 + int8 量化（纯内存产物 (turn_id, bytes)）
    let mut rows: Vec<(i64, Vec<u8>)> = Vec::with_capacity(db_chunk.len());
    for embed_chunk in db_chunk.chunks(embed_batch_size) {
        embed_quantize_chunk(embedder, embed_chunk, &mut rows, failed_count, errors);
    }

    // 2) 短事务：只写不联网
    conn.execute_batch("BEGIN IMMEDIATE")?;
    for (turn_id, embedding_bytes) in &rows {
        // C7 并发窗口：嵌入移出事务后，/capture（独立连接）可能在本批嵌入期间
        // 已合法索引同一 turn。已存在的行按"已索引"计（终态正确），不计 failed
        // ——否则并发写入会变成统计噪声与误导性 errors。
        let already: bool = conn
            .query_row(
                "SELECT 1 FROM vec_turns_rowids WHERE rowid = ?1",
                rusqlite::params![*turn_id],
                |_| Ok(()),
            )
            .is_ok();
        if already {
            tracing::debug!("turn_id={} 已被并发写入索引，跳过", turn_id);
            *vectors_indexed += 1;
            continue;
        }
        match conn.execute(
            "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![*turn_id, embedding_bytes],
        ) {
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

/// 事务外嵌入一个批次并量化为 int8 字节（纯内存）；嵌入失败仅记录不中断
/// （语义与原事务内 embed_and_insert_chunk 一致：failed_count += 批大小，
/// 错误样本受 MAX_ERR_SAMPLES 上限，整批跳过）。
fn embed_quantize_chunk(
    embedder: &crate::embedder::LazyEmbedder,
    embed_chunk: &[(i64, String)],
    rows: &mut Vec<(i64, Vec<u8>)>,
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
        rows.push((*turn_id, crate::embedder::onnx::quantize_to_int8(embedding)));
    }
}

/// 检查 JSONL 与 SQLite 索引的一致性
///
/// `in_sync` 复用增量重建检测的同一两级对比 helper（session_id 集合 + 每会话
/// 轮数），保持 doctor 与 rebuild 判定一致；计数仅作展示。
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
        in_sync: jsonl_db_sessions_in_sync(db, conversations_dir),
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
            Turn {
                ts: "2026-04-12T10:00:01.000+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "Rust ownership model".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-12T10:00:02.000+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "借用检查器保证内存安全".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-12T10:00:03.000+08:00".to_string(),
                seq: 3,
                role: "user".to_string(),
                content: "lifetime annotations".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-12T10:00:04.000+08:00".to_string(),
                seq: 4,
                role: "assistant".to_string(),
                content: "生命周期标注确保引用有效".to_string(),
                metadata: None,
            },
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
        assert!(
            !results.is_empty(),
            "keyword search 'Rust' must return results after rebuild"
        );

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

    fn header_for(session_id: &str, start_time: &str) -> SessionHeader {
        SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: session_id.to_string(),
            start_time: start_time.to_string(),
            profile_id: "default".to_string(),
            source: Some("test".to_string()),
            agent_model: None,
            title: None,
            tags: vec![],
        }
    }

    fn turn(seq: u32, content: &str) -> Turn {
        Turn {
            ts: "2026-04-01T10:00:05.000+08:00".to_string(),
            seq,
            role: "user".to_string(),
            content: content.to_string(),
            metadata: None,
        }
    }

    /// C6 回归：vec_turns 非空时以 embedder=None 做完整重建，向量必然被 Phase 1
    /// 清空且无法重建——必须通过 stats.errors 显式报告（旧实现静默报成功）。
    /// 增量路径（跳过 Phase 1）不得误报。
    #[test]
    fn test_rebuild_no_embedder_reports_vector_wipe() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_vecwipe_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // 预置一条真实 int8 向量（turn 存在但稍后会被重建替换）
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES ('vecwipe', 0, 'vecwipe.jsonl', 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('vecwipe', 1, 0, 'user', 'hello')",
                [],
            )
            .unwrap();
        let turn_id = db.conn().last_insert_rowid();
        let store = crate::index::vector::VectorStore::new(&db);
        let mut v = vec![0.0f32; db.dimensions()];
        v[0] = 1.0;
        store.insert(turn_id, &v).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        // JSONL 与会话一致（供后半段增量路径检测）
        let header = header_for("vecwipe", "2026-04-01T10:00:00.000+08:00");
        conversation::write_session(&tmp, &header, &[turn(1, "hello")]).unwrap();

        // 完整重建、无 embedder → 清空向量并报告错误
        let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
        assert!(
            stats.errors.iter().any(|e| e.contains("无嵌入后端")),
            "full rebuild without embedder must report the vector wipe, got: {:?}",
            stats.errors
        );
        assert_eq!(store.count().unwrap(), 0, "vec_turns should be wiped");

        // 增量路径：vec_turns 已空且 Phase 1 被跳过 → 不得出现该错误
        let stats2 = rebuild_from_jsonl(&tmp, &db, None, false).unwrap();
        assert!(
            stats2.errors.is_empty(),
            "incremental path must not report a wipe it did not perform: {:?}",
            stats2.errors
        );

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// C9 回归：完整重建重插 turns 后 id 全变，entities/relations.source_turn 与
    /// bounded_memory.source_turn_ids 必须按 (session_id, seq) 身份 remap；
    /// 映射不到的置 NULL / 从 JSON 数组中丢弃。
    #[test]
    fn test_rebuild_remaps_provenance_refs() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_remap_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // 旧索引：session "remap-1" 两条 turn；session "vanished" 一条（JSONL 中已无）
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES ('remap-1', 0, 'a.jsonl', 0, 0), ('vanished', 0, 'b.jsonl', 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('remap-1', 1, 0, 'user', 'one')",
                [],
            )
            .unwrap();
        let old_id1 = db.conn().last_insert_rowid();
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('remap-1', 2, 0, 'user', 'two')",
                [],
            )
            .unwrap();
        let old_id2 = db.conn().last_insert_rowid();
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('vanished', 1, 0, 'user', 'gone')",
                [],
            )
            .unwrap();
        let vanished_id = db.conn().last_insert_rowid();

        // 图谱软引用：A→可 remap，B→已消失会话（应置 NULL），relation→另一可 remap id
        db.conn()
            .execute(
                "INSERT INTO entities (canonical, name, entity_type, first_seen, last_seen, source_turn)
                 VALUES ('A', 'A', 'person', 0, 0, ?1), ('B', 'B', 'person', 0, 0, ?2)",
                rusqlite::params![old_id1, vanished_id],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO relations (src_canonical, rel_type, dst_canonical, source_turn, created_at)
                 VALUES ('A', 'knows', 'B', ?1, 0)",
                rusqlite::params![old_id2],
            )
            .unwrap();

        // 有界记忆溯源：可 remap 元素 + 已消失元素 + 从未存在过的 id
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, source_turn_ids)
                 VALUES ('memory', 'atom-x', 0, 0, ?1)",
                rusqlite::params![format!("[{}, {}, 999999]", old_id1, vanished_id)],
            )
            .unwrap();

        // JSONL 只含 session "remap-1"（seq 1,2）
        let header = header_for("remap-1", "2026-04-01T10:00:00.000+08:00");
        conversation::write_session(&tmp, &header, &[turn(1, "one"), turn(2, "two")]).unwrap();

        let stats = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
        assert!(stats.errors.is_empty(), "errors: {:?}", stats.errors);

        // 新 id（AUTOINCREMENT 接续，必然不同于旧 id——证明 remap 真实发生）
        let new_id_of = |sid: &str, seq: i64| -> i64 {
            db.conn()
                .query_row(
                    "SELECT id FROM turns WHERE session_id = ?1 AND seq = ?2",
                    rusqlite::params![sid, seq],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let new_id1 = new_id_of("remap-1", 1);
        let new_id2 = new_id_of("remap-1", 2);
        assert_ne!(new_id1, old_id1);
        assert_ne!(new_id2, old_id2);

        let source_of = |canon: &str| -> Option<i64> {
            db.conn()
                .query_row(
                    "SELECT source_turn FROM entities WHERE canonical = ?1",
                    rusqlite::params![canon],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            source_of("A"),
            Some(new_id1),
            "entity A must point at remapped turn"
        );
        assert_eq!(
            source_of("B"),
            None,
            "entity B (vanished session) must fall back to NULL"
        );

        let rel_src: Option<i64> = db
            .conn()
            .query_row(
                "SELECT source_turn FROM relations WHERE src_canonical = 'A'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rel_src, Some(new_id2), "relation must be remapped");

        let ids_json: String = db
            .conn()
            .query_row(
                "SELECT source_turn_ids FROM bounded_memory WHERE content = 'atom-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<i64>>(&ids_json).unwrap(),
            vec![new_id1],
            "unmappable source_turn_ids elements must be dropped"
        );

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// J3 回归：增量检测须做 session_id 集合 + 每会话轮数两级对比——等量换代、
    /// 同 id 换轮数都不能被放行；doctor（check_consistency）与 rebuild 判定一致。
    #[test]
    fn test_incremental_detection_two_tier() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_incsync_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // DB 侧种子：session "j3-a" 两条 turn（经完整重建写入）
        let header_a = header_for("j3-a", "2026-04-01T10:00:00.000+08:00");
        conversation::write_session(&tmp, &header_a, &[turn(1, "one"), turn(2, "two")]).unwrap();
        rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
        assert!(
            should_do_incremental_rebuild(&db, &tmp),
            "完全一致时应可增量"
        );
        assert!(check_consistency(&tmp, &db).unwrap().in_sync);

        // 等量换代：换成 session "j3-b"（同 1 个文件、同 2 条 turn）
        std::fs::remove_dir_all(&tmp).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        let header_b = header_for("j3-b", "2026-04-02T10:00:00.000+08:00");
        conversation::write_session(&tmp, &header_b, &[turn(1, "one"), turn(2, "two")]).unwrap();
        assert!(
            !should_do_incremental_rebuild(&db, &tmp),
            "等量换代必须触发完整重建"
        );
        assert!(
            !check_consistency(&tmp, &db).unwrap().in_sync,
            "doctor 与 rebuild 判定必须一致"
        );

        // 同 id 但轮数不同
        std::fs::remove_dir_all(&tmp).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        conversation::write_session(&tmp, &header_a, &[turn(1, "one")]).unwrap();
        assert!(
            !should_do_incremental_rebuild(&db, &tmp),
            "每会话轮数不同必须触发完整重建"
        );

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// C7 结构回归（Phase 2 事务重排后）：增量模式下预插一条 vec 行，
    /// 用不可达端点的 API embedder 跑 Phase 2——嵌入（失败前）不得破坏
    /// 已索引向量（断点续传 skip 生效），失败批计入 stats.errors（经
    /// failed_count），向量不新增。嵌入移出事务后，本测试同时钉住
    /// "失败只影响未索引部分" 的等价语义。（重试退避 ≈ 3-4s。）
    #[test]
    fn test_rebuild_phase2_resume_survives_embed_failure() {
        let tmp = std::env::temp_dir().join(format!(
            "asuna_vecresume_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // 2-turn 会话，先以无 embedder 完整重建建立 turns（不触发 C6 误报：
        // 此时 vec_turns 为空）
        let header = header_for("resume", "2026-04-01T10:00:00.000+08:00");
        conversation::write_session(&tmp, &header, &[turn(1, "one"), turn(2, "two")]).unwrap();
        let stats0 = rebuild_from_jsonl(&tmp, &db, None, true).unwrap();
        assert!(stats0.errors.is_empty(), "{:?}", stats0.errors);

        let turn_ids: Vec<i64> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT id FROM turns ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(turn_ids.len(), 2);

        // 模拟断点：第一条 turn 已有向量
        let store = crate::index::vector::VectorStore::new(&db);
        let mut v = vec![0.0f32; db.dimensions()];
        v[0] = 1.0;
        store.insert(turn_ids[0], &v).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        // 不可达 API embedder + 增量模式（JSONL↔DB 一致 → 跳过 Phase 1，
        // 保留 vec 行）：Phase 2 只嵌入第二条 turn，必然失败
        let mut emb_cfg = crate::config::Config::default().embedding;
        emb_cfg.api_url = "http://127.0.0.1:9/v1".to_string();
        emb_cfg.api_model = "unreachable-test".to_string();
        let embedder = crate::embedder::LazyEmbedder::from_config(&emb_cfg, None)
            .expect("配置了 api_url + api_model，API embedder 应构造成功");

        let stats = rebuild_from_jsonl(&tmp, &db, Some(&embedder), false).unwrap();
        assert!(
            stats.errors.iter().any(|e| e.contains("未能索引")),
            "embed 失败必须汇入 stats.errors，got: {:?}",
            stats.errors
        );
        assert_eq!(
            stats.vectors_indexed, 1,
            "已索引的那条按断点续传计入 skipped，不得重复嵌入"
        );
        assert_eq!(
            store.count().unwrap(),
            1,
            "嵌入失败不得破坏既有向量（写事务零网络调用）"
        );

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
