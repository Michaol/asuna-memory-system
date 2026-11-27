use crate::index::conversation;
use crate::index::db::Db;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 一致性检查结果
#[derive(Debug, serde::Serialize)]
pub struct ConsistencyResult {
    pub jsonl_count: usize,
    pub db_session_count: usize,
    pub in_sync: bool,
}

/// 判断是否应该使用增量重建模式
///
/// 条件：
/// 1. DB 中已有 sessions 数据（非首次运行）
/// 2. JSONL ↔ DB 两级一致（session_id 集合 + 每会话轮数，见 jsonl_db_sessions_in_sync）
///
/// 任一不一致说明 JSONL 发生了变化，需要完整重建。
pub(super) fn should_do_incremental_rebuild(db: &Db, conversations_dir: &Path) -> bool {
    let session_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or_else(|e| {
            tracing::warn!(
                target: "asuna_memory::index::rebuild",
                "增量检测 sessions 计数失败（{}），按 0 处理（触发完整重建）",
                e
            );
            0
        });

    if session_count == 0 {
        tracing::info!(
            target: "asuna_memory::index::rebuild",
            "增量检测：DB 为空，需要完整重建"
        );
        return false;
    }

    if !jsonl_db_sessions_in_sync(db, conversations_dir) {
        return false;
    }

    tracing::info!(
        target: "asuna_memory::index::rebuild",
        "增量检测：JSONL 与 DB 一致 ({} sessions)，可以增量重建",
        session_count
    );
    true
}

/// JSONL ↔ DB 会话状态两级对比：session_id 集合一致 + 每会话 turn 数一致。
fn jsonl_db_sessions_in_sync(db: &Db, conversations_dir: &Path) -> bool {
    let Ok(files) = conversation::try_list_sessions(conversations_dir) else {
        return false;
    };
    match jsonl_db_sessions_in_sync_for_files(db, &files) {
        Ok(in_sync) => in_sync,
        Err(e) => {
            tracing::debug!(
                target: "asuna_memory::index::rebuild",
                "增量检测：DB 读取失败（{}），按不一致处理",
                e
            );
            false
        }
    }
}

fn jsonl_db_sessions_in_sync_for_files(
    db: &Db,
    files: &[std::path::PathBuf],
) -> anyhow::Result<bool> {
    let conn = db.conn();
    let db_sessions: HashSet<String> =
        collect_rows(conn, "SELECT session_id FROM sessions", |r| {
            r.get::<_, String>(0)
        })?
        .into_iter()
        .collect();

    let db_turn_counts: HashMap<String, i64> = collect_rows(
        conn,
        "SELECT session_id, COUNT(*) FROM turns GROUP BY session_id",
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )?
    .into_iter()
    .collect();

    let Some((jsonl_sessions, jsonl_turn_counts)) = read_jsonl_session_counts(files) else {
        return Ok(false);
    };
    Ok(compare_session_counts(
        &db_sessions,
        &db_turn_counts,
        &jsonl_sessions,
        &jsonl_turn_counts,
    ))
}

fn read_jsonl_session_counts(
    files: &[std::path::PathBuf],
) -> Option<(HashSet<String>, HashMap<String, usize>)> {
    let mut jsonl_sessions = HashSet::new();
    let mut jsonl_turn_counts = HashMap::new();
    for file in files {
        // W5.3: 轻量摘要（首行 header + 非空行计数），不构建全部 Turn/metadata。
        let (header, turn_count) = match conversation::read_session_summary(file) {
            Ok(v) => v,
            Err(e) => {
                tracing::info!(
                    target: "asuna_memory::index::rebuild",
                    "增量检测：JSONL 解析失败（{}: {}），需要完整重建",
                    file.display(),
                    e
                );
                return None;
            }
        };
        if !jsonl_sessions.insert(header.session_id.clone()) {
            tracing::info!(
                target: "asuna_memory::index::rebuild",
                "增量检测：JSONL 中 session_id 重复（{}），需要完整重建",
                header.session_id
            );
            return None;
        }
        jsonl_turn_counts.insert(header.session_id, turn_count);
    }
    Some((jsonl_sessions, jsonl_turn_counts))
}

fn compare_session_counts(
    db_sessions: &HashSet<String>,
    db_turn_counts: &HashMap<String, i64>,
    jsonl_sessions: &HashSet<String>,
    jsonl_turn_counts: &HashMap<String, usize>,
) -> bool {
    if jsonl_sessions != db_sessions {
        tracing::info!(
            target: "asuna_memory::index::rebuild",
            "增量检测：session_id 集合不一致（JSONL {} 个 vs DB {} 个），需要完整重建",
            jsonl_sessions.len(),
            db_sessions.len()
        );
        return false;
    }
    for (sid, jsonl_count) in jsonl_turn_counts {
        let db_count = db_turn_counts.get(sid).copied().unwrap_or(0);
        if *jsonl_count as i64 != db_count {
            tracing::info!(
                target: "asuna_memory::index::rebuild",
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

/// 执行只读查询并收集全部行；向调用方传播 DB 错误。
fn collect_rows<T>(
    conn: &rusqlite::Connection,
    sql: &str,
    map: impl Fn(&rusqlite::Row) -> rusqlite::Result<T>,
) -> anyhow::Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], map)?;
    Ok(rows.collect::<rusqlite::Result<Vec<T>>>()?)
}

/// 检查 JSONL 与 SQLite 索引的一致性
///
/// `in_sync` 复用增量重建检测的同一两级对比 helper（session_id 集合 + 每会话
/// 轮数），保持 doctor 与 rebuild 判定一致；计数仅作展示。
pub fn check_consistency(conversations_dir: &Path, db: &Db) -> anyhow::Result<ConsistencyResult> {
    let jsonl_files = conversation::try_list_sessions(conversations_dir)?;

    let db_count: usize = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| {
            r.get::<_, i64>(0).map(|v| v as usize)
        })?;

    let in_sync = jsonl_db_sessions_in_sync_for_files(db, &jsonl_files)?;
    Ok(ConsistencyResult {
        jsonl_count: jsonl_files.len(),
        db_session_count: db_count,
        in_sync,
    })
}
