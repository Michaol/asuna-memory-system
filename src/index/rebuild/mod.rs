mod consistency;
mod phase1_fact;
mod phase2_vectors;
mod progress;

pub use consistency::{check_consistency, ConsistencyResult};
pub use progress::{new_shared_progress, RebuildProgress, RebuildStatus, SharedProgress};

#[cfg(test)]
use super::conversation;
use crate::index::db::Db;
use crate::util::time;
#[cfg(test)]
use consistency::should_do_incremental_rebuild;
use phase1_fact::run_phase1_rebuild;
use phase2_vectors::apply_phase2_vectors;
#[cfg(test)]
use phase2_vectors::{insert_quantized_vector_rows, VectorBatchCounters};
use progress::ProgressFn;
use std::path::Path;

/// 重建统计
#[derive(Debug, Clone, serde::Serialize)]
pub struct RebuildStats {
    pub sessions_processed: usize,
    pub turns_indexed: usize,
    pub vectors_indexed: usize,
    pub vectors_skipped: usize,
    pub errors: Vec<String>,
}

/// 带进度追踪的重建（仅兼容旧签名的测试入口；生产请调用 [`rebuild_from_jsonl_with_progress_and_preview`]）
#[cfg(test)]
pub fn rebuild_from_jsonl_with_progress(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    progress: &SharedProgress,
    full_rebuild: bool,
) -> anyhow::Result<RebuildStats> {
    rebuild_from_jsonl_with_progress_and_preview(
        conversations_dir,
        db,
        embedder,
        progress,
        full_rebuild,
        200,
    )
}

/// M8b: RAII panic 守卫——内层 rebuild unwind 时把 progress 置为 Failed
/// （含 finished_at 与错误信息），不再永久卡在 Running。正常返回路径
/// `std::thread::panicking()` 为 false，Drop 为空操作。
struct ProgressPanicGuard(SharedProgress);

impl Drop for ProgressPanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let mut p = crate::util::recover_poison(&self.0);
            p.status = RebuildStatus::Failed;
            p.finished_at = Some(time::now_unix_ms());
            p.errors
                .push("rebuild panicked (internal error); progress marked Failed".to_string());
        }
    }
}

/// 带进度追踪与可配置 `preview_length` 的重建（供 MCP 异步调用使用）。
pub fn rebuild_from_jsonl_with_progress_and_preview(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    progress: &SharedProgress,
    full_rebuild: bool,
    preview_length: usize,
) -> anyhow::Result<RebuildStats> {
    {
        let mut p = crate::util::recover_poison(progress);
        let caller_started_at = p.started_at;
        *p = RebuildProgress::default();
        p.status = RebuildStatus::Running;
        // N4: 保留调用侧占位 started_at（仅未置位才设），使 elapsed_ms 含 spawn 延迟。
        p.started_at = if caller_started_at > 0 {
            caller_started_at
        } else {
            time::now_unix_ms()
        };
    }

    // 创建进度回调，更新 SharedProgress
    let progress_clone = progress.clone();
    let callback = move |sessions: usize, turns: usize, done: usize, _total: usize| {
        let mut p = crate::util::recover_poison(&progress_clone);
        p.sessions_processed = sessions;
        p.turns_indexed = turns;
        p.vectors_indexed = done;
    };

    // M8b: 守卫覆盖内层 rebuild 全程——unwind 时 Drop 置 Failed。
    let _panic_guard = ProgressPanicGuard(progress.clone());
    let result = rebuild_from_jsonl_with_callback_and_preview(
        conversations_dir,
        db,
        embedder,
        Some(&callback),
        full_rebuild,
        preview_length,
    );

    {
        let mut p = crate::util::recover_poison(progress);
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

/// 从 JSONL 文件重建索引（仅兼容旧签名的测试入口；生产请调用 [`rebuild_from_jsonl_with_preview`]）
#[cfg(test)]
pub fn rebuild_from_jsonl(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    full_rebuild: bool,
) -> anyhow::Result<RebuildStats> {
    rebuild_from_jsonl_with_preview(conversations_dir, db, embedder, full_rebuild, 200)
}

/// 从 JSONL 文件重建索引，并透传 `Config.conversation.preview_length`。
pub fn rebuild_from_jsonl_with_preview(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    full_rebuild: bool,
    preview_length: usize,
) -> anyhow::Result<RebuildStats> {
    rebuild_from_jsonl_with_callback_and_preview(
        conversations_dir,
        db,
        embedder,
        None,
        full_rebuild,
        preview_length,
    )
}

/// 带进度回调的重建实现
///
/// 分两阶段执行：
/// 1. **元数据 + FTS 阶段**：清理表、插入 sessions/turns、重建 FTS 索引。
///    W5.2: JSONL 全量读取+解析在写事务外完成，事务内只做批量写入。
/// 2. **向量嵌入阶段**（分批事务）：批次大小由 `embedder.batch_size()` 决定，支持断点续传
///
/// **断点续传**：检查 vec_turns 中已有的 turn_id，跳过已索引的向量。
/// 崩溃后重跑时，只需嵌入剩余部分。
///
/// **增量模式**：当 `full_rebuild=false` 且 DB 中已有数据时，自动跳过 Phase 1，
/// 直接进入 Phase 2 继续未完成的向量嵌入。这避免了重新处理已完成的元数据阶段。
fn finish_atom_reencode_backfill(
    db: &Db,
    emb: &crate::embedder::LazyEmbedder,
    stats: &mut RebuildStats,
) {
    match db.maybe_backfill_bounded_memory_vec(emb) {
        Err(e) => {
            tracing::error!("rebuild: atom 向量重编码失败: {}", e);
            stats.errors.push(format!("atom 向量重编码失败: {}", e));
        }
        // N1: maybe_backfill 的 batch embed 失败 warn+continue 仍返 Ok，
        // 校验活 atom 缺向量数并计入 stats.errors。
        Ok(()) => match db.count_atoms_missing_vec() {
            Ok(missing) if missing > 0 => {
                tracing::warn!("rebuild: atom 向量重嵌入不完整，{} 条缺失", missing);
                stats.errors.push(format!(
                    "atom 向量重嵌入不完整：{} 条缺失，下次启动回填续嵌",
                    missing
                ));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("rebuild: atom 向量缺口校验失败: {}", e),
        },
    }
}

fn rebuild_from_jsonl_with_callback_and_preview(
    conversations_dir: &Path,
    db: &Db,
    embedder: Option<&crate::embedder::LazyEmbedder>,
    on_progress: Option<&ProgressFn>,
    full_rebuild: bool,
    preview_length: usize,
) -> anyhow::Result<RebuildStats> {
    let conn = db.conn();
    let incremental =
        !full_rebuild && consistency::should_do_incremental_rebuild(db, conversations_dir);
    let reencode_atom_vectors = !incremental && embedder.is_some();
    let vectors_will_be_wiped = embedder.is_none() && !incremental && has_indexed_vectors(conn);
    if vectors_will_be_wiped {
        tracing::warn!(
            "无嵌入后端且 vec_turns 非空：Phase 1 将清空 turn 向量索引，Phase 2 无法重建；\
             语义/混合搜索将降级为关键词（atom 向量保留旧标度，不清空）"
        );
    }

    let mut stats = if incremental {
        load_incremental_stats(conn)
    } else {
        run_phase1_rebuild(
            conversations_dir,
            db,
            preview_length,
            vectors_will_be_wiped,
            reencode_atom_vectors,
        )?
    };

    if let Some(cb) = on_progress {
        cb(stats.sessions_processed, stats.turns_indexed, 0, 0);
    }

    if let Some(emb) = embedder {
        apply_phase2_vectors(db, emb, on_progress, &mut stats);
        if reencode_atom_vectors {
            finish_atom_reencode_backfill(db, emb, &mut stats);
        }
    }

    Ok(stats)
}

fn has_indexed_vectors(conn: &rusqlite::Connection) -> bool {
    conn.query_row("SELECT COUNT(*) FROM vec_turns_rowids", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or_else(|e| {
        tracing::warn!("vec_turns_rowids 计数失败（{}），按无向量处理", e);
        0
    }) > 0
}

fn load_incremental_stats(conn: &rusqlite::Connection) -> RebuildStats {
    tracing::info!(
        "增量模式：DB 中已有数据，跳过 Phase 1（元数据+FTS），直接进入 Phase 2（向量嵌入）"
    );
    let session_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or_else(|e| {
            tracing::warn!("增量统计 sessions 计数失败（{}），按 0 处理", e);
            0
        });
    let turn_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap_or_else(|e| {
            tracing::warn!("增量统计 turns 计数失败（{}），按 0 处理", e);
            0
        });
    RebuildStats {
        sessions_processed: session_count as usize,
        turns_indexed: turn_count as usize,
        vectors_indexed: 0,
        vectors_skipped: 0,
        errors: Vec::new(),
    }
}

#[cfg(test)]
mod tests;
