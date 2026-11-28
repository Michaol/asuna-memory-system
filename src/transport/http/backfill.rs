//! Startup vector backfill (W28b) — lifecycle work, not HTTP plumbing.
//! Moved out of `http/mod.rs` so the gateway module stays focused on route
//! assembly, middleware stack and shutdown.

use crate::index::db::{
    drain_backfill_until_no_progress, is_permanent_backfill_error,
    run_backfill_chunks_with_control, try_acquire_backfill_lock, BackfillChunkOutcome,
    BackfillLockOutcome,
};
use crate::transport::state::AppState;

#[cfg(test)]
pub(super) fn is_permanent_backfill_error_for_test(e: &anyhow::Error) -> bool {
    is_permanent_backfill_error(e)
}

fn backfill_one_chunk(
    state: &AppState,
    emb: &crate::embedder::LazyEmbedder,
    chunk: &[(i64, String)],
) -> BackfillChunkOutcome {
    let texts: Vec<&str> = chunk.iter().map(|(_, c)| c.as_str()).collect();
    let embeddings = match emb.embed_documents(&texts) {
        Ok(embs) => embs,
        Err(e) => {
            tracing::warn!("vec_bounded_memory backfill: batch embed failed: {}", e);
            return if is_permanent_backfill_error(&e) {
                BackfillChunkOutcome::Permanent
            } else {
                BackfillChunkOutcome::Failed
            };
        }
    };
    let db = crate::util::recover_poison(&state.db);
    db.rollback_if_in_tx();
    if let Err(e) = db.insert_backfill_vec_chunk(chunk, &embeddings) {
        tracing::warn!("vec_bounded_memory backfill: insert failed: {}", e);
        return BackfillChunkOutcome::Failed;
    }
    BackfillChunkOutcome::Done
}

fn fetch_pending_backfill(state: &AppState) -> Option<Vec<(i64, String)>> {
    let db = crate::util::recover_poison(&state.db);
    db.rollback_if_in_tx();
    match db.prepare_bounded_memory_vec_backfill() {
        Ok(items) => Some(items),
        Err(e) => {
            tracing::warn!("vec_bounded_memory backfill skipped: {}", e);
            None
        }
    }
}

pub(super) fn run_startup_vec_backfill_blocking(state: &AppState) {
    let Some(ref emb) = state.embedder else {
        return;
    };
    // STEP 6 跨进程成本守卫：另一进程正在回填本 profile（Contended）则跳过——
    // 持有者的 drain loop 会把本进程 pending atom 一并索引；锁文件打开/加锁失败
    // （Degraded）则 warn 后不加守卫继续，绝不阻塞启动。守卫持有到整段 drain 结束。
    let _guard = match try_acquire_backfill_lock(&state.config.profile_dir()) {
        BackfillLockOutcome::Acquired(f) => Some(f),
        BackfillLockOutcome::Contended => {
            tracing::info!(
                "vec_bounded_memory backfill skipped: another process holds backfill.lock"
            );
            return;
        }
        BackfillLockOutcome::Degraded => None,
    };
    // drain loop（共享 D7 helper）：重算候选直到某一轮清零本轮处理过的候选再释放
    // backfill.lock，使并发写入者（含因本进程持锁而跳过的进程）的 pending atom 本轮
    // 即被索引（races N1/C5）；持续 inflow 下不断推进直到 shutdown，不在不可索引 atom
    // 上空转（清零一轮即终止）。
    let batch_size = emb.batch_size().max(1);
    if let Err(e) = drain_backfill_until_no_progress(
        || Ok(fetch_pending_backfill(state)),
        |pending| {
            run_backfill_chunks_with_control(
                pending,
                batch_size,
                Some(state.shutdown.as_ref()),
                |chunk| Ok(backfill_one_chunk(state, emb, chunk)),
            )
        },
        Some(state.shutdown.as_ref()),
    ) {
        tracing::warn!("vec_bounded_memory backfill failed: {}", e);
    }
}

pub(super) fn spawn_startup_vec_backfill(state: AppState) -> Option<tokio::task::JoinHandle<()>> {
    state.embedder.as_ref()?;
    Some(tokio::spawn(async move {
        if let Err(e) = tokio::task::spawn_blocking(move || {
            run_startup_vec_backfill_blocking(&state);
        })
        .await
        {
            tracing::error!("vec_bounded_memory backfill task failed: {}", e);
        }
    }))
}
